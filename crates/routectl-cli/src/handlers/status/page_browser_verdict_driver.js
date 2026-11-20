// ---- scenarios: the verdict strip and the favicon ---------------------------
//
// Test-only snippet for page_browser_verdict_tests.rs, injected after
// page_browser_driver.js. A fetch layer over the driver's own: every rule set
// for a path applies in order, rewriting the answer's JSON, and any rule that
// fails it makes the answer fail the way an unreachable daemon does. A row a
// test supplies arrives as `window.routectlVerdictRow`, set by a snippet ahead
// of this one. Every wait is on a page state or an answer count, never on a
// span of time, so a slow machine only makes a run take longer.
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

  function answers(path) { return answered[path] || 0; }

  function nextAnswer(path, limitMs) {
    var target = answers(path) + 1;
    return d.waitUntil('the next ' + path + ' answer', function () {
      return answers(path) >= target;
    }, limitMs);
  }

  function onAggregate(rewrite) {
    return function (path) { return path === '/status' ? { rewrite: rewrite } : null; };
  }

  // The aggregate with `verdict` as the attention panel's verdict. The
  // envelope's as_of is what the strip's time is read from, so it is noted.
  var servedAsOf = null;
  function withVerdict(verdict) {
    return function (json) {
      var env = json.panels.attention;
      env.data.verdict = verdict;
      servedAsOf = env.as_of;
      return json;
    };
  }

  function faviconFill() {
    var href = document.getElementById('favicon').getAttribute('href');
    var m = /fill='%23([0-9a-f]{6})'/.exec(href);
    return m ? m[1] : href;
  }

  function strip() {
    var node = document.getElementById('verdict');
    var note = document.getElementById('verdict-note');
    var link = document.getElementById('verdict-link');
    var anchor = link.querySelector('a');
    return {
      text: document.getElementById('verdict-text').textContent,
      kind: ['ok', 'warn', 'bad', 'idle'].filter(function (k) {
        return node.classList.contains('verdict--' + k);
      }).join(','),
      dimmed: node.classList.contains('verdict--stale'),
      note: note.hidden ? null : note.textContent,
      link_children: link.childElementCount,
      link_text: anchor ? anchor.textContent : null,
      link_href: anchor ? anchor.getAttribute('href') : null,
      favicon: faviconFill(),
      stats: document.getElementById('verdict-stats').textContent
    };
  }

  // The time the strip should name for the served verdict, formatted the way
  // this browser formats a local time.
  function servedTime() {
    return new Date(servedAsOf).toLocaleTimeString();
  }

  // Every text the strip's sentence takes, in order, from the moment this is
  // called.
  function recordTexts() {
    var seen = [];
    var node = document.getElementById('verdict-text');
    new MutationObserver(function () { seen.push(node.textContent); })
      .observe(node, { childList: true, characterData: true, subtree: true });
    return seen;
  }

  function selectView(view) {
    var tab = document.getElementById('tab-' + view);
    if (tab.getAttribute('aria-selected') !== 'true') { tab.click(); }
  }

  // ---- a failing non-attention source leaves the verdict alone ------------

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

  // One row on a fresh page: an ok verdict, the row's view open on its
  // source's first answer, then that source broken until it shows on the
  // source itself and one more aggregate round has landed.
  d.scenario('verdict-source-fails', function () {
    var row = window.routectlVerdictRow;
    rules.push(onAggregate(withVerdict(row.verdict)));
    var seen = {};
    return d.waitUntil('the served verdict', function () {
      return strip().text === row.verdict.text;
    }).then(function () {
      selectView(row.view);
      return nextAnswer(row.path, 30000);
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
      seen.after = strip();
      return seen;
    });
  });

  // ---- before the first verdict -------------------------------------------

  d.scenario('verdict-before-first', function () {
    var held = true;
    d.hold(function (url) { return (held && url.pathname === '/status') ? 3000 : 0; });
    rules.push(onAggregate(withVerdict(window.routectlVerdictRow.verdict)));
    var out = {};
    return d.waitUntil('the start-up text', function () {
      return strip().text !== '';
    }).then(function () {
      out.held = strip();
      held = false;
      return d.waitUntil('the served verdict', function () {
        return strip().text === window.routectlVerdictRow.verdict.text;
      }, 30000);
    }).then(function () {
      out.answered = strip();
      return out;
    });
  });

  // ---- a verdict as served ------------------------------------------------

  d.scenario('verdict-live', function () {
    var row = window.routectlVerdictRow;
    rules.push(onAggregate(withVerdict(row.verdict)));
    return d.waitUntil('the served verdict', function () {
      return strip().text === row.verdict.text;
    }, 30000).then(function () {
      var out = strip();
      out.marked_up = document.getElementById('verdict').querySelectorAll('b, i, script, img').length;
      return out;
    });
  });

  // ---- the last known verdict through an outage ---------------------------

  // A verdict lands; the daemon then fails `row.failures` aggregate rounds
  // in a row, observed after each; then it answers again with `row.after`.
  // Every sentence the strip takes from the first verdict on is recorded.
  d.scenario('verdict-outage', function () {
    var row = window.routectlVerdictRow;
    var verdict = row.verdict;
    rules.push(onAggregate(function (json) { return withVerdict(verdict)(json); }));
    var failing = false;
    rules.push(function (path) { return (failing && path === '/status') ? { fail: true } : null; });
    var out = { failed: [] };
    var texts = null;
    var step = d.waitUntil('the served verdict', function () {
      return strip().text === verdict.text;
    }, 30000).then(function () {
      texts = recordTexts();
      out.live = strip();
      out.served_time = servedTime();
      failing = true;
    });
    for (var i = 0; i < row.failures; i++) {
      step = step.then(function () {
        return nextAnswer('/status', 40000);
      }).then(function () {
        return d.waitUntil('the strip to mark the failure', function () {
          return strip().dimmed;
        });
      }).then(function () {
        out.failed.push(strip());
      });
    }
    return step.then(function () {
      verdict = row.after;
      failing = false;
      return d.waitUntil('the recovered verdict', function () {
        return strip().text === row.after.text;
      }, 45000);
    }).then(function () {
      out.recovered = strip();
      out.texts = texts;
      return out;
    });
  });

  // ---- a verdict the panel stops serving ----------------------------------

  // A verdict lands, then the attention panel answers with a refusal while
  // the daemon itself keeps answering.
  d.scenario('verdict-panel-unavailable', function () {
    var row = window.routectlVerdictRow;
    var refusing = false;
    rules.push(onAggregate(function (json) {
      if (!refusing) { return withVerdict(row.verdict)(json); }
      json.panels.attention = brokenEnvelope(json.panels.attention, 'unavailable');
      return json;
    }));
    var out = {};
    return d.waitUntil('the served verdict', function () {
      return strip().text === row.verdict.text;
    }, 30000).then(function () {
      out.served_time = servedTime();
      refusing = true;
      return d.waitUntil('the strip to mark the refusal', function () {
        return strip().dimmed;
      }, 30000);
    }).then(function () {
      out.refused = strip();
      return out;
    });
  });

  // ---- the target link ----------------------------------------------------

  // A verdict whose target names a view links to it; following the link
  // opens that view.
  d.scenario('verdict-link-opens', function () {
    var row = window.routectlVerdictRow;
    rules.push(onAggregate(withVerdict(row.verdict)));
    var out = {};
    return d.waitUntil('the verdict link', function () {
      return document.querySelector('#verdict-link a');
    }, 30000).then(function (link) {
      out.before = strip();
      link.click();
      return d.waitUntil('the linked view', function () {
        var tab = document.getElementById('tab-' + row.verdict.target);
        return tab.getAttribute('aria-selected') === 'true';
      });
    }).then(function () {
      out.hash = location.hash;
      out.pane_hidden = document.getElementById('pane-' + row.verdict.target).hidden;
      out.after = strip();
      return out;
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
})(window.routectlDriver);
