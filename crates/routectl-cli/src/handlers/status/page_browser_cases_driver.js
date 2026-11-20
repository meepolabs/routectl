// ---- scenarios: the review cases on Now and Accounts ------------------------
//
// Test-only snippet for page_browser_cases_tests.rs, injected after
// page_browser_driver.js. The harness serves one review case as its
// builders make it; nothing here rewrites a payload. Each scenario waits for
// the view to draw the case and reads back what a reviewer would see. The
// outage scenario wraps the driver's fetch so chosen aggregate answers fail
// the way an unreachable daemon's do.
(function (d) {
  'use strict';

  function texts(root, selector) {
    return Array.prototype.map.call(root.querySelectorAll(selector), function (n) {
      return n.textContent;
    });
  }

  function textIn(node, selector) {
    var found = node.querySelector(selector);
    return found ? found.textContent : null;
  }

  // ---- Now ------------------------------------------------------------------

  function attentionRows() {
    return Array.prototype.map.call(document.querySelectorAll('#body-now .attn-item'), function (row) {
      return {
        id: row.id,
        subject: textIn(row, '.attn-subject'),
        cause: textIn(row, '.attn-cause'),
        fix: textIn(row, '.attn-fix-code'),
        raw: textIn(row, '.attn-raw-code')
      };
    });
  }

  function nowSeen() {
    var body = document.getElementById('body-now');
    return {
      empty_title: textIn(body, '.empty-title'),
      base_url: textIn(body, '.empty-baseurl code'),
      glance_none: textIn(body, '.glance-none'),
      glance_pools: Array.prototype.map.call(body.querySelectorAll('.glance-pool'), function (pool) {
        return {
          name: textIn(pool, '.glance-pool-name'),
          seats: texts(pool, '.glance-seat-name')
        };
      }),
      attention: attentionRows(),
      attention_clear: textIn(body, '.attn-clear')
    };
  }

  // The Now view with every section it reads drawn: the attention list and
  // the accounts glance, and the traffic section past its loading state.
  d.scenario('case-now', function () {
    var empty = window.routectlCaseEmpty === true;
    return d.waitUntil('the case on Now', function () {
      var body = document.getElementById('body-now');
      var drawn = body.querySelector('.attn') && body.querySelector('.glance');
      var traffic = empty ? body.querySelector('.empty-baseurl') : body.querySelector('.provcard');
      return drawn && traffic;
    }, 30000).then(nowSeen);
  });

  // ---- Accounts -------------------------------------------------------------

  function accountRows() {
    return Array.prototype.map.call(document.querySelectorAll('#body-accounts .acct-row'), function (row) {
      return {
        id: row.id,
        name: textIn(row, '.acct-name'),
        login: textIn(row, '.acct-login'),
        cause: textIn(row, '.acct-cause'),
        fix: textIn(row, '.acct-fix-code'),
        nowin: textIn(row, '.acct-nowin'),
        windows: row.querySelectorAll('.acct-win').length,
        stale_windows: row.querySelectorAll('.acct-win--stale').length,
        resets: texts(row, '.acct-win-meta'),
        traffic: textIn(row, '.acct-traffic')
      };
    });
  }

  function accountsSeen() {
    var body = document.getElementById('body-accounts');
    return {
      pools: texts(body, '.acct-pool > .card-head .card-title'),
      pool_hints: texts(body, '.acct-pool > .card-head .card-hint'),
      rows: accountRows(),
      error_cards: body.querySelectorAll('.unavailable-msg, .incompatible-msg').length
    };
  }

  // Accounts with its rows drawn and the seat QUERY answered, so every
  // traffic line reads the answer rather than its loading note.
  d.scenario('case-accounts', function () {
    return d.waitUntil('the case on Accounts', function () {
      var rows = document.querySelectorAll('#body-accounts .acct-row');
      if (!rows.length) { return false; }
      return Array.prototype.every.call(rows, function (row) {
        var note = row.querySelector('.acct-traffic-note');
        return !note || note.textContent !== 'loading';
      });
    }, 30000).then(accountsSeen);
  });

  // ---- an outage on the open view --------------------------------------------

  var failing = false;
  var inner = window.fetch;
  window.fetch = function (input, init) {
    var url = new URL(String(input), location.href);
    return inner(input, init).then(function (resp) {
      if (failing && url.pathname === '/status') { throw new TypeError('Failed to fetch'); }
      return resp;
    });
  };

  function strip() {
    var node = document.getElementById('verdict');
    var note = document.getElementById('verdict-note');
    return {
      text: document.getElementById('verdict-text').textContent,
      dimmed: node.classList.contains('verdict--stale'),
      note: note.hidden ? null : note.textContent
    };
  }

  function badges() {
    var out = {};
    Array.prototype.forEach.call(document.querySelectorAll('.tab-badge'), function (node) {
      out[node.id.replace(/^badge-/, '')] = node.hidden ? null : {
        text: node.textContent,
        dimmed: node.classList.contains('tab-badge--dimmed')
      };
    });
    return out;
  }

  // The served verdict and badges, then `window.routectlCaseFailures`
  // aggregate rounds that fail, each observed once the strip marks it, with
  // the open view's section count kept as the proof its content stayed.
  d.scenario('case-outage', function () {
    var view = location.hash.replace(/^#/, '').split('?')[0];
    var body = document.getElementById('body-' + view);
    var out = { failed: [] };
    var step = d.waitUntil('the served verdict', function () {
      var t = strip().text;
      return t && t !== 'Checking routing state' && body.querySelector('.ovsection');
    }, 30000).then(function () {
      return d.waitFor('/status', 1);
    }).then(function () {
      out.live = { strip: strip(), badges: badges(), sections: body.querySelectorAll('.ovsection').length };
      failing = true;
    });
    for (var i = 0; i < window.routectlCaseFailures; i++) {
      step = step.then(function () {
        return d.waitFor('/status', 1);
      }).then(function () {
        return d.waitUntil('the strip to mark the failure', function () { return strip().dimmed; });
      }).then(function () {
        out.failed.push({ strip: strip(), badges: badges(), sections: body.querySelectorAll('.ovsection').length });
      });
    }
    return step.then(function () { return out; });
  });
})(window.routectlDriver);
