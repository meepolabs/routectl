// Test-only driver for the headless-Chrome page tests. page_browser_tests.rs
// injects it, any topic snippets and one `routectlDriver.run(<name>)` call
// just before </body>, after the page script; the served page never carries
// it. A scenario runs on the page's own clock, which headless Chrome advances
// as virtual time, and its result is written as JSON into one <pre> element
// that the test reads back from --dump-dom.
//
// Virtual time does not advance while a real fetch is pending, so a scenario
// that needs an answer held or reordered registers a hold rule: the real
// answer is read at once and handed to the page only after the rule's delay
// on the page's timers. A scenario that needs an answer shaped differently
// from the fixtures registers a rewrite rule over the answer's text.
(function () {
  'use strict';

  var RESULT_ID = 'browser-driver-result';
  var WAIT_LIMIT_MS = 20000;
  var POLL_MS = 25;
  // Long enough for the page to parse an answer and finish its render pass.
  var SETTLE_MS = 100;

  var scenarios = {};
  var requests = [];
  var delivered = {};
  var holdRules = [];
  var rewriteRules = [];
  var realFetch = window.fetch.bind(window);

  function abortError() {
    return new DOMException('The operation was aborted.', 'AbortError');
  }

  // Resolve after `ms` of page time, or reject the way the page's own fetch
  // would if `signal` aborts first.
  function heldFor(ms, signal) {
    if (ms <= 0) { return Promise.resolve(); }
    return new Promise(function (resolve, reject) {
      if (signal && signal.aborted) { reject(abortError()); return; }
      var timer = setTimeout(function () {
        if (signal) { signal.removeEventListener('abort', onAbort); }
        resolve();
      }, ms);
      function onAbort() { clearTimeout(timer); reject(abortError()); }
      if (signal) { signal.addEventListener('abort', onAbort); }
    });
  }

  // An answer's body as the page receives it: each rewrite rule maps the
  // text, given the URL and how many answers for that path came before.
  function rewritten(url, text) {
    var n = delivered[url.pathname] || 0;
    return rewriteRules.reduce(function (body, rule) { return rule(url, body, n); }, text);
  }

  function holdFor(url, init) {
    var ms = 0;
    holdRules.forEach(function (rule) { ms = Math.max(ms, Number(rule(url, init)) || 0); });
    return ms;
  }

  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    var signal = init && init.signal;
    requests.push({
      url: url.href,
      method: (init && init.method) || 'GET',
      body: (init && typeof init.body === 'string') ? init.body : null
    });
    var holdMs = holdFor(url, init);
    return realFetch(input, init).then(function (resp) {
      return resp.text().then(function (text) {
        return heldFor(holdMs, signal).then(function () {
          var body = rewritten(url, text);
          delivered[url.pathname] = (delivered[url.pathname] || 0) + 1;
          return new Response(body, {
            status: resp.status,
            statusText: resp.statusText,
            headers: resp.headers
          });
        });
      });
    });
  };

  function sleep(ms) {
    return new Promise(function (resolve) { setTimeout(resolve, ms); });
  }

  function waitUntil(label, test, limitMs) {
    var limit = limitMs || WAIT_LIMIT_MS;
    var waited = 0;
    return new Promise(function (resolve, reject) {
      (function poll() {
        var value = test();
        if (value) { setTimeout(function () { resolve(value); }, SETTLE_MS); return; }
        if (waited >= limit) { reject(new Error('waited ' + limit + ' ms for ' + label)); return; }
        waited += POLL_MS;
        setTimeout(poll, POLL_MS);
      })();
    });
  }

  // Wait until `n` more answers for `path` have been handed to the page,
  // counted from the call.
  function waitFor(path, n) {
    var target = (delivered[path] || 0) + n;
    return waitUntil(n + ' more ' + path + ' answers', function () {
      return (delivered[path] || 0) >= target;
    });
  }

  function waitForElement(id) {
    return waitUntil('#' + id, function () { return document.getElementById(id); });
  }

  // `observe()` now, then once after each of `rounds` further answers for
  // `path`: { baseline, rounds: [...] }.
  function acrossRounds(path, rounds, observe) {
    var out = { baseline: observe(), rounds: [] };
    var step = Promise.resolve();
    for (var i = 0; i < rounds; i++) {
      step = step.then(function () { return waitFor(path, 1); })
        .then(function () { out.rounds.push(observe()); });
    }
    return step.then(function () { return out; });
  }

  // Focus `node`, clear any selection, and press Ctrl+A on it as a keydown
  // that bubbles from the focused element the way a key press does. Reports
  // the focus, whether the page took over the key, and the selection after.
  function pressSelectAll(node) {
    node.focus();
    window.getSelection().removeAllRanges();
    var ev = new KeyboardEvent('keydown', {
      key: 'a', code: 'KeyA', ctrlKey: true, bubbles: true, cancelable: true
    });
    document.activeElement.dispatchEvent(ev);
    var out = {
      focused: document.activeElement === node,
      prevented: ev.defaultPrevented,
      selection: window.getSelection().toString()
    };
    window.getSelection().removeAllRanges();
    return out;
  }

  function report(out) {
    out.requests = requests;
    var pre = document.createElement('pre');
    pre.id = RESULT_ID;
    pre.textContent = JSON.stringify(out);
    document.body.appendChild(pre);
  }

  function run(name) {
    var fn = scenarios[name];
    var started = fn
      ? Promise.resolve().then(function () { return fn(api); })
      : Promise.reject(new Error('no scenario named ' + name));
    started.then(function (result) {
      report({ scenario: name, done: true, error: null, result: result });
    }, function (err) {
      report({ scenario: name, done: false, error: String((err && err.stack) || err), result: null });
    });
  }

  var api = {
    scenario: function (name, fn) { scenarios[name] = fn; },
    hold: function (rule) { holdRules.push(rule); },
    rewrite: function (rule) { rewriteRules.push(rule); },
    run: run,
    sleep: sleep,
    waitUntil: waitUntil,
    waitFor: waitFor,
    waitForElement: waitForElement,
    acrossRounds: acrossRounds,
    pressSelectAll: pressSelectAll
  };
  window.routectlDriver = api;
})();

// ---- scenarios: a refresh keeps the nodes the operator is using ----------
//
// Each observation reads node identity from the reference taken at setup
// and state from the live document, and carries one `figure` text whose
// change across rounds proves the rounds repainted.
(function (d) {
  'use strict';

  function byId(id) {
    var node = document.getElementById(id);
    if (!node) { throw new Error('no element #' + id); }
    return node;
  }

  function textOf(selector) {
    var node = document.querySelector(selector);
    if (!node) { throw new Error('no element matches ' + selector); }
    return node.textContent;
  }

  function focusObservation(node, figureSelector) {
    return {
      same_node: node === document.getElementById(node.id),
      connected: node.isConnected,
      focused: document.activeElement === node,
      figure: textOf(figureSelector)
    };
  }

  d.scenario('overview-provider-focus', function () {
    return d.waitForElement('overview-provider-all').then(function (btn) {
      btn.focus();
      return d.acrossRounds('/status/query', 2, function () {
        return focusObservation(btn, '#overview-provider-all .provcard-facts');
      });
    });
  });

  d.scenario('usage-groupby-focus', function () {
    return d.waitForElement('usage-groupby-kalias').then(function (btn) {
      btn.focus();
      btn.click();
      return d.waitFor('/status/query', 1).then(function () {
        return d.acrossRounds('/status/query', 2, function () {
          var obs = focusObservation(btn, '#traffic-groups tbody');
          obs.pressed = byId('usage-groupby-kalias').getAttribute('aria-pressed') === 'true';
          return obs;
        });
      });
    });
  });

  d.scenario('config-row-focus', function () {
    return d.waitForElement('config-models-kglm-toggle').then(function (toggle) {
      toggle.focus();
      toggle.click();
      var detail = byId('config-models-kglm-detail');
      var cells = byId('config-capabilities-cells');
      cells.open = true;
      return d.acrossRounds('/status', 2, function () {
        var obs = focusObservation(toggle, '#body-config .cfgstrip');
        obs.expanded = byId('config-models-kglm-toggle').getAttribute('aria-expanded') === 'true';
        obs.same_detail = detail === document.getElementById(detail.id);
        obs.detail_shown = !byId('config-models-kglm-detail').hidden;
        obs.same_disclosure = cells === document.getElementById(cells.id);
        obs.disclosure_open = byId('config-capabilities-cells').open;
        return obs;
      });
    });
  });

  d.scenario('doctor-selection', function () {
    return d.waitUntil('a doctor fix command', function () {
      return document.querySelector('#body-doctor .docfix-text');
    }).then(function (fix) {
      var node = fix.firstChild;
      var range = document.createRange();
      range.setStart(node, 0);
      range.setEnd(node, node.nodeValue.length);
      var sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
      var picked = sel.toString();
      return d.acrossRounds('/status', 2, function () {
        var now = window.getSelection();
        return {
          picked: picked,
          selection: now.toString(),
          same_text: picked !== '' && now.toString() === picked,
          same_anchor: now.anchorNode === node,
          connected: node.isConnected,
          figure: textOf('#body-doctor .docmatrix-replay')
        };
      });
    });
  });

  // The health fixture carries no figure that moves between rounds, so each
  // answer gives the first dispatch target one more rpm of headroom.
  d.scenario('models-target-kept', function () {
    var cardId = 'health-target-ksonnet';
    d.rewrite(function (url, text, n) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      agg.panels.health.data.targets[0].rpm_available = 40 + n;
      return JSON.stringify(agg);
    });
    return d.waitForElement(cardId).then(function (card) {
      var rpm = card.querySelector('.hcell-value [data-v]');
      return d.acrossRounds('/status', 2, function () {
        var live = document.getElementById(cardId);
        return {
          same_card: card === live,
          same_figure_node: !!live && rpm === live.querySelector('.hcell-value [data-v]'),
          connected: card.isConnected && rpm.isConnected,
          figure: rpm.textContent
        };
      });
    });
  });
})(window.routectlDriver);
