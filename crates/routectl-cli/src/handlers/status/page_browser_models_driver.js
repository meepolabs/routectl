// ---- scenarios: the Models view ------------------------------------------
//
// Test-only snippet for page_browser_models_tests.rs, injected after
// page_browser_driver.js. Inputs a test supplies arrive as
// `window.routectlModels`, set by a snippet ahead of this one:
//   holdLanesQuery    hold the seven-day per-model QUERY past the read
//   attentionDown     answer the attention panel as unavailable
//   recoveredLane     `{subject, minutes}`: add a recovered route item for
//                     that route, cleared that many minutes before as_of
//   spanLanes         `[ms, ...]`: add one problem lane per entry, named
//                     `span-<i>`, whose problem began that long before as_of
//                     and which no cause line covers
(function (d) {
  'use strict';

  var opts = window.routectlModels || {};
  var HOLD_MS = 60000;

  function isLanesQuery(init) {
    if (!init || typeof init.body !== 'string') { return false; }
    var body = JSON.parse(init.body);
    return body.window === 'last7d' && body.group_by === 'model' && !body.bucket;
  }

  d.hold(function (url, init) {
    return (opts.holdLanesQuery && url.pathname === '/status/query' && isLanesQuery(init)) ? HOLD_MS : 0;
  });

  // A copy of the first problem lane per `spanLanes` entry, each its own
  // lane and provider so it draws as its own row.
  function addSpanLanes(models) {
    var rows = models.data.rows;
    var model = rows.filter(function (row) { return row.state !== 'ok'; })[0];
    var asOf = Date.parse(models.as_of);
    opts.spanLanes.forEach(function (ms, i) {
      var row = JSON.parse(JSON.stringify(model));
      row.lane = 'span-' + i + '#up';
      row.provider = 'span-' + i;
      row.cause_key = null;
      row.since_ms = asOf - ms;
      rows.push(row);
    });
  }

  d.rewrite(function (url, text) {
    if (url.pathname !== '/status' || !(opts.attentionDown || opts.recoveredLane || opts.spanLanes)) {
      return text;
    }
    var agg = JSON.parse(text);
    if (opts.spanLanes) { addSpanLanes(agg.panels.models); }
    var attention = agg.panels.attention;
    if (opts.attentionDown) {
      agg.panels.attention = {
        schema_version: attention.schema_version,
        as_of: attention.as_of,
        unavailable: 'probe_down',
        data: null
      };
    }
    if (opts.recoveredLane) {
      var at = Date.parse(agg.panels.models.as_of) - opts.recoveredLane.minutes * 60000;
      attention.data.items.push({
        key: 'route_circuit_open:' + opts.recoveredLane.subject,
        kind: 'route_circuit_open',
        severity: 'degraded',
        state: 'recovered',
        subject: opts.recoveredLane.subject,
        account: null,
        cause: 'Circuit open',
        raw_token: 'breaker_open',
        since_ms: null,
        until_ms: null,
        recovered_at_ms: at,
        fix: null,
        remediation: null,
        target: 'models',
        targets: null
      });
    }
    return JSON.stringify(agg);
  });

  function textIn(node, selector) {
    var found = node.querySelector(selector);
    return found ? found.textContent : null;
  }

  // Every lane row and cause line of the view, in document order.
  function rows() {
    var body = document.getElementById('body-models');
    var out = [];
    Array.prototype.forEach.call(body.querySelectorAll('tbody > tr'), function (tr) {
      var table = tr.closest('table').id;
      if (tr.classList.contains('models-cause-row')) {
        out.push({ kind: 'cause', table: table, id: tr.id, text: textIn(tr, '.models-cause-text'),
          count: textIn(tr, '.models-cause-count'), fix: textIn(tr, '.models-cause-fix') });
        return;
      }
      if (!tr.classList.contains('exp-row')) { return; }
      var cells = tr.children;
      var detail = document.getElementById(tr.id + '-detail');
      out.push({
        kind: 'lane',
        table: table,
        id: tr.id,
        names: Array.prototype.map.call(tr.querySelectorAll('.models-link'), function (a) {
          return { text: a.textContent, href: a.getAttribute('href') };
        }),
        provider: cells[2].textContent,
        state: cells[5].textContent,
        refuses: cells[6].textContent,
        transit: cells[7].textContent,
        toggle_name: document.getElementById(tr.id + '-toggle').getAttribute('aria-label'),
        detail_labels: Array.prototype.map.call(detail.querySelectorAll('dt'), function (dt) {
          return dt.textContent;
        })
      });
    });
    return out;
  }

  function seen() {
    var body = document.getElementById('body-models');
    var unused = document.getElementById('models-unused');
    return {
      rows: rows(),
      unused: unused ? textIn(unused, 'summary') : null,
      error_cards: body.querySelectorAll('.unavailable-msg, .incompatible-msg').length,
      transit_nodes: Array.prototype.map.call(body.querySelectorAll('.models-transit'), function (n) {
        return n.textContent;
      }),
      detail_transit: Array.prototype.map.call(body.querySelectorAll('.detail-row .def'), function (n) {
        return n.textContent;
      }).filter(function (t) { return t.indexOf('changed in transit') === 0; })
    };
  }

  function lanesDrawn() {
    var body = document.getElementById('body-models');
    return !!document.getElementById('models-lanes') && !body.querySelector('.skeleton');
  }

  function lanesQueryAnswered() {
    return d.waitUntil('the per-model QUERY answer', function () {
      return window.routectlModelsAnswered;
    });
  }

  // The page's own fetch marks when an answer to the per-model QUERY has
  // been handed over, so a scenario reads the view after it applied. Once a
  // scenario sets `failLanes`, every per-model QUERY answers 503 instead.
  var pageFetch = window.fetch;
  var failLanes = false;
  var lanesFailed = 0;
  window.fetch = function (input, init) {
    var lanes = isLanesQuery(init);
    if (lanes && failLanes) {
      return Promise.resolve(new Response('', { status: 503 })).then(function (resp) {
        setTimeout(function () { lanesFailed += 1; }, 0);
        return resp;
      });
    }
    return pageFetch(input, init).then(function (resp) {
      if (lanes) { setTimeout(function () { window.routectlModelsAnswered = true; }, 0); }
      return resp;
    });
  };

  d.scenario('models-rows', function () {
    var ready = d.waitUntil('the lane table', lanesDrawn);
    var settled = opts.holdLanesQuery
      ? ready.then(function () { return d.sleep(3000); })
      : ready.then(lanesQueryAnswered).then(function () { return d.sleep(300); });
    return settled.then(seen);
  });

  // A lane row expanded, then two changed rounds of its transit count.
  d.scenario('models-kept', function () {
    var lane = 'anthropic#claude-sonnet-4-5';
    d.rewrite(function (url, text, n) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      agg.panels.models.data.rows.forEach(function (row) {
        if (row.lane === lane) { row.transit.checked += n; }
      });
      return JSON.stringify(agg);
    });
    var node = null;
    return d.waitUntil('the lane table', lanesDrawn).then(function () {
      node = Array.prototype.filter.call(document.querySelectorAll('#models-lanes tr.exp-row'), function (tr) {
        return tr.children[3].textContent === 'claude-sonnet-4-5' && tr.children[2].textContent === 'anthropic';
      })[0];
      if (!node) { throw new Error('no sonnet lane row'); }
      document.getElementById(node.id + '-toggle').click();
      return d.acrossRounds('/status', 2, function () {
        var live = document.getElementById(node.id);
        var detail = document.getElementById(node.id + '-detail');
        return {
          same_node: live === node,
          connected: node.isConnected,
          open: !!detail && !detail.hidden,
          detail: detail ? detail.textContent : null,
          figure: live ? live.children[7].textContent : null
        };
      });
    });
  });

  // An unused lane expanded inside its open collapsed line, then every
  // later per-model QUERY failing: the collapsed line and the row are read
  // once the first failure applied and again after the next round.
  d.scenario('models-stale-fold', function () {
    var fold = null;
    var node = null;
    function observe() {
      var live = node && document.getElementById(node.id);
      var detail = node && document.getElementById(node.id + '-detail');
      var nowFold = document.getElementById('models-unused');
      return {
        failed: lanesFailed,
        unused: nowFold ? textIn(nowFold, 'summary') : null,
        same_fold: !!fold && nowFold === fold,
        fold_open: !!nowFold && nowFold.open,
        same_node: !!node && live === node,
        connected: !!node && node.isConnected,
        in_fold: !!node && !!fold && fold.contains(node),
        open: !!detail && !detail.hidden
      };
    }
    return d.waitUntil('the lane table', lanesDrawn).then(lanesQueryAnswered).then(function () {
      return d.waitUntil('the collapsed lanes', function () {
        return document.querySelector('#models-unused-lanes tr.exp-row');
      });
    }).then(function (row) {
      fold = document.getElementById('models-unused');
      fold.open = true;
      node = row;
      document.getElementById(node.id + '-toggle').click();
      var before = observe();
      failLanes = true;
      return d.waitUntil('a failed per-model QUERY', function () { return lanesFailed > 0; })
        .then(function () { return d.sleep(300); })
        .then(function () {
          var failedOnce = observe();
          return d.waitFor('/status', 1).then(function () {
            return { before: before, failed_once: failedOnce, next_round: observe() };
          });
        });
    });
  });

  function focusedId() {
    var node = document.activeElement;
    return node === document.body ? 'body' : (node.id || node.tagName.toLowerCase());
  }

  // Follow a nickname's link from the keyboard (focused, then activated:
  // the click a keyboard Enter on a link dispatches) and read Traffic once
  // its QUERY answered, then again after the next refresh.
  d.scenario('models-link', function () {
    var href = null;
    var out = {};
    return d.waitUntil('the lane table', lanesDrawn).then(function () {
      var link = Array.prototype.filter.call(document.querySelectorAll('#models-lanes .models-link'), function (a) {
        return a.textContent === 'sonnet';
      })[0];
      if (!link) { throw new Error('no sonnet link'); }
      href = link.getAttribute('href');
      link.focus();
      out.focused_before = focusedId();
      link.click();
      return d.waitUntil('the Traffic table', function () {
        var tab = document.querySelector('#tabbar [aria-selected="true"]');
        return tab && tab.getAttribute('data-tab') === 'traffic' &&
          document.getElementById('traffic-groups');
      });
    }).then(function () {
      return d.sleep(500);
    }).then(function () {
      var strip = document.querySelector('#body-traffic .card-title');
      out.href = href;
      out.hash = location.hash;
      out.strip = strip ? strip.textContent : null;
      out.focused = focusedId();
      out.focused_text = document.activeElement.textContent;
      var heading = document.activeElement;
      return d.waitFor('/status/query', 1).then(function () {
        out.focused_after_refresh = focusedId();
        out.same_heading = document.activeElement === heading && heading.isConnected;
        return out;
      });
    });
  });

  // Every nickname link's address as drawn, read once the lanes drew.
  d.scenario('models-link-hrefs', function () {
    return d.waitUntil('the lane table', lanesDrawn).then(function () {
      return {
        hrefs: Array.prototype.map.call(document.querySelectorAll('#body-models .models-link'), function (a) {
          return a.getAttribute('href');
        })
      };
    });
  });

  // Open Traffic scoped from the address, then switch to Models and read
  // the address once the per-model QUERY answered.
  d.scenario('models-scope-then-lanes', function () {
    return d.waitUntil('the Traffic table', function () {
      return document.getElementById('traffic-groups');
    }).then(function () {
      document.getElementById('tab-models').click();
      return lanesQueryAnswered();
    }).then(function () {
      return { models_hash: location.hash };
    });
  });
})(window.routectlDriver);
