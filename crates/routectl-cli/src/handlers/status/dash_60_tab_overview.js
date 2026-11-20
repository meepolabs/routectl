  // ---- tab:overview ----------------------------------------------------

  // The coarsest bucket width that still reads as an hour of the day. Wider
  // grids label their busiest bucket by date instead.
  var HOUR_MS = 3600000;

  // A hole in the series grid: the server emits every bucket in the window,
  // traffic or not, so a jump wider than this many bucket widths means a
  // bucket is genuinely MISSING and the sparkline breaks rather than drawing
  // a line across it.
  var SERIES_GAP_FACTOR = 1.5;

  // How many provider cards the row shows before the rest collapse behind one
  // overflow card. Few providers is the common case, so the overflow rarely
  // engages -- but a machine with a dozen credentials must not push the KPI
  // grid off the first screen.
  var PROVIDER_CARD_CAP = 3;

  // A partial cost is the sum of the priced rows only; the qualifier rides
  // every figure that is one, so no partial dollar amount reads as the total.
  var PARTIAL_COST_NOTE = 'priced subtotal only';

  // Whether the overflow list under the provider row is open. View state only:
  // it survives the per-poll rebuild and reaches no fetch.
  var providerOverflowOpen = false;

  // The last UNSCOPED Overview view: its `{groups, totals}` are the whole
  // provider row and the share each provider holds of it.
  //
  // A provider-scoped QUERY narrows `groups` to the one scoped provider, so the
  // scoped response alone cannot draw the row the scope was set from -- and the
  // design requires every card to stay on screen while scoped, so the operator
  // can move the scope or reset it there. Held from the unscoped read and
  // refreshed every time one lands, so the row is never older than the last
  // unscoped poll and never invented from the scoped totals.
  //
  // Keyed by the window it was read for: a retained row from another window
  // would put one window's shares beside another window's tiles.
  var providerRowView = null;
  var providerRowWindow = null;

  // The provider scope lives on the provider cards themselves (the aggregate
  // card is the reset), so this tab carries no separate scope strip. Those
  // cards are part of the query-backed content, so the view composes the
  // reset line beside this section for the case they are not drawn
  // (scopeRecovery).
  function buildOverview(rec) {
    return safeSection(rec, buildOverviewLive);
  }

  // ---- needs attention -------------------------------------------------

  // The daemon's needs-attention list as it sent it: which conditions count,
  // their severity, wording, age, raw token and fix all come from the panel,
  // in the panel's order. The page adds only how long ago a cleared item
  // recovered, read against the panel's own as_of. Every string is set as
  // text.
  var MINUTE_MS = 60000;

  function attentionSection(rec) {
    var data = rec.data;
    if (!Array.isArray(data.items)) {
      throw new Error('attention payload carries no item list');
    }
    var counts = attentionTally(data.counts);
    var wrap = document.createElement('div');
    wrap.className = 'ovsection attn';
    var head = sectionHead('Needs attention', null);
    head.appendChild(attentionCounts(counts));
    wrap.appendChild(head);
    var active = data.items.filter(function (item) { return item.state !== 'recovered'; });
    if (!active.length) { wrap.appendChild(attentionClear()); }
    if (data.items.length) { wrap.appendChild(attentionList(data.items, panelNowMs(rec))); }
    return wrap;
  }

  // The tally of what is active now, as the daemon counted it. Recovered
  // items are listed below but are not part of it.
  function attentionCounts(counts) {
    var strip = document.createElement('span');
    strip.className = 'attn-counts';
    [['broken', counts.broken], ['degraded', counts.degraded]].forEach(function (c) {
      var n = c[1];
      if (n <= 0) { return; }
      var part = document.createElement('span');
      part.className = 'attn-count attn-count--' + c[0];
      part.setAttribute('data-severity', c[0]);
      part.textContent = n + ' ' + c[0];
      strip.appendChild(part);
    });
    return strip;
  }

  function attentionClear() {
    var line = document.createElement('p');
    line.className = 'attn-clear';
    line.textContent = 'Nothing needs attention now.';
    return line;
  }

  function attentionList(items, nowMs) {
    var list = document.createElement('div');
    list.className = 'attn-list';
    var seen = Object.create(null);
    items.forEach(function (item) {
      list.appendChild(attentionItem(item, uniqueItemId('now-attention-' + idKey(item.key), seen), nowMs));
    });
    return list;
  }

  // One item, its own refresh unit: a selection inside its fix or raw token
  // holds this item alone while the rest of the list keeps updating.
  function attentionItem(item, id, nowMs) {
    var recovered = item.state === 'recovered';
    var row = markRefreshUnit(document.createElement('div'), id);
    row.id = id;
    row.className = 'attn-item attn-item--' + (recovered ? 'recovered' : String(item.severity));
    row.appendChild(attentionHead(item, recovered));
    row.appendChild(attentionText('div', 'attn-cause', item.cause));
    if (recovered) {
      row.appendChild(attentionText('div', 'attn-recovered', recoveredText(item.recovered_at_ms, nowMs)));
    }
    if (item.fix) { row.appendChild(attentionFix(item.fix)); }
    if (item.remediation) { row.appendChild(attentionRemediation(item.remediation)); }
    row.appendChild(attentionRaw(item.raw_token));
    return row;
  }

  function attentionHead(item, recovered) {
    var head = document.createElement('div');
    head.className = 'attn-head';
    var dot = document.createElement('span');
    dot.className = 'attn-dot';
    dot.setAttribute('aria-hidden', 'true');
    head.appendChild(dot);
    head.appendChild(attentionText('span', 'attn-sev', recovered ? 'recovered' : item.severity));
    head.appendChild(attentionText('span', 'attn-subject', item.subject));
    var link = attentionTargetLink(item.target);
    if (link) { head.appendChild(link); }
    return head;
  }

  // The fix is the line to run, so it carries the item's emphasis and is a
  // focusable code element: Tab reaches it and Ctrl+A / Ctrl+C copy it.
  function attentionFix(fix) {
    var wrap = document.createElement('div');
    wrap.className = 'attn-fix';
    wrap.appendChild(attentionText('span', 'attn-label', 'fix'));
    wrap.appendChild(attentionCode('attn-fix-code', fix));
    return wrap;
  }

  // A remediation is an instruction in words, not a line to run, so it is
  // drawn as plain text rather than as a copyable code element.
  function attentionRemediation(text) {
    var wrap = document.createElement('div');
    wrap.className = 'attn-fix attn-remediation';
    wrap.appendChild(attentionText('span', 'attn-label', 'fix'));
    wrap.appendChild(attentionText('span', 'attn-remediation-text', text));
    return wrap;
  }

  function attentionRaw(token) {
    var wrap = document.createElement('div');
    wrap.className = 'attn-raw';
    wrap.appendChild(attentionText('span', 'attn-label', 'raw'));
    wrap.appendChild(attentionCode('attn-raw-code', token));
    return wrap;
  }

  function attentionCode(cls, value) {
    var code = document.createElement('code');
    code.className = cls;
    code.tabIndex = 0;
    code.textContent = String(value);
    return code;
  }

  function attentionText(tag, cls, value) {
    var node = document.createElement(tag);
    node.className = cls;
    node.textContent = (value === null || value === undefined) ? '' : String(value);
    return node;
  }

  // A link to the view that shows the item's detail. Only a registered view
  // is linked; any other token gets no link rather than one to nowhere.
  function attentionTargetLink(target) {
    var view = (typeof target === 'string') ? viewFor(target) : null;
    if (!view) { return null; }
    var link = viewLink(view);
    link.className = 'attn-target';
    return link;
  }

  // Whole minutes since the item was first seen cleared, against the panel's
  // own clock, so client skew cannot move it.
  function recoveredText(recoveredAtMs, nowMs) {
    var at = Number(recoveredAtMs);
    if (recoveredAtMs === null || recoveredAtMs === undefined || !isFinite(at) || !isFinite(nowMs)) {
      return 'recovered';
    }
    var min = Math.floor(Math.max(0, nowMs - at) / MINUTE_MS);
    return min < 1 ? 'recovered under 1 min ago' : 'recovered ' + min + ' min ago';
  }

  // ---- end needs attention ---------------------------------------------

  // ---- accounts at a glance --------------------------------------------

  // Every subscription account in the current config, under each pool it
  // dispatches in; an account in no pool is a pool of its own. Read from the
  // accounts panel alone. API-key and cloud credentials report no usage
  // windows and are left to the Accounts view.
  //
  // A meter stays the accent until a window is nearly used, then amber, then
  // red at its limit, and a word rides with each of those colors. The line
  // is the one the daemon's attention list uses for "near its limit"
  // (above 90% used). A stale reading is dimmed, never recolored.
  var GLANCE_NEAR_FULL = 0.9;
  var DAY_SECS = 86400;
  var HOUR_SECS = 3600;

  function accountsGlanceSection(rec) {
    var data = rec.data;
    if (!isPlainObject(data) || !Array.isArray(data.accounts)) {
      throw new Error('accounts payload carries no account list');
    }
    var wrap = document.createElement('div');
    wrap.className = 'ovsection glance';
    var head = sectionHead('Accounts', 'subscription logins by pool');
    var detail = viewFor('accounts');
    if (detail) {
      var link = viewLink(detail);
      link.className = 'glance-link';
      head.appendChild(link);
    }
    wrap.appendChild(head);
    var pools = glancePools(data.accounts);
    if (!pools.length) {
      wrap.appendChild(attentionText('p', 'glance-none', 'No subscription login in this config.'));
      return wrap;
    }
    var grid = document.createElement('div');
    grid.className = 'glance-grid';
    var nowMs = panelNowMs(rec);
    pools.forEach(function (pool) { grid.appendChild(glancePool(pool, nowMs)); });
    wrap.appendChild(grid);
    return wrap;
  }

  // Named pools first, by name, each listing its members in the panel's
  // account order; then each pool-less account as a pool of one.
  function glancePools(accounts) {
    var named = Object.create(null);
    var solo = [];
    accounts.forEach(function (row) {
      if (!isPlainObject(row) || row.subscription !== true) { return; }
      var pools = Array.isArray(row.pools) ? row.pools : [];
      if (!pools.length) {
        solo.push({ key: String(row.account), name: null, seats: [row] });
        return;
      }
      pools.forEach(function (p) {
        var name = String(p);
        if (!named[name]) { named[name] = { key: name, name: name, seats: [] }; }
        named[name].seats.push(row);
      });
    });
    return Object.keys(named).sort().map(function (k) { return named[k]; }).concat(solo);
  }

  function glancePool(pool, nowMs) {
    var prefix = pool.name === null ? 'now-solo-' : 'now-pool-';
    var id = prefix + idKey(pool.key);
    var card = markRefreshUnit(document.createElement('div'), id);
    card.id = id;
    card.className = 'glance-pool';
    card.appendChild(attentionText('div', 'glance-pool-name',
      pool.name === null ? 'no pool' : 'pool ' + pool.name));
    pool.seats.forEach(function (row) {
      card.appendChild(glanceSeat(row, id, nowMs));
    });
    return card;
  }

  // A seat is named "provider_entry (login_label)", or by its entry alone
  // when the login carries no label.
  function glanceSeatName(row) {
    var name = String(row.provider_entry);
    return row.login_label ? name + ' (' + String(row.login_label) + ')' : name;
  }

  function glanceSeat(row, poolId, nowMs) {
    var seat = document.createElement('div');
    seat.id = poolId + '-' + idKey(row.account);
    seat.className = 'glance-seat';
    seat.setAttribute('data-account', String(row.account));
    var head = document.createElement('div');
    head.className = 'glance-seat-head';
    head.appendChild(attentionText('span', 'glance-seat-name', glanceSeatName(row)));
    head.appendChild(glanceLogin(row.credential));
    seat.appendChild(head);
    seat.appendChild(glanceReading(row, nowMs));
    return seat;
  }

  // The login state as one word. The daemon's cause and fix for a login
  // that needs one are the attention item's and the Accounts view's to show:
  // on Now one cause appears once.
  var GLANCE_LOGIN_WORDS = {
    usable: 'usable',
    unusable: 'unusable',
    parked: 'parked',
    not_built: 'not built'
  };

  function glanceLogin(credential) {
    var c = isPlainObject(credential) ? credential : {};
    var state = Object.prototype.hasOwnProperty.call(GLANCE_LOGIN_WORDS, c.state)
      ? c.state : 'unknown';
    var span = document.createElement('span');
    span.className = 'glance-login glance-login--' + state;
    var dot = document.createElement('span');
    dot.className = 'glance-login-dot';
    dot.setAttribute('aria-hidden', 'true');
    span.appendChild(dot);
    var words = state === 'unknown' ? 'login state unknown' : GLANCE_LOGIN_WORDS[state];
    span.appendChild(attentionText('span', 'glance-login-text', words));
    return span;
  }

  // What the account has reported: its windows, the statement that it
  // reports none, or that nothing has been read yet. Never a meter at zero
  // standing in for a missing reading.
  function glanceReading(row, nowMs) {
    var windows = Array.isArray(row.windows) ? row.windows : [];
    if (row.report === 'windows' && windows.length) {
      var list = document.createElement('div');
      list.className = 'glance-meters';
      windows.forEach(function (w) { list.appendChild(glanceMeter(w, nowMs)); });
      return list;
    }
    var text = row.report === 'no_windows' ? 'reports no usage windows' : 'no reading';
    return attentionText('div', 'glance-noreading', text);
  }

  // A window's used fraction and length are always sent with it; one missing
  // either throws to the section's invalid_payload card rather than drawing
  // a measured-looking 0% meter.
  function glanceMeter(w, nowMs) {
    if (!isPlainObject(w) || !isFiniteNumber(w.used) || !isFiniteNumber(w.length_secs)) {
      throw new Error('accounts window carries no used fraction or length');
    }
    var used = Math.max(0, w.used);
    var level = used >= 1 ? 'full' : (used > GLANCE_NEAR_FULL ? 'warn' : 'calm');
    var line = document.createElement('div');
    line.className = 'glance-meter glance-meter--' + level + (w.stale ? ' glance-meter--stale' : '');
    line.appendChild(attentionText('span', 'glance-meter-label', glanceWindowLabel(w)));
    var track = document.createElement('span');
    track.className = 'glance-bar';
    var fill = document.createElement('span');
    fill.className = 'glance-bar-fill';
    var pct = Math.min(100, used * 100);
    fill.style.setProperty('--pct', pct);
    markValue(fill, pct);
    track.appendChild(fill);
    line.appendChild(track);
    var value = figure(String(Math.round(used * 100)), '%', null);
    value.classList.add('glance-meter-pct');
    line.appendChild(value);
    var word = level === 'full' ? 'at limit' : (level === 'warn' ? 'nearly used' : '');
    if (word) { line.appendChild(attentionText('span', 'glance-meter-word', word)); }
    line.appendChild(attentionText('span', 'glance-meter-age', glanceAge(w, nowMs)));
    return line;
  }

  // The window's length in whole days or hours, then its vendor scope.
  function glanceWindowLabel(w) {
    var secs = w.length_secs;
    var span;
    if (secs > 0 && secs % DAY_SECS === 0) { span = (secs / DAY_SECS) + ' d'; }
    else if (secs > 0 && secs % HOUR_SECS === 0) { span = (secs / HOUR_SECS) + ' h'; }
    else { span = Math.round(secs / 60) + ' min'; }
    return w.scope ? span + ' ' + String(w.scope) : span;
  }

  // The reading's age against the panel's own clock, and its stale mark.
  function glanceAge(w, nowMs) {
    var age = ageSince(w.observed_at_ms, nowMs);
    var text = age === '-' ? 'age unknown' : age + ' ago';
    return w.stale ? 'stale, ' + text : text;
  }

  // ---- end accounts at a glance ----------------------------------------

  // The live Overview: one query response feeds the provider cards, the eight
  // KPI tiles, and the eight per-tile sparklines. A live Overview payload
  // always carries the series its request shape asked for, so a missing one is
  // a malformed same-version payload -- the throw lands on safeSection and
  // degrades this section alone.
  function buildOverviewLive() {
    var view = querySeriesView();
    if (!view || !view.series) {
      throw new Error('overview query payload carries no series');
    }
    if (!selectedProvider) {
      providerRowView = view;
      providerRowWindow = selectedWindow;
    }
    if (num0(view.totals.requests) <= 0) {
      return ledgerIsEmpty() ? firstRunEmpty() : overviewEmpty();
    }
    var row = providerRowWindow === selectedWindow ? providerRowView : null;
    var stack = tabStack();
    stack.appendChild(providerRowSection(row || view));
    stack.appendChild(kpiSection(view));
    return stack;
  }

  // The welcoming empty-ledger state: ONE state, never eight zero tiles and
  // never a flat zero sparkline.
  function overviewEmpty() {
    return emptyCard('No requests in this window',
      'Nothing was routed ' + WINDOW_SPAN[selectedWindow] +
      '. Pick a wider window, or send a request through the proxy.',
      ['polling every 5s', WINDOW_SPAN[selectedWindow]]);
  }

  // Whether the ledger holds no request at all, as opposed to none in the
  // selected window or scope. An unscoped all-time window answers it
  // directly; a provider scope never does. Otherwise the all-history usage
  // read answers it, and while that read has no live answer the question
  // stays open and the window's own empty state is shown. That read is not a
  // source of this view, so a late answer from it shows here on the view's
  // next render (within one 5 s round).
  function ledgerIsEmpty() {
    if (selectedProvider) { return false; }
    if (selectedWindow === 'all') { return true; }
    var rec = SOURCES[USAGE_ALL_SOURCE];
    if (rec.state !== 'live' || !rec.data || !rec.data.totals) { return false; }
    return num0(rec.data.totals.requests) <= 0;
  }

  // First run: nothing has ever been routed, so the one useful fact is where
  // to point a client. The address is the config panel's listener; while that
  // panel has no live answer the line is left out rather than guessed.
  function firstRunEmpty() {
    var card = emptyCard('No requests yet',
      'Nothing has been routed through this daemon. Point a client at the base URL ' +
      'below and its requests appear here.',
      ['polling every 5s']);
    var url = clientBaseAddress();
    if (url) { card.querySelector('.empty-inner').appendChild(baseUrlLine(url)); }
    return card;
  }

  function clientBaseAddress() {
    var rec = SOURCES.config;
    if (rec.state !== 'live' || !rec.data || !rec.data.source) { return null; }
    var addr = rec.data.source.listen_addr;
    return addr ? 'http://' + String(addr) : null;
  }

  function baseUrlLine(url) {
    var line = document.createElement('div');
    line.className = 'empty-baseurl';
    var label = document.createElement('span');
    label.textContent = 'base URL';
    var code = document.createElement('code');
    code.className = 'code-inline';
    code.tabIndex = 0;
    code.textContent = url;
    line.appendChild(label);
    line.appendChild(code);
    return line;
  }

  // Scope every figure on this tab to one provider. Picking a DIFFERENT provider
  // while scoped moves the scope rather than lifting it; the aggregate card
  // (label null) is the reset. A view-state change: commitViewState re-issues
  // the query through the standard input-changed path (which aborts the
  // in-flight request and bumps the generation, so a late response for the
  // previous scope cannot repaint this one) and records it in the address.
  function onProviderScope(label) {
    if (selectedProvider === label) { return; }
    commitViewState({ scope: label });
  }

  function onProviderOverflow() {
    providerOverflowOpen = !providerOverflowOpen;
    renderActiveTab();
  }

  // The reset affordance of last resort: shown only when a provider-scoped
  // query is unavailable or refused. The state card that replaces the content
  // then carries no provider card, so without it the scope would be
  // irreversible.
  function scopeRecovery() {
    return scopeStrip('overview', 'this provider scope has no renderable data');
  }

  // A scoped-to-one-provider header carrying the affordance that lifts the
  // scope. Overview normally resets through its aggregate provider card, so this
  // serves Usage (which has no provider cards) and Overview's recovery case.
  function scopeStrip(view, hint) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    var head = sectionHead('Scoped to ' + selectedProvider, hint);
    var reset = document.createElement('button');
    reset.type = 'button';
    reset.id = view + '-scope-reset';
    reset.className = 'scope-reset';
    reset.textContent = 'all providers';
    setAction(reset, 'provider-scope', null);
    head.appendChild(reset);
    wrap.appendChild(head);
    return wrap;
  }

  // The provider row: the all-providers aggregate card, then the busiest
  // PROVIDER_CARD_CAP providers, then -- only when more remain -- one overflow
  // card whose expansion lists the rest. Cards, not a table: a provider is a
  // separate object an operator acts on.
  //
  // Every card stays visible while a provider is scoped (the scoped one is
  // highlighted), so the operator can move the scope or reset it from the same
  // row they set it in.
  //
  // Named for the ROW it draws, not for its subject: every dash_*.js part is
  // concatenated into one function scope, so a bare `providerSection` here would
  // collide with the Config tab's same-named table builder. The later
  // declaration wins for BOTH call sites, and Config's part is concatenated
  // after this one, so it is THIS tab that loses: its row call would run the
  // table builder and throw on the view it is handed, while Config keeps
  // rendering correctly.
  function providerRowSection(view) {
    var wrap = markRefreshUnit(document.createElement('div'), 'providers');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Providers', 'pick one to scope the whole page'));
    var totalReq = num0(view.totals.requests);
    var ranked = view.groups.slice().sort(function (a, b) {
      return num0(b.metrics.requests) - num0(a.metrics.requests);
    });
    var shown = ranked.slice(0, PROVIDER_CARD_CAP);
    var rest = ranked.slice(PROVIDER_CARD_CAP);
    var grid = document.createElement('div');
    grid.className = 'provgrid';
    grid.appendChild(aggregateCard(view.totals));
    shown.forEach(function (g) {
      grid.appendChild(providerCard(g, totalReq));
    });
    if (rest.length) { grid.appendChild(overflowCard(rest, totalReq)); }
    wrap.appendChild(grid);
    if (rest.length && providerOverflowOpen) {
      wrap.appendChild(providerOverflowList(rest, totalReq));
    }
    return wrap;
  }

  // The leading card: the window's TOTALS, not a group -- one hundred percent of
  // the traffic by construction, and the affordance that lifts a provider scope.
  function aggregateCard(totals) {
    var card = provCardShell(null, totals, 'all providers',
      humanCount(totals.requests) + ' req', 100);
    card.classList.add('provcard--all');
    return card;
  }

  function providerCard(group, totalReq) {
    var m = group.metrics;
    var share = totalReq > 0 ? num0(m.requests) / totalReq * 100 : 0;
    return provCardShell(group.label, m, group.label,
      humanCount(m.requests) + ' req', share);
  }

  // The shared card face: name, cost, the design's two facts (requests and the
  // share of traffic they are) and the share bar, all inside the card's scope
  // button.
  function provCardShell(label, m, name, reqFact, share) {
    var scoped = selectedProvider === label;
    var cardEl = document.createElement('div');
    cardEl.className = 'provcard' + (scoped ? ' provcard--on' : '');

    var btn = document.createElement('button');
    btn.type = 'button';
    btn.id = label === null ? 'overview-provider-all' : 'overview-provider-' + idKey(label);
    btn.className = 'provcard-scope';
    btn.setAttribute('aria-pressed', scoped ? 'true' : 'false');

    var nameEl = document.createElement('span');
    nameEl.className = 'provcard-name';
    nameEl.textContent = name;
    nameEl.title = name;
    btn.appendChild(nameEl);

    var cost = document.createElement('span');
    cost.className = 'provcard-cost';
    cost.appendChild(costFigure(m));
    btn.appendChild(cost);
    if (m.cost_status === 'partial') {
      var qual = document.createElement('span');
      qual.className = 'provcard-qual';
      qual.textContent = PARTIAL_COST_NOTE;
      btn.appendChild(qual);
    }

    var facts = document.createElement('span');
    facts.className = 'provcard-facts';
    facts.textContent = reqFact + ' - ' + Math.round(share) + '% of traffic';
    btn.appendChild(facts);

    btn.appendChild(shareBar(share));
    setAction(btn, 'provider-scope', label);
    cardEl.appendChild(btn);
    return cardEl;
  }

  // The overflow card: how many providers are folded away, their combined
  // requests, and the control that lists them. Not a scope affordance -- a set
  // of providers is not a provider, and the QUERY scopes to one.
  function overflowCard(rest, totalReq) {
    var reqs = rest.reduce(function (a, g) { return a + num0(g.metrics.requests); }, 0);
    var share = totalReq > 0 ? reqs / totalReq * 100 : 0;
    var cardEl = document.createElement('div');
    cardEl.className = 'provcard provcard--more';
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.id = 'overview-provider-more';
    btn.className = 'provcard-scope';
    btn.setAttribute('aria-expanded', providerOverflowOpen ? 'true' : 'false');

    var nameEl = document.createElement('span');
    nameEl.className = 'provcard-name';
    nameEl.textContent = rest.length + phrase(rest.length, ' more provider', ' more providers');
    btn.appendChild(nameEl);

    var facts = document.createElement('span');
    facts.className = 'provcard-facts';
    facts.textContent = humanCount(reqs) + ' req - ' +
      (providerOverflowOpen ? 'hide' : 'show all');
    btn.appendChild(facts);

    btn.appendChild(shareBar(share));
    setAction(btn, 'provider-overflow', null);
    cardEl.appendChild(btn);
    return cardEl;
  }

  // The expanded overflow: the folded providers as their own cards, so a
  // provider reached through the overflow carries the same reading and the same
  // scope affordance as one on the row above.
  function providerOverflowList(rest, totalReq) {
    var grid = document.createElement('div');
    grid.className = 'provgrid provgrid--rest';
    rest.forEach(function (g) {
      grid.appendChild(providerCard(g, totalReq));
    });
    return grid;
  }

  // The eight KPI tiles, hairline-gridded so they read as facets of one
  // reading. Each carries its own sparkline over the SERVER's per-bucket
  // series -- no point is synthesized, and each is drawn at its own bucket
  // start rather than on an assumed stride.
  //
  // The provider scope is labeled HERE, directly above the grid, because these
  // are the figures the scope moves: a label further up the page would leave the
  // reader to remember which block it governs.
  function kpiSection(view) {
    var t = view.totals;
    var series = view.series;
    var gap = num0(series.bucket_ms) * SERIES_GAP_FACTOR;
    var reqSpark = bucketSamples(series, function (m) { return m.requests; });
    var wrap = markRefreshUnit(document.createElement('div'), 'kpis');
    wrap.className = 'ovsection';
    if (selectedProvider) {
      wrap.appendChild(sectionHead('Scoped to ' + selectedProvider,
        'every number below is scoped to this provider'));
    }
    var grid = document.createElement('div');
    grid.className = 'kpigrid';
    [
      requestsTile(t, reqSpark, gap),
      ttftTile(t, series, gap),
      fallbackTile(t, series, gap),
      busiestTile(t, series, reqSpark, gap),
      tokensTile(t, series, gap),
      cacheTrafficTile(t, series, gap),
      cacheHitTile(t, series, gap),
      costTile(t, series, gap)
    ].forEach(function (tile) { grid.appendChild(tile); });
    wrap.appendChild(grid);
    return wrap;
  }

  // One sample per bucket, its value exactly as read: a bucket with nothing
  // reported stays null so its sparkline draws a gap, never a zero.
  function bucketSamples(series, read) {
    return series.buckets.map(function (b) {
      return { t: b.start_ms, v: read(b.metrics) };
    });
  }

  function requestsTile(t, reqSpark, gap) {
    var errs = num0(t.errors);
    return kpiTile('Requests',
      magSpan(t.requests),
      subNote(errs === 0 ? 'no errors' : humanCount(errs) + ' errors', errs > 0),
      sparkline(reqSpark, gap));
  }

  // Time to first token. The headline is the mean over streamed successes;
  // the figure beside it is the observed maximum. Neither is a percentile, so
  // the tile says avg and max.
  function ttftTile(t, series, gap) {
    var avg = msParts(t.ttft_avg_ms);
    var observed = num0(t.ttft_avg_ms) > 0;
    return kpiTile('Time to first token (avg)',
      figure(avg.v, avg.u, null),
      wrappedNote(observed
        ? 'max ' + msText(t.ttft_max_ms) + ' - avg over streamed successes'
        : 'no streamed response in this window'),
      sparkArea(bucketSamples(series, function (m) { return m.ttft_avg_ms; }), gap));
  }

  function fallbackTile(t, series, gap) {
    var req = num0(t.requests), served = num0(t.fallback_served);
    var pct = req > 0 ? Math.round(served / req * 100) : 0;
    return kpiTile('Served by fallback',
      figure(String(pct), '%', null),
      subNote(served === 0
        ? 'primary held for every request'
        : humanCount(served) + ' of ' + humanCount(req) + ' took a later step', false),
      sparkArea(bucketSamples(series, function (m) { return m.fallback_served; }), gap));
  }

  // The busiest bucket of the window, read off the same request series the
  // tile draws, so the peak and the curve cannot disagree.
  function busiestTile(t, series, reqSpark, gap) {
    var bucketMs = num0(series.bucket_ms);
    var peak = busiestBucket(series);
    var peakReq = peak ? num0(peak.metrics.requests) : 0;
    var req = num0(t.requests);
    return kpiTile('Busiest ' + (bucketMs <= HOUR_MS ? 'hour' : 'day'),
      figure(peak ? bucketLabel(peak.start_ms, bucketMs) : '-', null, null),
      subNote(peak
        ? humanCount(peakReq) + ' req - ' + (req > 0 ? Math.round(peakReq / req * 100) : 0) +
          '% of the window'
        : 'no bucketed traffic in this window', false),
      sparkArea(reqSpark, gap));
  }

  function busiestBucket(series) {
    var best = null;
    series.buckets.forEach(function (b) {
      if (!best || num0(b.metrics.requests) > num0(best.metrics.requests)) { best = b; }
    });
    return (best && num0(best.metrics.requests) > 0) ? best : null;
  }

  // An hour-grid bucket reads as an hour of its day; anything wider reads as
  // a date. Both come from the bucket's own start instant.
  function bucketLabel(startMs, bucketMs) {
    var d = new Date(num0(startMs));
    if (isNaN(d.getTime())) { return '-'; }
    if (bucketMs > 0 && bucketMs <= HOUR_MS) {
      var h = d.getHours();
      return (h < 10 ? '0' : '') + h + ':00';
    }
    return d.toLocaleDateString();
  }

  function tokensTile(t, series, gap) {
    var req = num0(t.requests);
    var inSpark = bucketSamples(series, function (m) { return m.input_tokens; });
    var outSpark = bucketSamples(series, function (m) { return m.output_tokens; });
    return kpiSplitTile('Tokens',
      [['in', magSpan(t.input_tokens), false], ['out', magSpan(t.output_tokens), true]],
      subNote(req > 0
        ? humanCount(Math.round(num0(t.input_tokens) / req)) + ' in / ' +
          humanCount(Math.round(num0(t.output_tokens) / req)) + ' out avg/req'
        : 'no requests to average over', false),
      sparklinePair(inSpark, outSpark, gap));
  }

  // Cache volumes come from the ledger's own billed read and write counters,
  // never from a ratio of the input tokens.
  function cacheTrafficTile(t, series, gap) {
    var read = num0(t.cache_read_billed);
    var written = cacheWritten(t);
    var readSpark = bucketSamples(series, function (m) { return m.cache_read_billed; });
    var writeSpark = bucketSamples(series, cacheWritten);
    return kpiSplitTile('Cache traffic',
      [['read', magSpan(read), false], ['write', magSpan(written), true]],
      subNote(cacheAmplification(read, written), false),
      sparklinePair(readSpark, writeSpark, gap));
  }

  // Written cache volume is the write total, which spans every cache lifetime
  // and both ledger eras. The per-lifetime fields cover older rows only. With
  // no row reporting a write the total is null, never zero: a provider that
  // sent no write field has not been observed writing nothing, and magSpan
  // renders the null as the faint "-".
  function cacheWritten(m) {
    return num0(m.cache_write_present) > 0 ? num0(m.cache_write_tokens) : null;
  }

  // How many times over the window read back what it wrote. Undefined without
  // a write, so it says so instead of dividing by zero.
  function cacheAmplification(read, written) {
    if (written === null) { return 'no cache write reported'; }
    if (written <= 0) {
      return read > 0 ? 'read from an earlier window' : 'nothing written to cache yet';
    }
    return (read / written).toFixed(1) + 'x read amplification';
  }

  function cacheHitTile(t, series, gap) {
    var hit = num0(t.cache_hit_pct);
    var shown = t.cache_hit_pct === null ? figure('-', null, null)
      : figure(hit > 0 ? hit.toFixed(1) : '0', '%', null);
    return kpiTile('Cache hit',
      shown,
      wrappedNote(hit > 0
        ? 'weighted by prompt tokens'
        : 'nothing served warm yet'),
      sparkArea(bucketSamples(series, function (m) { return m.cache_hit_pct; }), gap));
  }

  // A note that qualifies what its figure means: it wraps rather than
  // truncating, and carries its full text as a title as well.
  function wrappedNote(text) {
    var note = subNote(text, false);
    note.classList.add('kpi-sub-wrap');
    note.title = text;
    return note;
  }

  // The spend figure, then the API-price equivalent of the subscription rows
  // as a separate figure: the two are different quantities (money spent and
  // notional replacement value) and are never added together.
  function costTile(t, series, gap) {
    var tile = kpiTile('Est. cost',
      costFigure(t),
      wrappedNote(costNote(t)),
      sparkArea(bucketSamples(series, function (m) { return m.cost_usd; }), gap));
    var spark = tile.lastChild;
    var alt = apiPriceBlock(t.equivalent_cost_usd);
    if (alt) { tile.insertBefore(alt, tile.querySelector('.kpi-sub')); }
    if (t.cost_status === 'partial') { tile.insertBefore(costSparkCaption(), spark); }
    return tile;
  }

  // Absent, not zero, when no subscription row could be priced at API rates:
  // the server sends null then, and there is nothing to approximate.
  function apiPriceBlock(equivalent) {
    if (equivalent === null || equivalent === undefined) { return null; }
    var block = document.createElement('div');
    block.className = 'kpi-alt';
    var head = document.createElement('div');
    head.className = 'kpi-alt-head';
    var label = document.createElement('span');
    label.className = 'kpi-alt-label';
    label.textContent = 'At API prices';
    var value = figure(money(equivalent), null, '~$');
    value.classList.add('kpi-alt-value');
    head.appendChild(label);
    head.appendChild(value);
    var note = document.createElement('div');
    note.className = 'kpi-alt-note';
    note.textContent = 'what this traffic would cost on pay-as-you-go API pricing';
    block.appendChild(head);
    block.appendChild(note);
    return block;
  }

  // Each bucket is priced over its own rows, so a row unpriced over the
  // window can be priced in a bucket, and the buckets can then add up to more
  // than a partial headline.
  function costSparkCaption() {
    var caption = document.createElement('div');
    caption.className = 'kpi-caption';
    caption.textContent = 'each bucket priced on its own; buckets need not sum to the subtotal';
    return caption;
  }

  // The honest cost read: a dollar figure ONLY where the rows were priced. An
  // unpriced or managed-subscription window has no dollar cost to show, so it
  // says which of the two it is rather than claiming a zero; a real zero
  // renders faint, never as a failure.
  function costFigure(m) {
    var lab = labelFor('cost', m.cost_status);
    var priced = m.cost_status === 'priced' || m.cost_status === 'partial';
    var span = priced ? figure(money(m.cost_usd), null, '$') : faintFigure(lab.label);
    if (lab.title) { span.title = lab.title; }
    return span;
  }

  function costNote(t) {
    var req = num0(t.requests);
    if (t.cost_status === 'priced') {
      return req > 0 ? '$' + (num0(t.cost_usd) / req).toFixed(3) + ' avg/req' : 'priced';
    }
    if (t.cost_status === 'partial') { return PARTIAL_COST_NOTE + ' - some rows have no price'; }
    if (t.cost_status === 'subscription') { return 'managed subscription - no per-token cost'; }
    if (t.cost_status === 'unpriced') { return 'no price resolved - configure pricing'; }
    return labelFor('cost', t.cost_status).label;
  }

  function kpiTile(label, valueNode, subNode, sparkNode) {
    var tile = document.createElement('div');
    tile.className = 'kpi';
    var l = document.createElement('div');
    l.className = 'kpi-label';
    l.textContent = label;
    var v = document.createElement('div');
    v.className = 'kpi-value';
    v.appendChild(valueNode);
    var s = document.createElement('div');
    s.className = 'kpi-sub';
    s.appendChild(subNode);
    tile.appendChild(l);
    tile.appendChild(v);
    tile.appendChild(s);
    tile.appendChild(sparkNode);
    return tile;
  }

  // A tile carrying TWO figures of one reading (in/out, read/write), each
  // keyed to the hue its series is drawn in.
  function kpiSplitTile(label, halves, subNode, sparkNode) {
    var split = document.createElement('div');
    split.className = 'kpi-split';
    halves.forEach(function (half) {
      var col = document.createElement('div');
      col.className = 'kpi-half';
      var key = document.createElement('span');
      key.className = 'kpi-key';
      var dot = document.createElement('span');
      dot.className = 'kpi-dot' + (half[2] ? ' kpi-dot--b' : '');
      key.appendChild(dot);
      key.appendChild(document.createTextNode(half[0]));
      var v = document.createElement('span');
      v.className = 'kpi-halfvalue';
      v.appendChild(half[1]);
      col.appendChild(key);
      col.appendChild(v);
      split.appendChild(col);
    });
    return kpiTile(label, split, subNode, sparkNode);
  }
  // ---- end tab:overview ------------------------------------------------

