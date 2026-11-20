// ---- scenarios: the Accounts view -----------------------------------------
//
// Test-only snippet injected after page_browser_driver.js. The accounts
// panel arrives in the /status aggregate; each scenario rewrites that panel
// per answer (n = answers for /status before this one) and reads the rows
// the page drew from it. Ids are the page's own: `accounts-row-` plus the
// encoded pool and account keys.
(function (d) {
  'use strict';

  var BODY = 'body-accounts';
  var SEAT_ROW = 'accounts-row-kseats-kanthropic_23_a';
  var SEAT_ROW_IN_BACKUP = 'accounts-row-kbackup-kanthropic_23_a';
  var CODEX_ROW = 'accounts-row-kseats-kcodex';
  var API_KEY_ROW = 'accounts-row-k_23_anthropic';
  var CLOUD_ROW = 'accounts-row-k_23_bed';
  var COMPAT_ROW = 'accounts-row-k_23_compat';
  var POLLED_WINDOW = SEAT_ROW + '-win-k18000_3a_';
  var STALE_WINDOW = SEAT_ROW + '-win-k604800_3a_opus';

  function byId(id) {
    var node = document.getElementById(id);
    if (!node) { throw new Error('no #' + id); }
    return node;
  }

  function textIn(node, selector) {
    var found = node.querySelector(selector);
    return found ? found.textContent : null;
  }

  function texts(root, selector) {
    return Array.prototype.map.call(root.querySelectorAll(selector), function (n) {
      return n.textContent;
    });
  }

  function rewriteAccounts(edit) {
    d.rewrite(function (url, text, n) {
      if (url.pathname !== '/status') { return text; }
      var agg = JSON.parse(text);
      edit(agg.panels.accounts.data, n);
      return JSON.stringify(agg);
    });
  }

  function row(data, account) {
    var found = data.accounts.filter(function (r) { return r.account === account; })[0];
    if (!found) { throw new Error('no fixture row ' + account); }
    return found;
  }

  function settled() {
    return d.waitUntil('the accounts rows', function () {
      return document.getElementById(SEAT_ROW);
    });
  }

  function color(node, prop) {
    return getComputedStyle(node)[prop];
  }

  // ---- what each row says -------------------------------------------------

  d.scenario('accounts-rows', function () {
    rewriteAccounts(function (data) {
      row(data, '#compat').poll.applicability = 'off';
      row(data, '#bed').poll.applicability = 'not_yet_named';
    });
    return settled().then(function () {
      var body = byId(BODY);
      var seat = byId(SEAT_ROW);
      var fix = seat.querySelector('.acct-fix-code');
      return {
        pools: texts(body, '.acct-pool > .card-head .card-title'),
        pool_hints: texts(body, '.acct-pool > .card-head .card-hint'),
        names: texts(body, '.acct-name'),
        seat_in_both_pools: !!document.getElementById(SEAT_ROW_IN_BACKUP),
        seat_login: textIn(seat, '.acct-login'),
        seat_cause: textIn(seat, '.acct-cause'),
        seat_fix: fix ? { tag: fix.tagName.toLowerCase(), tab_index: fix.tabIndex, text: fix.textContent } : null,
        seat_poll: textIn(seat, '.acct-poll'),
        codex_login: textIn(byId(CODEX_ROW), '.acct-login'),
        codex_cause: textIn(byId(CODEX_ROW), '.acct-cause'),
        codex_nowin: textIn(byId(CODEX_ROW), '.acct-nowin'),
        api_key_nowin: textIn(byId(API_KEY_ROW), '.acct-nowin'),
        api_key_login: textIn(byId(API_KEY_ROW), '.acct-login'),
        cloud_nowin: textIn(byId(CLOUD_ROW), '.acct-nowin'),
        cloud_cause: textIn(byId(CLOUD_ROW), '.acct-cause'),
        compat_poll: textIn(byId(COMPAT_ROW), '.acct-poll'),
        cloud_poll: textIn(byId(CLOUD_ROW), '.acct-poll'),
        poll_colors: [COMPAT_ROW, CLOUD_ROW, API_KEY_ROW].map(function (id) {
          return color(byId(id).querySelector('.acct-poll'), 'color');
        }),
        error_cards: body.querySelectorAll('.unavailable-msg, .incompatible-msg').length
      };
    });
  });

  // ---- a row is kept across a refresh ------------------------------------

  d.scenario('accounts-row-kept', function () {
    rewriteAccounts(function (data, n) {
      row(data, 'anthropic#a').windows[0].used = 0.42 + n / 100;
    });
    return settled().then(function () {
      var kept = byId(SEAT_ROW);
      var fill = byId(POLLED_WINDOW).querySelector('.sharebar-fill');
      return d.acrossRounds('/status', 2, function () {
        return {
          same_node: document.getElementById(SEAT_ROW) === kept,
          same_meter: byId(POLLED_WINDOW).querySelector('.sharebar-fill') === fill,
          connected: kept.isConnected,
          figure: textIn(byId(POLLED_WINDOW), '.acct-win-used')
        };
      });
    });
  });

  // ---- windows by length, level and age ----------------------------------

  function window_(length, used, extra) {
    var w = {
      class: length <= 86400 ? 'short' : 'long',
      length_secs: length,
      scope: null,
      used: used,
      remaining: 1 - used,
      reset_at_ms: null,
      started: 'yes',
      source: 'poll',
      observed_at_ms: 0,
      age_ms: 60000,
      stale: false
    };
    Object.keys(extra || {}).forEach(function (k) { w[k] = extra[k]; });
    return w;
  }

  function windowSeen(id) {
    var node = byId(id);
    var fill = node.querySelector('.sharebar-fill');
    return {
      label: textIn(node, '.acct-win-label'),
      used: textIn(node, '.acct-win-used'),
      room: textIn(node, '.acct-win-room'),
      level: textIn(node, '.acct-win-level'),
      meta: textIn(node, '.acct-win-meta'),
      stale_mark: textIn(node, '.acct-stale'),
      opacity: color(node, 'opacity'),
      fill: fill ? color(fill, 'backgroundColor') : null
    };
  }

  // The Codex login reports three windows: the 18000 s one that Codex calls
  // its primary, a 7 day one nearly used and a 30 day one at its limit.
  d.scenario('accounts-windows', function () {
    rewriteAccounts(function (data) {
      var codex = row(data, 'codex');
      codex.report = 'windows';
      codex.windows = [
        window_(18000, 0.5),
        window_(604800, 0.95),
        window_(2592000, 1)
      ];
      row(data, 'anthropic#a').windows[1].age_ms = 3 * 3600000;
    });
    return settled().then(function () {
      var codex = byId(CODEX_ROW);
      return {
        codex_text: codex.textContent,
        five_hour: windowSeen(CODEX_ROW + '-win-k18000_3a_'),
        week: windowSeen(CODEX_ROW + '-win-k604800_3a_'),
        month: windowSeen(CODEX_ROW + '-win-k2592000_3a_'),
        polled: windowSeen(POLLED_WINDOW),
        stale: windowSeen(STALE_WINDOW)
      };
    });
  });

  // ---- drivers -------------------------------------------------------------

  function driverSeen(key) {
    var card = byId('accounts-driver-' + key);
    var state = card.querySelector('.acct-driver-state');
    return {
      state: state.textContent,
      stopped_class: card.classList.contains('acct-driver--stopped'),
      state_color: color(state, 'color'),
      border: color(card, 'borderTopColor')
    };
  }

  d.scenario('accounts-drivers', function () {
    rewriteAccounts(function (data) { data.poll_driver.running = false; });
    return settled().then(function () {
      return { refresh: driverSeen('refresh'), poll: driverSeen('poll') };
    });
  });
})(window.routectlDriver);
