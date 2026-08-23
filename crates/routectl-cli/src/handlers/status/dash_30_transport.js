  // ---- transport -------------------------------------------------------

  // NO automated runtime harness covers this part. The single-flight
  // generation guard below is verified BY HAND only -- see
  // dashboard-manual-checklist.md beside this source, and run it before
  // shipping a change here.

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

  // GET a status URL under the 2s budget. No request body, no mutating verb
  // -- fetch defaults to GET and none is set.
  function fetchStatus(url, ctrl) {
    return safeRequest(url, null, ctrl, TIMEOUT_MS);
  }

  // ---- QUERY transport -------------------------------------------------

  // QUERY carries its OWN backoff state. A failing aggregate must never be
  // slowed by a failing QUERY and vice versa: the GET loop's healthy 5s
  // cadence is the page's liveness signal, so it stays 5s no matter how
  // badly the ledger aggregate is doing.
  var queryBackoffIndex = -1;      // -1 = follow the GET cadence; 0..n = index
  var queryTimer = null;
  var queryCtrl = null;            // in-flight controller, for single-flight
  var queryGeneration = 0;         // bumped on every selection change
  // The body key a 400/405 refused. While the selection still produces this
  // key there is nothing to retry -- a deterministic refusal repeated every
  // 10s is noise, not recovery -- so the loop stops until the input changes
  // or the page reloads.
  var queryRejectedKey = null;
  // The body key of the last QUERY ATTEMPT and when it was issued. The 5s
  // aggregate nudge reads these so a selection that already fetched during
  // this interval is not fetched again the instant its round lands.
  var queryLastAttemptKey = null;
  var queryLastAttemptAtMs = 0;

  // The transport-level unavailable codes a 200 QUERY can carry. These are
  // the SAME failure families the HTTP-level arms cover (a busy or
  // unreadable ledger, a fired query deadline), so they engage the same
  // QUERY-only backoff even though they arrive over a 200.
  var QUERY_RETRY_CODES = { db_busy: true, db_unavailable: true, query_timeout: true };

  // The request body for a tab, or null when the tab is not query-backed. The
  // shape is looked up whole from QUERY_SHAPES for the live selection and
  // copied field-for-field -- nothing is overwritten afterwards, so every body
  // this page can emit is a declared shape the drift test has validated. Only
  // the optional provider scope is added on top (the server accepts it against
  // any shape).
  function queryBodyFor(tab) {
    var shape = shapeFor(tab);
    if (!shape) { return null; }
    var body = {};
    Object.keys(shape).forEach(function (k) { body[k] = shape[k]; });
    if (selectedProvider) { body.provider = selectedProvider; }
    return body;
  }

  // Overview asks for a series; Usage deliberately does not (no chart, and a
  // byte-identical non-series read path). Usage's grain follows the group-by
  // toggle.
  function shapeFor(tab) {
    if (tab === 'overview') {
      return findShape(selectedWindow, 'provider', true);
    }
    if (tab === 'usage') {
      return findShape(selectedWindow, selectedGroupBy, false);
    }
    return null;
  }

  // The source record a tab's QUERY response belongs to, or null when the tab is
  // not query-backed. One record per SHAPE, not one per route: the two shapes
  // answer with different payloads, so a single record would let whichever tab
  // polled last define what the other renders from.
  function querySourceFor(tab) {
    if (tab === 'overview') { return QUERY_SERIES_SOURCE; }
    if (tab === 'usage') { return QUERY_SOURCE; }
    return null;
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

  // Issue one QUERY. Single-flight: an in-flight request is aborted and the
  // generation bumped, so a late response from the OLD selection can never
  // repaint the new one (it is dropped at the guard below rather than
  // rendered). Never reuses a consumed Request; the body is stringified per
  // logical key.
  function queryStatus(body) {
    if (queryCtrl) { queryCtrl.abort(); }
    queryGeneration += 1;
    var generation = queryGeneration;
    var ctrl = new AbortController();
    queryCtrl = ctrl;
    var init = {
      method: 'QUERY',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body)
    };
    return safeRequest('/status/query', init, ctrl, QUERY_TIMEOUT_MS).then(function (out) {
      if (queryCtrl === ctrl) { queryCtrl = null; }
      // The generation guard: a newer selection has already been issued, so
      // this response describes a selection the operator has left.
      if (generation !== queryGeneration) { return { stale: true }; }
      return out;
    });
  }

  // Run one QUERY round for the active tab and re-arm the QUERY-only
  // schedule from its outcome. Returns a promise that never rejects.
  //
  // Deliberately carries NO in-flight guard of its own: single-flight is
  // enforced inside queryStatus, which aborts the previous request and bumps
  // the generation. A guard here would make an input change unable to
  // supersede a hung request -- exactly the case the generation guard exists
  // for -- so callers that must NOT supersede (the 5s nudge) check
  // queryInFlight() instead.
  function queryRound() {
    var body = queryBodyFor(activeTab);
    var source = querySourceFor(activeTab);
    if (terminal || !body || !source) { return Promise.resolve(); }
    var key = queryBodyKey(body);
    if (queryRejectedKey === key) { return Promise.resolve(); }
    queryLastAttemptKey = key;
    queryLastAttemptAtMs = Date.now();
    return queryStatus(body).then(function (out) {
      if (!out || out.stale) { return; }
      scheduleNextQuery(applyQueryOutcome(source, out, key));
    }).catch(function () {
      // A render throw must not wedge the QUERY loop; treat it as a failed
      // round and keep the GET loop untouched.
      scheduleNextQuery(false);
    });
  }

  function queryInFlight() {
    return queryCtrl !== null;
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
      // waiting.
      queryRejectedKey = key;
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
      markSourceTransport(source, out.kind === 'overloaded' ? 'stale' : 'dead');
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

  // Every input that changes what the QUERY asks for funnels through here:
  // it aborts the in-flight request (via the generation bump inside
  // queryStatus), clears a stale deterministic-refusal marker, and refreshes
  // immediately so an active tab never shows another selection's numbers.
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
      if (queryCtrl) { queryCtrl.abort(); queryCtrl = null; queryGeneration += 1; }
      return;
    }
    if (queryRejectedKey !== null && queryRejectedKey !== queryBodyKey(body)) {
      queryRejectedKey = null;
    }
    [QUERY_SOURCE, QUERY_SERIES_SOURCE].forEach(function (name) {
      setSource(name, { state: 'loading', code: null, data: null });
    });
    clearTimeout(queryTimer);
    queryBackoffIndex = -1;
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
  // query-backed tab in step with the 5s poll.
  function runRound() {
    var aggCtrl = new AbortController();
    return fetchStatus('/status', aggCtrl).then(function (agg) {
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
  var usageAllGeneration = 0;      // bumped when an in-flight read is invalidated

  function usageAllRound() {
    if (terminal || usageAllInFlight) { return Promise.resolve(); }
    usageAllInFlight = true;
    var ctrl = new AbortController();
    usageAllCtrl = ctrl;
    var generation = usageAllGeneration;
    // A response that lands after the page went terminal (or after this read
    // was invalidated) describes a page state the operator has left: dropping
    // it is what keeps a live Routing repaint from appearing under the
    // terminal banner.
    function settled() {
      if (usageAllCtrl === ctrl) { usageAllCtrl = null; }
      usageAllInFlight = false;
      return !terminal && generation === usageAllGeneration;
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
      markSourceTransport(USAGE_ALL_SOURCE, out.kind === 'overloaded' ? 'stale' : 'dead');
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

