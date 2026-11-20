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

  // ---- traffic in the selected range -------------------------------------

  var SEAT_TRAFFIC = SEAT_ROW + '-traffic';
  var CODEX_TRAFFIC = CODEX_ROW + '-traffic';

  // The seat-grouped QUERY answers as the page receives them, newest last.
  var seatAnswers = [];

  function isSeatAnswer(data) {
    return !!data && Array.isArray(data.groups) &&
      data.groups.some(function (g) { return Object.prototype.hasOwnProperty.call(g, 'account'); });
  }

  function rewriteSeatAnswers(edit) {
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status/query') { return text; }
      var env = JSON.parse(text);
      if (!isSeatAnswer(env.data)) { return text; }
      if (edit) { edit(env.data); }
      seatAnswers.push(env.data);
      return JSON.stringify(env);
    });
  }

  function group(data, account) {
    var found = data.groups.filter(function (g) { return g.account === account; })[0];
    if (!found) { throw new Error('no seat group ' + account); }
    return found;
  }

  function seatGroup(data, account, metrics) {
    var copy = JSON.parse(JSON.stringify(group(data, 'anthropic#a').metrics));
    Object.keys(metrics).forEach(function (k) { copy[k] = metrics[k]; });
    return { label: account, account: account, provider_entry: null, login_label: null, metrics: copy };
  }

  function trafficSeen(id) {
    var line = byId(id);
    return {
      text: line.textContent,
      req: textIn(line, '.acct-traffic-req'),
      cost: textIn(line, '.acct-traffic-cost'),
      alt: textIn(line, '.acct-traffic-alt'),
      note: textIn(line, '.acct-traffic-note')
    };
  }

  function trafficSettled(id) {
    return d.waitUntil('the traffic line of #' + id, function () {
      var line = document.getElementById(id);
      return line && line.querySelector('.acct-traffic-req');
    });
  }

  function lastSeatAnswer() {
    return seatAnswers[seatAnswers.length - 1];
  }

  function requestsOf(data, account) {
    var g = data.groups.filter(function (x) { return x.account === account; })[0];
    return g ? g.metrics.requests : null;
  }

  // The seat answer gains a decoy group keyed `CODEX` (configured: `codex`),
  // a priced group for the API-key entry and a partial one for the compat
  // entry; the labeled seat's group is subscription traffic with an
  // API-price equivalent. The retired login's group stays as served.
  d.scenario('accounts-traffic', function () {
    rewriteSeatAnswers(function (data) {
      var seat = group(data, 'anthropic#a').metrics;
      seat.cost_status = 'subscription';
      seat.cost_usd = null;
      seat.equivalent_cost_usd = 1.234;
      data.groups.push(seatGroup(data, 'CODEX', { requests: 777 }));
      data.groups.push(seatGroup(data, '#anthropic', {
        cost_status: 'priced', cost_usd: 0.5, equivalent_cost_usd: null
      }));
      data.groups.push(seatGroup(data, '#compat', {
        cost_status: 'partial', cost_usd: 0.25, equivalent_cost_usd: null
      }));
    });
    return trafficSettled(SEAT_TRAFFIC).then(function () {
      var answer = lastSeatAnswer();
      var body = byId(BODY);
      return {
        answered_accounts: answer.groups.map(function (g) { return g.account; }),
        answered_seat_requests: requestsOf(answer, 'anthropic#a'),
        answered_decoy_requests: requestsOf(answer, 'CODEX'),
        row_accounts: Array.prototype.map.call(body.querySelectorAll('.acct-name'), function (n) {
          return n.title;
        }),
        traffic_lines: body.querySelectorAll('.acct-traffic').length,
        seat: trafficSeen(SEAT_TRAFFIC),
        seat_in_backup: trafficSeen(SEAT_ROW_IN_BACKUP + '-traffic'),
        codex: trafficSeen(CODEX_TRAFFIC),
        api_key: trafficSeen(API_KEY_ROW + '-traffic'),
        compat: trafficSeen(COMPAT_ROW + '-traffic'),
        cloud: trafficSeen(CLOUD_ROW + '-traffic'),
        range_head: byId('accounts-traffic-range').textContent
      };
    });
  });

  // The seat answer arrives with its groups stripped of their account
  // identity, as a model or provider grouping is: nothing in it can be joined
  // to an account.
  d.scenario('accounts-traffic-not-seat', function () {
    rewriteSeatAnswers(function (data) {
      data.groups.forEach(function (g) {
        delete g.account;
        delete g.provider_entry;
        delete g.login_label;
      });
    });
    return d.waitUntil('the seat answer', function () { return seatAnswers.length > 0; })
      .then(function () {
        return d.waitUntil('a traffic note', function () {
          var line = document.getElementById(SEAT_TRAFFIC);
          return line && textIn(line, '.acct-traffic-note') !== 'loading';
        });
      })
      .then(function () {
        var body = byId(BODY);
        return {
          seat: trafficSeen(SEAT_TRAFFIC),
          codex: trafficSeen(CODEX_TRAFFIC),
          rows: body.querySelectorAll('.acct-row').length,
          error_cards: body.querySelectorAll('.unavailable-msg, .incompatible-msg').length
        };
      });
  });

  // Opened with a provider scope in the address: the seat QUERY carries it,
  // and the view says so beside a control that lifts it.
  d.scenario('accounts-traffic-scoped', function () {
    rewriteSeatAnswers(null);
    return trafficSettled(SEAT_TRAFFIC).then(function () {
      var reset = document.getElementById('accounts-scope-reset');
      return {
        head: byId('accounts-traffic-range').textContent,
        codex_note: textIn(byId(CODEX_ROW), '.acct-traffic-note'),
        reset: reset ? reset.textContent : null,
        rows: byId(BODY).querySelectorAll('.acct-row').length
      };
    });
  });

  // ---- the range moves the traffic lines and nothing else ----------------

  function windowButton(name) {
    var btn = document.querySelector('#windowsel button[data-window="' + name + '"]');
    if (!btn) { throw new Error('no window button ' + name); }
    return btn;
  }

  function isWeekQuery(init) {
    if (!init || init.method !== 'QUERY' || typeof init.body !== 'string') { return false; }
    return JSON.parse(init.body).window === 'week';
  }

  // Everything on a row but its traffic line, with the nodes that carry it.
  function rowState() {
    var rows = Array.prototype.map.call(byId(BODY).querySelectorAll('.acct-row'), function (rowEl) {
      var parts = Array.prototype.filter.call(rowEl.children, function (child) {
        return !child.classList.contains('acct-traffic');
      });
      return {
        id: rowEl.id,
        text: parts.map(function (p) { return p.textContent; }).join(' | ')
      };
    });
    return { rows: rows };
  }

  function rangeSeen() {
    var pools = document.querySelector('#body-accounts [data-source="accounts"]');
    var picker = byId('windowsel');
    return {
      head: byId('accounts-traffic-range').textContent,
      covers: byId('window-covers').textContent,
      covers_hidden: byId('window-covers').hidden,
      windowless: picker.getAttribute('data-windowless'),
      week_pressed: windowButton('week').getAttribute('aria-pressed'),
      seat: trafficSeen(SEAT_TRAFFIC),
      codex: trafficSeen(CODEX_TRAFFIC),
      pools_pending: pools.classList.contains('section--pending'),
      pools_skeleton: !!pools.querySelector('.skeleton'),
      seat_row_kept: document.getElementById(SEAT_ROW) === keptRow,
      window_kept: document.getElementById(POLLED_WINDOW) === keptWindow,
      rows: rowState().rows
    };
  }

  var keptRow = null;
  var keptWindow = null;

  // Today's traffic, then Week with its seat answer held: the lines say they
  // are loading under the new heading while every login, window and poll
  // line stays as it was, in the same nodes; then Week's answer lands.
  d.scenario('accounts-traffic-range', function () {
    rewriteSeatAnswers(null);
    d.hold(function (url, init) { return isWeekQuery(init) ? 1500 : 0; });
    var out = {};
    return trafficSettled(SEAT_TRAFFIC).then(function () {
      keptRow = byId(SEAT_ROW);
      keptWindow = byId(POLLED_WINDOW);
      out.today = rangeSeen();
      out.today_requests = requestsOf(lastSeatAnswer(), 'anthropic#a');
      out.answers_at_click = seatAnswers.length;
      windowButton('week').click();
      return d.sleep(200);
    }).then(function () {
      out.held = rangeSeen();
      return d.waitUntil('the week seat answer', function () {
        return seatAnswers.length > out.answers_at_click;
      });
    }).then(function () {
      return trafficSettled(SEAT_TRAFFIC);
    }).then(function () {
      out.week = rangeSeen();
      out.week_requests = requestsOf(lastSeatAnswer(), 'anthropic#a');
      return out;
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
