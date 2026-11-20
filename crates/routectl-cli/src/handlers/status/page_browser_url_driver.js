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
  var inner = window.fetch;
  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    if (url.pathname === '/status/query') { sent.push(JSON.parse(init.body)); }
    return inner(input, init).then(function (resp) {
      if (url.pathname !== '/status/query' || !resp.ok) { return resp; }
      return resp.clone().json().then(function (json) {
        answers.push({ body: JSON.parse(init.body), json: json });
        return resp;
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

  // Open rows, dialogs, focus and scroll leave the address alone; the tab
  // click in the middle is the control proving the observation sees a change.
  d.scenario('url-ui-state-stays-out', function () {
    var obs = [];
    function observe(name) {
      obs.push({ name: name, hash: location.hash, history_length: history.length });
    }
    return waitSettled().then(function () {
      observe('start');
      return d.waitForElement('overview-seats-kclaude_2d_seat');
    }).then(function (open) {
      open.click();
      return d.waitForElement('overview-seats-close-kclaude_2d_seat');
    }).then(function (close) {
      close.focus();
      window.scrollTo(0, 400);
      return d.sleep(200);
    }).then(function () {
      observe('dialog open, focus inside, scrolled');
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
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

  // Each row's address is followed as a link would be: from Config, which
  // asks no QUERY, so the one QUERY Now issues for it carries the scope the
  // page settled on.
  d.scenario('url-scope-rows', function () {
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
  // verbatim and no element is made from it.
  d.scenario('url-scope-text', function () {
    return d.waitUntil('the scoped Now figures', function () {
      return settled() && document.querySelector('#body-now .card-title');
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
})(window.routectlDriver);
