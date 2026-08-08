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
  // card is the reset), so this tab carries no separate scope strip. But those
  // cards are part of the query-backed content: when a provider-scoped query is
  // unavailable or refused, the state card that replaces the content carries no
  // affordance at all, so the scope would be irreversible. The lone reset line
  // below renders ONLY in that case.
  function buildOverview(rec) {
    var section = safeSection(rec, buildOverviewLive);
    if (!selectedProvider || !stateCard(rec)) { return section; }
    var stack = tabStack();
    stack.appendChild(scopeRecovery());
    stack.appendChild(section);
    return stack;
  }

  // The live Overview: one query response feeds the provider cards, the eight
  // KPI tiles, and the eight per-tile sparklines. A live Overview payload
  // always carries the series its request shape asked for, so a missing one is
  // a malformed same-version payload -- the throw lands on safeSection and
  // degrades this section alone.
  function buildOverviewLive() {
    var view = queryView();
    if (!view || !view.series) {
      throw new Error('overview query payload carries no series');
    }
    if (!selectedProvider) {
      providerRowView = view;
      providerRowWindow = selectedWindow;
    }
    if (num0(view.totals.requests) <= 0) { return overviewEmpty(); }
    var row = providerRowWindow === selectedWindow ? providerRowView : null;
    var stack = tabStack();
    stack.appendChild(providerSection(row || view));
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

  // Scope every figure on this tab to one provider. Picking a DIFFERENT provider
  // while scoped moves the scope rather than lifting it; the aggregate card
  // (label null) is the reset. The query is re-issued through the standard
  // input-changed path (which aborts the in-flight request and bumps the
  // generation, so a late response for the previous scope cannot repaint this
  // one), and only this tab repaints -- the GET-backed siblings read no query
  // source.
  function onProviderScope(label) {
    if (selectedProvider === label) { return; }
    selectedProvider = label;
    queryInputChanged();
    renderActiveTab();
    renderVerdict();
  }

  function onProviderOverflow() {
    providerOverflowOpen = !providerOverflowOpen;
    renderActiveTab();
  }

  // The reset affordance of last resort: shown only when the scoped query is
  // unrenderable, so the cards that normally carry the reset are not on screen.
  function scopeRecovery() {
    return scopeStrip('this provider scope has no renderable data');
  }

  // A scoped-to-one-provider header carrying the affordance that lifts the
  // scope. Overview normally resets through its aggregate provider card, so this
  // serves Usage (which has no provider cards) and Overview's recovery case.
  function scopeStrip(hint) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    var head = sectionHead('Scoped to ' + selectedProvider, hint);
    var reset = document.createElement('button');
    reset.type = 'button';
    reset.className = 'scope-reset';
    reset.textContent = 'all providers';
    reset.addEventListener('click', function () { onProviderScope(null); });
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
  // row they set it in. The seat quota rides on the card because a provider's
  // credential headroom is a fact about that provider; it comes from the usage
  // source, not the query one, so it is read through the seat index below and a
  // usage failure costs the seat surface alone.
  function providerSection(view) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Providers', 'pick one to scope the whole page'));
    var totalReq = num0(view.totals.requests);
    var seats = seatIndex(SOURCES.usage);
    var ranked = view.groups.slice().sort(function (a, b) {
      return num0(b.metrics.requests) - num0(a.metrics.requests);
    });
    var shown = ranked.slice(0, PROVIDER_CARD_CAP);
    var rest = ranked.slice(PROVIDER_CARD_CAP);
    var grid = document.createElement('div');
    grid.className = 'provgrid';
    grid.appendChild(aggregateCard(view.totals, seats));
    shown.forEach(function (g) {
      grid.appendChild(providerCard(g, totalReq, seats));
    });
    if (rest.length) { grid.appendChild(overflowCard(rest, totalReq)); }
    wrap.appendChild(grid);
    if (rest.length && providerOverflowOpen) {
      wrap.appendChild(providerOverflowList(rest, totalReq, seats));
    }
    renderSeatModal(seats);
    return wrap;
  }

  // The leading card: the window's TOTALS, not a group -- one hundred percent of
  // the traffic by construction, and the affordance that lifts a provider scope.
  function aggregateCard(totals, seats) {
    var card = provCardShell(null, totals, 'all providers',
      humanCount(totals.requests) + ' req', 100, seats);
    card.classList.add('provcard--all');
    return card;
  }

  function providerCard(group, totalReq, seats) {
    var m = group.metrics;
    var share = totalReq > 0 ? num0(m.requests) / totalReq * 100 : 0;
    return provCardShell(group.label, m, group.label,
      humanCount(m.requests) + ' req', share, seats);
  }

  // The shared card face: name, cost, the design's two facts (requests and the
  // share of traffic they are), the share bar, and the provider's seat
  // affordance. The scope affordance is its own button INSIDE the card so the
  // seat control beside it is separate -- a button cannot contain one, and a
  // card-wide handler would fire the scope change on every seat click.
  function provCardShell(label, m, name, reqFact, share, seats) {
    var scoped = selectedProvider === label;
    var cardEl = document.createElement('div');
    cardEl.className = 'provcard' + (scoped ? ' provcard--on' : '');

    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'provcard-scope';
    btn.setAttribute('aria-pressed', scoped ? 'true' : 'false');

    var nameEl = document.createElement('span');
    nameEl.className = 'provcard-name';
    nameEl.textContent = name;
    btn.appendChild(nameEl);

    var cost = document.createElement('span');
    cost.className = 'provcard-cost';
    cost.appendChild(costFigure(m));
    btn.appendChild(cost);

    var facts = document.createElement('span');
    facts.className = 'provcard-facts';
    facts.textContent = reqFact + ' - ' + Math.round(share) + '% of traffic';
    btn.appendChild(facts);

    btn.appendChild(shareBar(share));
    btn.addEventListener('click', function () { onProviderScope(label); });
    cardEl.appendChild(btn);

    if (label !== null) {
      var seatBlock = providerSeats(label, seats);
      if (seatBlock) { cardEl.appendChild(seatBlock); }
    }
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
    btn.addEventListener('click', onProviderOverflow);
    cardEl.appendChild(btn);
    return cardEl;
  }

  // The expanded overflow: the folded providers as their own cards, so a
  // provider reached through the overflow carries the same reading and the same
  // scope affordance as one on the row above.
  function providerOverflowList(rest, totalReq, seats) {
    var grid = document.createElement('div');
    grid.className = 'provgrid provgrid--rest';
    rest.forEach(function (g) {
      grid.appendChild(providerCard(g, totalReq, seats));
    });
    return grid;
  }

  // The usage panel's quota rows grouped by the provider whose credential each
  // seat belongs to, or null when that source carries nothing renderable.
  //
  // Guarded exactly as `safeSection` guards a section node: a throw is recorded
  // against the usage source so the pane status line and the page verdict
  // report the degraded seat surface, while the KPI and provider blocks --
  // which read the query source -- keep rendering untouched. Returning null
  // rather than a card is what keeps the failure inside the seat affordance.
  function seatIndex(rec) {
    if (stateCard(rec)) { return null; }
    try {
      var index = groupQuotaByProvider(rec);
      markRenderAttempt(rec.name);
      return index;
    } catch (e) {
      recordRenderFault(rec.name);
      return null;
    }
  }

  // A seat key is `provider` or `provider#label`, so the provider a quota row
  // belongs to is the segment before the label separator. A row whose seat is
  // absent (legacy history, a forwarded client credential) belongs to no
  // provider card and is left to the Health tab's full list -- attaching it to
  // a card by guessing would claim a credential identity nobody recorded.
  function groupQuotaByProvider(rec) {
    if (!rec.data || !Array.isArray(rec.data.quota)) {
      throw new Error('usage payload carries no quota list');
    }
    var nowMs = panelNowMs(rec);
    var byProvider = Object.create(null);
    rec.data.quota.forEach(function (q) {
      if (!q || !q.seat) { return; }
      var provider = String(q.seat).split(SEAT_LABEL_SEP)[0];
      if (!provider) { return; }
      if (!byProvider[provider]) { byProvider[provider] = []; }
      byProvider[provider].push(q);
    });
    return { byProvider: byProvider, nowMs: nowMs };
  }

  // The seat-key label separator (`provider#label`), built from its char code
  // for the same reason HASH_SEP is: this script carries no literal that the
  // mutation-channel scan in page.rs would have to reason about.
  var SEAT_LABEL_SEP = String.fromCharCode(35);

  // The card's seat affordance: one dot per seat plus the seat count, opening a
  // modal that lists every seat in full. Dots, never a pooled bar -- a pooled
  // figure over several seats is a number no provider reported.
  //
  // A provider with no quota row of its own gets NO affordance at all -- an
  // empty tile would read as a reported zero, and the quota fields are the only
  // thing that may put a seat line on the page. When the usage source itself is
  // not renderable the card says so in one faint line instead, so the absence
  // is not mistaken for a provider that reports no quota.
  function providerSeats(provider, seats) {
    if (!seats) { return seatSurfaceUnavailable(); }
    var rows = seats.byProvider[provider];
    if (!rows || !rows.length) { return null; }
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'provseats';
    btn.title = 'show every seat';
    btn.appendChild(seatDots(rows));
    var note = document.createElement('span');
    note.className = 'provseats-count';
    note.textContent = seatSummary(rows);
    btn.appendChild(note);
    btn.addEventListener('click', function () { openSeatModal(provider); });
    return btn;
  }

  // One dot per seat, in the order the panel reported them. A dot carries the
  // seat's own name and nothing derived from its siblings.
  function seatDots(rows) {
    var wrap = document.createElement('span');
    wrap.className = 'seatdots';
    rows.forEach(function (q) {
      var dot = document.createElement('span');
      dot.className = 'seatdot';
      if (q.seat) { dot.title = String(q.seat); }
      wrap.appendChild(dot);
    });
    return wrap;
  }

  // How many seats this provider reports, and nothing else. No max, no average,
  // no rollup across seats -- a seat's quota is a fact about one credential, and
  // a headline over several would be a figure no provider reported.
  function seatSummary(rows) {
    return rows.length + phrase(rows.length, ' seat', ' seats');
  }

  function seatSurfaceUnavailable() {
    var note = document.createElement('p');
    note.className = 'footnote provseats-none';
    note.textContent = 'seat quota unavailable';
    return note;
  }

  // ---- seat modal ------------------------------------------------------

  // The provider whose seat modal is open, or null. View state only: it drives
  // the modal from the SAME render pass that draws the cards, so the seat
  // figures behind it keep landing with every poll instead of freezing at the
  // instant the modal was opened.
  var seatModalProvider = null;

  function openSeatModal(provider) {
    seatModalProvider = provider;
    renderActiveTab();
  }

  function closeSeatModal() {
    if (seatModalProvider === null) { return; }
    seatModalProvider = null;
    renderActiveTab();
  }

  // The provider's seats in full: ONE row per seat, each the same quota tile the
  // Health tab renders. A modal rather than an inline expander because the seat
  // list is a detour off the reading, not part of it -- and it deliberately
  // carries no footer figure over the seats it lists.
  //
  // Drawn into the page-level modal host rather than into the section, because
  // the panes are animated and an animated ancestor would make the fixed
  // backdrop center on the pane instead of the viewport.
  //
  // Draws nothing when nothing is open, or when the open provider no longer has
  // seats in the payload that just landed: a modal over a credential the panel
  // has stopped reporting would show a snapshot nothing backs.
  function renderSeatModal(seats) {
    var host = el('modal-host');
    if (!host) { return; }
    var rows = (seatModalProvider !== null && seats)
      ? seats.byProvider[seatModalProvider]
      : null;
    if (!rows || !rows.length) {
      host.replaceChildren();
      return;
    }
    var backdrop = document.createElement('div');
    backdrop.className = 'modal-backdrop';
    backdrop.addEventListener('click', closeSeatModal);
    var dialog = document.createElement('div');
    dialog.className = 'modal';
    dialog.setAttribute('role', 'dialog');
    dialog.setAttribute('aria-modal', 'true');
    dialog.setAttribute('aria-label', seatModalProvider + ' seats');
    // A click on the dialog must not reach the backdrop's close handler, or
    // every click inside the modal would dismiss it.
    dialog.addEventListener('click', function (e) { e.stopPropagation(); });
    dialog.appendChild(seatModalHead(seatModalProvider, rows));
    var body = document.createElement('div');
    body.className = 'modal-body qlist qlist--card';
    rows.forEach(function (q) { body.appendChild(quotaTile(q, seats.nowMs)); });
    dialog.appendChild(body);
    backdrop.appendChild(dialog);
    host.replaceChildren(backdrop);
  }

  function seatModalHead(provider, rows) {
    var head = document.createElement('div');
    head.className = 'modal-head';
    var title = document.createElement('span');
    title.className = 'modal-title';
    title.textContent = provider;
    head.appendChild(title);
    var sub = document.createElement('span');
    sub.className = 'modal-sub';
    sub.textContent = seatSummary(rows) + ' - each reported on its own';
    head.appendChild(sub);
    var close = document.createElement('button');
    close.type = 'button';
    close.className = 'modal-close';
    close.setAttribute('aria-label', 'Close');
    close.textContent = 'esc x';
    close.addEventListener('click', closeSeatModal);
    head.appendChild(close);
    return head;
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
    var wrap = document.createElement('div');
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

  function bucketSamples(series, read) {
    return series.buckets.map(function (b) {
      return { t: b.start_ms, v: num0(read(b.metrics)) };
    });
  }

  function requestsTile(t, reqSpark, gap) {
    var errs = num0(t.errors);
    return kpiTile('Requests',
      magSpan(t.requests),
      subNote(errs === 0 ? 'no errors' : humanCount(errs) + ' errors', errs > 0),
      sparkline(reqSpark, gap));
  }

  // Time to first token. The headline is the request-weighted p50; the peak
  // beside it is an observed MAXIMUM, so it is labeled "peak" and never as a
  // percentile.
  function ttftTile(t, series, gap) {
    var p50 = msParts(t.ttft_p50_ms);
    var observed = num0(t.ttft_p50_ms) > 0;
    return kpiTile('Time to first token',
      figure(p50.v, p50.u, null),
      subNote(observed
        ? 'peak ' + msText(t.ttft_p95_ms) + ' - weighted over ' + humanCount(t.requests) + ' req'
        : 'no streamed response in this window', false),
      sparkArea(bucketSamples(series, function (m) { return m.ttft_p50_ms; }), gap));
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

  // Cache volumes come from the ledger's own billed read and write counters
  // (both write horizons summed), never from a ratio of the input tokens.
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

  // Written cache volume is the sum of both write horizons: they are separate
  // billing lines but one physical write stream.
  function cacheWritten(m) {
    return num0(m.cache_write_5m) + num0(m.cache_write_1h);
  }

  // How many times over the window read back what it wrote. Undefined without
  // a write, so it says so instead of dividing by zero.
  function cacheAmplification(read, written) {
    if (written <= 0) {
      return read > 0 ? 'read from an earlier window' : 'nothing written to cache yet';
    }
    return (read / written).toFixed(1) + 'x read amplification';
  }

  function cacheHitTile(t, series, gap) {
    var hit = num0(t.cache_hit_pct);
    return kpiTile('Cache hit',
      figure(hit > 0 ? hit.toFixed(1) : '0', '%', null),
      subNote(hit > 0
        ? 'weighted over ' + humanCount(t.requests) + ' req'
        : 'nothing served warm yet', false),
      sparkArea(bucketSamples(series, function (m) { return m.cache_hit_pct; }), gap));
  }

  function costTile(t, series, gap) {
    return kpiTile('Est. cost',
      costFigure(t),
      subNote(costNote(t), false),
      sparkArea(bucketSamples(series, function (m) { return m.cost_usd; }), gap));
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
    if (t.cost_status === 'partial') { return 'priced subtotal only - some rows have no price'; }
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

