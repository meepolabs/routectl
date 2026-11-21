// ---- scenarios: the view state in the address ------------------------------
//
// Test-only snippet injected after page_browser_driver.js. Every QUERY the
// page sends is paired with the answer it got, so a scenario can tie a figure
// on screen to the request that produced it. Scenario inputs a test supplies
// arrive as `window.routectlUrlRows`, set by a snippet ahead of this one.
(function (d) {
  'use strict';

  var answers = [];
  var sent = [];
  // Hold rules over QUERY answers: the first unused rule whose `match` takes
  // a request body holds that answer `ms` on the page's timers; a `deaf`
  // rule also hands it to the page after the page aborted the request.
  var holds = [];
  var inner = window.fetch;

  function takeHold(body) {
    for (var i = 0; i < holds.length; i++) {
      if (!holds[i].used && holds[i].match(body)) {
        holds[i].used = true;
        holds[i].issued = true;
        return holds[i];
      }
    }
    return null;
  }

  function withoutSignal(init) {
    var copy = {};
    Object.keys(init || {}).forEach(function (k) { if (k !== 'signal') { copy[k] = init[k]; } });
    return copy;
  }

  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    var body = url.pathname === '/status/query' ? JSON.parse(init.body) : null;
    if (body) { sent.push(body); }
    var hold = body ? takeHold(body) : null;
    var call = (hold && hold.deaf) ? inner(input, withoutSignal(init)) : inner(input, init);
    return call.then(function (resp) {
      if (!body || !resp.ok) { return resp; }
      return resp.clone().json().then(function (json) {
        return d.sleep(hold ? hold.ms : 0).then(function () {
          answers.push({ body: body, json: json, held: !!hold });
          if (hold) { hold.delivered = true; }
          return resp;
        });
      });
    });
  };

  function activeView() {
    var tab = document.querySelector('#tabbar [aria-selected="true"]');
    return tab ? tab.getAttribute('data-tab') : null;
  }

  function pressed(selector) {
    var node = document.querySelector(selector + ' [aria-pressed="true"]');
    return node ? node.textContent.trim() : null;
  }

  function queryBodies() {
    return sent.slice();
  }

  function settled() {
    var body = document.getElementById('body-' + activeView());
    return !!body && body.childElementCount > 0 && !body.querySelector('.skeleton');
  }

  function waitSettled() {
    return d.waitUntil('the active view to settle', settled);
  }

  function nextHashChange() {
    return new Promise(function (resolve) {
      window.addEventListener('hashchange', function () {
        setTimeout(resolve, 0);
      }, { once: true });
    });
  }

  function requestsFigure() {
    var tiles = document.querySelectorAll('#body-now .kpi');
    for (var i = 0; i < tiles.length; i++) {
      if (tiles[i].querySelector('.kpi-label').textContent === 'Requests') {
        return tiles[i].querySelector('.kpi-value').textContent;
      }
    }
    throw new Error('no Requests tile on Now');
  }

  function lastAnswer() {
    return answers.length ? answers[answers.length - 1] : null;
  }

  function stateReport() {
    return {
      hash: location.hash,
      view: activeView(),
      range: pressed('#windowsel'),
      group: pressed('#body-traffic .gbseg'),
      titles: Array.prototype.map.call(
        document.querySelectorAll('#body-' + activeView() + ' .card-title'),
        function (n) { return n.textContent; }),
      history_length: history.length,
      queries: queryBodies()
    };
  }

  // What the Now view shows beside the answer it was drawn from.
  function figureReport() {
    var answer = lastAnswer();
    return {
      hash: location.hash,
      range: pressed('#windowsel'),
      history_length: history.length,
      figure: requestsFigure(),
      answered_window: answer ? answer.body.window : null,
      answered_requests: answer ? String(answer.json.data.totals.requests) : null,
      queries: queryBodies().length
    };
  }

  function click(id) {
    var node = document.getElementById(id);
    if (!node) { throw new Error('no element #' + id); }
    node.click();
  }

  d.scenario('url-state', function () {
    return waitSettled().then(stateReport);
  });

  // Load, report, reload the same address, report again. The driver runs on
  // each load; sessionStorage says which side of the reload this one is.
  var RELOAD_KEY = 'routectl-url-reload';
  d.scenario('url-reload', function () {
    var before = sessionStorage.getItem(RELOAD_KEY);
    if (before === null) {
      return waitSettled().then(function () {
        sessionStorage.setItem(RELOAD_KEY, JSON.stringify(stateReport()));
        location.reload();
        return new Promise(function () {});
      });
    }
    return waitSettled().then(function () {
      return { before: JSON.parse(before), after: stateReport() };
    });
  });

  // One range click is one history entry, and Back brings the prior range
  // back with figures from a QUERY issued for it.
  d.scenario('url-range-history', function () {
    var out = {};
    var week = document.querySelector('#windowsel [data-window="week"]');
    function answeredSince(from, range) {
      return answers.slice(from).filter(function (a) { return a.body.window === range; })
        .map(function (a) { return String(a.json.data.totals.requests); });
    }
    return d.waitUntil('the Now figures', function () {
      return settled() && answers.length > 0;
    }).then(function () {
      out.start = figureReport();
      var from = answers.length;
      week.click();
      out.clicked = { history_length: history.length, hash: location.hash };
      return d.waitUntil('the week answer', function () {
        return settled() && answeredSince(from, 'week').length > 0;
      });
    }).then(function () {
      out.week = figureReport();
      var queriesBefore = sent.length;
      var from = answers.length;
      var back = nextHashChange();
      history.back();
      return back.then(function () {
        out.back_issued = sent.slice(queriesBefore);
        return d.waitUntil('the today answer after Back', function () {
          return settled() && answeredSince(from, 'today').length > 0;
        });
      }).then(function () {
        out.back = figureReport();
        out.back.today_answers = answeredSince(from, 'today');
      });
    }).then(function () {
      return out;
    });
  });

  function rangeButton(range) {
    var node = document.querySelector('#windowsel [data-window="' + range + '"]');
    if (!node) { throw new Error('no picker button for ' + range); }
    return node;
  }

  function coversReport() {
    var covers = document.getElementById('window-covers');
    var rect = covers.getBoundingClientRect();
    return {
      range: document.querySelector('#windowsel [aria-pressed="true"]').getAttribute('data-window'),
      text: covers.textContent,
      shown: !covers.hidden && rect.width > 0 && rect.height > 0,
      beside_picker: covers.parentNode === document.getElementById('windowsel').parentNode
    };
  }

  // Each range in `routectlUrlRows` is clicked in turn; for each, the QUERY
  // bodies sent after the click once its answer applied, and the strip's
  // req/span line drawn from that answer.
  d.scenario('url-range-pick', function () {
    var rows = window.routectlUrlRows || [];
    var out = [];
    var chain = waitSettled();
    rows.forEach(function (range) {
      chain = chain.then(function () {
        var from = sent.length;
        var answered = answers.length;
        rangeButton(range).click();
        return d.waitUntil('the ' + range + ' answer', function () {
          return settled() && answers.slice(answered).some(function (a) {
            return a.body.window === range;
          });
        }).then(function () {
          out.push({
            range: range,
            hash: location.hash,
            view: activeView(),
            queries: sent.slice(from),
            stats: document.getElementById('verdict-stats').textContent
          });
        });
      });
    });
    return chain.then(function () { return out; });
  });

  // The covers line for each range in `routectlUrlRows`, clicked in turn,
  // then on a view the picker does not apply to.
  d.scenario('url-range-covers', function () {
    var rows = window.routectlUrlRows || [];
    var out = { ranges: [] };
    var chain = waitSettled();
    rows.forEach(function (range) {
      chain = chain.then(function () {
        rangeButton(range).click();
        return waitSettled();
      }).then(function () {
        out.ranges.push(coversReport());
      });
    });
    return chain.then(function () {
      click('tab-models');
      return waitSettled();
    }).then(function () {
      out.windowless = coversReport();
      return out;
    });
  });

  // Every user control that changes the selection pushes one entry; a
  // control that changes nothing in the selection pushes none.
  d.scenario('url-controls-history', function () {
    var steps = [];
    function step(name, act, ready) {
      return function () {
        var before = history.length;
        act();
        return d.waitUntil(name, ready).then(function () {
          steps.push({ name: name, added: history.length - before, hash: location.hash });
        });
      };
    }
    var chain = waitSettled()
      .then(step('range', function () {
        document.querySelector('#windowsel [data-window="week"]').click();
      }, settled))
      .then(step('scope', function () { click('overview-provider-kanthropic'); }, settled))
      .then(step('view', function () { click('tab-traffic'); }, settled))
      .then(step('group', function () { click('usage-groupby-kalias'); }, settled))
      .then(step('same range again', function () {
        document.querySelector('#windowsel [data-window="week"]').click();
      }, settled));
    return chain.then(function () { return steps; });
  });

  // Open rows, focus and scroll leave the address alone; the tab click in
  // the middle is the control proving the observation sees a change.
  d.scenario('url-ui-state-stays-out', function () {
    var obs = [];
    function observe(name) {
      obs.push({ name: name, hash: location.hash, history_length: history.length });
    }
    return waitSettled().then(function () {
      observe('start');
      return d.waitUntil('a glance link', function () {
        return document.querySelector('#body-now .glance-link');
      });
    }).then(function (link) {
      link.focus();
      window.scrollTo(0, 400);
      return d.sleep(200);
    }).then(function () {
      observe('focused, scrolled');
      click('tab-config');
      return d.waitForElement('config-models-kglm-toggle');
    }).then(function (toggle) {
      observe('config tab');
      toggle.focus();
      toggle.click();
      return d.sleep(200);
    }).then(function () {
      observe('row open, focused');
      return obs;
    });
  });

  // On Now, the provider card named `routectlUrlCardLabel` (opening the
  // overflow when it sits there) is clicked, then the anthropic card: the
  // address and the QUERY bodies after each click.
  d.scenario('url-card-scope', function () {
    var label = window.routectlUrlCardLabel;
    var out = {};
    function card(name) {
      return Array.prototype.filter.call(document.querySelectorAll('#body-now .provcard-scope'),
        function (btn) { return btn.querySelector('.provcard-name').textContent === name; })[0];
    }
    function press(name, key) {
      var from = sent.length;
      var btn = card(name);
      if (!btn) { throw new Error('no provider card ' + name); }
      btn.click();
      return d.sleep(1000).then(waitSettled).then(function () {
        out[key] = {
          hash: location.hash,
          action: btn.getAttribute('data-action'),
          disabled: btn.disabled,
          queries: sent.slice(from)
        };
      });
    }
    return d.waitUntil('the provider cards', function () {
      return settled() && document.getElementById('overview-provider-more');
    }).then(function () {
      if (!card(label)) { click('overview-provider-more'); }
      return d.waitUntil('the ' + label + ' card', function () { return card(label); });
    }).then(function () {
      return press(label, 'placeholder');
    }).then(function () {
      return press('anthropic', 'provider');
    }).then(function () { return out; });
  });

  // Each row's address is followed as a link would be: from Config, which
  // asks no QUERY, so every QUERY sent after the arrival is the one the
  // address led to. The row is read once the view settled on its answer.
  d.scenario('url-param-rows', function () {
    var rows = window.routectlUrlRows || [];
    var out = [];
    var chain = waitSettled();
    rows.forEach(function (raw) {
      chain = chain.then(function () {
        if (location.hash === '#config') { return null; }
        var left = nextHashChange();
        location.hash = '#config';
        return left;
      }).then(function () {
        var issued = queryBodies().length;
        var arrived = nextHashChange();
        location.hash = raw;
        return arrived.then(function () {
          return d.waitUntil('a QUERY for ' + raw, function () {
            return queryBodies().length > issued;
          });
        }).then(function () {
          return d.sleep(1000);
        }).then(waitSettled).then(function () {
          out.push({
            raw: raw,
            hash: location.hash,
            view: activeView(),
            queries: queryBodies().slice(issued)
          });
        });
      });
    });
    return chain.then(function () { return out; });
  });

  // How long the page's own load handler ran; that handler is where the page
  // first reads the address. This snippet runs while the page is still
  // parsing, after the page script registered its handler, so the mark comes
  // before that handler and the listener below runs after it. A test that
  // sets `routectlUrlLoadAddress` has it written in place here, before the
  // handler reads it: a command line cannot carry a long address. Only this
  // first load is timed, since headless virtual time stops the page clock in
  // the tasks that come after it.
  var loadMark = performance.now();
  var loadHandledMs = null;
  if (typeof window.routectlUrlLoadAddress === 'string') {
    history.replaceState(null, '', window.routectlUrlLoadAddress);
  }
  document.addEventListener('DOMContentLoaded', function () {
    loadHandledMs = performance.now() - loadMark;
  });

  d.scenario('url-scope-load-timing', function () {
    return waitSettled().then(function () {
      return { handled_ms: loadHandledMs, hash: location.hash, view: activeView() };
    });
  });

  // A scope that looks like markup is drawn as text: the label carries it
  // verbatim and no element is made from it. The ledger is made to carry a
  // provider of that name, the one way such a scope is ever kept.
  d.scenario('url-scope-text', function () {
    var label = window.routectlUrlLedgerLabel;
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status/query') { return text; }
      var env = JSON.parse(text);
      if (!env.data || !Array.isArray(env.data.groups) || !env.data.groups.length) { return text; }
      var extra = JSON.parse(JSON.stringify(env.data.groups[0]));
      extra.label = label;
      env.data.groups.push(extra);
      return JSON.stringify(env);
    });
    return d.waitUntil('the scoped Now figures', function () {
      return settled() && queryBodies().some(function (b) { return b.provider === label; }) &&
        document.querySelector('#body-now .card-title');
    }).then(function () {
      var titles = Array.prototype.map.call(
        document.querySelectorAll('#body-now .card-title'),
        function (n) { return n.textContent; });
      return {
        hash: location.hash,
        titles: titles,
        b_elements: document.querySelectorAll('b').length,
        queries: queryBodies()
      };
    });
  });

  // The config panel answers unavailable for the first `routectlUrlConfigDown`
  // aggregate rounds, then live. Reports the address and every QUERY body
  // while it is down, then the same once a QUERY has carried the scope.
  d.scenario('url-held-before-config', function () {
    var downRounds = window.routectlUrlConfigDown;
    d.rewrite(function (url, text, n) {
      if (url.pathname !== '/status' || n >= downRounds) { return text; }
      var agg = JSON.parse(text);
      agg.panels.config = {
        schema_version: agg.panels.config.schema_version,
        as_of: null,
        data: null,
        unavailable: 'config_unavailable'
      };
      return JSON.stringify(agg);
    });
    var out = {};
    return d.waitFor('/status', downRounds).then(function () {
      out.down = { hash: location.hash, queries: queryBodies(), titles: stateReport().titles };
      return d.waitUntil('a scoped QUERY answered', function () {
        return settled() && answers.some(function (a) { return a.body.model; });
      });
    }).then(function () {
      out.live = stateReport();
      return out;
    });
  });

  function focusedId() {
    var node = document.activeElement;
    return node === document.body ? 'body' : (node.id || node.tagName.toLowerCase());
  }

  // The strip names the model and provider scope; its reset, focused and
  // activated as a keyboard Enter activates it, lifts both.
  d.scenario('url-scope-strip-reset', function () {
    var out = {};
    return d.waitUntil('the scope strip', function () {
      return settled() && document.getElementById('usage-scope-reset');
    }).then(function (reset) {
      out.before = stateReport();
      out.reset_text = reset.textContent;
      var issued = queryBodies().length;
      var before = history.length;
      reset.focus();
      out.focused_before = focusedId();
      reset.click();
      out.added = history.length - before;
      out.focused_at_once = focusedId();
      return d.waitUntil('the unscoped answer', function () {
        return settled() && answers.length > 0 && queryBodies().length > issued;
      }).then(function () {
        out.after = stateReport();
        out.after.queries = queryBodies().slice(issued);
        out.after.reset_present = !!document.getElementById('usage-scope-reset');
        out.after.focused = focusedId();
        return d.waitFor('/status/query', 1);
      }).then(function () {
        out.focused_after_refresh = focusedId();
      });
    }).then(function () { return out; });
  });

  function verdictStats() {
    return document.getElementById('verdict-stats').textContent;
  }

  function stripTitle() {
    var head = document.querySelector('#body-traffic .card-title');
    return head ? head.textContent : null;
  }

  // Model A is on screen; A's next answer is held past the switch to model
  // B (and handed over even though the page aborted it), and B's own answer
  // is held a little so the switch has a window to show something. Every
  // repaint until both answers landed is sampled.
  d.scenario('url-model-switch-held', function () {
    var from = window.routectlUrlModelSwitch[0];
    var to = window.routectlUrlModelSwitch[1];
    var holdA = { match: function (b) { return b.model === from; }, ms: 3000, deaf: true };
    var holdB = { match: function (b) { return b.model === to; }, ms: 800, deaf: false };
    var samples = [];
    var sampler = null;
    var out = {};
    return d.waitUntil('model A on screen', function () {
      return settled() && answers.some(function (a) { return a.body.model === from; }) &&
        / req /.test(verdictStats());
    }).then(function () {
      out.a_shown = { stats: verdictStats(), title: stripTitle() };
      holds.push(holdA);
      return d.waitUntil('A held in flight', function () { return holdA.issued; }, 15000);
    }).then(function () {
      holds.push(holdB);
      sampler = setInterval(function () {
        samples.push({ stats: verdictStats(), title: stripTitle(), hash: location.hash });
      }, 10);
      var arrived = nextHashChange();
      location.hash = '#traffic?model=' + encodeURIComponent(to);
      return arrived;
    }).then(function () {
      samples.push({ stats: verdictStats(), title: stripTitle(), hash: location.hash, first: true });
      return d.waitUntil('both answers handed over', function () {
        return holdA.delivered && holdB.delivered;
      });
    }).then(function () {
      return d.sleep(300);
    }).then(function () {
      clearInterval(sampler);
      samples.push({ stats: verdictStats(), title: stripTitle(), hash: location.hash });
      out.samples = samples;
      out.answers = answers.map(function (a) {
        return { model: a.body.model || null, held: a.held, requests: a.json.data.totals.requests };
      });
      out.queries = queryBodies();
      return out;
    });
  });
})(window.routectlDriver);
