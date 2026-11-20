  // ---- view:accounts ---------------------------------------------------

  // Every configured credential from /status/accounts, grouped by the pools
  // that dispatch on it, plus the two background drivers that keep the
  // logins fresh and read their usage. Read as the daemon sent it: the
  // login cause and fix, each window's length, room, reset, age, stale and
  // kept flags, and each seat's placement rank and reason are the panel's;
  // the page adds only how far an instant is from the panel's own as_of and
  // how many seats a ranking holds. A pool header's usable count comes from
  // the health panel, read through acctPoolCounts so a dark health source
  // costs that count alone. Each row's traffic over the selected range comes
  // from the seat-grouped QUERY, read through acctSeatTraffic on the same
  // terms; it is the only part of a row the range picker moves.

  // A window this full reads amber, and red once at or over its limit. The
  // same line the needs-attention list draws for a nearly used window.
  var ACCT_NEAR_LIMIT = 0.9;

  var ACCT_SECOND_MS = 1000;
  var ACCT_MINUTE_MS = 60000;
  var ACCT_HOUR_MS = 3600000;
  var ACCT_DAY_MS = 86400000;

  // The words for each credential state; a token this build does not know
  // is shown as sent, neutrally.
  var ACCT_LOGIN_WORDS = {
    usable: 'usable',
    unusable: 'unusable',
    parked: 'parked',
    not_built: 'not built'
  };

  // The usage poll's applicability, in words. Every one is neutral: the
  // needs-attention list, not this line, says when a poll is a problem.
  var ACCT_POLL_WORDS = {
    awaiting_login: 'usage poll waits for a fresh login',
    not_polled: 'not read by the usage poll',
    off: 'usage poll off'
  };

  // The last poll's result, in words: what the attempt did, and for a
  // failure, why. A token this build does not know is shown as sent; the
  // token itself rides in the line's title for copying.
  var ACCT_POLL_RESULT_WORDS = {
    accepted: 'reading accepted',
    no_windows: 'read, no usage windows reported',
    token_not_ready: 'skipped, no usable access token',
    paused: 'deferred, the provider asked to wait',
    redirect: 'failed (redirected)',
    auth: 'failed (access refused)',
    http: 'failed (HTTP error)',
    timeout: 'failed (timed out)',
    transport: 'failed (connection error)',
    too_large: 'failed (answer too large)',
    malformed: 'failed (answer not understood)',
    superseded: 'dropped, the login changed meanwhile'
  };

  var ACCT_SOURCE_WORDS = {
    poll: 'from the usage poll',
    response: 'from a response'
  };

  // Why a seat stands where it does in a placement ranking, in words: the
  // standing reasons first, then the refusals that leave a seat unranked. A
  // token this build does not know is shown as sent, neutrally; the token
  // itself rides in the title for copying.
  var ACCT_REASON_WORDS = {
    overage: 'billing beyond its included usage',
    no_cap: 'reports no usage limits',
    no_reading: 'no usage reading yet',
    stale_reading: 'its usage reading is stale',
    low_room: 'little room left in a usage window',
    pace: 'ranked on usage pace',
    unstarted: 'its usage windows have not started',
    credential_unusable: 'its login is not usable',
    account_parked: 'parked after an upstream refusal',
    breaker_open: 'breaker open after repeated failures',
    rate_limited: 'rate limited'
  };

  // ---- drivers -----------------------------------------------------------

  function accountsDriverSection(rec) {
    var data = rec.data;
    if (!isPlainObject(data.refresh_driver) || !isPlainObject(data.poll_driver)) {
      throw new Error('accounts payload carries no driver health');
    }
    var nowMs = panelNowMs(rec);
    var wrap = markRefreshUnit(document.createElement('div'), 'accounts-drivers');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Background seat drivers',
      'memory only - a restart starts them from zero'));
    var list = document.createElement('div');
    list.className = 'acct-drivers';
    list.appendChild(acctDriverCard('refresh', 'Credential refresh', data.refresh_driver, nowMs));
    list.appendChild(acctDriverCard('poll', 'Usage poll', data.poll_driver, nowMs));
    wrap.appendChild(list);
    return wrap;
  }

  // A stopped driver reads amber: nothing keeps its seats' logins or
  // readings fresh while it is down.
  function acctDriverCard(key, name, driver, nowMs) {
    var running = driver.running === true;
    var cardEl = document.createElement('div');
    cardEl.id = 'accounts-driver-' + key;
    cardEl.className = 'acct-driver acct-driver--' + (running ? 'running' : 'stopped');
    var head = document.createElement('div');
    head.className = 'acct-driver-head';
    head.appendChild(acctText('span', 'acct-driver-name', name));
    head.appendChild(acctText('span', 'acct-driver-state', running ? 'running' : 'stopped'));
    cardEl.appendChild(head);
    cardEl.appendChild(buildDefList([
      ['last sweep', acctStamp(driver.last_sweep_ms, nowMs)],
      ['last clean sweep', acctStamp(driver.last_good_sweep_ms, nowMs)],
      ['sweeps', acctCountNode(driver.sweeps)],
      ['transient failures', acctCountNode(driver.transient)],
      ['definitive failures', acctCountNode(driver.definitive)]
    ]));
    return cardEl;
  }

  function acctStamp(ms, nowMs) {
    var ago = acctAgo(ms, nowMs);
    return ago === null ? faintFigure('never') : document.createTextNode(ago);
  }

  // A count as text; an absent one is a dash, never a zero.
  function acctCountText(v) {
    return (v === null || v === undefined || !isFinite(Number(v))) ? '-' : String(Number(v));
  }

  function acctCountNode(v) {
    var text = acctCountText(v);
    return text === '0' ? faintFigure('0') : figure(text, null, null);
  }

  // ---- accounts by pool --------------------------------------------------

  function accountsPoolSection(rec) {
    var rows = rec.data.accounts;
    if (!Array.isArray(rows)) { throw new Error('accounts payload carries no account list'); }
    rows.forEach(acctCheckRow);
    var wrap = markRefreshUnit(document.createElement('div'), 'accounts-pools');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Accounts by pool',
      'every configured credential - an account in several pools is listed under each'));
    if (!rows.length) {
      wrap.appendChild(emptyCard('No credential configured',
        'Every account a provider entry names appears here with its login state and ' +
        'its usage windows.', ['current state']));
      return wrap;
    }
    var head = acctText('div', 'acct-traffic-range', acctTrafficHeadText());
    head.id = 'accounts-traffic-range';
    wrap.appendChild(head);
    var ctx = {
      nowMs: panelNowMs(rec),
      traffic: acctSeatTraffic(SOURCES[QUERY_SEATS_SOURCE]),
      places: acctPlacementIndex(rec.data.placement),
      usablePools: poolsWithUsableLogin(rows)
    };
    var counts = acctPoolCounts(SOURCES.health);
    acctPoolNames(rows).forEach(function (pool) {
      var members = rows.filter(function (row) { return row.pools.indexOf(pool) >= 0; });
      wrap.appendChild(acctPoolGroup(pool, members, counts, ctx));
    });
    var loose = rows.filter(function (row) { return !row.pools.length; });
    if (loose.length) { wrap.appendChild(acctLooseGroup(loose, ctx)); }
    return wrap;
  }

  // The shape every row must have before any of it is drawn: a row missing
  // its key, its window list or its standing is a malformed payload.
  function acctCheckRow(row) {
    if (!isPlainObject(row) || typeof row.account !== 'string' ||
        typeof row.provider_entry !== 'string' || !Array.isArray(row.windows) ||
        !Array.isArray(row.pools) || !isPlainObject(row.credential) ||
        !isPlainObject(row.poll)) {
      throw new Error('accounts payload carries a malformed account row');
    }
  }

  // Every pool any row names, sorted, so the groups keep their order across
  // refreshes whatever order the rows arrive in.
  function acctPoolNames(rows) {
    var seen = Object.create(null);
    var names = [];
    rows.forEach(function (row) {
      row.pools.forEach(function (pool) {
        if (seen[pool]) { return; }
        seen[pool] = true;
        names.push(String(pool));
      });
    });
    return names.sort();
  }

  // Each pool's usable and configured seat counts from the health panel, or
  // null when that source carries nothing renderable. Guarded as safeSection
  // guards a section: a throw is recorded against health, and the headers
  // say the count is not available while the rows keep rendering.
  function acctPoolCounts(rec) {
    if (stateCard(rec)) { return null; }
    try {
      var pools = rec.data.pools;
      if (!Array.isArray(pools)) { throw new Error('health payload carries no pools'); }
      var byPool = Object.create(null);
      pools.forEach(function (p) { byPool[String(p.pool)] = p; });
      markRenderAttempt(rec.name);
      return byPool;
    } catch (e) {
      recordRenderFault(rec.name);
      return null;
    }
  }

  function acctUsableText(pool, counts) {
    if (!counts) { return 'usable count not available'; }
    var p = counts[pool];
    if (!p) { return 'usable count not reported'; }
    return acctCountText(p.eligible) + ' of ' + acctCountText(p.configured) + ' usable';
  }

  function acctPoolGroup(pool, members, counts, ctx) {
    var group = document.createElement('div');
    group.id = 'accounts-pool-' + idKey(pool);
    group.className = 'acct-pool';
    group.appendChild(sectionHead(pool, acctUsableText(pool, counts)));
    var list = document.createElement('div');
    list.className = 'acct-list';
    members.forEach(function (row) {
      list.appendChild(acctRow(row, 'accounts-row-' + idKey(pool) + '-' + idKey(row.account), ctx, pool));
    });
    group.appendChild(list);
    return group;
  }

  function acctLooseGroup(rows, ctx) {
    var group = document.createElement('div');
    group.id = 'accounts-nopool';
    group.className = 'acct-pool';
    group.appendChild(sectionHead('Not in a pool', null));
    var list = document.createElement('div');
    list.className = 'acct-list';
    rows.forEach(function (row) {
      list.appendChild(acctRow(row, 'accounts-row-' + idKey(row.account), ctx, null));
    });
    group.appendChild(list);
    return group;
  }

  // ---- one account ---------------------------------------------------------

  // One account under `pool` (null for no pool), its own refresh unit: a
  // selection inside its fix holds this row alone while the rest of the list
  // keeps updating.
  function acctRow(row, id, ctx, pool) {
    var cred = row.credential;
    var rowEl = markRefreshUnit(document.createElement('div'), id);
    rowEl.id = id;
    var kind = loginKind(cred.state, pool === null ? null : ctx.usablePools[pool] === true);
    rowEl.className = 'acct-row acct-row--' + kind;
    rowEl.appendChild(acctHead(row, kind));
    var cause = acctCauseText(cred);
    if (cause) { rowEl.appendChild(acctText('div', 'acct-cause', cause)); }
    if (cred.fix) { rowEl.appendChild(acctFix(cred.fix)); }
    if (cred.remediation) { rowEl.appendChild(acctRemediation(cred.remediation)); }
    rowEl.appendChild(acctWindows(row, id, ctx.nowMs));
    var places = ctx.places[row.account];
    if (places) { rowEl.appendChild(acctPlacement(places)); }
    rowEl.appendChild(acctTrafficLine(row, ctx.traffic, id + '-traffic'));
    rowEl.appendChild(acctPollLine(row.poll, ctx.nowMs));
    return rowEl;
  }

  // "provider_entry (login_label)", or the entry alone for an unlabeled
  // credential. The account key other panels join on rides in the title.
  function acctHead(row, kind) {
    var head = document.createElement('div');
    head.className = 'acct-head';
    var name = acctText('span', 'acct-name', row.login_label
      ? row.provider_entry + ' (' + row.login_label + ')'
      : row.provider_entry);
    name.title = row.account;
    head.appendChild(name);
    head.appendChild(acctText('span', 'acct-login acct-login--' + kind,
      acctLoginWord(row.credential.state, row.subscription === true)));
    return head;
  }

  function acctLoginWord(state, subscription) {
    if (state === 'usable' && subscription) { return 'logged in'; }
    return Object.prototype.hasOwnProperty.call(ACCT_LOGIN_WORDS, state)
      ? ACCT_LOGIN_WORDS[state]
      : String(state);
  }

  // The daemon's plain-words cause (a park's already says when it is tried
  // again), then when it began. An unbuilt account has no router state
  // at all, which is said rather than left blank.
  function acctCauseText(cred) {
    if (cred.state === 'not_built') {
      return 'its provider did not build, so the router has no state for it';
    }
    var parts = [];
    if (cred.cause) { parts.push(String(cred.cause)); }
    if (acctHasNumber(cred.since_ms)) { parts.push('since ' + fmtTs(Number(cred.since_ms))); }
    return parts.join(' - ');
  }

  // The fix is the line to run, so it carries the row's emphasis and is a
  // copyable code element: Tab reaches it, and Ctrl+A then Ctrl+C copy it.
  function acctFix(fix) {
    var wrap = document.createElement('div');
    wrap.className = 'acct-fix';
    wrap.appendChild(acctText('span', 'acct-label', 'fix'));
    wrap.appendChild(copyableCode('acct-fix-code', fix));
    return wrap;
  }

  // A correction no single command makes is an instruction in words, so it
  // is plain text, never a code element offered for copying.
  function acctRemediation(text) {
    var wrap = document.createElement('div');
    wrap.className = 'acct-fix acct-remediation';
    wrap.appendChild(acctText('span', 'acct-label', 'fix'));
    wrap.appendChild(acctText('span', 'acct-remediation-text', String(text)));
    return wrap;
  }

  // ---- windows -------------------------------------------------------------

  // Every window the account's reading holds, or the reason there is none:
  // a provider that reports no usage windows says so, and a subscription seat
  // with no reading says that, never a meter at zero.
  function acctWindows(row, rowId, nowMs) {
    var wrap = document.createElement('div');
    wrap.className = 'acct-wins';
    if (row.windows.length) {
      row.windows.forEach(function (w) {
        var key = String(w.length_secs) + ':' + (w.scope === null || w.scope === undefined ? '' : w.scope);
        wrap.appendChild(acctWindow(w, rowId + '-win-' + idKey(key), nowMs));
      });
      return wrap;
    }
    wrap.appendChild(acctText('div', 'acct-nowin', acctNoWindowText(row)));
    return wrap;
  }

  function acctNoWindowText(row) {
    if (row.subscription !== true) { return 'this provider reports no usage windows'; }
    if (row.report === 'no_windows') { return 'its last reading carried no usage windows'; }
    return 'no reading yet';
  }

  function acctWindow(w, id, nowMs) {
    var used = acctHasNumber(w.used) ? Number(w.used) : NaN;
    var level = acctLevel(used);
    var winEl = document.createElement('div');
    winEl.id = id;
    winEl.className = 'acct-win acct-win--' + level + (w.stale === true ? ' acct-win--stale' : '');
    winEl.appendChild(acctText('span', 'acct-win-label', acctWindowLabel(w)));
    winEl.appendChild(acctUsedFigure(used));
    winEl.appendChild(acctRoom(w.remaining));
    if (level !== 'calm') {
      winEl.appendChild(acctText('span', 'acct-win-level', level === 'full' ? 'at limit' : 'nearly used'));
    }
    if (isFinite(used)) { winEl.appendChild(shareBar(used * 100)); }
    winEl.appendChild(acctWindowMeta(w, nowMs));
    return winEl;
  }

  function acctLevel(used) {
    if (!isFinite(used)) { return 'calm'; }
    if (used >= 1) { return 'full'; }
    return used > ACCT_NEAR_LIMIT ? 'near' : 'calm';
  }

  // The window named by its reported length, never by its provider: the
  // same vendor's windows differ by plan.
  function acctWindowLabel(w) {
    var label = acctLengthText(Number(w.length_secs));
    return (w.scope === null || w.scope === undefined) ? label : label + ' - ' + w.scope;
  }

  function acctLengthText(secs) {
    if (!isFinite(secs) || secs <= 0) { return 'window of unknown length'; }
    var ms = secs * ACCT_SECOND_MS;
    if (ms % ACCT_DAY_MS === 0) { return acctUnits(ms / ACCT_DAY_MS, ' day', ' days'); }
    if (ms % ACCT_HOUR_MS === 0) { return (ms / ACCT_HOUR_MS) + ' h'; }
    if (ms % ACCT_MINUTE_MS === 0) { return (ms / ACCT_MINUTE_MS) + ' min'; }
    return secs + ' s';
  }

  function acctUnits(n, one, many) { return n + (n === 1 ? one : many); }

  function acctUsedFigure(used) {
    var wrap = document.createElement('span');
    wrap.className = 'acct-win-used';
    wrap.appendChild(isFinite(used)
      ? figure(String(Math.round(used * 100)), '%', null)
      : faintFigure('-'));
    wrap.appendChild(document.createTextNode(' used'));
    return wrap;
  }

  // A stale reading gives no room: the panel sends null, and it reads as
  // unknown rather than as the room an old reading had.
  function acctRoom(remaining) {
    var wrap = document.createElement('span');
    wrap.className = 'acct-win-room';
    if (!acctHasNumber(remaining)) {
      wrap.appendChild(faintFigure('room unknown'));
      return wrap;
    }
    wrap.appendChild(figure(String(Math.round(Number(remaining) * 100)), '%', null));
    wrap.appendChild(document.createTextNode(' left'));
    return wrap;
  }

  // When the window resets, how old the reading is and where it came from,
  // then the stale mark. The age is the panel's own; a reading whose age the
  // daemon could not order says so.
  function acctWindowMeta(w, nowMs) {
    var meta = document.createElement('div');
    meta.className = 'acct-win-meta';
    var parts = [acctResetText(w, nowMs)];
    parts.push(acctHasNumber(w.age_ms) ? 'read ' + acctSpanText(Number(w.age_ms)) + ' ago' : 'age unknown');
    parts.push(Object.prototype.hasOwnProperty.call(ACCT_SOURCE_WORDS, w.source)
      ? ACCT_SOURCE_WORDS[w.source]
      : 'from ' + w.source);
    meta.appendChild(document.createTextNode(parts.join(' - ')));
    if (w.stale === true) { meta.appendChild(acctText('span', 'acct-stale', 'stale')); }
    if (w.kept === true) { meta.appendChild(acctText('span', 'acct-kept', 'kept from an earlier report')); }
    return meta;
  }

  function acctResetText(w, nowMs) {
    if (w.started === 'no') { return 'not started'; }
    if (!acctHasNumber(w.reset_at_ms)) { return 'reset time not reported'; }
    if (!isFinite(nowMs)) { return 'resets at ' + fmtTs(Number(w.reset_at_ms)); }
    var left = Number(w.reset_at_ms) - nowMs;
    return left <= 0 ? 'reset time passed' : 'resets in ' + acctSpanText(left);
  }

  // ---- placement rank ------------------------------------------------------

  // Each account's entries in the placement ranking, by its exact account
  // key: one per pool and upstream model that ranks it, in the panel's
  // order, each the seat beside the ranking it stands in. A member that
  // forwards the client's credential names no account and lands on no row.
  // A panel without a ranking ranks nothing. A ranking with no seats, or
  // naming one account twice, has no "rank N of M" that reads true, so it
  // is a malformed payload rather than a line drawn twice or not at all.
  function acctPlacementIndex(placement) {
    var byAccount = Object.create(null);
    if (placement === undefined || placement === null) { return byAccount; }
    if (!isPlainObject(placement) || !Array.isArray(placement.pools)) {
      throw new Error('accounts payload carries a malformed placement');
    }
    placement.pools.forEach(function (entry) {
      if (!isPlainObject(entry) || !Array.isArray(entry.seats) || !entry.seats.length) {
        throw new Error('accounts payload carries a malformed placement');
      }
      var ranked = Object.create(null);
      entry.seats.forEach(function (seat) {
        if (!isPlainObject(seat) || typeof seat.account !== 'string') { return; }
        if (ranked[seat.account]) {
          throw new Error('accounts payload ranks one account twice in a placement');
        }
        ranked[seat.account] = true;
        var list = byAccount[seat.account] || (byAccount[seat.account] = []);
        list.push({ ranking: entry, seat: seat });
      });
    });
    return byAccount;
  }

  function acctPlacement(places) {
    var wrap = document.createElement('div');
    wrap.className = 'acct-place';
    wrap.appendChild(acctText('span', 'acct-label', 'placement'));
    var list = document.createElement('div');
    list.className = 'acct-place-list';
    places.forEach(function (place) { list.appendChild(acctPlaceEntry(place)); });
    wrap.appendChild(list);
    return wrap;
  }

  // "rank N of M in <pool> for <model> - <reason>"; an unranked seat says
  // so with its reason and no number.
  function acctPlaceEntry(place) {
    var ranking = place.ranking;
    var seat = place.seat;
    var line = document.createElement('div');
    line.className = 'acct-place-entry';
    line.appendChild(acctText('span', 'acct-place-rank', acctHasNumber(seat.rank)
      ? 'rank ' + Number(seat.rank) + ' of ' + ranking.seats.length
      : 'not ranked'));
    line.appendChild(document.createTextNode(' in '));
    line.appendChild(acctText('span', 'acct-place-key', ranking.pool));
    line.appendChild(document.createTextNode(' for '));
    line.appendChild(acctText('span', 'acct-place-key', ranking.model));
    line.appendChild(document.createTextNode(' - '));
    var token = String(seat.reason);
    var reason = acctText('span', 'acct-place-reason',
      Object.prototype.hasOwnProperty.call(ACCT_REASON_WORDS, token) ? ACCT_REASON_WORDS[token] : token);
    reason.title = token;
    line.appendChild(reason);
    return line;
  }

  // ---- traffic in the selected range --------------------------------------

  // What the traffic lines count, named for the range the picker selects
  // (and the provider scope, when one is set): the one heading that moves
  // with the picker on this view.
  function acctTrafficHeadText() {
    var text = 'Traffic ' + WINDOW_SPAN[selectedWindow] + ' - requests and cost per account';
    return selectedProvider ? text + ', through ' + selectedProvider + ' only' : text;
  }

  // The seat-grouped answer as metrics by exact account key, or the reason
  // there is none. A group whose account is null (the unattributed rows) or
  // names no configured account (a retired login) has no row to land on.
  // Guarded as acctPoolCounts guards health: an answer that is not current,
  // or not grouped by seat, costs the traffic lines alone.
  function acctSeatTraffic(rec) {
    if (rec.state === 'loading') { return { note: 'loading', byAccount: null, stale: false }; }
    var view = queryViewOf(rec.name);
    if (!view) { return { note: 'not available', byAccount: null, stale: false }; }
    try {
      var byAccount = Object.create(null);
      rec.data.groups.forEach(function (g, i) {
        if (!Object.prototype.hasOwnProperty.call(g, 'account')) {
          throw new Error('query payload is not grouped by seat');
        }
        if (typeof g.account === 'string') { byAccount[g.account] = view.groups[i].metrics; }
      });
      markRenderAttempt(rec.name);
      return { note: null, byAccount: byAccount, stale: rec.state === 'stale' };
    } catch (e) {
      recordRenderFault(rec.name);
      return { note: 'not available', byAccount: null, stale: false };
    }
  }

  function acctTrafficLine(row, traffic, id) {
    var line = document.createElement('div');
    line.id = id;
    line.className = 'acct-traffic' + (traffic.stale ? ' acct-traffic--stale' : '');
    line.appendChild(acctText('span', 'acct-label', 'traffic'));
    var m = traffic.byAccount ? traffic.byAccount[row.account] : null;
    if (!m) {
      line.appendChild(acctText('span', 'acct-traffic-note', traffic.note || acctNoTrafficText()));
      return line;
    }
    var req = document.createElement('span');
    req.className = 'acct-traffic-req';
    req.appendChild(figure(humanCount(m.requests), null, null));
    req.appendChild(document.createTextNode(' req'));
    line.appendChild(req);
    acctCostNodes(m).forEach(function (node) { line.appendChild(node); });
    if (traffic.stale) { line.appendChild(acctText('span', 'acct-stale', 'stale')); }
    return line;
  }

  // A configured account with no seat group carried nothing in the range,
  // which is said in words, never as a zero count.
  function acctNoTrafficText() {
    var through = selectedProvider ? ' through ' + selectedProvider : '';
    return 'no traffic' + through + ' ' + WINDOW_SPAN[selectedWindow];
  }

  // Real spend where rows were priced, a partial subtotal saying so, and the
  // subscription rows' API-price equivalent as a separate figure, never added
  // to spend. With neither, the cost status in words, never a dollar zero.
  function acctCostNodes(m) {
    var nodes = [];
    if (acctHasNumber(m.cost_usd)) {
      var spend = document.createElement('span');
      spend.className = 'acct-traffic-cost';
      spend.appendChild(figure(money(m.cost_usd), null, '$'));
      if (m.cost_status === 'partial') {
        spend.appendChild(acctText('span', 'acct-traffic-qual', PARTIAL_COST_NOTE));
      }
      nodes.push(spend);
    }
    if (acctHasNumber(m.equivalent_cost_usd)) {
      var alt = document.createElement('span');
      alt.className = 'acct-traffic-alt';
      alt.appendChild(figure(money(m.equivalent_cost_usd), null, '~$'));
      alt.appendChild(acctText('span', 'acct-traffic-qual', 'At API prices'));
      alt.title = 'what this traffic would cost on pay-as-you-go API pricing';
      nodes.push(alt);
    }
    if (!nodes.length) { nodes.push(faintFigure(labelFor('cost', m.cost_status).label)); }
    return nodes;
  }

  // ---- usage poll ----------------------------------------------------------

  function acctPollLine(poll, nowMs) {
    var line = document.createElement('div');
    line.className = 'acct-poll';
    line.textContent = acctPollText(poll, nowMs);
    if (poll.applicability === 'polled' && poll.last_result) {
      line.title = 'last result: ' + String(poll.last_result);
    }
    return line;
  }

  function acctPollResultText(token) {
    if (!token) { return 'attempt'; }
    return Object.prototype.hasOwnProperty.call(ACCT_POLL_RESULT_WORDS, token)
      ? 'attempt ' + ACCT_POLL_RESULT_WORDS[token]
      : String(token);
  }

  function acctPollText(poll, nowMs) {
    var token = poll.applicability;
    if (token !== 'polled') {
      return Object.prototype.hasOwnProperty.call(ACCT_POLL_WORDS, token)
        ? ACCT_POLL_WORDS[token]
        : 'usage poll: ' + token;
    }
    var parts = [];
    var last = acctAgo(poll.last_attempt_ms, nowMs);
    parts.push(last === null
      ? 'usage poll: not attempted yet'
      : 'usage poll: last ' + acctPollResultText(poll.last_result) + ' ' + last);
    var failing = acctAgo(poll.failure_since_ms, nowMs);
    if (failing !== null) { parts.push('failing since ' + failing); }
    if (acctHasNumber(poll.next_due_ms) && isFinite(nowMs)) {
      var due = Number(poll.next_due_ms) - nowMs;
      parts.push(due <= 0 ? 'due now' : 'next in ' + acctSpanText(due));
    }
    return parts.join(' - ');
  }

  // ---- shared --------------------------------------------------------------

  function acctHasNumber(v) {
    return v !== null && v !== undefined && isFinite(Number(v));
  }

  // "<span> ago" against the panel's clock, or null with no instant to read.
  function acctAgo(ms, nowMs) {
    if (!acctHasNumber(ms) || !isFinite(nowMs)) { return null; }
    return acctSpanText(Math.max(0, nowMs - Number(ms))) + ' ago';
  }

  // A coarse length of time, worded as the daemon words its causes: under a
  // minute, whole minutes, whole hours, then whole days.
  function acctSpanText(ms) {
    if (ms < ACCT_MINUTE_MS) { return 'under 1 min'; }
    if (ms < ACCT_HOUR_MS) { return Math.floor(ms / ACCT_MINUTE_MS) + ' min'; }
    if (ms < ACCT_DAY_MS) { return Math.floor(ms / ACCT_HOUR_MS) + ' h'; }
    return acctUnits(Math.floor(ms / ACCT_DAY_MS), ' day', ' days');
  }

  function acctText(tag, cls, value) {
    var node = document.createElement(tag);
    node.className = cls;
    node.textContent = (value === null || value === undefined) ? '' : String(value);
    return node;
  }
  // ---- end view:accounts -----------------------------------------------
