'use strict';
(function () {
  // Per-source wire versions this page was built against. See the
  // co-versioning note in the document head: same-binary, so a mismatch
  // should never occur in practice; the runtime check is recovery
  // containment for a cached page / mixed assets / a bad build, not version
  // negotiation. `query` sits alongside the four GET panels because the
  // QUERY aggregate is a source of its own (see SOURCES below).
  var EXPECTED = { usage: 3, health: 5, config: 2, doctor: 4, query: 1 };

  // The five data sources, each with its own envelope + freshness. NOT the
  // same list as TABS: a tab is a view, a source is a fetch.
  var GET_SOURCES = ['usage', 'health', 'config', 'doctor'];
  var QUERY_SOURCE = 'query';
  var ALL_SOURCES = GET_SOURCES.concat([QUERY_SOURCE]);

  var TABS = ['overview', 'usage', 'routing', 'health', 'config', 'doctor'];
  var TAB_LABELS = {
    overview: 'Overview',
    usage: 'Usage',
    routing: 'Routing',
    health: 'Health',
    config: 'Config',
    doctor: 'Doctor'
  };

  // Which sources feed each tab. The poll loop re-renders a tab only when one
  // of ITS sources lands, so a dead QUERY degrades Overview and Usage and
  // leaves the four GET-backed tabs untouched.
  var TAB_SOURCES = {
    overview: [QUERY_SOURCE],
    usage: [QUERY_SOURCE],
    routing: ['usage', 'config'],
    health: ['health', 'usage'],
    config: ['config'],
    doctor: ['doctor']
  };

  // The windowless tabs: they report CURRENT state, not a windowed
  // aggregate, so the window picker dims and goes inert on them.
  var WINDOWLESS_TABS = { config: true, doctor: true };

  var WINDOWS = ['today', 'week', 'month', 'all'];

  var BASE_MS = 5000;              // steady GET cadence
  // The one backoff ladder, 30s cap. GET and QUERY keep SEPARATE indexes and
  // timers (a failing QUERY must not slow healthy GET polling) but share these
  // steps, so the two schedules cannot drift apart.
  var BACKOFF_STEPS_MS = [10000, 20000, 30000];
  var TIMEOUT_MS = 2000;           // per-GET AbortController budget
  // QUERY budget: 2000 body-read + 1000 query deadline + slack, still under
  // the 5s cadence so a slow QUERY can never overlap its own next attempt.
  var QUERY_TIMEOUT_MS = 3500;
  var SKEW_TOLERANCE_SEC = 2;      // future as_of within this is not clock skew
  var FRESH_SLACK_MS = 3000;       // grace past the due round before "stale"

  var selectedWindow = 'today';
  var selectedGroupBy = 'model';
  var selectedProvider = null;     // provider scope, null = every provider
  var activeTab = 'overview';

  var terminal = false;            // set on 403; stops every loop permanently
  var running = false;             // one aggregate round in flight at a time
  var timer = null;
  var backoffIndex = -1;           // -1 = base cadence; 0..n = backoff step index
  var lastSuccess = null;          // wall time of the last good aggregate round
  var backingOff = false;          // reflects GET backoff state in the clock
  var nextDueAtMs = 0;             // wall-clock instant the next round is due
  var countdownTimer = null;       // 1s interval driving the poll indicator
  var remainingSec = 0;            // seconds until the next scheduled round

  // Expansion state keyed by table caption -> { rowKey: true }. Survives the
  // per-poll DOM rebuild so an operator's expanded row stays open across
  // refreshes.
  var expanded = {};
  var uid = 0;

  // Server clock for the CURRENT render round: Date.parse(env.as_of),
  // refreshed per source in renderPanel. All data-age arithmetic (circuit
  // "open for", quota reset elapsed-flagging) reads THIS, never Date.now(),
  // so client clock skew can neither invent a negative age nor un-elapse a
  // reset. NaN when as_of is absent/unparseable -- callers degrade to "-".
  var serverNowMs = NaN;

  var el = function (id) { return document.getElementById(id); };

  // ---- per-source state records ----------------------------------------

  // One record per data source: a failure in one never clears a sibling.
  // `state` is one of loading | live | empty | unavailable | incompatible |
  // invalid_payload | stale | dead.
  function freshRecord() {
    return { state: 'loading', code: null, data: null, asOf: null, badge: null };
  }

  var SOURCES = {};
  ALL_SOURCES.forEach(function (n) { SOURCES[n] = freshRecord(); });

  function setSource(name, next) {
    var prev = SOURCES[name];
    var merged = { state: prev.state, code: prev.code, data: prev.data, asOf: prev.asOf, badge: prev.badge };
    Object.keys(next).forEach(function (k) { merged[k] = next[k]; });
    SOURCES[name] = merged;
  }

  // ---- number + time humanizers (match the CLI's usage formatters) -----

  // Compact a count exactly as the CLI's `human_count`: below 10000 the
  // plain integer; else one decimal with a K/M/B suffix, trimming ".0".
  // Null/undefined/non-finite render as "-".
  function humanCount(n) {
    if (n === null || n === undefined) { return '-'; }
    var x = Number(n);
    if (!isFinite(x)) { return '-'; }
    var neg = x < 0, a = Math.abs(x);
    if (a < 10000) { return (neg ? '-' : '') + String(Math.trunc(a)); }
    var v, s;
    if (a >= 1e9) { v = a / 1e9; s = 'B'; }
    else if (a >= 1e6) { v = a / 1e6; s = 'M'; }
    else { v = a / 1e3; s = 'K'; }
    var body = v.toFixed(1);
    if (body.slice(-2) === '.0') { body = body.slice(0, -2); }
    return (neg ? '-' : '') + body + s;
  }

  function num0(x) { var n = Number(x); return isFinite(n) ? n : 0; }

  // The CLI's `cache_hit_ratio` rendered as a percentage: `-` when the
  // denominator is degenerate (never "0%"), else one decimal + "%".
  function hitPct(num, den) {
    if (den <= 0) { return '-'; }
    return (num / den * 100).toFixed(1) + '%';
  }

  // Compact absolute local timestamp for an epoch-ms instant.
  function fmtTs(ms) {
    if (ms === null || ms === undefined) { return '-'; }
    var d = new Date(ms);
    return isNaN(d.getTime()) ? String(ms) : d.toLocaleString();
  }

  // Timestamp cell that treats a non-positive epoch as absent ("-").
  // Guards the quota-reset field (a real reset is always well in the future).
  function fmtTsPos(ms) {
    if (ms === null || ms === undefined || Number(ms) <= 0) { return '-'; }
    return fmtTs(ms);
  }

  // Relative age of an as_of instant in seconds -> humane phrase.
  function relAge(sec) {
    if (sec < 10) { return 'just now'; }
    if (sec < 60) { return sec + 's ago'; }
    if (sec < 3600) { return Math.floor(sec / 60) + 'm ago'; }
    return Math.floor(sec / 3600) + 'h ago';
  }

  // Percentage from a 0..1 fraction (quota utilization), rounded to whole.
  function pctFrac(f) {
    if (f === null || f === undefined) { return '-'; }
    var x = Number(f);
    return isFinite(x) ? Math.round(x * 100) + '%' : '-';
  }

  // Humane duration for a millisecond span (circuit "open for"). Coarse by
  // design: whole seconds under a minute, whole minutes under an hour, else
  // whole hours. Negative spans are clamped by the caller.
  function humanDuration(ms) {
    var sec = Math.floor(ms / 1000);
    if (sec < 60) { return sec + 's'; }
    if (sec < 3600) { return Math.floor(sec / 60) + 'm'; }
    return Math.floor(sec / 3600) + 'h';
  }

  // The window's plain-language span, for the verdict strip's req/span pair.
  var WINDOW_SPAN = {
    today: 'today',
    week: 'this week',
    month: 'this month',
    all: 'all time'
  };

  // ---- label maps ------------------------------------------------------

  // Humanized labels + hover tooltips for the value-domain wire tokens,
  // keyed by token FAMILY. The families here MIRROR the `.tok` CSS families
  // in dashboard.css (search "Value-domain tokens"): a new token is added in
  // BOTH adjacent, mutually-pointing places -- a color rule there, a label
  // entry here.
  //
  // Each entry is `{ label, title }`. `label` is the humanized VISIBLE text
  // (falls back to the raw token when omitted, so cramped wire tokens like
  // `five_hour` stay verbatim); `title` is the hover tooltip, which always
  // preserves the raw token or its meaning so nothing is lost. An unknown
  // token (or unknown family) passes through as its raw string with no
  // tooltip -- parity with the neutral `.tok` CSS fallback.
  var LABELS = {
    circuit: {
      closed: { label: 'ok', title: 'closed - healthy, requests flow' },
      open: { label: 'open', title: 'open - failing fast, requests shed' },
      half_open_ready: { label: 'half-open', title: 'half_open_ready - probing recovery' }
    },
    reach: {
      reachable: { label: 'reachable', title: 'last dispatch outcome was ok' },
      degraded: { label: 'degraded', title: 'last dispatch outcome was a failure family or gate refusal' },
      unknown: { label: 'unknown', title: 'no settled outcome yet (fresh state or post-restart)' }
    },
    status: {
      Pass: { label: 'pass', title: 'Pass' },
      Warn: { label: 'warn', title: 'Warn' },
      Fail: { label: 'fail', title: 'Fail' }
    },
    verdict: {
      'route-away': { label: 'route away', title: 'route-away - capability filtered out for this target' },
      'force-supported': { label: 'force supported', title: 'force-supported - capability forced on for this target' }
    },
    learned: {
      verified: { label: 'verified', title: 'verified - capability confirmed supported for this target' },
      broken: { label: 'broken', title: 'broken - capability confirmed unsupported for this target' }
    },
    prov: {
      provider: { label: 'provider', title: 'provider - legacy per-provider unsupported_features' },
      model: { label: 'model', title: 'model - legacy per-model unsupported_features' },
      override: { label: 'override', title: 'override - a [capability.overrides.<spec>] entry' },
      learned: { label: 'learned', title: 'learned - a non-expired acting negative in the learned registry' }
    },
    src: {
      user: { label: 'user', title: 'user - operator catalog layer' },
      import: { label: 'import', title: 'import - imported catalog layer' },
      baked: { label: 'baked', title: 'baked - built-in catalog layer' },
      disabled: { label: 'disabled', title: 'disabled - row present but disabled' },
      missing: { label: 'missing', title: 'missing - no catalog row resolved' }
    },
    tier: {
      'self-identifying': { label: 'self-identifying', title: 'the upstream declared the capability itself' },
      inferred: { label: 'inferred', title: 'inferred from observed behavior' }
    },
    // Quota representative-claim. The raw token stays visible (no label
    // override for five_hour); the window meaning goes in the tooltip.
    claim: {
      five_hour: { title: 'five_hour - 5-hour rolling subscription window' },
      overage: { label: 'overage', title: 'overage - billing past the included subscription quota' }
    },
    // Quota STATUS / OVERAGE pass-through values (color families in CSS as
    // `.qstatus-*`). Tooltips are light; unknown values pass through raw.
    qstatus: {
      allowed: { title: 'within quota' },
      allowed_warning: { title: 'approaching the quota limit' },
      rejected: { title: 'quota exhausted, requests rejected' },
      queued: { title: 'requests queued behind the quota' },
      triggered: { title: 'overage billing engaged' }
    },
    // Provider auto-activation reason codes. Raw token stays visible where
    // space is tight; the meaning goes in the tooltip.
    activation: {
      oauth_missing: { title: 'oauth_missing - no OAuth credential on file' },
      oauth_expired: { title: 'oauth_expired - OAuth token expired, no refresh available' },
      oauth_store_unavailable: { title: 'oauth_store_unavailable - no OAuth store to probe (HOME or XDG absent)' },
      not_cataloged: { title: 'not_cataloged - own-credential provider has no baked catalog rows yet' },
      unknown: { title: 'unknown - an activation state this build does not recognize' }
    },
    // Health last-settled-outcome tokens.
    outcome: {
      ok: { label: 'ok' },
      rate_limited: { label: 'rate limited', title: 'rate_limited' },
      timeout: { label: 'timeout' },
      transport_error: { label: 'transport error', title: 'transport_error' },
      http_4xx: { label: 'http 4xx', title: 'http_4xx - a 4xx client-side rejection' },
      http_5xx: { label: 'http 5xx', title: 'http_5xx - a 5xx server-side failure' },
      circuit_open: { label: 'circuit open', title: 'circuit_open - derived from an open breaker' }
    },
    // Resolved failure-class names for the usage error breakdown.
    class: {
      'rate-limited': { label: 'rate limited' },
      auth: { label: 'auth' },
      'bad-request': { label: 'bad request' },
      'content-policy': { label: 'content policy' },
      'context-window': { label: 'context window' },
      'server-error': { label: 'server error' },
      timeout: { label: 'timeout' },
      'network-error': { label: 'network error' },
      overloaded: { label: 'overloaded' },
      'feature-unsupported': { label: 'feature unsupported' },
      unclassified: { label: 'unclassified', title: 'errors with no resolved failure class' }
    },
    // Query cost-resolution status (the `cost_status` wire token). Drives the
    // honest "unpriced" read -- an unpriced or subscription group never
    // renders a "$0".
    cost: {
      priced: { label: 'priced', title: 'every row in this group was priced' },
      unpriced: { label: 'unpriced', title: 'no row in this group had a usable price' },
      subscription: { label: 'subscription', title: 'managed-subscription usage: real usage, no per-token cost' },
      partial: { label: 'partial', title: 'mixed cost kinds: the figure is the priced subtotal only' }
    }
  };

  // Resolve a wire token to { label, title } for a family. Unknown token or
  // unknown family -> raw passthrough (label = raw, no tooltip), mirroring
  // the neutral `.tok` CSS fallback so nothing crashes or hides.
  function labelFor(family, raw) {
    var key = (raw === null || raw === undefined) ? '' : String(raw);
    var fam = LABELS[family];
    var entry = fam ? fam[key] : undefined;
    if (!entry) { return { label: key || '-', title: null }; }
    return { label: (entry.label === undefined) ? key : entry.label, title: entry.title || null };
  }

  // A humanized text cell (a plain value, or a titled span when the family
  // carries a tooltip) for tokens that are NOT `.tok` color pills: quota
  // claim, activation reasons, health last-outcomes, error-class names.
  // Returns a string when there is no tooltip (rendered via textContent) or
  // a titled span Node otherwise.
  function labelCell(family, raw) {
    var lab = labelFor(family, raw);
    if (!lab.title) { return lab.label; }
    var span = document.createElement('span');
    span.textContent = lab.label;
    span.title = lab.title;
    return span;
  }

  // ---- query field vocabulary + adapter --------------------------------

  // The field vocabulary of a `/status/query` metrics object, split by how a
  // value is read rather than by what it means. These two arrays are the ONLY
  // place a raw query field name appears in this file: adaptMetrics below is
  // their sole consumer, and every render path reads adapter properties
  // instead of touching raw JSON or repeating a field name. A page.rs drift
  // test asserts every name in BOTH arrays is a field of the server's
  // `QueryMetrics`, derived from serde rather than a second hardcoded list, so
  // a server-side rename cannot silently turn a column into zeroes.
  var QUERY_METRICS = [
    'requests',
    'ok',
    'errors',
    'input_tokens',
    'output_tokens',
    'reasoning_tokens',
    'cache_read_billed',
    'cache_write_5m',
    'cache_write_1h',
    'server_tool_calls',
    'stream_count',
    'client_disconnect_total',
    'fallback_served',
    'ttft_p50_ms',
    'ttft_p95_ms',
    'latency_p50_ms',
    'latency_p95_ms',
    'throughput_tok_s',
    'ctx_avg',
    'ctx_peak',
    'cache_hit_pct',
    'cost_usd'
  ];

  // Fields whose value is a TOKEN, not a figure: carried through verbatim,
  // never coerced (a num0 pass would turn `unpriced` into a zero cost).
  var QUERY_TOKENS = [
    'cost_status'
  ];

  // The COMPLETE set of request bodies this page can issue: every selectable
  // window crossed with every group-by the tabs use, in both the series read
  // path (Overview) and the non-series one (Usage), each carrying the bucket
  // that window resolves to -- hourly reads well over a day, anything wider
  // needs daily. An emitted body is one of these entries verbatim plus the
  // optional provider scope, so nothing is overwritten at runtime and the
  // page.rs drift test validates the whole request vocabulary through the
  // server's own parser. Written as strict JSON for that test.
  var QUERY_SHAPES = [
    {"window":"today","group_by":"provider","bucket":"hour"},
    {"window":"week","group_by":"provider","bucket":"day"},
    {"window":"month","group_by":"provider","bucket":"day"},
    {"window":"all","group_by":"provider","bucket":"day"},
    {"window":"today","group_by":"model"},
    {"window":"today","group_by":"alias"},
    {"window":"today","group_by":"provider"},
    {"window":"week","group_by":"model"},
    {"window":"week","group_by":"alias"},
    {"window":"week","group_by":"provider"},
    {"window":"month","group_by":"model"},
    {"window":"month","group_by":"alias"},
    {"window":"month","group_by":"provider"},
    {"window":"all","group_by":"model"},
    {"window":"all","group_by":"alias"},
    {"window":"all","group_by":"provider"}
  ];

  // Flatten ONE metrics object: every numeric field coerced through num0,
  // every token field carried through as it arrived. No rename, no derived
  // field.
  function adaptMetrics(raw) {
    var from = raw || {};
    var out = {};
    QUERY_METRICS.forEach(function (key) { out[key] = num0(from[key]); });
    QUERY_TOKENS.forEach(function (key) { out[key] = from[key]; });
    return out;
  }

  // The thin flat-extraction layer over a live QUERY payload: it walks
  // `{groups, totals, series}` ONCE and hands back the same shape with every
  // metric coerced. Deliberately NOT a render model -- it renames nothing
  // and computes nothing, so if it ever grows either, inline it back into
  // the callers.
  function QueryAdapter(raw) {
    var from = raw || {};
    var series = from.series
      ? {
        bucket_ms: num0(from.series.bucket_ms),
        buckets: (from.series.buckets || []).map(function (b) {
          return { start_ms: num0(b.start_ms), metrics: adaptMetrics(b.metrics) };
        })
      }
      : null;
    return {
      groups: (from.groups || []).map(function (g) {
        return { label: g.label, metrics: adaptMetrics(g.metrics) };
      }),
      totals: adaptMetrics(from.totals),
      series: series
    };
  }

  // The adapted QUERY payload for the current selection, or null when the
  // query source is not live. Every query-backed builder reads THIS.
  function queryView() {
    var rec = SOURCES[QUERY_SOURCE];
    return (rec.state === 'live' && rec.data) ? QueryAdapter(rec.data) : null;
  }

  // ---- transport -------------------------------------------------------

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
    if (terminal || !body) { return Promise.resolve(); }
    var key = queryBodyKey(body);
    if (queryRejectedKey === key) { return Promise.resolve(); }
    return queryStatus(body).then(function (out) {
      if (!out || out.stale) { return; }
      scheduleNextQuery(applyQueryOutcome(out, key));
    }).catch(function () {
      // A render throw must not wedge the QUERY loop; treat it as a failed
      // round and keep the GET loop untouched.
      scheduleNextQuery(false);
    });
  }

  function queryInFlight() {
    return queryCtrl !== null;
  }

  // Map a QUERY outcome onto the query source record. Returns whether the
  // round counts as healthy for backoff purposes.
  function applyQueryOutcome(out, key) {
    if (out.kind === 'forbidden') { enterTerminal(); return false; }
    if (out.kind === 'rejected') {
      // A deterministic refusal of THIS body: stop retrying it, and say so
      // rather than showing a transport failure the operator cannot fix by
      // waiting.
      queryRejectedKey = key;
      setSource(QUERY_SOURCE, {
        state: 'incompatible',
        code: 'query_rejected',
        data: null,
        badge: null
      });
      renderSourceChanged(QUERY_SOURCE);
      return true;
    }
    if (out.kind !== 'ok') {
      markSourceTransport(QUERY_SOURCE, out.kind === 'overloaded' ? 'stale' : 'dead');
      return false;
    }
    renderPanelGuarded(QUERY_SOURCE, out.json);
    var rec = SOURCES[QUERY_SOURCE];
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
    setSource(QUERY_SOURCE, { state: 'loading', code: null, data: null });
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
    startCountdown(Math.round(delay / 1000));
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
      // step with the 5s poll. Skipped while one is already in flight (this
      // is a top-up, not a new selection) or while QUERY is backing off on
      // its own clock.
      if (queryBackoffIndex < 0 && !queryInFlight()) { queryRound(); }
      return { ok: true };
    });
  }

  // ---- render dispatch -------------------------------------------------

  // Render one source's envelope under its own error boundary. A throw from
  // ANY source -- a malformed same-version payload, a builder fault -- becomes
  // `invalid_payload` for THAT source and nothing else: the caller's round
  // still counts as successful, so a healthy transport keeps its 5s cadence
  // and the sibling sources keep their rendered values.
  function renderPanelGuarded(name, env) {
    try {
      renderPanel(name, env);
    } catch (e) {
      setInvalidPayload(name);
    }
  }

  // The single validate-and-classify path shared by the GET poll and the
  // QUERY aggregate (and by any future push, which would be the same
  // per-source envelope keyed by source name).
  //
  // Ordering is load-bearing: `schema_version` is checked BEFORE `data` is
  // read at all, and a mismatch is fail-closed -- a field's semantics may have
  // changed, so a best-effort render would show numbers that mean something
  // else. A mismatch is NOT a transport failure and must not back off a
  // healthy source.
  //
  // The envelope invariant is then enforced before `data` is read: exactly one
  // of a meaningful `data` or an `unavailable` code. Neither present means the
  // source told us nothing while claiming success; both present means it
  // contradicted itself. Either way the payload does not match its declared
  // shape, so it is `invalid_payload` for this source -- never rendered as a
  // live empty object, never silently resolved in favor of one side.
  function renderPanel(name, env) {
    if (!env) {
      return setUnavailable(name, 'missing_panel', 'source absent from response');
    }
    if (env.schema_version !== EXPECTED[name]) {
      return setIncompatible(name, env.schema_version);
    }
    var hasData = isMeaningfulData(env.data);
    var hasUnavailable = !!env.unavailable;
    if (hasData === hasUnavailable) {
      return setInvalidPayload(name);
    }
    if (hasUnavailable) {
      return setUnavailable(name, env.unavailable, null);
    }
    // Pin the server clock for this render round so all data-age arithmetic
    // downstream uses as_of, never the client's Date.now().
    serverNowMs = Date.parse(env.as_of);
    var data = env.data;
    setSource(name, {
      state: 'live',
      code: null,
      data: data,
      asOf: env.as_of,
      badge: sourceBadge(name, data)
    });
    renderSourceChanged(name);
  }

  // A `data` member carries meaning when it is an object with at least one
  // key. `null`, a scalar, and `{}` all say nothing, so none of them satisfies
  // the envelope's data side.
  function isMeaningfulData(data) {
    if (!data || typeof data !== 'object' || Array.isArray(data)) { return false; }
    return Object.keys(data).length > 0;
  }

  // A same-version payload that did not match its declared shape. NOT a
  // transport failure: the source is answering, so no backoff is engaged and
  // the poll cadence is untouched.
  function setInvalidPayload(name) {
    setSource(name, {
      state: 'invalid_payload',
      code: null,
      data: null,
      badge: { label: 'n/a', kind: 'muted' }
    });
    renderSourceChanged(name);
  }

  function setUnavailable(name, code, extra) {
    setSource(name, {
      state: 'unavailable',
      code: code,
      data: null,
      badge: extra ? { label: 'n/a', kind: 'muted', title: extra } : { label: 'n/a', kind: 'muted' }
    });
    renderSourceChanged(name);
  }

  function setIncompatible(name, received) {
    setSource(name, {
      state: 'incompatible',
      code: 'expected ' + EXPECTED[name] + ', received ' + received,
      data: null,
      badge: { label: 'n/a', kind: 'muted' }
    });
    renderSourceChanged(name);
  }

  // A failed refresh must NEVER leave last-good numbers rendered as
  // current. 503 marks the source visibly stale (muted last-known values
  // plus a "last success" note); a network/timeout failure clears the
  // numbers to an explicit no-current-data state. Either way the DATA is
  // retained or dropped per mode, and siblings are untouched.
  function markSourceTransport(name, mode) {
    var stale = mode === 'stale';
    setSource(name, {
      state: stale ? 'stale' : 'dead',
      code: null,
      data: stale ? SOURCES[name].data : null,
      badge: stale ? { label: 'stale', kind: 'warn' } : { label: 'offline', kind: 'error' }
    });
    renderSourceChanged(name);
  }

  function markTransport(kind) {
    var mode = (kind === 'overloaded') ? 'stale' : 'dead';
    GET_SOURCES.forEach(function (n) { markSourceTransport(n, mode); });
    if (kind === 'overloaded') {
      setBanner('warn', 'status overloaded: showing last known values, backing off');
    } else if (kind === 'timeout') {
      setBanner('error', 'status request timed out, backing off');
    } else {
      setBanner('error', 'daemon unreachable, backing off');
    }
  }

  function getPanel(agg, name) {
    return (agg && agg.panels) ? agg.panels[name] : undefined;
  }

  // A source landed (fresh, unavailable, or down). Repaint the active tab if
  // it reads that source, and refresh the chrome that summarizes every
  // source. A tab that does NOT read the source is left alone -- this is
  // what keeps a dead QUERY confined to Overview and Usage.
  function renderSourceChanged(name) {
    if (TAB_SOURCES[activeTab].indexOf(name) >= 0) { renderActiveTab(); }
    renderVerdict();
    syncTabBadges();
  }

  // ---- tab rendering ---------------------------------------------------

  // Render the active tab's body from its sources. The tab builder runs in its
  // own try/catch so a malformed same-version payload degrades to one
  // `invalid_payload` card instead of a blank page or a failed round. This is
  // the LAST RESORT, not the containment plan: it replaces the WHOLE tab, so a
  // multi-source builder that lets a secondary source throw here would wipe
  // out healthy primary content. Multi-source builders confine each source to
  // its own section via `safeSection` instead.
  function renderActiveTab() {
    var tab = activeTab;
    var pane = el('pane-' + tab);
    var status = el('status-' + tab);
    var body = el('body-' + tab);
    if (!pane || !body) { return; }
    var primary = SOURCES[TAB_SOURCES[tab][0]];
    pane.classList.remove('section--stale', 'section--dead');
    if (primary.state === 'stale') { pane.classList.add('section--stale'); }
    if (primary.state === 'dead') { pane.classList.add('section--dead'); }
    renderSectionStatus(status, primary);
    try {
      body.replaceChildren(BUILDERS[tab](primary));
    } catch (e) {
      body.replaceChildren(errorCard('invalid_payload',
        'this section could not be rendered from the payload it received'));
    }
  }

  // Render ONE visual section under its own error boundary, against the source
  // record that section reads. Returns the section's node, or -- when that
  // source is not live, or when building from it throws -- a card describing
  // just this section's state. Siblings are untouched either way.
  //
  // INVARIANT for multi-source tab builders: every section that reads a
  // TAB_SOURCES entry goes through this helper, with that entry's own record.
  // The tab-level try/catch replaces the whole tab, so a secondary source's
  // throw reaching it would blank content the primary source rendered
  // correctly while the status line still reported the primary live.
  function safeSection(rec, build) {
    var pending = stateCard(rec);
    if (pending) { return pending; }
    try {
      return build(rec);
    } catch (e) {
      return errorCard('invalid_payload',
        'this section could not be rendered from the payload it received');
    }
  }

  function renderSectionStatus(status, rec) {
    if (!status) { return; }
    status.replaceChildren();
    if (rec.state === 'loading') {
      status.appendChild(document.createTextNode('loading'));
      return;
    }
    status.appendChild(makeLiveDot());
    if (rec.state === 'stale' || rec.state === 'dead') {
      var note = lastSuccess
        ? 'last success at ' + lastSuccess.toLocaleTimeString()
        : 'no successful poll yet';
      var lead = rec.state === 'stale' ? 'stale: ' : 'no current data: ';
      status.appendChild(document.createTextNode(lead + note));
      return;
    }
    status.appendChild(document.createTextNode(formatAsOf(rec.asOf)));
    pulse(status);
  }

  // The small round status indicator shared by the fresh-render and
  // transport-down status lines.
  function makeLiveDot() {
    var dot = document.createElement('span');
    dot.className = 'live-dot';
    dot.setAttribute('aria-hidden', 'true');
    return dot;
  }

  // Brief opacity pulse to signal a fresh poll landed. Re-triggered each
  // round by clearing and re-adding the class (a reflow read restarts the
  // animation); prefers-reduced-motion nulls the animation in CSS.
  function pulse(node) {
    node.classList.remove('pulse');
    void node.offsetWidth;
    node.classList.add('pulse');
  }

  // as_of age as a humane relative phrase plus a compact absolute local
  // time. If as_of is in the future beyond a small tolerance, label clock
  // skew rather than showing a negative age.
  function formatAsOf(asOf) {
    if (!asOf) { return ''; }
    var then = new Date(asOf);
    if (isNaN(then.getTime())) { return 'as of ' + asOf; }
    var ageSec = Math.round((Date.now() - then.getTime()) / 1000);
    if (ageSec < -SKEW_TOLERANCE_SEC) {
      return 'clock skew: source clock ahead (' + then.toLocaleTimeString() + ')';
    }
    if (ageSec < 0) { ageSec = 0; }
    return relAge(ageSec) + ' - ' + then.toLocaleTimeString();
  }

  // ---- per-source state presentation -----------------------------------

  // The state a builder must render instead of content, or null when the
  // source is live. Every buildX starts by consulting this, so no tab has to
  // reinvent its loading / empty / failure shapes.
  function stateCard(rec) {
    if (rec.state === 'loading') { return skeletonCard(); }
    if (rec.state === 'unavailable') {
      return errorCard(rec.code, 'this source is not answering right now');
    }
    if (rec.state === 'incompatible') { return incompatibleCard(rec.code); }
    if (rec.state === 'invalid_payload') {
      return errorCard('invalid_payload', 'the payload did not match its declared shape');
    }
    if (rec.state === 'dead') { return errorCard('transport failure', null); }
    return null;
  }

  function skeletonCard() {
    var card = document.createElement('div');
    card.className = 'card';
    var sk = document.createElement('div');
    sk.className = 'skeleton';
    sk.setAttribute('aria-hidden', 'true');
    for (var i = 0; i < 3; i++) { sk.appendChild(document.createElement('span')); }
    var sr = document.createElement('span');
    sr.className = 'sr-only';
    sr.textContent = 'Loading';
    card.appendChild(sk);
    card.appendChild(sr);
    return card;
  }

  function errorCard(code, extra) {
    var div = document.createElement('div');
    div.className = 'unavailable-msg';
    div.textContent = 'unavailable: ' + code + (extra ? ' (' + extra + ')' : '');
    return div;
  }

  // A version mismatch is a build / cached-asset fault, not a broken
  // daemon: it says which versions disagree and what fixes it, and stays
  // visually distinct from a failure.
  function incompatibleCard(detail) {
    var div = document.createElement('div');
    div.className = 'incompatible-msg';
    div.appendChild(document.createTextNode('incompatible: ' + detail));
    var hint = document.createElement('span');
    hint.className = 'hint';
    hint.textContent = 'reload this page after a daemon upgrade';
    div.appendChild(hint);
    return div;
  }

  // A welcoming empty / not-yet-populated state. Never a grid of zeros.
  function emptyCard(title, bodyText, facts) {
    var card = document.createElement('div');
    card.className = 'card empty';
    var inner = document.createElement('div');
    inner.className = 'empty-inner';
    var h = document.createElement('div');
    h.className = 'empty-title';
    h.textContent = title;
    var p = document.createElement('div');
    p.className = 'empty-body';
    p.textContent = bodyText;
    inner.appendChild(h);
    inner.appendChild(p);
    if (facts && facts.length) {
      var row = document.createElement('div');
      row.className = 'empty-facts';
      facts.forEach(function (f) {
        var s = document.createElement('span');
        s.textContent = f;
        row.appendChild(s);
      });
      inner.appendChild(row);
    }
    card.appendChild(inner);
    return card;
  }

  function enterTerminal() {
    terminal = true;
    clearTimeout(timer);
    clearTimeout(queryTimer);
    clearInterval(countdownTimer);
    if (queryCtrl) { queryCtrl.abort(); queryCtrl = null; }
    setBanner('terminal',
      'This dashboard must be opened via the loopback or bound address ' +
      '(for example http://127.0.0.1:8787). Polling has stopped.');
    ALL_SOURCES.forEach(function (n) { markSourceTransport(n, 'dead'); });
    updateWindowSel();
    syncRefreshBtn();
    renderVerdict();
    updateFavicon();
  }

  function setBanner(kind, text) {
    var b = el('banner');
    b.hidden = false;
    b.textContent = text;
    b.className = 'banner banner--' + kind;
  }

  function clearBanner() {
    var b = el('banner');
    b.hidden = true;
    b.textContent = '';
    b.className = 'banner';
  }

  // ---- verdict strip ---------------------------------------------------

  // The plain-language verdict: ONE sentence about whether routing is
  // working, plus the window's req/span figures and the poll indicator.
  // Deliberately carries NO cost figure -- cost is a Usage/Overview reading,
  // and repeating it here would show the same fact twice.
  function renderVerdict() {
    var strip = el('verdict');
    var verdict = verdictState();
    strip.classList.remove('verdict--ok', 'verdict--warn', 'verdict--bad', 'verdict--idle');
    strip.classList.add('verdict--' + verdict.kind);
    el('verdict-text').textContent = verdict.text;
    el('verdict-stats').textContent = verdictStats();
    renderPollIndicator();
  }

  // Precedence: a stopped page, then a dead aggregate, then any source a
  // fresh round found unusable, then healthy.
  function verdictState() {
    if (terminal) { return { kind: 'bad', text: 'Polling stopped - open this page on the bound address' }; }
    var down = GET_SOURCES.filter(function (n) { return SOURCES[n].state === 'dead'; });
    if (down.length === GET_SOURCES.length) {
      return { kind: 'bad', text: 'Daemon unreachable - showing no current data' };
    }
    if (SOURCES.health.state === 'stale' || SOURCES.usage.state === 'stale') {
      return { kind: 'warn', text: 'Status overloaded - showing last known values' };
    }
    var degraded = ALL_SOURCES.filter(function (n) {
      var s = SOURCES[n].state;
      return s === 'unavailable' || s === 'incompatible' || s === 'dead';
    });
    if (degraded.length) {
      return { kind: 'warn', text: 'Routing healthy - ' + degraded.join(', ') + ' unavailable' };
    }
    if (ALL_SOURCES.every(function (n) { return SOURCES[n].state === 'loading'; })) {
      return { kind: 'idle', text: 'Checking routes' };
    }
    return { kind: 'ok', text: 'All routes healthy' };
  }

  // req/span for the ACTIVE tab: a query-backed tab reads the adapter's
  // totals; a GET-backed tab reads the aggregate usage totals. One
  // derivation each, never both.
  function verdictStats() {
    var span = WINDOWLESS_TABS[activeTab] ? 'current state' : WINDOW_SPAN[selectedWindow];
    var view = TAB_SOURCES[activeTab][0] === QUERY_SOURCE ? queryView() : null;
    if (view) { return humanCount(view.totals.requests) + ' req ' + span; }
    var usage = SOURCES.usage;
    if (usage.state === 'live' && usage.data && usage.data.totals) {
      return humanCount(num0(usage.data.totals.requests)) + ' req ' + span;
    }
    return span;
  }

  function renderPollIndicator() {
    var poll = el('poll');
    poll.classList.remove('poll--warn', 'poll--dead', 'poll--idle');
    var label;
    if (terminal) {
      poll.classList.add('poll--dead');
      label = 'polling stopped';
    } else if (backingOff) {
      poll.classList.add('poll--warn');
      label = 'reconnecting - retry in ' + remainingSec + 's';
    } else if (lastSuccess === null) {
      poll.classList.add('poll--idle');
      label = 'polling';
    } else {
      label = 'live - next in ' + remainingSec + 's';
    }
    el('poll-label').textContent = label;
  }

  // ---- tab badges ------------------------------------------------------

  // The badge for a source, computed ONLY from a fresh successful render.
  // Counts a genuine problem: usage errors, health non-closed circuits,
  // doctor warn+fail findings. Config carries no error concept, so it never
  // badges. A zero count returns null so a clean source shows nothing.
  function sourceBadge(name, data) {
    if (name === 'usage') {
      var errs = num0((data.totals || {}).errors);
      return errs > 0 ? { label: humanCount(errs), kind: 'error' } : null;
    }
    if (name === 'health') {
      var open = (data.targets || []).filter(function (t) {
        return t.circuit && t.circuit !== 'closed';
      }).length;
      return open > 0 ? { label: String(open), kind: 'warn' } : null;
    }
    if (name === 'doctor') {
      var findings = (data.report || {}).findings || [];
      var warn = 0, fail = 0;
      findings.forEach(function (f) {
        if (f.status === 'Warn') { warn += 1; }
        else if (f.status === 'Fail') { fail += 1; }
      });
      var total = warn + fail;
      if (total === 0) { return null; }
      return { label: String(total), kind: fail > 0 ? 'error' : 'warn' };
    }
    return null;
  }

  // Surface each tab's badge from its PRIMARY source, so a problem is
  // visible without switching to that tab.
  function syncTabBadges() {
    TABS.forEach(function (tab) {
      setTabBadge(tab, SOURCES[TAB_SOURCES[tab][0]].badge);
    });
  }

  function setTabBadge(tab, state) {
    var badge = el('badge-' + tab);
    if (!badge) { return; }
    if (!state) {
      badge.hidden = true;
      badge.textContent = '';
      badge.className = 'tab-badge';
      badge.removeAttribute('title');
      return;
    }
    badge.hidden = false;
    badge.textContent = state.label;
    badge.className = 'tab-badge tab-badge--' + state.kind;
    if (state.title) { badge.title = state.title; }
    else { badge.removeAttribute('title'); }
  }

  // ---- expansion state -------------------------------------------------

  function getExpanded(tableKey, rowKey) {
    return !!(expanded[tableKey] && expanded[tableKey][rowKey]);
  }

  function setExpanded(tableKey, rowKey, open) {
    if (!expanded[tableKey]) { expanded[tableKey] = {}; }
    if (open) { expanded[tableKey][rowKey] = true; }
    else { delete expanded[tableKey][rowKey]; }
  }

  // ---- DOM table helpers (textContent only, never innerHTML) -----------

  // Column descriptors: C = text, N = numeric (right-aligned, mono,
  // tabular), R = row-header identifier column (<th scope="row">),
  // W = wrapping prose column.
  function C(label) { return { label: label }; }
  function N(label) { return { label: label, num: true }; }
  function R(label) { return { label: label, row: true }; }
  function W(label) { return { label: label, wrap: true }; }

  // Attach a header tooltip to a column descriptor, returning a NEW
  // descriptor (the base C/N/R/W factories stay single-purpose).
  function withTitle(col, title) {
    var next = {};
    Object.keys(col).forEach(function (k) { next[k] = col[k]; });
    next.title = title;
    return next;
  }

  // Build a table with a visually-hidden caption, `scope="col"` headers,
  // and per-column alignment metadata. `expandable` prepends a leading
  // control column whose cells carry a disclosure button.
  function mkTable(caption, cols, expandable) {
    var t = document.createElement('table');
    var cap = document.createElement('caption');
    cap.className = 'sr-only';
    cap.textContent = caption;
    t.appendChild(cap);
    var thead = document.createElement('thead');
    var hr = document.createElement('tr');
    if (expandable) {
      var eth = document.createElement('th');
      eth.scope = 'col';
      var sr = document.createElement('span');
      sr.className = 'sr-only';
      sr.textContent = 'Details';
      eth.appendChild(sr);
      hr.appendChild(eth);
    }
    cols.forEach(function (col) {
      var th = document.createElement('th');
      th.scope = 'col';
      th.textContent = col.label;
      if (col.title) { th.title = col.title; }
      if (col.num) { th.className = 'num'; }
      hr.appendChild(th);
    });
    thead.appendChild(hr);
    t.appendChild(thead);
    var tb = document.createElement('tbody');
    t.appendChild(tb);
    return { t: t, tb: tb, cols: cols, key: caption, expandable: !!expandable };
  }

  // A table wrapped so it scrolls INSIDE its card: the page itself never
  // scrolls sideways, down to 380px.
  function tableScroll(tbl) {
    var wrap = document.createElement('div');
    wrap.className = 'tablewrap';
    wrap.appendChild(tbl.t);
    return wrap;
  }

  function cellNode(col, cell) {
    var node = document.createElement(col.row ? 'th' : 'td');
    if (col.row) { node.scope = 'row'; }
    if (col.num) { node.className = 'num'; }
    if (col.wrap) { node.className = 'wrap'; }
    if (cell instanceof Node) {
      node.appendChild(cell);
    } else {
      node.textContent = (cell === null || cell === undefined) ? '-' : String(cell);
    }
    return node;
  }

  // Append one plain row. Cells are parallel to the table's columns; a cell
  // may be a raw value (rendered via textContent) or a Node (a pill).
  function trow(tbl, cells) {
    var tr = document.createElement('tr');
    tbl.cols.forEach(function (col, i) { tr.appendChild(cellNode(col, cells[i])); });
    tbl.tb.appendChild(tr);
  }

  // Append an expandable row: the primary cells plus a hidden detail row
  // (a definition grid of the secondary fields) toggled by a keyboard-
  // accessible disclosure button. The WHOLE primary row is clickable (the
  // button stays the visible affordance); a single toggle closure drives
  // both, with the button stopping propagation so a button click does not
  // also fire the row handler and double-toggle. Expansion persists across
  // poll rebuilds via the `expanded` map keyed by (table caption, rowKey).
  // An optional `rowClass` de-emphasizes the primary row (zero-traffic).
  function xrow(tbl, rowKey, cells, detailPairs, rowClass) {
    var tr = document.createElement('tr');
    tr.className = 'exp-row' + (rowClass ? ' ' + rowClass : '');

    var detailRow = document.createElement('tr');
    detailRow.className = 'detail-row';
    var did = 'detail-' + (uid++);
    detailRow.id = did;
    var dcell = document.createElement('td');
    dcell.colSpan = tbl.cols.length + 1;
    dcell.appendChild(buildDefList(detailPairs));
    detailRow.appendChild(dcell);
    var open = getExpanded(tbl.key, rowKey);
    detailRow.hidden = !open;

    var extd = document.createElement('td');
    extd.className = 'exp-cell';
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'exp-btn';
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
    btn.setAttribute('aria-controls', did);
    btn.setAttribute('aria-label', 'Toggle row details');
    btn.textContent = open ? 'v' : '>';

    function toggle() {
      var nowOpen = !getExpanded(tbl.key, rowKey);
      setExpanded(tbl.key, rowKey, nowOpen);
      btn.setAttribute('aria-expanded', nowOpen ? 'true' : 'false');
      btn.textContent = nowOpen ? 'v' : '>';
      detailRow.hidden = !nowOpen;
    }

    btn.addEventListener('click', function (e) {
      // The row-level handler also toggles; stop here so a button click is
      // not counted twice.
      e.stopPropagation();
      toggle();
    });
    tr.addEventListener('click', toggle);
    extd.appendChild(btn);
    tr.appendChild(extd);

    tbl.cols.forEach(function (col, i) { tr.appendChild(cellNode(col, cells[i])); });
    tbl.tb.appendChild(tr);
    tbl.tb.appendChild(detailRow);
  }

  // A definition grid of label:value pairs. Values may be humanized
  // strings or Nodes (pills). textContent only.
  function buildDefList(pairs) {
    var dl = document.createElement('dl');
    dl.className = 'deflist';
    pairs.forEach(function (p) {
      var wrap = document.createElement('div');
      wrap.className = 'def';
      var dt = document.createElement('dt');
      dt.textContent = p[0];
      var dd = document.createElement('dd');
      if (p[1] instanceof Node) {
        dd.appendChild(p[1]);
      } else {
        dd.textContent = (p[1] === null || p[1] === undefined) ? '-' : String(p[1]);
      }
      wrap.appendChild(dt);
      wrap.appendChild(dd);
      dl.appendChild(wrap);
    });
    return dl;
  }

  // Value-domain token cell: raw wire string as textContent, styled by a
  // class keyed on the exact string. `prefix + '-' + raw` plus the `.tok`
  // fallback, so an unknown token stays readable and unstyled. Families that
  // carry a colorblind-safe leading dot get an explicit `.tok-dot` marker so
  // the dot is keyed on the family, never on a substring of the raw token.
  var DOT_FAMILIES = { circuit: 1, reach: 1, status: 1, qstatus: 1 };
  function tok(prefix, raw) {
    var span = document.createElement('span');
    span.className = 'tok ' + prefix + '-' + raw + (DOT_FAMILIES[prefix] ? ' tok-dot' : '');
    span.textContent = (raw === null || raw === undefined) ? '-' : String(raw);
    return span;
  }

  // A `.tok` pill whose COLOR class stays keyed on the raw wire token (so
  // styling is unaffected) but whose VISIBLE text is the humanized label
  // and whose title preserves the raw token / meaning. Unknown token ->
  // raw passthrough with no tooltip, exactly like `tok`.
  function tokLabeled(prefix, family, raw) {
    var span = tok(prefix, raw);
    var lab = labelFor(family, raw);
    span.textContent = lab.label;
    if (lab.title) { span.title = lab.title; }
    return span;
  }

  // A native default-collapsed disclosure: a summary line plus arbitrary
  // content. No JS state and no mutation affordance -- used for list-shaped
  // detail that must stay reachable (data floor) without dominating a tab.
  function buildExpander(summaryText, contentNode) {
    var det = document.createElement('details');
    var sum = document.createElement('summary');
    sum.textContent = summaryText;
    det.appendChild(sum);
    det.appendChild(contentNode);
    return det;
  }

  // Relative age from the SERVER clock: max(0, serverNowMs - ms). Serves a
  // circuit's "open for" duration (from open_since_ms) and a learned
  // negative's last-observation age (from last_seen_ms). A null / absent
  // input (closed / post-restart, or no last_seen) or an unusable server
  // clock renders "-" -- never a negative age, never invented pre-restart
  // history.
  function ageSince(ms) {
    if (ms === null || ms === undefined || !isFinite(serverNowMs)) {
      return '-';
    }
    return humanDuration(Math.max(0, serverNowMs - Number(ms)));
  }

  // Quota reset cell: a reset instant at or before the server's as_of has
  // already passed, so it renders "elapsed" (with the absolute time in the
  // title) rather than as a live countdown target. Compared to serverNowMs,
  // never Date.now(). Non-positive / absent -> "-".
  function quotaResetCell(resetMs) {
    if (resetMs === null || resetMs === undefined || Number(resetMs) <= 0) { return '-'; }
    if (isFinite(serverNowMs) && Number(resetMs) <= serverNowMs) {
      var span = document.createElement('span');
      span.textContent = 'elapsed';
      span.title = 'reset time already passed: ' + fmtTs(resetMs);
      return span;
    }
    return fmtTs(resetMs);
  }

  // Error breakdown as buildDefList pairs: one [humanized-class, count] pair
  // per resolved failure class (incl. the "unclassified" bucket). The counts
  // sum to the group's `errors` by construction, so an operator can eyeball
  // the reconciliation against the "errors" entry above them.
  function errorClassPairs(byClass) {
    var pairs = [];
    if (!byClass) { return pairs; }
    Object.keys(byClass).forEach(function (k) {
      pairs.push([labelFor('class', k).label, humanCount(byClass[k])]);
    });
    return pairs;
  }

  // A humanized numeric cell that carries the error color only when the
  // value is positive (errors / negative signals), else a plain figure.
  // Renders through magSpan so its K/M/B suffix stays muted like the rest.
  function negCell(n) {
    var span = magSpan(n);
    if (num0(n) > 0) { span.classList.add('neg'); }
    return span;
  }

  // ---- quantitative render helpers -------------------------------------

  // Magnitude number with a smaller, muted K/M/B suffix in its own span.
  // Number formatting is IDENTICAL to humanCount (humanCount stays untouched
  // for deflists / footnotes); this only splits the suffix for typography and
  // de-emphasizes a zero / absent value.
  function magSpan(n) {
    var span = document.createElement('span');
    if (n === null || n === undefined || !isFinite(Number(n))) {
      span.className = 'mag-zero';
      span.textContent = '-';
      return span;
    }
    var x = Number(n), neg = x < 0, a = Math.abs(x);
    if (x === 0) { span.className = 'mag-zero'; }
    if (a < 10000) {
      span.appendChild(document.createTextNode((neg ? '-' : '') + String(Math.trunc(a))));
      return span;
    }
    var v, s;
    if (a >= 1e9) { v = a / 1e9; s = 'B'; }
    else if (a >= 1e6) { v = a / 1e6; s = 'M'; }
    else { v = a / 1e3; s = 'K'; }
    var body = v.toFixed(1);
    if (body.slice(-2) === '.0') { body = body.slice(0, -2); }
    span.appendChild(document.createTextNode((neg ? '-' : '') + body));
    var unit = document.createElement('span');
    unit.className = 'unit';
    unit.textContent = s;
    span.appendChild(unit);
    return span;
  }

  // SVG namespace for the sparklines. All sparkline SVG is built with
  // createElementNS + setAttribute ONLY -- every attribute value is numeric-
  // by-construction (viewBox, polyline points) or a static literal, so no
  // wire / user string is ever written into the DOM as markup. This keeps the
  // page's textContent-only posture (innerHTML is never used) and the
  // page.rs mutation-channel scan intact (this namespace literal opens with a
  // quote-then-letter, not a quote-then-slash, so it is not a path literal).
  var SVG_NS = 'http://www.w3.org/2000/svg';

  // Hand-rolled trend sparkline over `{t, v}` samples (a query series'
  // buckets, keyed by bucket start). The polyline splits into independent
  // segments wherever the inter-sample gap exceeds `gapMs`, so a hole in the
  // grid renders as a break, never a line drawn across it. The vertical
  // scale fits the sample min/max with a div-by-zero guard (a flat series
  // draws a centered line). `seriesB` picks the second, non-semantic data
  // hue for a two-series read.
  function sparkline(samples, gapMs, seriesB) {
    var W = 120, H = 24;
    var svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('class', 'spark');
    svg.setAttribute('viewBox', '0 0 ' + W + ' ' + H);
    svg.setAttribute('preserveAspectRatio', 'none');
    svg.setAttribute('aria-hidden', 'true');
    if (!samples || samples.length < 2) { return svg; }
    var min = samples[0].v, max = samples[0].v;
    samples.forEach(function (s) {
      if (s.v < min) { min = s.v; }
      if (s.v > max) { max = s.v; }
    });
    var vspan = max - min;
    var t0 = samples[0].t, t1 = samples[samples.length - 1].t, tspan = t1 - t0;
    function px(s) {
      var x = tspan > 0 ? (s.t - t0) / tspan * W : W;
      var y = vspan > 0 ? (H - 2) - ((s.v - min) / vspan) * (H - 4) : H / 2;
      return x.toFixed(1) + ',' + y.toFixed(1);
    }
    var seg = [];
    function flush() {
      if (seg.length >= 2) {
        var pl = document.createElementNS(SVG_NS, 'polyline');
        pl.setAttribute('points', seg.join(' '));
        if (seriesB) { pl.setAttribute('class', 'series-b'); }
        svg.appendChild(pl);
      }
      seg = [];
    }
    for (var i = 0; i < samples.length; i++) {
      if (i > 0 && gapMs > 0 && (samples[i].t - samples[i - 1].t) > gapMs) { flush(); }
      seg.push(px(samples[i]));
    }
    flush();
    return svg;
  }

  // A conic-gradient ring gauge with a centered numeral (data floor: the
  // numeral is primary; the ring is a static gauge of the current value).
  // pct is clamped to 0..100; --pct drives the CSS conic-gradient.
  function ringGauge(pct, centerText, small) {
    var clamped = Math.max(0, Math.min(100, isFinite(pct) ? pct : 0));
    var ring = document.createElement('div');
    ring.className = 'ring' + (small ? ' ring--sm' : '');
    ring.style.setProperty('--pct', clamped);
    var center = document.createElement('span');
    center.className = 'ring-center';
    center.textContent = centerText;
    ring.appendChild(center);
    return ring;
  }

  // A numeric cell wrapped in a proportion bar sized by value/max. The
  // content node (a magSpan) rides on top; --pct is the fill fraction.
  function barCell(contentNode, value, max) {
    var span = document.createElement('span');
    span.className = 'barcell';
    var pct = max > 0 ? Math.max(0, Math.min(100, (num0(value) / max) * 100)) : 0;
    span.style.setProperty('--pct', pct);
    span.appendChild(contentNode);
    return span;
  }

  // Three fixed heat bands for a cache-hit ratio (higher is better): hi >=
  // 70%, mid 40-70%, lo < 40%. A degenerate denominator gets no band (the
  // ratio itself renders "-"). Thresholds are fixed and theme-checkable.
  function heatBand(num, den) {
    if (den <= 0) { return ''; }
    var pct = num / den * 100;
    if (pct >= 70) { return 'hit-hi'; }
    if (pct >= 40) { return 'hit-mid'; }
    return 'hit-lo';
  }

  // The hit% cell: the ratio string on a fixed heat band.
  function hitCell(num, den) {
    var span = document.createElement('span');
    span.className = 'hitcell';
    var band = heatBand(num, den);
    if (band) { span.classList.add(band); }
    span.textContent = hitPct(num, den);
    return span;
  }

  function subhead(container, text) {
    var h = document.createElement('h3');
    h.textContent = text;
    container.appendChild(h);
  }

  function none() {
    var d = document.createElement('div');
    d.className = 'none';
    d.textContent = '(none)';
    return d;
  }

  function statCard(label, value, opts) {
    var o = opts || {};
    var card = document.createElement('div');
    card.className = 'stat' + (o.flow ? ' stat--flow' : '');
    var v = document.createElement('div');
    v.className = 'stat-value' + (o.neg ? ' neg' : '');
    if (value instanceof Node) { v.appendChild(value); }
    else { v.textContent = value; }
    var l = document.createElement('div');
    l.className = 'stat-label';
    l.textContent = label;
    card.appendChild(v);
    card.appendChild(l);
    return card;
  }

  // A card shell with a title, an optional hint, and a body. The single
  // container every tab section builds into, so card chrome lives in one
  // place rather than once per tab.
  function card(title, hint, bodyNode) {
    var c = document.createElement('div');
    c.className = 'card';
    var head = document.createElement('div');
    head.className = 'card-head';
    var h = document.createElement('h2');
    h.className = 'card-title';
    h.textContent = title;
    head.appendChild(h);
    if (hint) {
      var hp = document.createElement('span');
      hp.className = 'card-hint';
      hp.textContent = hint;
      head.appendChild(hp);
    }
    c.appendChild(head);
    if (bodyNode) { c.appendChild(bodyNode); }
    return c;
  }

  // =====================================================================
  // Per-tab section builders.
  //
  // One contiguous marker-delimited block per tab, in TABS order, each ending
  // with its registration in BUILDERS. Every builder:
  //   1. consults `stateCard(rec)` first and returns it when non-null, so
  //      loading / unavailable / incompatible / dead states are uniform;
  //   2. reads a query-backed tab's data through `queryView()` (adapter
  //      properties only, never raw JSON) and a GET-backed tab's through
  //      `rec.data` / `SOURCES[<name>].data` per TAB_SOURCES;
  //   3. wraps EVERY section that reads a TAB_SOURCES entry other than the
  //      primary in `safeSection(SOURCES[<name>], build)`, so a secondary
  //      source's fault costs that section only -- renderActiveTab's
  //      try/catch replaces the whole tab and is the floor, not the plan;
  //   4. returns ONE node.
  // =====================================================================

  // ---- tab:overview ----------------------------------------------------
  function buildOverview(rec) {
    var pending = stateCard(rec);
    if (pending) { return pending; }
    return emptyCard('No traffic yet',
      'The proxy is running and idle. Point a client at http://127.0.0.1:8787 and this page fills in on the next poll.',
      ['polling every 5s', WINDOW_SPAN[selectedWindow]]);
  }
  // ---- end tab:overview ------------------------------------------------

  // ---- tab:usage -------------------------------------------------------
  function buildUsage(rec) {
    var pending = stateCard(rec);
    if (pending) { return pending; }
    return emptyCard('No requests in this window',
      'Nothing was routed in the selected window. Pick a wider window, or send a request through the proxy.',
      ['grouped by ' + selectedGroupBy, WINDOW_SPAN[selectedWindow]]);
  }
  // ---- end tab:usage ---------------------------------------------------

  // ---- tab:routing -----------------------------------------------------
  function buildRouting(rec) {
    var pending = stateCard(rec);
    if (pending) { return pending; }
    return emptyCard('No routing history yet',
      'Fallback chains render here once the ledger has traffic to attribute to their steps.',
      ['approximate attribution', 'all history']);
  }
  // ---- end tab:routing -------------------------------------------------

  // ---- tab:health ------------------------------------------------------
  function buildHealth(rec) {
    var pending = stateCard(rec);
    if (pending) { return pending; }
    return emptyCard('No targets observed yet',
      'Configured targets appear here with their breaker state and last settled outcome once they have been dispatched to.',
      ['current state']);
  }
  // ---- end tab:health --------------------------------------------------

  // ---- tab:config ------------------------------------------------------
  function buildConfig(rec) {
    var pending = stateCard(rec);
    if (pending) { return pending; }
    return emptyCard('Configuration',
      'The loaded config source, resolved aliases, and class policies render here.',
      ['current state']);
  }
  // ---- end tab:config --------------------------------------------------

  // ---- tab:doctor ------------------------------------------------------
  function buildDoctor(rec) {
    var pending = stateCard(rec);
    if (pending) { return pending; }
    return emptyCard('No findings yet',
      'The doctor verdict and its findings render here once a report has been produced.',
      ['current state']);
  }
  // ---- end tab:doctor --------------------------------------------------

  var BUILDERS = {
    overview: buildOverview,
    usage: buildUsage,
    routing: buildRouting,
    health: buildHealth,
    config: buildConfig,
    doctor: buildDoctor
  };

  // ---- chrome: poll countdown ------------------------------------------

  // Drive the poll indicator's 1s countdown toward the next scheduled round
  // so it visibly decrements instead of looking frozen.
  function startCountdown(sec) {
    remainingSec = sec;
    renderPollIndicator();
    clearInterval(countdownTimer);
    countdownTimer = setInterval(function () {
      remainingSec = Math.max(0, remainingSec - 1);
      renderPollIndicator();
    }, 1000);
  }

  // ---- chrome: favicon state machine -----------------------------------

  // Four author-static favicon SVGs, differing by fill COLOR only -- no data
  // is interpolated into the markup, so no wire/user value can reach the
  // icon (an XSS floor). The '#' of each hex color is percent-encoded (%23)
  // so it is not parsed as a URI fragment; explicit close tags avoid a
  // self-closing tag so no quote-then-slash literal exists here (the
  // page.rs mutation-channel scan treats such a literal as a fetch path).
  var FAVICON_OK = "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><circle cx='8' cy='8' r='7' fill='%234caf50'></circle></svg>";
  var FAVICON_WARN = "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><circle cx='8' cy='8' r='7' fill='%23f0a020'></circle></svg>";
  var FAVICON_STALE = "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><circle cx='8' cy='8' r='7' fill='%239aa5b5'></circle></svg>";
  var FAVICON_TERMINAL = "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><circle cx='8' cy='8' r='7' fill='%23d32f2f'></circle></svg>";
  var FAVICONS = {
    ok: FAVICON_OK,
    warn_error: FAVICON_WARN,
    stale_unknown: FAVICON_STALE,
    terminal: FAVICON_TERMINAL
  };

  // Stale-or-unknown: the GET loop is backing off, OR no round has ever
  // succeeded, OR the wall clock has passed the next round's due time by
  // more than a fetch budget plus slack. The wall-clock arm matters because
  // a hidden/background tab throttles the poll timer -- without it, a
  // long-hidden tab would keep a green icon while its data quietly aged, so
  // the visibility events re-check this on wake.
  function isStaleUnknown() {
    if (backingOff) { return true; }
    if (lastSuccess === null) { return true; }
    return Date.now() > nextDueAtMs + TIMEOUT_MS + FRESH_SLACK_MS;
  }

  // Warn-or-error, computed ONLY from the last fresh render of each source
  // (never from transport-failure state, which surfaces via backoff /
  // staleness at a higher precedence): any source a fresh round found
  // unusable, or any badge of kind warn/error.
  function isWarnError() {
    for (var i = 0; i < ALL_SOURCES.length; i++) {
      var rec = SOURCES[ALL_SOURCES[i]];
      if (rec.state === 'unavailable' || rec.state === 'incompatible') { return true; }
      if (rec.badge && (rec.badge.kind === 'warn' || rec.badge.kind === 'error')) { return true; }
    }
    return false;
  }

  // Precedence: terminal > stale_unknown > warn_error > ok.
  function faviconState() {
    if (terminal) { return 'terminal'; }
    if (isStaleUnknown()) { return 'stale_unknown'; }
    if (isWarnError()) { return 'warn_error'; }
    return 'ok';
  }

  function updateFavicon() {
    var link = el('favicon');
    if (!link) { return; }
    var next = FAVICONS[faviconState()];
    if (link.getAttribute('href') !== next) { link.setAttribute('href', next); }
  }

  // ---- chrome: manual refresh ------------------------------------------

  // The Refresh button is an edge trigger sharing the poll loop's single
  // gate: enabled only when the loop is live and idle (not terminal, not
  // mid-round, not backing off), so a click can neither queue a refresh nor
  // reset backoff.
  function syncRefreshBtn() {
    var btn = el('refresh');
    if (!btn) { return; }
    btn.disabled = terminal || running || backingOff;
  }

  function initRefresh() {
    var btn = el('refresh');
    if (btn) { btn.addEventListener('click', kickRefresh); }
    syncRefreshBtn();
  }

  // ---- chrome: hash routing (dumb view state) --------------------------

  // Grammar: `#tab[/window]`. `tab` is one of TABS; the optional `window` is
  // one of WINDOWS. An unknown or absent tab canonicalizes to `overview`, an
  // unknown or absent window to `today`. The hash carries VIEW STATE ONLY --
  // applyHash mutates activeTab / selectedWindow and nothing about fetch
  // state. Because it writes those directly, its CALLER owns detecting a move
  // and refreshing the QUERY (see the hashchange listener); a selection change
  // that skips that refresh lets a stale generation repaint the new selection.
  // The separator is built from its char code so this script carries no
  // slash-leading string literal (see the mutation-channel scan in page.rs).
  var HASH_SEP = String.fromCharCode(47);

  function applyHash() {
    var h = location.hash;
    var raw = (h.charAt(0) === '#') ? h.slice(1) : h;
    var parts = raw.split(HASH_SEP);
    activeTab = (TABS.indexOf(parts[0]) >= 0) ? parts[0] : 'overview';
    selectedWindow = (WINDOWS.indexOf(parts[1]) >= 0) ? parts[1] : 'today';
  }

  // Write the canonical hash for the current view state, only when it
  // differs from the current one (loop-safe: replaceState never fires
  // hashchange, and the guard stops a location.hash-assignment fallback from
  // looping). The window segment is omitted for the `today` default.
  function writeHash() {
    var next = '#' + activeTab + (selectedWindow !== 'today' ? HASH_SEP + selectedWindow : '');
    if (location.hash === next) { return; }
    if (history.replaceState) {
      history.replaceState(null, '', next);
    } else {
      location.hash = next;
    }
  }

  // ---- chrome: window picker -------------------------------------------

  // The picker reflects the live selection and goes DIM + inert on the
  // windowless tabs: Config and Doctor report current state, so a window
  // there would promise a filter that does not exist.
  function updateWindowSel() {
    var group = el('windowsel');
    var windowless = !!WINDOWLESS_TABS[activeTab];
    group.setAttribute('data-windowless', windowless ? 'true' : 'false');
    group.title = windowless ? 'this tab is not windowed - it shows current state' : '';
    var buttons = group.querySelectorAll('button');
    Array.prototype.forEach.call(buttons, function (b) {
      var active = b.getAttribute('data-window') === selectedWindow;
      b.setAttribute('aria-pressed', active ? 'true' : 'false');
      b.disabled = terminal || windowless;
    });
  }

  function onWindowChange(w) {
    if (terminal || WINDOWLESS_TABS[activeTab] || w === selectedWindow) { return; }
    selectedWindow = w;
    updateWindowSel();
    writeHash();
    queryInputChanged();
    renderVerdict();
  }

  function initWindowSel() {
    var buttons = el('windowsel').querySelectorAll('button');
    Array.prototype.forEach.call(buttons, function (b) {
      b.addEventListener('click', function () { onWindowChange(b.getAttribute('data-window')); });
    });
    updateWindowSel();
  }

  // ---- chrome: tabs ----------------------------------------------------

  // Selecting a tab switches the visible pane, follows the window picker's
  // applicability, and -- for a query-backed tab -- refreshes its QUERY at
  // once rather than waiting for the next scheduled round.
  function selectTab(name) {
    var changed = activeTab !== name;
    activeTab = name;
    TABS.forEach(function (n) {
      var tab = el('tab-' + n);
      var pane = el('pane-' + n);
      var sel = n === name;
      tab.setAttribute('aria-selected', sel ? 'true' : 'false');
      tab.tabIndex = sel ? 0 : -1;
      pane.hidden = !sel;
    });
    updateWindowSel();
    writeHash();
    renderActiveTab();
    renderVerdict();
    if (changed) { queryInputChanged(); }
  }

  function onTabKey(e, tabs, i) {
    var n = tabs.length;
    var next = -1;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') { next = (i + 1) % n; }
    else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') { next = (i - 1 + n) % n; }
    else if (e.key === 'Home') { next = 0; }
    else if (e.key === 'End') { next = n - 1; }
    if (next >= 0) {
      e.preventDefault();
      tabs[next].focus();
      selectTab(tabs[next].getAttribute('data-tab'));
    }
  }

  function initTabs() {
    var tabs = el('tabbar').querySelectorAll('[role="tab"]');
    Array.prototype.forEach.call(tabs, function (t, i) {
      t.addEventListener('click', function () { selectTab(t.getAttribute('data-tab')); });
      t.addEventListener('keydown', function (e) { onTabKey(e, tabs, i); });
    });
    selectTab(activeTab);
  }

  document.addEventListener('DOMContentLoaded', function () {
    // Restore view state from the hash BEFORE the first tick so the initial
    // render lands on the requested tab/window.
    applyHash();
    initWindowSel();
    initTabs();
    initRefresh();
    renderVerdict();
    syncTabBadges();
    updateFavicon();
    // Back/forward or a manual hash edit re-applies view state; the sync
    // path canonicalizes any bogus hash. applyHash writes activeTab and
    // selectedWindow DIRECTLY, so selectTab's own change check cannot see the
    // move -- capture the prior selection here and refresh the QUERY whenever
    // either half of it changed. Without this, an in-flight QUERY for the old
    // selection would repaint the new one.
    window.addEventListener('hashchange', function () {
      var priorTab = activeTab;
      var priorWindow = selectedWindow;
      applyHash();
      var moved = activeTab !== priorTab || selectedWindow !== priorWindow;
      selectTab(activeTab);
      if (moved) { queryInputChanged(); }
    });
    // A hidden tab throttles the poll timer, so re-evaluate staleness (and
    // the icon) whenever the tab regains attention.
    document.addEventListener('visibilitychange', updateFavicon);
    window.addEventListener('focus', updateFavicon);
    window.addEventListener('pageshow', updateFavicon);
    tick();
  });
})();
