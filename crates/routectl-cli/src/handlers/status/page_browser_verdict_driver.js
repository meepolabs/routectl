// ---- scenarios: the verdict strip and the favicon ---------------------------
//
// Test-only snippet for page_browser_verdict_tests.rs, injected after
// page_browser_driver.js. A fetch layer over the driver's own: every rule set
// for a path applies in order, rewriting the answer's JSON, and any rule that
// fails it makes the answer fail the way an unreachable daemon does. A row a
// test supplies arrives as `window.routectlVerdictRow`, set by a snippet ahead
// of this one.
(function (d) {
  'use strict';

  var inner = window.fetch;
  var rules = [];
  var answered = {};

  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    var picked = rules.map(function (rule) { return rule(url.pathname); })
      .filter(function (r) { return !!r; });
    return inner(input, init).then(function (resp) {
      return resp.text().then(function (text) {
        answered[url.pathname] = (answered[url.pathname] || 0) + 1;
        if (picked.some(function (r) { return r.fail; })) { throw new TypeError('Failed to fetch'); }
        var rewrites = picked.filter(function (r) { return r.rewrite; });
        var body = rewrites.length
          ? JSON.stringify(rewrites.reduce(function (json, r) { return r.rewrite(json); }, JSON.parse(text)))
          : text;
        return new Response(body, { status: resp.status, headers: resp.headers });
      });
    });
  };

  function nextAnswer(path, limitMs) {
    var target = (answered[path] || 0) + 1;
    return d.waitUntil('the next ' + path + ' answer', function () {
      return (answered[path] || 0) >= target;
    }, limitMs);
  }

  // Every pool full and no target with an eligibility reason, so the verdict
  // reads healthy and any source that moved it would show.
  function healthyRouting(json) {
    var data = json.panels.health.data;
    data.pools.forEach(function (p) { p.eligible = p.configured; });
    data.targets.forEach(function (t) { t.eligibility.reason = null; });
    return json;
  }

  function onAggregate(rewrite) {
    return function (path) { return path === '/status' ? { rewrite: rewrite } : null; };
  }

  function faviconFill() {
    var href = document.getElementById('favicon').getAttribute('href');
    var m = /fill='%23([0-9a-f]{6})'/.exec(href);
    return m ? m[1] : href;
  }

  function strip() {
    var node = document.getElementById('verdict');
    return {
      text: document.getElementById('verdict-text').textContent,
      kind: ['ok', 'warn', 'bad', 'idle'].filter(function (k) {
        return node.classList.contains('verdict--' + k);
      }).join(','),
      favicon: faviconFill(),
      stats: document.getElementById('verdict-stats').textContent
    };
  }

  function selectView(view) {
    var tab = document.getElementById('tab-' + view);
    if (tab.getAttribute('aria-selected') !== 'true') { tab.click(); }
  }

  // ---- a failing doctor, usage or QUERY source leaves the verdict alone ---

  // How a row breaks its source's answer: `unavailable` swaps the envelope
  // for a refusal, `invalid` keeps the data and adds a refusal (an envelope
  // that contradicts itself).
  function brokenEnvelope(env, mode) {
    if (mode === 'unavailable') {
      return { schema_version: env.schema_version, as_of: null, data: null, unavailable: 'db_unavailable' };
    }
    env.unavailable = 'db_unavailable';
    return env;
  }

  function ruleFor(row) {
    return function (path) {
      if (path !== row.path) { return null; }
      return {
        rewrite: function (json) {
          if (row.panel) {
            json.panels[row.panel] = brokenEnvelope(json.panels[row.panel], row.mode);
            return json;
          }
          return brokenEnvelope(json, row.mode);
        }
      };
    };
  }

  // The row's own evidence that its source really broke: the error card in
  // a section bound to that source.
  function controlShows(row) {
    return !!document.querySelector(row.card);
  }

  // One row on a fresh page: routing healthy, the row's view open on its
  // source's first answer, then that source broken until it shows on the
  // source itself and one more aggregate round has landed.
  d.scenario('verdict-source-fails', function () {
    var row = window.routectlVerdictRow;
    rules.push(onAggregate(healthyRouting));
    var seen = {};
    return d.waitUntil('a health verdict', function () {
      return strip().text === 'All routes healthy';
    }).then(function () {
      selectView(row.view);
      return nextAnswer(row.path, 30000);
    }).then(function () {
      return d.sleep(200);
    }).then(function () {
      seen.before = strip();
      seen.control_before = controlShows(row);
      rules.push(ruleFor(row));
      return d.waitUntil('the failure to show on its source', function () {
        return controlShows(row);
      }, 40000);
    }).then(function () {
      seen.control_after = true;
      return nextAnswer('/status', 30000);
    }).then(function () {
      return d.sleep(200);
    }).then(function () {
      seen.after = strip();
      return seen;
    });
  });

  // ---- before the first health payload -----------------------------------

  d.scenario('verdict-before-first-health', function () {
    var held = true;
    d.hold(function (url) { return (held && url.pathname === '/status') ? 3000 : 0; });
    var out = {};
    return d.sleep(1500).then(function () {
      out.held = strip();
      held = false;
      return nextAnswer('/status', 30000);
    }).then(function () {
      out.answered = strip();
      return out;
    });
  });

  // ---- an empty pool ------------------------------------------------------

  function emptyFirstPool(json) {
    json.panels.health.data.pools[0].eligible = 0;
    return json;
  }

  d.scenario('verdict-empty-pool', function () {
    rules.push(onAggregate(emptyFirstPool));
    return d.waitUntil('a seat verdict', function () {
      return strip().kind !== 'idle';
    }).then(function () {
      return strip();
    });
  });

  // ---- the request count beside a span it does not cover ------------------

  d.scenario('verdict-stats-unanswered', function () {
    var holding = true;
    d.hold(function (url) { return (holding && url.pathname === '/status/query') ? 8000 : 0; });
    var out = {};
    return nextAnswer('/status', 30000).then(function () {
      return nextAnswer('/status', 30000);
    }).then(function () {
      out.unanswered = strip();
      holding = false;
      return d.waitUntil('a QUERY answer', function () {
        return /\d/.test(strip().stats);
      }, 45000);
    }).then(function () {
      out.answered = strip();
      return out;
    });
  });

  // ---- a network failure after a good round -------------------------------

  d.scenario('verdict-network-failure', function () {
    rules.push(onAggregate(healthyRouting));
    var failing = false;
    rules.push(function () { return failing ? { fail: true } : null; });
    var out = {};
    return d.waitUntil('a health verdict', function () {
      return strip().text === 'All routes healthy';
    }).then(function () {
      return nextAnswer('/status', 30000);
    }).then(function () {
      out.good = strip();
      failing = true;
      return nextAnswer('/status', 30000);
    }).then(function () {
      return d.sleep(200);
    }).then(function () {
      out.failed = strip();
      out.health_badge = document.getElementById('badge-accounts').textContent;
      return out;
    });
  });
})(window.routectlDriver);
