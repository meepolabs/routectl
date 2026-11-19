  // ---- transport -------------------------------------------------------

  // The browser tests drive the late-answer cases of the response-currency
  // guard below (page_browser_epoch_tests.rs); a real slow ledger is verified
  // BY HAND -- see dashboard-manual-checklist.md beside this source, and run
  // it before shipping a change here.

  // Issue one request under an abort budget and classify the outcome. The
  // ONLY place a fetch happens; both the GET poll and the QUERY aggregate
  // share this classifier so their failure vocabularies cannot drift.
  //
  // Outcomes: ok(json) | overloaded (503) | forbidden (403) |
  // rejected (400/405 -- a deterministic refusal of THIS request shape) |
  // timeout (abort) | network (refused / other error / other non-2xx).
  function safeRequest(url, init, ctrl, budgetMs) {
    var timeout = setTimeout(function () { ctrl.abort(); }, budgetMs);
    var opts = { signal: ctrl.signal, cache: 'no-store' };
    Object.keys(init || {}).forEach(function (k) { opts[k] = init[k]; });
    return fetch(url, opts).then(function (resp) {
      if (resp.status === 403) { return { kind: 'forbidden' }; }
      if (resp.status === 503) { return { kind: 'overloaded' }; }
      if (resp.status === 400 || resp.status === 405) {
        return { kind: 'rejected', status: resp.status };
      }
      if (!resp.ok) { return { kind: 'network' }; }
      return resp.json().then(function (json) { return { kind: 'ok', json: json }; });
    }).catch(function (err) {
      return { kind: (err && err.name === 'AbortError') ? 'timeout' : 'network' };
    }).then(function (out) {
      clearTimeout(timeout);
      return out;
    });
  }

  // GET a status URL under the TIMEOUT_MS budget. No request body, no mutating verb
  // -- fetch defaults to GET and none is set.
  function fetchStatus(url, ctrl) {
    return safeRequest(url, null, ctrl, TIMEOUT_MS);
  }

  // ---- response currency -----------------------------------------------

  // Every independently fetched source has its own epoch: the number of the
  // newest request issued for it, bumped again when the page goes terminal.
  // A request takes a ticket (source, epoch, state key) when it is issued,
  // and its answer applies only while BOTH still hold: no newer request for
  // the source was issued since (the epoch), and the page still wants the
  // source for that same state (the key, see wantedFetchKey). Aborting a
  // superseded request is only an optimization: an answer already on its way,
  // or a fetch that never honours the signal, is stopped here.
  var AGGREGATE_FETCH = 'aggregate';
  var FETCH_SOURCES = [AGGREGATE_FETCH, USAGE_ALL_SOURCE, QUERY_SOURCE, QUERY_SERIES_SOURCE];
  var FETCH_EPOCHS = {};
  FETCH_SOURCES.forEach(function (name) { FETCH_EPOCHS[name] = 0; });

  function issueFetchTicket(source, key) {
    FETCH_EPOCHS[source] += 1;
    return { source: source, epoch: FETCH_EPOCHS[source], key: key };
  }

  function fetchTicketIsCurrent(ticket) {
    return ticket.epoch === FETCH_EPOCHS[ticket.source] &&
      ticket.key === wantedFetchKey(ticket.source);
  }

  // The state key the page wants `source` answered for right now. A QUERY
  // source is wanted only while the active view issues it, and then for the
  // body it would send; a GET source asks for one fixed URL, so its key is
  // its own name.
  function wantedFetchKey(source) {
    if (source !== QUERY_SOURCE && source !== QUERY_SERIES_SOURCE) { return source; }
    var body = queryBodyFor(activeTab);
    return (body && querySourceFor(activeTab) === source) ? queryBodyKey(body) : null;
  }

  // Terminal: no answer to any request issued so far may apply, and none of
  // them is worth waiting for.
  function invalidateEveryFetch() {
    FETCH_SOURCES.forEach(function (name) { FETCH_EPOCHS[name] += 1; });
    [aggregateCtrl, usageAllCtrl, queryCtrl].forEach(function (ctrl) {
      if (ctrl) { ctrl.abort(); }
    });
    aggregateCtrl = null;
    usageAllCtrl = null;
    queryCtrl = null;
  }

  // ---- QUERY transport -------------------------------------------------

  // QUERY carries its OWN backoff state. A failing aggregate must never be
  // slowed by a failing QUERY and vice versa: the GET loop's healthy 5s
  // cadence is the page's liveness signal, so it stays 5s no matter how
  // badly the ledger aggregate is doing.
  var queryBackoffIndex = -1;      // -1 = follow the GET cadence; 0..n = index
  var queryTimer = null;
  var queryCtrl = null;            // in-flight controller, for single-flight
  // The body key a 400/405 refused, PER QUERY SOURCE. Overview and Usage
  // request different shapes against the same endpoint, so a refusal of one
  // shape says nothing about the other; keying by source keeps a rejected
  // Overview bucket from silencing Usage's retries (and vice versa) while
  // each shape's own key still stops repeating a refusal that will never
  // succeed -- a deterministic refusal repeated every 10s is noise, not
  // recovery -- until the input changes or the page reloads.
  var queryRejectedKeyBySource = {};
  // The body key of the last QUERY ATTEMPT and when it was issued. The 5s
  // aggregate nudge reads these so a selection that already fetched during
  // this interval is not fetched again the instant its round lands.
  var queryLastAttemptKey = null;
  var queryLastAttemptAtMs = 0;
  // Set when the page aborts its own in-flight QUERY, cleared once a later
  // QUERY gets an answer it renders. The server holds its one read permit until
  // the aborted read notices the cancel, so the QUERY that superseded it can
  // meet `db_busy` from the page's own abandoned read: that answer is retried
  // on the short schedule below, never rendered as an outage.
  var queryOwnAbortPending = false;
  var queryBusyRetryIndex = -1;    // -1 = no short busy retry armed
  var SUPERSEDED_BUSY_RETRY_MS = [150, 300, 600];

  // The transport-level unavailable codes a 200 QUERY can carry. These are
  // the SAME failure families the HTTP-level arms cover (a busy or
  // unreadable ledger, a fired query deadline), so they engage the same
  // QUERY-only backoff even though they arrive over a 200.
  var QUERY_RETRY_CODES = { db_busy: true, db_unavailable: true, query_timeout: true };

  // The request body for a view, or null when the view is not query-backed.
  // The shape is looked up whole from QUERY_SHAPES for the live selection and
  // copied field-for-field -- nothing is overwritten afterwards, so every body
  // this page can emit is a declared shape the drift test has validated. Only
  // the optional provider scope is added on top (the server accepts it against
  // any shape).
  function queryBodyFor(tab) {
    return bodyFromShape(shapeFor(querySourceFor(tab)));
  }

  function queryBodyForSource(source) {
    return bodyFromShape(shapeFor(source));
  }

  function bodyFromShape(shape) {
    if (!shape) { return null; }
    var body = {};
    Object.keys(shape).forEach(function (k) { body[k] = shape[k]; });
    if (selectedProvider) { body.provider = selectedProvider; }
    return body;
  }

  // The request shape each query source stands for. The bucketed source asks
  // for a series; the series-less one deliberately does not (no chart, and a
  // byte-identical non-series read path), and its grain follows the group-by
  // toggle.
  function shapeFor(source) {
    if (source === QUERY_SERIES_SOURCE) {
      return findShape(selectedWindow, 'provider', true);
    }
    if (source === QUERY_SOURCE) {
      return findShape(selectedWindow, selectedGroupBy, false);
    }
    return null;
  }

  // The source record a view's QUERY response belongs to, or null when the
  // view is not query-backed: its registered query source (see
  // viewQuerySource). One record per SHAPE, not one per route: the two shapes
  // answer with different payloads, so a single record would let whichever
  // view polled last define what the other renders from.
  function querySourceFor(tab) {
    var view = viewFor(tab);
    return view ? viewQuerySource(view) : null;
  }

  function findShape(window, groupBy, wantBucket) {
    for (var i = 0; i < QUERY_SHAPES.length; i++) {
      var s = QUERY_SHAPES[i];
      if (s.window === window && s.group_by === groupBy && !!s.bucket === wantBucket) {
        return s;
      }
    }
    return null;
  }

  // A stable key for a body: the same logical selection always serializes
  // identically, so the rejected-shape guard and the single-flight check can
  // compare selections by string.
  function queryBodyKey(body) {
    return JSON.stringify([body.window, body.group_by, body.bucket || '', body.provider || '']);
  }

  // Issue one QUERY for `source`. Single-flight: an in-flight request (for
  // either query source) is aborted, and this one's ticket supersedes every
  // earlier request for `source`, so a late response from the OLD selection
  // can never repaint the new one (it is dropped at the ticket check below
  // rather than rendered). Never reuses a consumed Request; the body is
  // stringified per logical key.
  function queryStatus(source, body) {
    if (queryCtrl) { queryCtrl.abort(); queryOwnAbortPending = true; }
    var ticket = issueFetchTicket(source, queryBodyKey(body));
    var ctrl = new AbortController();
    queryCtrl = ctrl;
    var init = {
      method: 'QUERY',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body)
    };
    return safeRequest('/status/query', init, ctrl, QUERY_TIMEOUT_MS).then(function (out) {
      if (queryCtrl === ctrl) { queryCtrl = null; }
      // A newer request for this source was issued, or the operator has left
      // the selection (or the view) this response describes.
      if (!fetchTicketIsCurrent(ticket)) { return { stale: true }; }
      return out;
    });
  }

  // Run one QUERY round for the active tab and re-arm the QUERY-only
  // schedule from its outcome. Returns a promise that never rejects.
  //
  // Deliberately carries NO in-flight guard of its own: single-flight is
  // enforced inside queryStatus, which aborts the previous request and issues
  // a newer ticket. A guard here would make an input change unable to
  // supersede a hung request -- exactly the case the ticket check exists
  // for -- so callers that must NOT supersede (the 5s nudge) check
  // queryInFlight() instead.
  function queryRound() {
    var body = queryBodyFor(activeTab);
    var source = querySourceFor(activeTab);
    if (terminal || !body || !source) { return Promise.resolve(); }
    var key = queryBodyKey(body);
    if (queryRejectedKeyBySource[source] === key) {
      // Nothing will answer a refused selection, so nothing may wait on one.
      settlePending(source);
      return Promise.resolve();
    }
    queryLastAttemptKey = key;
    queryLastAttemptAtMs = Date.now();
    return queryStatus(source, body).then(function (out) {
      if (!out || out.stale) { return; }
      if (retrySupersededBusy(out)) { return; }
      queryOwnAbortPending = false;
      queryBusyRetryIndex = -1;
      scheduleNextQuery(applyQueryOutcome(source, out, key));
    }).catch(function () {
      // A render throw must not wedge the QUERY loop; treat it as a failed
      // round and keep the GET loop untouched.
      scheduleNextQuery(false);
    });
  }

  // A short busy retry counts as in flight: it is the same selection's round,
  // still waiting for the permit its aborted predecessor holds.
  function queryInFlight() {
    return queryCtrl !== null || queryBusyRetryIndex >= 0;
  }

  // A `db_busy` answer to a QUERY sent after the page aborted its own previous
  // read. The panel stays in its loading state and the same round is retried
  // on SUPERSEDED_BUSY_RETRY_MS, outside the backoff ladder. Returns false,
  // leaving the answer to the normal path, for any other outcome, for a busy
  // gate the page did not cause, and once the short schedule is spent.
  function retrySupersededBusy(out) {
    if (!queryOwnAbortPending || out.kind !== 'ok' || !out.json) { return false; }
    if (out.json.unavailable !== 'db_busy') { return false; }
    if (queryBusyRetryIndex + 1 >= SUPERSEDED_BUSY_RETRY_MS.length) { return false; }
    queryBusyRetryIndex += 1;
    clearTimeout(queryTimer);
    queryTimer = setTimeout(queryRound, SUPERSEDED_BUSY_RETRY_MS[queryBusyRetryIndex]);
    return true;
  }

  // Whether the 5s aggregate round should nudge the QUERY at all. The nudge is
  // a TOP-UP for a selection nobody has refreshed this interval -- a selection
  // change already issues its own immediate round, so nudging right after one
  // lands would double-fetch the same body. Both arms are needed: the
  // in-flight check covers a round still running, this one covers a round that
  // has just finished.
  function queryNudgeDue() {
    var body = queryBodyFor(activeTab);
    if (!body) { return false; }
    if (queryLastAttemptKey !== queryBodyKey(body)) { return true; }
    return (Date.now() - queryLastAttemptAtMs) >= BASE_MS;
  }

  // Map a QUERY outcome onto the query source record the round was issued for.
  // Returns whether the round counts as healthy for backoff purposes.
  function applyQueryOutcome(source, out, key) {
    if (out.kind === 'forbidden') { enterTerminal(); return false; }
    if (out.kind === 'rejected') {
      // A deterministic refusal of THIS body: stop retrying it, and say so
      // rather than showing a transport failure the operator cannot fix by
      // waiting. Recorded against `source`, not page-wide, so the OTHER
      // query shape keeps retrying on its own schedule.
      queryRejectedKeyBySource[source] = key;
      setSource(source, {
        state: 'incompatible',
        code: 'query_rejected',
        data: null,
        badge: null
      });
      renderSourceChanged(source);
      return true;
    }
    if (out.kind !== 'ok') {
      markSourceTransport(source);
      return false;
    }
    renderPanelGuarded(source, out.json);
    var rec = SOURCES[source];
    // A retryable data-source failure that arrived over a 200. A malformed
    // payload is NOT one: the source answered, so it earns no backoff.
    return !(rec.state === 'unavailable' && QUERY_RETRY_CODES[rec.code]);
  }

  function scheduleNextQuery(ok) {
    clearTimeout(queryTimer);
    if (terminal || !queryBodyFor(activeTab)) { return; }
    queryBackoffIndex = ok ? -1 : Math.min(queryBackoffIndex + 1, BACKOFF_STEPS_MS.length - 1);
    // A healthy QUERY rides the GET cadence -- runRound nudges it once per
    // 5s round -- so it arms NO timer of its own here; a second timer would
    // double-fire it against that nudge. A failing one owns its own clock
    // and backs off on it, without ever touching the GET schedule.
    if (queryBackoffIndex < 0) { return; }
    queryTimer = setTimeout(queryRound, BACKOFF_STEPS_MS[queryBackoffIndex]);
  }

  var QUERY_SOURCES = [QUERY_SOURCE, QUERY_SERIES_SOURCE];

  // The request key each query source would be fetched for under the current
  // selection, whichever view is active.
  function queryKeysBySource() {
    var keys = {};
    QUERY_SOURCES.forEach(function (name) {
      var body = queryBodyForSource(name);
      keys[name] = body ? queryBodyKey(body) : null;
    });
    return keys;
  }

  // A selection change, made on any view: every query source whose request
  // key moved since `prior` drops the payload it holds for the old key and is
  // marked pending, whether or not the active view issues it. A view opened
  // later then shows its loading state, never the old selection's figures
  // under the new labels, and its own round settles the mark. A source whose
  // key did not move keeps its payload and is not marked.
  function resetMovedQuerySources(prior) {
    var now = queryKeysBySource();
    var moved = QUERY_SOURCES.filter(function (name) { return prior[name] !== now[name]; });
    markPending(moved);
    moved.forEach(function (name) {
      setSource(name, { state: 'loading', code: null, data: null, liveAt: null });
    });
  }

  // Every input that changes what the QUERY asks for funnels through here:
  // it aborts the in-flight request (queryStatus supersedes it), clears any
  // stale deterministic-refusal marker, and
  // refreshes immediately so an active tab never shows another selection's
  // numbers.
  //
  // BOTH query sources are reset, not just the active tab's. Window, group-by,
  // and provider scope are page-wide, so a change to any of them invalidates the
  // payload each source holds; the inactive one is refetched when its tab is
  // next selected (this same path runs on every tab switch). Leaving it holding
  // the previous selection's numbers is what would put one window's figures
  // under another window's picker.
  function queryInputChanged() {
    var body = queryBodyFor(activeTab);
    if (!body) {
      clearTimeout(queryTimer);
      // No view issues a QUERY now, so wantedFetchKey drops whatever the
      // aborted request still returns.
      if (queryCtrl) {
        queryCtrl.abort();
        queryCtrl = null;
        queryOwnAbortPending = true;
      }
      queryBusyRetryIndex = -1;
      return;
    }
    // Each source's refusal marker is checked against ITS OWN shape, not the
    // active tab's: the two shapes can go stale on different tab switches,
    // and clearing only the active one would leave the inactive source
    // suppressed under a key its own next round will never produce again.
    // Iterates ALL views (not just the two current query-backed ones) so a
    // future query-backed view is covered automatically -- queryBodyFor
    // returns null for a non-query view, which the guard below filters out.
    VIEWS.forEach(function (view) {
      var tabBody = queryBodyFor(view.id);
      var tabSource = querySourceFor(view.id);
      if (!tabBody || !tabSource) { return; }
      if (queryRejectedKeyBySource[tabSource] !== queryBodyKey(tabBody)) {
        delete queryRejectedKeyBySource[tabSource];
      }
    });
    QUERY_SOURCES.forEach(function (name) {
      setSource(name, { state: 'loading', code: null, data: null });
    });
    clearTimeout(queryTimer);
    queryBackoffIndex = -1;
    queryBusyRetryIndex = -1;
    queryRound();
  }

  // ---- poll loop -------------------------------------------------------

  function tick() {
    if (terminal || running) { return; }
    running = true;
    syncRefreshBtn();
    runRound().then(function (res) {
      running = false;
      if (res && res.terminal) { updateFavicon(); return; }
      scheduleNext(!!(res && res.ok));
      updateFavicon();
    }).catch(function () {
      // A bug in rendering must not wedge the loop; treat as a failed
      // round and keep polling with backoff.
      running = false;
      scheduleNext(false);
      updateFavicon();
    });
  }

  function scheduleNext(ok) {
    if (terminal) { return; }
    backoffIndex = ok ? -1 : Math.min(backoffIndex + 1, BACKOFF_STEPS_MS.length - 1);
    var delay = backoffIndex < 0 ? BASE_MS : BACKOFF_STEPS_MS[backoffIndex];
    backingOff = backoffIndex >= 0;
    nextDueAtMs = Date.now() + delay;
    startAgeTicker();
    clearTimeout(timer);
    timer = setTimeout(tick, delay);
    syncRefreshBtn();
  }

  // Immediately kick a fresh aggregate round (the operator clicked
  // Refresh). Shares the single-in-flight / terminal gate; the Refresh
  // button is additionally disabled during backoff (see syncRefreshBtn), so
  // a manual click is an edge trigger with no queue and no backoff reset.
  function kickRefresh() {
    if (terminal || running) { return; }
    clearTimeout(timer);
    tick();
  }

  // One aggregate round. The four GET panels always render from it; the
  // QUERY aggregate is scheduled separately (its own cadence, its own
  // backoff), and is only nudged here so a healthy page keeps the visible
  // query-backed tab in step with the 5s poll. Its controller is
  // module-level so going terminal can abort it.
  var aggregateCtrl = null;

  function runRound() {
    var ctrl = new AbortController();
    aggregateCtrl = ctrl;
    var ticket = issueFetchTicket(AGGREGATE_FETCH, AGGREGATE_FETCH);
    return fetchStatus('/status', ctrl).then(function (agg) {
      if (aggregateCtrl === ctrl) { aggregateCtrl = null; }
      if (!fetchTicketIsCurrent(ticket)) { return { stale: true }; }
      if (agg.kind === 'forbidden') { enterTerminal(); return { terminal: true }; }
      if (agg.kind !== 'ok') {
        markTransport(agg.kind);
        return { ok: false };
      }
      // Each panel renders under its OWN error boundary: one malformed
      // same-version payload marks THAT source invalid_payload and leaves the
      // other three -- and the round's 5s cadence -- untouched. Without this,
      // a single bad panel would reject the round and back off every source.
      GET_SOURCES.forEach(function (name) {
        renderPanelGuarded(name, getPanel(agg.json, name));
      });
      lastSuccess = new Date();
      clearBanner();
      // Nudge the visible query-backed tab so a healthy page keeps it in
      // step with the 5s poll. Skipped while one is already in flight, while
      // QUERY is backing off on its own clock, or when this selection has
      // already been fetched during the current cadence interval (a selection
      // change issues its own round, and a top-up on its heels is a duplicate).
      if (queryBackoffIndex < 0 && !queryInFlight() && queryNudgeDue()) { queryRound(); }
      return { ok: true };
    });
  }

  // ---- all-history usage read ------------------------------------------

  // Routing's attribution source: the SAME usage panel at `window=all`,
  // fetched separately from the aggregate so the aggregate's today-scoped
  // panel keeps serving its own readers untouched. Its own controller, its own
  // backoff index, its own timer -- a failing all-history read never slows the
  // aggregate's 5s cadence, and vice versa.
  var usageAllBackoffIndex = -1;
  var usageAllTimer = null;
  var usageAllInFlight = false;
  var usageAllCtrl = null;         // in-flight controller, aborted on terminal

  function usageAllRound() {
    if (terminal || usageAllInFlight) { return Promise.resolve(); }
    usageAllInFlight = true;
    var ctrl = new AbortController();
    usageAllCtrl = ctrl;
    var ticket = issueFetchTicket(USAGE_ALL_SOURCE, USAGE_ALL_SOURCE);
    // A response that lands after the page went terminal describes a page
    // state the operator has left: dropping it is what keeps a live Routing
    // repaint from appearing under the terminal banner.
    function settled() {
      if (usageAllCtrl === ctrl) { usageAllCtrl = null; }
      usageAllInFlight = false;
      return fetchTicketIsCurrent(ticket);
    }
    return fetchStatus(USAGE_ALL_URL, ctrl).then(function (out) {
      if (!settled()) { return; }
      scheduleNextUsageAll(applyUsageAllOutcome(out));
    }).catch(function () {
      if (!settled()) { return; }
      scheduleNextUsageAll(false);
    });
  }

  // Same outcome vocabulary as the aggregate round, applied to one source.
  // A 400/405 cannot happen for a fixed URL with no body, so it is classified
  // with the other non-retryable refusals rather than given a special arm.
  function applyUsageAllOutcome(out) {
    if (out.kind === 'forbidden') { enterTerminal(); return false; }
    if (out.kind !== 'ok') {
      markSourceTransport(USAGE_ALL_SOURCE);
      return false;
    }
    renderPanelGuarded(USAGE_ALL_SOURCE, out.json);
    return true;
  }

  function scheduleNextUsageAll(ok) {
    clearTimeout(usageAllTimer);
    if (terminal) { return; }
    usageAllBackoffIndex = ok ? -1 : Math.min(usageAllBackoffIndex + 1, BACKOFF_STEPS_MS.length - 1);
    var delay = usageAllBackoffIndex < 0 ? BASE_MS : BACKOFF_STEPS_MS[usageAllBackoffIndex];
    usageAllTimer = setTimeout(usageAllRound, delay);
  }

