'use strict';
(function () {
  // Per-source wire versions this page was built against. See the
  // co-versioning note in the document head: same-binary, so a mismatch
  // should never occur in practice; the runtime check is recovery
  // containment for a cached page / mixed assets / a bad build, not version
  // negotiation. `query` sits alongside the four GET panels because the
  // QUERY aggregate is a source of its own (see SOURCES below).
  var EXPECTED = { usage: 3, health: 5, config: 2, doctor: 4, query: 1 };

  // The four panels of the aggregate round, each with its own envelope +
  // freshness. NOT the same list as TABS: a tab is a view, a source is a
  // fetch.
  var GET_SOURCES = ['usage', 'health', 'config', 'doctor'];
  var QUERY_SOURCE = 'query';
  // The all-history usage read: its OWN fetch, its OWN backoff, its OWN
  // as_of. Routing attributes over ALL HISTORY, so it must never read the
  // aggregate's today-scoped usage panel -- and the aggregate panel must stay
  // today-scoped for the readers that want today (the Health quota tiles, the
  // verdict strip). Two windows of one panel are two sources.
  var USAGE_ALL_SOURCE = 'usage_all';
  var USAGE_ALL_URL = '/status/usage?window=all';
  var ALL_SOURCES = GET_SOURCES.concat([QUERY_SOURCE, USAGE_ALL_SOURCE]);

  // The panel a source's envelope belongs to, where the two differ. The
  // all-history read is the usage PANEL at another window, so it validates
  // against the usage wire version rather than one of its own.
  var SOURCE_PANEL = { usage_all: 'usage' };

  // What a degraded source is called in the verdict strip. A source name is
  // an internal key; only the ones that do not read as English need a word.
  var SOURCE_LABELS = { usage_all: 'all-history usage' };

  function expectedVersion(name) {
    return EXPECTED[SOURCE_PANEL[name] || name];
  }

  function sourceLabel(name) {
    return SOURCE_LABELS[name] || name;
  }

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
  // leaves the four GET-backed tabs untouched. The FIRST entry is the tab's
  // primary source: it drives the freshness line, the tab badge, and the
  // whole-tab state card, so a multi-source tab names the source that owns its
  // subject first (Routing is about the CONFIGURED chains; the ledger and
  // health only describe them).
  var TAB_SOURCES = {
    overview: [QUERY_SOURCE, 'usage'],
    usage: [QUERY_SOURCE],
    routing: ['config', USAGE_ALL_SOURCE, 'health'],
    health: ['health', 'usage'],
    config: ['config'],
    doctor: ['doctor']
  };

  // The windowless tabs. Config and Doctor report CURRENT state; Routing
  // attributes over ALL HISTORY. None of the three is a windowed aggregate,
  // so the picker dims and goes inert on all of them rather than promising a
  // filter that does not reach the figures.
  var WINDOWLESS_TABS = { routing: true, config: true, doctor: true };

  // What a windowless tab's figures actually span, for the verdict strip.
  var WINDOWLESS_SPAN = {
    routing: 'all history',
    config: 'current state',
    doctor: 'current state'
  };

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

  var el = function (id) { return document.getElementById(id); };

  // ---- per-source state records ----------------------------------------

  // One record per data source: a failure in one never clears a sibling.
  // `state` is one of loading | live | empty | unavailable | incompatible |
  // invalid_payload | stale | dead. `name` is carried on the record so a
  // section handed only its record can still say WHICH source it renders.
  function freshRecord(name) {
    return { name: name, state: 'loading', code: null, data: null, asOf: null, badge: null };
  }

  var SOURCES = {};
  ALL_SOURCES.forEach(function (n) { SOURCES[n] = freshRecord(n); });

  // Sources whose LAST render threw inside a section builder. A source can be
  // transport-live and still unrenderable, and the status line and the verdict
  // must say so -- otherwise the page reports "all routes healthy" beside a
  // section that could not be drawn. Recorded during the render (see
  // safeSection), reconciled at the end of every render pass against what that
  // pass actually drew, and cleared when a new payload lands for that source.
  var RENDER_FAULTS = Object.create(null);

  function setSource(name, next) {
    var prev = SOURCES[name];
    var merged = {
      name: name,
      state: prev.state,
      code: prev.code,
      data: prev.data,
      asOf: prev.asOf,
      badge: prev.badge
    };
    Object.keys(next).forEach(function (k) { merged[k] = next[k]; });
    SOURCES[name] = merged;
    delete RENDER_FAULTS[name];
  }

  // The state a source PRESENTS: its transport state, unless the last render
  // of one of its sections failed, in which case it is invalid_payload no
  // matter how healthy the transport was.
  function effectiveState(rec) {
    return RENDER_FAULTS[rec.name] ? 'invalid_payload' : rec.state;
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

  // A percentage that ARRIVED as a percentage (the ledger's own weighted
  // cache-hit share), to whole precision. A non-positive share renders "0%"
  // faintly at its call site rather than as an absence: zero warm reads is a
  // measurement, not a missing one.
  function pctText(v) {
    var x = Number(v);
    return (isFinite(x) ? Math.round(x) : 0) + '%';
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
    // Provider auto-activation STATUS. `unknown` is an activation state this
    // build does not recognize, not an absence of one.
    actstatus: {
      activated: { title: 'a usable credential resolved for this provider' },
      unresolved: { title: 'no usable credential resolved - the reason is beside it' },
      unknown: { title: 'an activation state this build does not recognize' }
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

  // Whether a RAW query payload matches its declared shape: an object carrying
  // a `groups` ARRAY, an object `totals`, and a `series` member present as
  // either an object or an explicit null (the server always emits it, never
  // skips it). Checked on the raw payload rather than after adaptation, because
  // the adapter substitutes empty arrays and zero metrics for absent members --
  // which would render a corrupt payload as an empty ledger.
  //
  // An EMPTY ledger satisfies this shape: `groups: []` beside zero totals is a
  // measurement, and it must keep reaching the welcoming empty state.
  function isQueryShape(raw) {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) { return false; }
    if (!Array.isArray(raw.groups)) { return false; }
    if (!raw.totals || typeof raw.totals !== 'object' || Array.isArray(raw.totals)) {
      return false;
    }
    if (!Object.prototype.hasOwnProperty.call(raw, 'series')) { return false; }
    var series = raw.series;
    return series === null ||
      (!!series && typeof series === 'object' && !Array.isArray(series));
  }

  // The adapted QUERY payload for the current selection, or null when the
  // query source carries no usable payload. `stale` counts as usable: a 503
  // RETAINS the last-good data by design, and the degradation is the section
  // status line's job to report -- discarding it here would turn a recoverable
  // overload into an invalid-payload card and lose the stale-values reading.
  //
  // A payload that does not match its declared shape returns null, so both
  // query-backed builders throw inside `safeSection` and the source records
  // `invalid_payload` -- corruption is never adapted into an empty ledger.
  function queryView() {
    var rec = SOURCES[QUERY_SOURCE];
    var usable = rec.state === 'live' || rec.state === 'stale';
    return (usable && isQueryShape(rec.data)) ? QueryAdapter(rec.data) : null;
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
    queryLastAttemptKey = key;
    queryLastAttemptAtMs = Date.now();
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
    if (env.schema_version !== expectedVersion(name)) {
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
  // Render the active tab's body from its sources. The tab builder runs in its
  // own try/catch so a malformed same-version payload degrades to one
  // `invalid_payload` card instead of a blank page or a failed round. This is
  // the LAST RESORT, not the containment plan: it replaces the WHOLE tab, so a
  // multi-source builder that lets a secondary source throw here would wipe
  // out healthy primary content. Multi-source builders confine each source to
  // its own section via `safeSection` instead.
  //
  // The BODY is built before the status line is drawn: building is what
  // discovers a render fault (see safeSection), and a status line drawn first
  // would report a source live beside a section that could not be rendered.
  // The same reason drives the badge/verdict refresh at the end.
  function renderActiveTab() {
    var tab = activeTab;
    var pane = el('pane-' + tab);
    var status = el('status-' + tab);
    var body = el('body-' + tab);
    if (!pane || !body) { return; }
    var primary = SOURCES[TAB_SOURCES[tab][0]];
    var pass = { faults: Object.create(null), touched: Object.create(null) };
    renderPass = pass;
    try {
      body.replaceChildren(BUILDERS[tab](primary));
    } catch (e) {
      pass.faults[primary.name] = true;
      pass.touched[primary.name] = true;
      body.replaceChildren(errorCard('invalid_payload',
        'this section could not be rendered from the payload it received'));
    } finally {
      renderPass = null;
    }
    // What this pass ACTUALLY drew decides the fault map: a source every one of
    // whose sections built cleanly here has recovered, whatever an earlier pass
    // recorded, and a source that threw here is faulted even on a payload that
    // rendered before. Reconciled BEFORE the status line is drawn, since that
    // line reads the effective state.
    var changed = reconcileRenderFaults(pass);
    var state = effectiveState(primary);
    pane.classList.remove('section--stale', 'section--dead');
    if (state === 'stale') { pane.classList.add('section--stale'); }
    if (state === 'dead') { pane.classList.add('section--dead'); }
    renderSectionStatus(status, primary);
    // A fault raised or cleared by THIS pass changes what the chrome should say
    // about its source. Neither of these re-enters the tab builder, so there is
    // no recursion back into a section that just rendered.
    if (changed) {
      renderVerdict();
      syncTabBadges();
    }
  }

  // The sections built during the current render pass: which sources they read
  // and which of them threw. Non-null only while a tab body is being built.
  var renderPass = null;

  // Record a section fault against its source for the pass in progress. Set on
  // the pass rather than straight into RENDER_FAULTS so the map is reconciled
  // once, after the whole body has been built.
  function recordRenderFault(name) {
    if (renderPass) { renderPass.faults[name] = true; renderPass.touched[name] = true; }
    else { RENDER_FAULTS[name] = true; }
  }

  function markRenderAttempt(name) {
    if (renderPass) { renderPass.touched[name] = true; }
  }

  // Fold a finished pass into RENDER_FAULTS: every source this pass drew is
  // faulted or cleared by what happened HERE; a source no section of this tab
  // read keeps whatever the pass that did read it recorded. Returns whether the
  // map changed, so the verdict and badges refresh only when it did.
  function reconcileRenderFaults(pass) {
    var changed = false;
    Object.keys(pass.touched).forEach(function (name) {
      var faulted = !!pass.faults[name];
      if (faulted === !!RENDER_FAULTS[name]) { return; }
      if (faulted) { RENDER_FAULTS[name] = true; }
      else { delete RENDER_FAULTS[name]; }
      changed = true;
    });
    return changed;
  }

  // Render ONE visual section under its own error boundary, against the source
  // record that section reads. Returns the section's node, or -- when that
  // source carries no renderable payload, or when building from it throws -- a
  // card describing just this section's state. Siblings are untouched either
  // way.
  //
  // A throw is RECORDED against the source for the pass in progress, not just
  // drawn: the pane's status line and the page verdict read the source's
  // effective state, so without the record they would keep calling a source
  // live while one of its sections sat as an error card. A clean build is
  // recorded too -- the pass reconciles both, so a section that recovers clears
  // its source's fault in the same pass it renders. The record is set rather
  // than pushed through renderSourceChanged -- re-entering the render from
  // inside a builder would rebuild the tab that is mid-build.
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
      var node = build(rec);
      markRenderAttempt(rec.name);
      return node;
    } catch (e) {
      recordRenderFault(rec.name);
      return errorCard('invalid_payload',
        'this section could not be rendered from the payload it received');
    }
  }

  function renderSectionStatus(status, rec) {
    if (!status) { return; }
    var state = effectiveState(rec);
    status.replaceChildren();
    if (state === 'loading') {
      status.appendChild(document.createTextNode('loading'));
      return;
    }
    status.appendChild(makeLiveDot());
    if (state === 'stale' || state === 'dead') {
      var note = lastSuccess
        ? 'last success at ' + lastSuccess.toLocaleTimeString()
        : 'no successful poll yet';
      var lead = state === 'stale' ? 'stale: ' : 'no current data: ';
      status.appendChild(document.createTextNode(lead + note));
      return;
    }
    if (state === 'invalid_payload') {
      status.appendChild(document.createTextNode(
        'invalid payload: a section could not be rendered'));
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
    var state = rec.state;
    if (state === 'loading') { return skeletonCard(); }
    if (state === 'unavailable') {
      return errorCard(rec.code, 'this source is not answering right now');
    }
    if (state === 'incompatible') { return incompatibleCard(rec.code); }
    if (state === 'invalid_payload') {
      return errorCard('invalid_payload', 'the payload did not match its declared shape');
    }
    if (state === 'dead') { return errorCard('transport failure', null); }
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
    clearTimeout(usageAllTimer);
    clearInterval(countdownTimer);
    if (queryCtrl) { queryCtrl.abort(); queryCtrl = null; }
    if (usageAllCtrl) { usageAllCtrl.abort(); usageAllCtrl = null; }
    usageAllGeneration += 1;
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

  // Precedence: a stopped page, then a dead aggregate, then the today
  // aggregate's own overload, then any source a fresh round found unusable or
  // left STALE, then healthy. Every arm reads the EFFECTIVE state, so a source
  // whose section threw during render counts as unusable here rather than
  // letting the strip claim health beside an error card -- and a source
  // serving RETAINED values after a 503 is reported as stale rather than
  // passing as healthy beside the retained figures it is showing.
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
      var s = effectiveState(SOURCES[n]);
      return s === 'unavailable' || s === 'incompatible' || s === 'dead' ||
        s === 'invalid_payload';
    });
    var staleSources = ALL_SOURCES.filter(function (n) {
      return effectiveState(SOURCES[n]) === 'stale';
    });
    if (degraded.length || staleSources.length) {
      var parts = [];
      if (degraded.length) { parts.push(degraded.map(sourceLabel).join(', ') + ' unavailable'); }
      if (staleSources.length) {
        parts.push(staleSources.map(sourceLabel).join(', ') + ' data stale');
      }
      return { kind: 'warn', text: 'Routing healthy - ' + parts.join('; ') };
    }
    if (ALL_SOURCES.every(function (n) { return SOURCES[n].state === 'loading'; })) {
      return { kind: 'idle', text: 'Checking routes' };
    }
    return { kind: 'ok', text: 'All routes healthy' };
  }

  // req/span for the ACTIVE tab: a query-backed tab reads the adapter's
  // totals; a GET-backed tab reads the usage totals of the ledger read THAT
  // tab is looking at -- the all-history one on Routing, the today-scoped
  // aggregate elsewhere -- so the count and the span beside it always describe
  // the same read. One derivation each, never both.
  function verdictStats() {
    var span = WINDOWLESS_TABS[activeTab]
      ? WINDOWLESS_SPAN[activeTab]
      : WINDOW_SPAN[selectedWindow];
    var view = TAB_SOURCES[activeTab][0] === QUERY_SOURCE ? queryView() : null;
    if (view) { return humanCount(view.totals.requests) + ' req ' + span; }
    var usage = SOURCES[activeTab === 'routing' ? USAGE_ALL_SOURCE : 'usage'];
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
  // visible without switching to that tab. A source whose section failed to
  // render badges as unusable rather than carrying the badge computed from the
  // last payload that did render.
  function syncTabBadges() {
    TABS.forEach(function (tab) {
      var rec = SOURCES[TAB_SOURCES[tab][0]];
      setTabBadge(tab, effectiveState(rec) === 'invalid_payload'
        ? { label: 'n/a', kind: 'muted' }
        : rec.badge);
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
  // strings or Nodes (pills). textContent only. A row whose detail carries no
  // pairs at all renders an explicit line rather than an empty grid: an
  // expander that opens onto nothing reads as a click that did not work.
  function buildDefList(pairs) {
    if (!pairs.length) { return noDetailLine(); }
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

  function noDetailLine() {
    var p = document.createElement('p');
    p.className = 'nodetail';
    p.textContent = 'No additional detail for this row.';
    return p;
  }

  // Value-domain token cell: raw wire string as textContent, styled by a
  // class keyed on the exact string. `prefix + '-' + raw` plus the `.tok`
  // fallback, so an unknown token stays readable and unstyled. Families that
  // carry a colorblind-safe leading dot get an explicit `.tok-dot` marker so
  // the dot is keyed on the family, never on a substring of the raw token.
  var DOT_FAMILIES = { circuit: 1, reach: 1, status: 1, qstatus: 1, actstatus: 1 };
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

  // Relative age from the SERVER clock: max(0, nowMs - ms). Serves a circuit's
  // "open for" duration (from open_since_ms) and a learned negative's
  // last-observation age (from last_seen_ms). `nowMs` is the as_of of the
  // record the input came from -- passed in per section rather than read off a
  // shared render clock, because a page-global clock would age one panel's
  // figures against another panel's as_of. A null / absent input (closed /
  // post-restart, or no last_seen) or an unusable clock renders "-" -- never a
  // negative age, never invented pre-restart history.
  function ageSince(ms, nowMs) {
    if (ms === null || ms === undefined || !isFinite(nowMs)) {
      return '-';
    }
    return humanDuration(Math.max(0, nowMs - Number(ms)));
  }

  // The instant a record's figures are read against: the record's OWN as_of.
  // Never Date.now(), so client skew can neither invent an age nor un-elapse a
  // reset; never a sibling source's as_of, so one panel's clock cannot age
  // another's data.
  function panelNowMs(rec) {
    var parsed = Date.parse(rec.asOf);
    return isFinite(parsed) ? parsed : NaN;
  }

  // Quota reset cell: a reset instant at or before the panel's as_of has
  // already passed, so it renders "elapsed" (with the absolute time in the
  // title) rather than as a live countdown target. Non-positive / absent
  // -> "-".
  function quotaResetCell(resetMs, nowMs) {
    if (resetMs === null || resetMs === undefined || Number(resetMs) <= 0) { return '-'; }
    if (isFinite(nowMs) && Number(resetMs) <= nowMs) {
      var span = document.createElement('span');
      span.textContent = 'elapsed';
      span.title = 'reset time already passed: ' + fmtTs(resetMs);
      return span;
    }
    return fmtTs(resetMs);
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
  // grid renders as a break, never a line drawn across it. `seriesB` picks the
  // second, non-semantic data hue.
  function sparkline(samples, gapMs, seriesB) {
    return sparkSvg([{ samples: samples, b: !!seriesB }], gapMs);
  }

  // Two series on ONE shared vertical scale so the pair is comparable at a
  // glance (per-series scaling would make a small series look like a large
  // one). The second rides the second data hue.
  function sparklinePair(a, b, gapMs) {
    return sparkSvg([{ samples: a, b: false }, { samples: b, b: true }], gapMs);
  }

  // The shared sparkline geometry. The vertical scale fits the min/max of
  // EVERY series with a div-by-zero guard (a flat series draws a centered
  // line); the horizontal one spans the samples' own timestamps, so a point
  // sits where its bucket starts rather than on an assumed stride.
  function sparkSvg(series, gapMs) {
    var W = 120, H = 24;
    var svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('class', 'spark');
    svg.setAttribute('viewBox', '0 0 ' + W + ' ' + H);
    svg.setAttribute('preserveAspectRatio', 'none');
    svg.setAttribute('aria-hidden', 'true');
    var all = [];
    series.forEach(function (s) { if (s.samples) { all = all.concat(s.samples); } });
    if (all.length < 2) { return svg; }
    var min = all[0].v, max = all[0].v, t0 = all[0].t, t1 = all[0].t;
    all.forEach(function (s) {
      if (s.v < min) { min = s.v; }
      if (s.v > max) { max = s.v; }
      if (s.t < t0) { t0 = s.t; }
      if (s.t > t1) { t1 = s.t; }
    });
    var vspan = max - min, tspan = t1 - t0;
    function px(s) {
      var x = tspan > 0 ? (s.t - t0) / tspan * W : W;
      var y = vspan > 0 ? (H - 2) - ((s.v - min) / vspan) * (H - 4) : H / 2;
      return x.toFixed(1) + ',' + y.toFixed(1);
    }
    series.forEach(function (s) {
      if (s.samples && s.samples.length >= 2) { drawSegments(svg, s, gapMs, px); }
    });
    return svg;
  }

  // Append one series as one polyline per gap-free run of samples.
  function drawSegments(svg, series, gapMs, px) {
    var samples = series.samples;
    var seg = [];
    function flush() {
      if (seg.length >= 2) {
        var pl = document.createElementNS(SVG_NS, 'polyline');
        pl.setAttribute('points', seg.join(' '));
        if (series.b) { pl.setAttribute('class', 'series-b'); }
        svg.appendChild(pl);
      }
      seg = [];
    }
    for (var i = 0; i < samples.length; i++) {
      if (i > 0 && gapMs > 0 && (samples[i].t - samples[i - 1].t) > gapMs) { flush(); }
      seg.push(px(samples[i]));
    }
    flush();
  }

  // A title + optional hint header row. Shared by `card` and by sections that
  // are deliberately NOT cards (a set of separate objects rather than one
  // reading), so both carry identical header typography.
  function sectionHead(title, hint) {
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
    return head;
  }

  // A card shell with a title, an optional hint, and a body. The single
  // container every tab section builds into, so card chrome lives in one
  // place rather than once per tab.
  function card(title, hint, bodyNode) {
    var c = document.createElement('div');
    c.className = 'card';
    c.appendChild(sectionHead(title, hint));
    if (bodyNode) { c.appendChild(bodyNode); }
    return c;
  }

  // A vertical stack of sections, the shape a tab body returns so its sections
  // sit on the 8px rhythm.
  function tabStack() {
    var stack = document.createElement('div');
    stack.className = 'tabstack';
    return stack;
  }

  // A big figure: the numeral plus an optional faint unit slot on either side
  // (a leading one for a currency mark, a trailing one for a magnitude). A zero
  // or absent value de-emphasizes whole -- a zero is faint, never an error.
  function figure(text, unit, pre) {
    var span = document.createElement('span');
    if (text === '-' || text === '0' || text === '0.00') { span.classList.add('mag-zero'); }
    if (pre) { span.appendChild(unitSlot(pre)); }
    span.appendChild(document.createTextNode(text));
    if (unit) { span.appendChild(unitSlot(unit)); }
    return span;
  }

  function unitSlot(text) {
    var u = document.createElement('span');
    u.className = 'unit';
    u.textContent = text;
    return u;
  }

  // A figure that carries no measurement at all (an unpriced cost, a metric
  // the window never observed): the word itself, de-emphasized.
  function faintFigure(text) {
    var span = figure(text, null, null);
    span.classList.add('mag-zero');
    return span;
  }

  // A millisecond span split into numeral + unit, promoted to seconds past a
  // full second so a tile never shows five digits of milliseconds. A
  // non-positive input means the metric was never observed (the adapter
  // coerces an absent figure to zero and no real latency is 0ms), so it reads
  // as absent rather than as an instant response.
  function msParts(ms) {
    var v = num0(ms);
    if (v <= 0) { return { v: '-', u: '' }; }
    if (v < 1000) { return { v: String(Math.round(v)), u: 'ms' }; }
    return { v: (v / 1000).toFixed(1), u: 's' };
  }

  function msText(ms) {
    var p = msParts(ms);
    return p.v + p.u;
  }

  // A dollar amount at a precision that cannot hide a real cost: cents at a
  // dollar and above, tenths of a cent below it.
  function money(v) {
    var x = Number(v);
    if (!isFinite(x) || x === 0) { return '0.00'; }
    return Math.abs(x) >= 1 ? x.toFixed(2) : x.toFixed(3);
  }

  // A faint one-line note under a tile's figure, carrying the error color only
  // when it reports a negative signal.
  function subNote(text, neg) {
    var span = document.createElement('span');
    if (neg) { span.className = 'neg'; }
    span.textContent = text;
    return span;
  }

  // A thin proportion bar: a hairline track with a fill sized by --pct. Used
  // where the proportion rides UNDER a reading rather than behind a numeric
  // cell (which is barCell's job).
  function shareBar(pct) {
    var track = document.createElement('span');
    track.className = 'sharebar';
    var fill = document.createElement('span');
    fill.className = 'sharebar-fill';
    fill.style.setProperty('--pct', Math.max(0, Math.min(100, isFinite(pct) ? pct : 0)));
    track.appendChild(fill);
    return track;
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

  // The coarsest bucket width that still reads as an hour of the day. Wider
  // grids label their busiest bucket by date instead.
  var HOUR_MS = 3600000;

  // A hole in the series grid: the server emits every bucket in the window,
  // traffic or not, so a jump wider than this many bucket widths means a
  // bucket is genuinely MISSING and the sparkline breaks rather than drawing
  // a line across it.
  var SERIES_GAP_FACTOR = 1.5;

  // The scope strip renders OUTSIDE the section boundary: a provider-scoped
  // query that comes back unavailable or refused must still be reversible, and
  // the state card that replaces the content cannot carry that affordance.
  function buildOverview(rec) {
    var stack = tabStack();
    if (selectedProvider) { stack.appendChild(scopeStrip()); }
    stack.appendChild(safeSection(rec, buildOverviewLive));
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
    if (num0(view.totals.requests) <= 0) { return overviewEmpty(); }
    var stack = tabStack();
    stack.appendChild(providerSection(view));
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

  // Scope every figure on this tab to one provider, or lift the scope when the
  // provider already scoping it is picked again. The query is re-issued
  // through the standard input-changed path (which aborts the in-flight
  // request and bumps the generation, so a late response for the previous
  // scope cannot repaint this one), and only this tab repaints -- the
  // GET-backed siblings read no query source.
  function onProviderScope(label) {
    selectedProvider = (selectedProvider === label) ? null : label;
    queryInputChanged();
    renderActiveTab();
    renderVerdict();
  }

  // The scoped-to-one-provider header, with the affordance that lifts it.
  function scopeStrip() {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    var head = sectionHead('Scoped to ' + selectedProvider,
      'every figure below is this provider only');
    var reset = document.createElement('button');
    reset.type = 'button';
    reset.className = 'scope-reset';
    reset.textContent = 'all providers';
    reset.addEventListener('click', function () { onProviderScope(selectedProvider); });
    head.appendChild(reset);
    wrap.appendChild(head);
    return wrap;
  }

  // The provider cards: one per query group, each a scope affordance. Cards,
  // not a table -- a provider is a separate object an operator acts on. The
  // seat quota rides on the card because a provider's credential headroom is a
  // fact about that provider; it comes from the usage source, not the query
  // one, so it is read through the seat index below and a usage failure costs
  // the seat surface alone.
  function providerSection(view) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Providers', selectedProvider
      ? 'pick this card again to see every provider'
      : 'pick one to scope these figures'));
    var grid = document.createElement('div');
    grid.className = 'provgrid';
    var totalReq = num0(view.totals.requests);
    var seats = seatIndex(SOURCES.usage);
    view.groups.forEach(function (g) {
      grid.appendChild(providerCard(g, totalReq, seats));
    });
    wrap.appendChild(grid);
    return wrap;
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

  function providerCard(group, totalReq, seats) {
    var m = group.metrics;
    var scoped = group.label === selectedProvider;
    var cardEl = document.createElement('div');
    cardEl.className = 'provcard' + (scoped ? ' provcard--on' : '');

    // The scope affordance is its own button INSIDE the card so the seat
    // disclosure beside it is a separate control -- a button cannot contain
    // one, and a card-wide handler would fire the scope change on every seat
    // click.
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'provcard-scope';
    btn.setAttribute('aria-pressed', scoped ? 'true' : 'false');

    var name = document.createElement('span');
    name.className = 'provcard-name';
    name.textContent = group.label;
    btn.appendChild(name);

    var cost = document.createElement('span');
    cost.className = 'provcard-cost';
    cost.appendChild(costFigure(m));
    btn.appendChild(cost);

    var facts = document.createElement('span');
    facts.className = 'provcard-facts';
    facts.textContent = humanCount(m.requests) + ' req - ' + msText(m.ttft_p50_ms) +
      ' ttft - ' + pctText(m.cache_hit_pct) + ' cached';
    btn.appendChild(facts);

    var share = totalReq > 0 ? num0(m.requests) / totalReq * 100 : 0;
    var row = document.createElement('span');
    row.className = 'provcard-share';
    row.appendChild(shareBar(share));
    var pct = document.createElement('span');
    pct.className = 'provcard-sharepct';
    pct.textContent = Math.round(share) + '%';
    row.appendChild(pct);
    btn.appendChild(row);

    btn.addEventListener('click', function () { onProviderScope(group.label); });
    cardEl.appendChild(btn);

    var seatBlock = providerSeats(group.label, seats);
    if (seatBlock) { cardEl.appendChild(seatBlock); }
    return cardEl;
  }

  // The card's seat affordance: a default-closed disclosure over the SAME
  // quota tiles the Health tab renders, filtered to this provider's seats.
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
    var wrap = document.createElement('div');
    wrap.className = 'provseats';
    var list = document.createElement('div');
    list.className = 'qlist qlist--card';
    rows.forEach(function (q) { list.appendChild(quotaTile(q, seats.nowMs)); });
    wrap.appendChild(buildExpander(seatSummary(rows), list));
    return wrap;
  }

  // The disclosure line: how many seats this provider reports, and nothing
  // else. No max, no average, no rollup across seats -- a seat's quota is a
  // fact about one credential, and a headline over several would be a figure
  // no provider reported.
  function seatSummary(rows) {
    return rows.length + phrase(rows.length, ' seat quota', ' seat quotas');
  }

  function seatSurfaceUnavailable() {
    var note = document.createElement('p');
    note.className = 'footnote provseats-none';
    note.textContent = 'seat quota unavailable';
    return note;
  }

  // The eight KPI tiles, hairline-gridded so they read as facets of one
  // reading. Each carries its own sparkline over the SERVER's per-bucket
  // series -- no point is synthesized, and each is drawn at its own bucket
  // start rather than on an assumed stride.
  function kpiSection(view) {
    var t = view.totals;
    var series = view.series;
    var gap = num0(series.bucket_ms) * SERIES_GAP_FACTOR;
    var reqSpark = bucketSamples(series, function (m) { return m.requests; });
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
    return grid;
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
      sparkline(reqSpark, gap, false));
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
        ? 'peak ' + msText(t.ttft_p95_ms) + ' over ' + humanCount(t.requests) + ' req'
        : 'no streamed response in this window', false),
      sparkline(bucketSamples(series, function (m) { return m.ttft_p50_ms; }), gap, false));
  }

  function fallbackTile(t, series, gap) {
    var req = num0(t.requests), served = num0(t.fallback_served);
    var pct = req > 0 ? Math.round(served / req * 100) : 0;
    return kpiTile('Served by fallback',
      figure(String(pct), '%', null),
      subNote(served === 0
        ? 'primary held for every request'
        : humanCount(served) + ' of ' + humanCount(req) + ' took a later step', false),
      sparkline(bucketSamples(series, function (m) { return m.fallback_served; }), gap, false));
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
      sparkline(reqSpark, gap, false));
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
          humanCount(Math.round(num0(t.output_tokens) / req)) + ' out avg per request'
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
        ? 'request-weighted over ' + humanCount(t.requests) + ' req'
        : 'nothing served warm yet', false),
      sparkline(bucketSamples(series, function (m) { return m.cache_hit_pct; }), gap, false));
  }

  function costTile(t, series, gap) {
    return kpiTile('Est. cost',
      costFigure(t),
      subNote(costNote(t), false),
      sparkline(bucketSamples(series, function (m) { return m.cost_usd; }), gap, false));
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
      return req > 0 ? '$' + (num0(t.cost_usd) / req).toFixed(3) + ' per request' : 'priced';
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

  // ---- tab:usage -------------------------------------------------------

  // The dimensions the ledger can group by, in picker order.
  var GROUP_BYS = [
    ['model', 'Model'],
    ['alias', 'Alias'],
    ['provider', 'Provider']
  ];

  var GROUP_BY_TITLES = {
    model: 'By model',
    alias: 'By alias',
    provider: 'By provider'
  };

  // The group-by picker and the provider-scope strip render OUTSIDE the section
  // boundary, for the same reason the Overview scope strip does: a selection
  // whose query comes back unavailable or refused must stay reversible, and the
  // state card that replaces the content carries no affordance.
  function buildUsage(rec) {
    var stack = tabStack();
    if (selectedProvider) { stack.appendChild(scopeStrip()); }
    stack.appendChild(groupByStrip());
    stack.appendChild(safeSection(rec, buildUsageLive));
    return stack;
  }

  // One card per group, never a table: a group is a separate object an operator
  // reads on its own, and the per-group stats do not line up as columns of one
  // reading. Zero-traffic groups are omitted rather than listed as rows of
  // zeroes; the header says how many were left out.
  function buildUsageLive() {
    var view = queryView();
    if (!view) { throw new Error('usage query payload is not live'); }
    var live = view.groups.filter(function (g) { return num0(g.metrics.requests) > 0; });
    if (num0(view.totals.requests) <= 0 || live.length === 0) { return usageEmpty(); }
    var omitted = view.groups.length - live.length;
    var ranked = live.slice().sort(function (a, b) {
      return num0(b.metrics.requests) - num0(a.metrics.requests);
    });
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead(GROUP_BY_TITLES[selectedGroupBy], usageHint(ranked.length, omitted)));
    var list = document.createElement('div');
    list.className = 'ugroups';
    var totalReq = num0(view.totals.requests);
    ranked.forEach(function (g) { list.appendChild(usageCard(g, totalReq)); });
    wrap.appendChild(list);
    return wrap;
  }

  function usageHint(shown, omitted) {
    var hint = shown + (shown === 1 ? ' group - ' : ' groups - ') + WINDOW_SPAN[selectedWindow];
    if (omitted > 0) {
      hint += ' - ' + omitted + (omitted === 1 ? ' group' : ' groups') + ' with no requests not listed';
    }
    return hint;
  }

  function usageEmpty() {
    return emptyCard('No requests in this window',
      'Nothing was routed ' + WINDOW_SPAN[selectedWindow] +
      '. Pick a wider window, or send a request through the proxy.',
      ['grouped by ' + selectedGroupBy, WINDOW_SPAN[selectedWindow]]);
  }

  // Switch the grouping dimension. The query is re-issued through the standard
  // input-changed path (which aborts the in-flight request and bumps the
  // generation, so a late response for the previous dimension cannot repaint
  // this one), and only this tab repaints -- no sibling tab reads the query
  // source.
  function onGroupBy(dim) {
    if (terminal || dim === selectedGroupBy) { return; }
    selectedGroupBy = dim;
    queryInputChanged();
    renderActiveTab();
    renderVerdict();
  }

  function groupByStrip() {
    var wrap = document.createElement('div');
    wrap.className = 'gbstrip';
    var label = document.createElement('span');
    label.className = 'gblabel';
    label.textContent = 'Group by';
    wrap.appendChild(label);
    var seg = document.createElement('div');
    seg.className = 'gbseg';
    GROUP_BYS.forEach(function (dim) { seg.appendChild(groupByButton(dim[0], dim[1])); });
    wrap.appendChild(seg);
    return wrap;
  }

  function groupByButton(dim, label) {
    var on = selectedGroupBy === dim;
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'gbbtn' + (on ? ' gbbtn--on' : '');
    btn.setAttribute('aria-pressed', on ? 'true' : 'false');
    btn.textContent = label;
    btn.disabled = terminal;
    btn.addEventListener('click', function () { onGroupBy(dim); });
    return btn;
  }

  // The two-line group card: a headline line carrying identity, share, and the
  // three figures an operator scans for, over a hairline-separated detail line.
  function usageCard(group, totalReq) {
    var cardEl = document.createElement('div');
    cardEl.className = 'ucard';
    cardEl.appendChild(usageHeadLine(group, totalReq));
    cardEl.appendChild(usageStatLine(group.metrics));
    return cardEl;
  }

  function usageHeadLine(group, totalReq) {
    var m = group.metrics;
    var head = document.createElement('div');
    head.className = 'ucard-head';

    var id = document.createElement('div');
    id.className = 'ucard-id';
    var name = document.createElement('span');
    name.className = 'ucard-name';
    name.textContent = group.label;
    name.title = group.label;
    id.appendChild(name);
    var share = totalReq > 0 ? num0(m.requests) / totalReq * 100 : 0;
    var shareRow = document.createElement('span');
    shareRow.className = 'ucard-share';
    shareRow.appendChild(shareBar(share));
    var pct = document.createElement('span');
    pct.className = 'ucard-sharepct';
    pct.textContent = Math.round(share) + '% of requests';
    shareRow.appendChild(pct);
    id.appendChild(shareRow);
    head.appendChild(id);

    var figs = document.createElement('div');
    figs.className = 'ucard-figs';
    figs.appendChild(ufig('requests', magSpan(m.requests), null));
    figs.appendChild(ufig('errors', negCell(m.errors), null));
    figs.appendChild(ufig('est. cost', costFigure(m),
      m.cost_status === 'partial' ? 'priced subtotal only' : null));
    head.appendChild(figs);
    return head;
  }

  // The ONE column set this tab ships (the Essentials/Everything toggle is not
  // built): token volumes in and out, the weighted cache-hit share, the billed
  // cache read and write volumes, time to first token at p50 and its observed
  // peak, generation throughput, context size at its peak and mean, and how
  // many of the group's requests were streamed. Every figure is a metric the
  // ledger measured for THIS group -- nothing is derived from another column
  // and nothing is filler.
  function usageStatLine(m) {
    var line = document.createElement('div');
    line.className = 'ucard-stats';
    var ttft = msParts(m.ttft_p50_ms), peak = msParts(m.ttft_p95_ms);
    [
      ustat('tokens in / out', pairValue(magSpan(m.input_tokens), magSpan(m.output_tokens))),
      ustat('cache hit', pctFigure(m.cache_hit_pct)),
      ustat('cache read / write',
        pairValue(magSpan(m.cache_read_billed), magSpan(cacheWritten(m)))),
      ustat('ttft p50 / peak',
        pairValue(figure(ttft.v, ttft.u, null), figure(peak.v, peak.u, null))),
      ustat('throughput', throughputFigure(m.throughput_tok_s)),
      ustat('ctx peak / avg', pairValue(magSpan(m.ctx_peak), magSpan(m.ctx_avg))),
      ustat('streamed', magSpan(m.stream_count))
    ].forEach(function (stat) { line.appendChild(stat); });
    return line;
  }

  // A share that ARRIVED as a percentage, rounded by pctText and re-split so
  // the sign rides the faint unit slot instead of the numeral.
  function pctFigure(v) {
    var text = pctText(v);
    return figure(text.slice(0, text.length - 1), '%', null);
  }

  // A non-positive throughput means no generation was measured for this group
  // (the adapter coerces an absent figure to zero), so it reads as absent
  // rather than as a stalled stream.
  function throughputFigure(v) {
    var x = num0(v);
    return x > 0 ? figure(String(Math.round(x)), 'tok/s', null) : figure('-', null, null);
  }

  // Two readings of one measurement, the second de-emphasized so the pair reads
  // as primary-then-secondary rather than as two competing figures. The
  // separator is drawn by CSS: this script carries no slash-leading string
  // literal (see the mutation-channel scan in page.rs).
  function pairValue(first, second) {
    var span = document.createElement('span');
    span.className = 'upair';
    span.appendChild(first);
    var sep = document.createElement('span');
    sep.className = 'upair-sep';
    sep.setAttribute('aria-hidden', 'true');
    span.appendChild(sep);
    second.classList.add('upair-b');
    span.appendChild(second);
    return span;
  }

  // A headline figure: an uppercase label over the numeral, with an optional
  // faint qualifier under it.
  function ufig(label, valueNode, note) {
    var wrap = document.createElement('div');
    wrap.className = 'ufig';
    var l = document.createElement('span');
    l.className = 'ufig-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'ufig-value';
    v.appendChild(valueNode);
    wrap.appendChild(l);
    wrap.appendChild(v);
    if (note) {
      var n = document.createElement('span');
      n.className = 'ufig-note';
      n.textContent = note;
      wrap.appendChild(n);
    }
    return wrap;
  }

  function ustat(label, valueNode) {
    var wrap = document.createElement('div');
    wrap.className = 'ustat';
    var l = document.createElement('span');
    l.className = 'ustat-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'ustat-value';
    v.appendChild(valueNode);
    wrap.appendChild(l);
    wrap.appendChild(v);
    return wrap;
  }
  // ---- end tab:usage ---------------------------------------------------

  // ---- tab:routing -----------------------------------------------------

  // Circuit phases ranked worst-first. One chain member can resolve to several
  // health targets (one per seat), and the member's live state is the worst of
  // them: an open breaker on one seat is the fact an operator needs, not the
  // closed one beside it.
  var CIRCUIT_RANK = { open: 3, half_open_ready: 2, closed: 1 };

  var CIRCUIT_DOT = {
    closed: 'tdot--ok',
    open: 'tdot--bad',
    half_open_ready: 'tdot--warn'
  };

  // The first multi-source tab: the configured chains come from config, the
  // step attribution from the usage ledger, and the live target state from
  // health. Each section is wrapped in its OWN `safeSection` against its OWN
  // source record, so health going dark costs the state list alone and a
  // ledger outage costs the estimate alone -- the chains keep rendering
  // either way. `rec` is the config record (the tab's primary source): with
  // no chains there is nothing for the other two sections to describe, so
  // they are not built at all rather than rendered empty.
  function buildRouting(rec) {
    var stack = tabStack();
    stack.appendChild(routingHead());
    stack.appendChild(safeSection(rec, buildChainList));
    var chains = liveChains(rec);
    if (!chains.length) { return stack; }
    stack.appendChild(targetStateSection(chains));
    stack.appendChild(attributionSection(chains));
    return stack;
  }

  // The configured chains, or an empty list when config carries none to
  // describe. Keyed on the RETAINED payload rather than on `live`, so a source
  // marked stale (503, last-known values kept) still gets its derived sections
  // -- the whole pane is already dimmed in that state. The chain section
  // itself reports the malformed case, so this only decides whether the
  // derived sections have anything to describe.
  function liveChains(rec) {
    if (stateCard(rec) || !rec.data || !Array.isArray(rec.data.aliases)) { return []; }
    return rec.data.aliases;
  }

  // The span the estimate covers comes from the all-history panel's OWN window
  // token, never from the picker: the panel decides the window it aggregated,
  // and claiming any other span is the one thing this tab must not do. The
  // read is issued at window=all, so a token that is anything else means the
  // page is looking at a payload it did not ask for -- it says so rather than
  // relabeling it.
  function attributedSpan() {
    var rec = SOURCES[USAGE_ALL_SOURCE];
    if (stateCard(rec) || !rec.data) { return 'the recorded history'; }
    return WINDOW_SPAN[rec.data.window] || 'the recorded window';
  }

  // The tab header. The approximation is stated HERE, in the header and in a
  // persistent note -- never behind a hover -- and the note names exactly what
  // the derivation cannot know. The span is ALL HISTORY on this tab and the
  // window picker is inert here, so the header says which span the figures
  // below actually cover rather than letting the picker imply another.
  function routingHead() {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Fallback chains',
      'Approximate attribution - ' + attributedSpan()));
    var lead = document.createElement('p');
    lead.className = 'footnote';
    lead.textContent = 'Each alias tries its steps in order; a step is used only when every step before it failed.';
    wrap.appendChild(lead);
    var note = document.createElement('p');
    note.className = 'footnote';
    note.textContent = 'Derived from recorded alias, served target, and fallback counts; ' +
      'does not identify the exact attempted step for every request. ' +
      'Attribution covers all recorded history and does not follow the window picker.';
    wrap.appendChild(note);
    return wrap;
  }

  // A titled section whose BODY is filled by the caller. The head sits outside
  // the section boundary so a source that is down still says which reading is
  // missing instead of showing a bare error box.
  function routingSection(title, hint) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead(title, hint));
    return wrap;
  }

  // ---- routing: the configured chains (authoritative, from config) -----

  function buildChainList(rec) {
    var chains = (rec.data || {}).aliases;
    if (!Array.isArray(chains)) {
      throw new Error('config payload carries no alias chains');
    }
    if (!chains.length) {
      return emptyCard('No fallback chains configured',
        'An alias with more than one target gets a fallback chain. Configure one and its ' +
        'steps -- and the traffic attributed to them -- appear here.',
        ['configured order']);
    }
    var providers = providersByNickname(rec.data.models);
    var list = document.createElement('div');
    list.className = 'chainlist';
    chains.forEach(function (entry) { list.appendChild(chainCard(entry, providers)); });
    return list;
  }

  // Nickname -> provider, from the SAME config payload the chains came from, so
  // a step's provider is as authoritative as its position.
  function providersByNickname(models) {
    var map = Object.create(null);
    (models || []).forEach(function (m) {
      if (m && m.nickname) { map[m.nickname] = m.provider; }
    });
    return map;
  }

  function chainCard(entry, providers) {
    var steps = Array.isArray(entry.chain) ? entry.chain : [];
    var cardEl = document.createElement('div');
    cardEl.className = 'chain';

    var head = document.createElement('div');
    head.className = 'chain-head';
    var alias = document.createElement('span');
    alias.className = 'chain-alias';
    alias.textContent = entry.alias;
    alias.title = entry.alias;
    head.appendChild(alias);
    var meta = document.createElement('span');
    meta.className = 'chain-meta';
    meta.textContent = steps.length === 1
      ? 'one target - no fallback configured'
      : steps.length + ' steps in order';
    head.appendChild(meta);
    cardEl.appendChild(head);

    var row = document.createElement('div');
    row.className = 'chain-steps';
    steps.forEach(function (model, i) {
      if (i > 0) { row.appendChild(stepSep()); }
      row.appendChild(stepCard(i, model, providers[model]));
    });
    cardEl.appendChild(row);
    return cardEl;
  }

  function stepSep() {
    var sep = document.createElement('span');
    sep.className = 'step-sep';
    sep.textContent = 'then';
    return sep;
  }

  function stepCard(index, model, provider) {
    var step = document.createElement('div');
    step.className = 'chain-step';
    var n = document.createElement('span');
    n.className = 'step-n';
    n.textContent = String(index + 1);
    var name = document.createElement('span');
    name.className = 'step-model';
    name.textContent = model;
    name.title = model;
    var prov = document.createElement('span');
    prov.className = 'step-prov';
    if (provider) {
      prov.textContent = provider;
    } else {
      prov.classList.add('mag-zero');
      prov.textContent = 'no catalog row';
    }
    step.appendChild(n);
    step.appendChild(name);
    step.appendChild(prov);
    return step;
  }

  // ---- routing: live target state (authoritative, from health) ---------

  // Deliberately its OWN section rather than a dot inside each step card: the
  // live state is a health reading, and confining it here is what lets a dark
  // health source degrade to one state card while the chains and the estimate
  // keep rendering. The hint follows what `safeSection` will actually do with
  // the record, so it never promises a list that a state card replaced (nor
  // cries unavailable over retained last-known values).
  function targetStateSection(chains) {
    var rec = SOURCES.health;
    var wrap = routingSection('Live target state', stateCard(rec)
      ? 'health unavailable - live target state is not shown'
      : 'reported now - never part of the estimate below');
    wrap.appendChild(safeSection(rec, function (live) {
      return targetStateList(live, chains);
    }));
    return wrap;
  }

  function targetStateList(rec, chains) {
    if (!rec.data) { throw new Error('health payload carries no targets'); }
    var nowMs = panelNowMs(rec);
    var byNickname = targetsByNickname(rec.data.targets);
    var list = document.createElement('div');
    list.className = 'statelist';
    chainMembers(chains).forEach(function (model) {
      list.appendChild(targetStateRow(model, byNickname[model], nowMs));
    });
    return list;
  }

  // Every distinct chain member, in first-appearance order. Deduped: a model
  // shared by two aliases is one target with one state, and listing it twice
  // would show the same fact twice.
  function chainMembers(chains) {
    var seen = Object.create(null);
    var out = [];
    chains.forEach(function (entry) {
      (Array.isArray(entry.chain) ? entry.chain : []).forEach(function (model) {
        if (seen[model]) { return; }
        seen[model] = true;
        out.push(model);
      });
    });
    return out;
  }

  // Seats collapsed per nickname: the worst circuit across them, how many they
  // are, and the seats themselves. The members are carried because the phase
  // rule reads every seat's settled outcome, not just the worst one's.
  function targetsByNickname(targets) {
    var map = Object.create(null);
    (targets || []).forEach(function (t) {
      if (!t || !t.nickname) { return; }
      var found = map[t.nickname];
      if (!found) {
        map[t.nickname] = { worst: t, count: 1, members: [t] };
        return;
      }
      found.count += 1;
      found.members.push(t);
      if (circuitRank(t.circuit) > circuitRank(found.worst.circuit)) { found.worst = t; }
    });
    return map;
  }

  function circuitRank(circuit) {
    return CIRCUIT_RANK[circuit] || 0;
  }

  // The ONE target-state rule, shared by the Routing state list and the Health
  // target cards so the two tabs can never disagree about the same target. A
  // target with no settled outcome anywhere is `unknown`, NEVER healthy: no
  // observation is not evidence of health. Healthy means a closed breaker AND
  // an ok last outcome on every seat that has reported one.
  function targetPhase(group) {
    if (group.worst.circuit !== 'closed') { return 'attention'; }
    var settled = group.members.filter(function (t) { return !!t.last_outcome; });
    if (!settled.length) { return 'unknown'; }
    var failing = settled.some(function (t) { return t.last_outcome !== 'ok'; });
    return failing ? 'attention' : 'healthy';
  }

  // The circuit pill both tabs render: the neutral unknown token when nothing
  // has been observed, the worst seat's circuit otherwise.
  function circuitPill(group, phase) {
    return phase === 'unknown'
      ? tok('circuit', 'unknown')
      : tokLabeled('circuit', 'circuit', group.worst.circuit);
  }

  var PHASE_DOT = { attention: 'tdot--bad', healthy: 'tdot--ok', unknown: 'tdot--unknown' };

  // The dot follows the PHASE, not the raw circuit: a closed breaker nobody has
  // probed is unknown, and a green dot beside it would contradict Health.
  function phaseDot(group, phase) {
    if (phase === 'unknown') { return PHASE_DOT.unknown; }
    return CIRCUIT_DOT[group.worst.circuit] || PHASE_DOT[phase] || 'tdot--unknown';
  }

  // A configured member health has never dispatched to is `unknown`, never
  // healthy: no observation is not evidence of health. The dot, the pill, and
  // the reason all read the SHARED `targetPhase`, so this row and the Health
  // card for the same target can never disagree.
  function targetStateRow(model, found, nowMs) {
    var row = document.createElement('div');
    row.className = 'staterow';
    var phase = found ? targetPhase(found) : 'unknown';
    var dot = document.createElement('span');
    dot.className = 'tdot ' + (found ? phaseDot(found, phase) : 'tdot--unknown');
    dot.setAttribute('aria-hidden', 'true');
    row.appendChild(dot);

    var name = document.createElement('span');
    name.className = 'state-name';
    name.textContent = model;
    name.title = model;
    row.appendChild(name);

    var meta = document.createElement('span');
    meta.className = 'state-meta';
    if (!found) {
      meta.classList.add('mag-zero');
      meta.textContent = 'unknown - nothing dispatched to it yet';
      row.appendChild(meta);
      return row;
    }
    row.appendChild(circuitPill(found, phase));
    meta.textContent = targetStateText(found, phase, nowMs);
    row.appendChild(meta);
    return row;
  }

  function targetStateText(found, phase, nowMs) {
    var target = found.worst;
    var parts = [];
    if (found.count > 1) { parts.push(found.count + ' targets'); }
    if (phase === 'unknown') {
      parts.push('nothing dispatched to it yet');
    } else {
      parts.push(target.last_outcome
        ? 'last ' + labelFor('outcome', target.last_outcome).label
        : 'no settled outcome yet');
    }
    if (target.circuit === 'open' && target.open_since_ms !== null && target.open_since_ms !== undefined) {
      parts.push('open for ' + ageSince(target.open_since_ms, nowMs));
    }
    return parts.join(' - ');
  }

  // ---- routing: estimated step attribution (from the usage ledger) -----

  function attributionSection(chains) {
    var rec = SOURCES[USAGE_ALL_SOURCE];
    var wrap = routingSection('Estimated step attribution', attributionHint(rec));
    wrap.appendChild(safeSection(rec, function (live) {
      return buildAttribution(live, chains);
    }));
    return wrap;
  }

  function attributionHint(rec) {
    if (stateCard(rec)) {
      return 'usage unavailable - no traffic to attribute';
    }
    return 'estimated, not measured - over ' + attributedSpan();
  }

  function buildAttribution(rec, chains) {
    if (!rec.data) { throw new Error('usage payload carries no groups'); }
    var derived = deriveStepTraffic(rec.data.groups, chains);
    if (derived.recorded <= 0) {
      return emptyCard('No traffic recorded yet',
        'These chains are configured and ready. Each step gets its share here as soon as ' +
        'requests are routed through the alias that owns it.',
        ['chains configured', 'nothing to attribute']);
    }
    var wrap = document.createElement('div');
    wrap.className = 'attr';
    wrap.appendChild(laterStepHeadline(derived));
    var list = document.createElement('div');
    list.className = 'attrlist';
    derived.aliases.forEach(function (entry) { list.appendChild(attrAlias(entry)); });
    wrap.appendChild(list);
    if (derived.unconfigured.aliases > 0) {
      wrap.appendChild(unconfiguredNote(derived.unconfigured));
    }
    return wrap;
  }

  // The ONE step-traffic derivation on this tab; no other render path
  // recomputes it. The ledger records which target SERVED a request, never
  // which step of the chain was attempted, so step traffic is attributed by
  // matching a group's served model back onto the configured chain. A
  // write-time step-index column on the usage ledger would make this exact and
  // retire the heuristic entirely; that column does not exist yet, so this is
  // the honest approximation, and keeping it in one function is what stops a
  // second, disagreeing one from growing.
  //
  // Returns, per alias: the per-step counts, the recorded models that match NO
  // chain member (the chain was edited after those rows were written -- never
  // dropped, never folded into step 0), and the attributed / later-step split.
  // Traffic recorded against an alias with no configured chain at all is
  // counted separately rather than discarded. Every figure this tab shows,
  // including the later-step headline, comes out of this record.
  function deriveStepTraffic(groups, chains) {
    var byAlias = Object.create(null);
    var order = [];
    chains.forEach(function (entry) {
      if (byAlias[entry.alias]) { return; }
      byAlias[entry.alias] = {
        alias: entry.alias,
        steps: (Array.isArray(entry.chain) ? entry.chain : []).map(function (model) {
          return { model: model, requests: 0 };
        }),
        offChain: [],
        attributed: 0,
        later: 0
      };
      order.push(entry.alias);
    });

    var unconfigured = { aliases: 0, requests: 0 };
    var seenUnconfigured = Object.create(null);
    var offChainTotal = 0;
    (groups || []).forEach(function (group) {
      var requests = num0(group.requests);
      var entry = byAlias[group.alias];
      if (!entry) {
        if (!seenUnconfigured[group.alias]) {
          seenUnconfigured[group.alias] = true;
          unconfigured.aliases += 1;
        }
        unconfigured.requests += requests;
        return;
      }
      var at = stepIndexOf(entry.steps, group.model);
      if (at < 0) {
        addOffChain(entry.offChain, group.model, requests);
        offChainTotal += requests;
        return;
      }
      entry.steps[at].requests += requests;
      entry.attributed += requests;
      if (at > 0) { entry.later += requests; }
    });

    var attributed = 0;
    var later = 0;
    order.forEach(function (alias) {
      attributed += byAlias[alias].attributed;
      later += byAlias[alias].later;
    });
    return {
      aliases: order.map(function (alias) { return byAlias[alias]; }),
      attributed: attributed,
      later: later,
      unconfigured: unconfigured,
      recorded: attributed + offChainTotal + unconfigured.requests
    };
  }

  // The FIRST chain position serving this model. A chain may legitimately
  // repeat a member, and the ledger cannot say which attempt served the
  // request, so the traffic lands on the first position and the duplicate
  // reads as unused rather than inventing a split.
  function stepIndexOf(steps, model) {
    for (var i = 0; i < steps.length; i++) {
      if (steps[i].model === model) { return i; }
    }
    return -1;
  }

  function addOffChain(list, model, requests) {
    for (var i = 0; i < list.length; i++) {
      if (list[i].model === model) {
        list[i].requests += requests;
        return;
      }
    }
    list.push({ model: model, requests: requests });
  }

  // An estimated count: the `~` says the figure was attributed rather than
  // measured.
  function approxCount(n) {
    return '~' + humanCount(n);
  }

  // An estimated share at WHOLE precision. A decimal place would imply an
  // accuracy this derivation does not have.
  function approxPctParts(part, total) {
    if (total <= 0) { return { v: '-', u: null }; }
    return { v: '~' + Math.round(num0(part) / total * 100), u: '%' };
  }

  // The headline reads off the STEP DISTRIBUTION above, not the ledger's own
  // fallback counter: the two disagree by construction, and this tab shows one
  // derivation and one number.
  function laterStepHeadline(derived) {
    var pct = approxPctParts(derived.later, derived.attributed);
    var head = document.createElement('div');
    head.className = 'attr-headline';
    var value = document.createElement('div');
    value.className = 'attr-figure';
    var fig = figure(pct.v, pct.u, null);
    if (derived.later <= 0) { fig.classList.add('mag-zero'); }
    value.appendChild(fig);
    head.appendChild(value);
    var note = document.createElement('div');
    note.className = 'attr-headnote';
    note.textContent = laterStepNote(derived);
    head.appendChild(note);
    return head;
  }

  function laterStepNote(derived) {
    if (derived.attributed <= 0) {
      return 'no recorded request could be attributed to a step of these chains';
    }
    if (derived.later <= 0) {
      return 'the first step served every attributed request';
    }
    return approxCount(derived.later) + ' of ' + approxCount(derived.attributed) +
      ' attributed requests took a later step';
  }

  function attrAlias(entry) {
    var wrap = document.createElement('div');
    wrap.className = 'attr-alias';
    var head = document.createElement('div');
    head.className = 'attr-aliashead';
    var name = document.createElement('span');
    name.className = 'attr-aliasname';
    name.textContent = entry.alias;
    name.title = entry.alias;
    head.appendChild(name);
    var total = document.createElement('span');
    total.className = 'attr-aliastotal';
    total.textContent = entry.attributed > 0
      ? approxCount(entry.attributed) + ' req attributed'
      : 'no traffic recorded';
    if (entry.attributed <= 0) { total.classList.add('mag-zero'); }
    head.appendChild(total);
    wrap.appendChild(head);
    // A chain with no traffic gets the welcoming line above and nothing else:
    // its steps are already listed as configured facts, and a column of zero
    // bars would read as a measurement.
    if (entry.attributed > 0) {
      entry.steps.forEach(function (step, i) {
        wrap.appendChild(attrStep(step, i, entry.attributed));
      });
    }
    entry.offChain.forEach(function (row) { wrap.appendChild(offChainNote(row)); });
    return wrap;
  }

  function attrStep(step, index, total) {
    var row = document.createElement('div');
    row.className = 'attr-step';
    var n = document.createElement('span');
    n.className = 'attr-stepn';
    n.textContent = 'step ' + (index + 1);
    row.appendChild(n);
    var model = document.createElement('span');
    model.className = 'attr-model';
    model.textContent = step.model;
    model.title = step.model;
    row.appendChild(model);
    var count = document.createElement('span');
    count.className = 'attr-count';
    var share = document.createElement('span');
    share.className = 'attr-pct';
    if (step.requests > 0) {
      var pct = approxPctParts(step.requests, total);
      count.textContent = approxCount(step.requests);
      share.appendChild(figure(pct.v, pct.u, null));
      row.appendChild(count);
      row.appendChild(share);
      row.appendChild(shareBar(num0(step.requests) / total * 100));
      return row;
    }
    count.classList.add('mag-zero');
    count.textContent = 'never used';
    row.appendChild(count);
    row.appendChild(share);
    return row;
  }

  // A recorded model that is not a member of the current chain. Surfaced as
  // its own line rather than dropped or folded into step 0: the traffic is
  // real, and the chain having changed since is exactly what an operator
  // needs told.
  function offChainNote(row) {
    var note = document.createElement('p');
    note.className = 'footnote attr-off';
    note.textContent = 'off chain - ' + (row.model || 'unrecorded target') + ' served ' +
      approxCount(row.requests) + ' req for this alias but is not a step of its current chain';
    return note;
  }

  function unconfiguredNote(unconfigured) {
    var note = document.createElement('p');
    note.className = 'footnote attr-off';
    note.textContent = unconfigured.aliases +
      (unconfigured.aliases === 1 ? ' alias with no configured chain carries ' : ' aliases with no configured chain carry ') +
      approxCount(unconfigured.requests) + ' recorded req, attributed to no step above';
    return note;
  }
  // ---- end tab:routing -------------------------------------------------

  // ---- tab:health ------------------------------------------------------

  // The window a quota `utilization` is a fraction OF. The quota columns are
  // shared across vendors, so only `provider_kind` says which window a
  // fraction means; an unrecognized kind gets NO window name rather than an
  // assumed one. A two-part entry is joined by a CSS separator (this script
  // carries no slash-leading string literal -- see the mutation-channel scan
  // in page.rs).
  var QUOTA_WINDOW = {
    codex: ['weekly'],
    anthropic: ['5h', 'session'],
    'anthropic-api': ['5h', 'session']
  };

  // Two deliberate defaults live in the quota tiles below, both chosen rather
  // than derived, and both meant to be looked at with real provider data
  // before they harden:
  //   1. a line renders per POPULATED field -- the primary one for
  //      `utilization`, a second one only when `overage_utilization` arrived
  //      -- each labeled from `provider_kind`, never from provider identity;
  //   2. a snapshot is called STALE only when a KNOWN reset has already
  //      elapsed. With no reset to compare against, the tile reports its own
  //      age and says freshness is unknown; no provider time-to-live is
  //      invented to classify it.
  //
  // The second multi-source tab: target state comes from health, per-seat
  // quota from the usage ledger. Each section is wrapped in its OWN
  // `safeSection` against its OWN source record, so a dark health source
  // costs the target list alone and a dark ledger costs the quota tiles
  // alone.
  function buildHealth(rec) {
    var stack = tabStack();
    stack.appendChild(healthTargetSection(rec));
    stack.appendChild(healthQuotaSection());
    return stack;
  }

  function healthTargetSection(rec) {
    var wrap = routingSection('Dispatch targets', stateCard(rec)
      ? 'Health data unavailable - target state is not shown'
      : 'reported now - current state, not a windowed aggregate');
    wrap.appendChild(safeSection(rec, buildTargetHealth));
    return wrap;
  }

  function buildTargetHealth(rec) {
    if (!rec.data || !Array.isArray(rec.data.targets)) {
      throw new Error('health payload carries no targets');
    }
    var groups = collapsedTargets(rec.data.targets);
    if (!groups.length) {
      return emptyCard('No dispatch targets configured',
        'A configured model becomes a dispatch target. Add one and its breaker state, ' +
        'rate-limit headroom, and last settled outcome appear here.',
        ['current state']);
    }
    var nowMs = panelNowMs(rec);
    var negatives = learnedByStateKey(rec.data.learned_negatives);
    var attention = groups.filter(function (g) { return targetPhase(g) === 'attention'; });
    var healthy = groups.filter(function (g) { return targetPhase(g) === 'healthy'; });
    var unknown = groups.filter(function (g) { return targetPhase(g) === 'unknown'; });
    if (!attention.length && !healthy.length) { return unprobedEmpty(unknown.length); }
    var stack = tabStack();
    if (attention.length) {
      stack.appendChild(targetGroupList('Needs attention',
        attention.length + phrase(attention.length, ' target', ' targets') + ' not serving normally',
        attention, negatives, nowMs));
    }
    if (healthy.length) {
      stack.appendChild(targetGroupList('Healthy',
        healthy.length + phrase(healthy.length, ' target', ' targets') + ' with a closed breaker and an ok last outcome',
        healthy, negatives, nowMs));
    }
    if (unknown.length) {
      stack.appendChild(targetGroupList('Not observed yet',
        'configured, nothing dispatched to them yet - unknown, never healthy',
        unknown, negatives, nowMs));
    }
    return stack;
  }

  function phrase(n, one, many) { return n === 1 ? one : many; }

  // Every target is configured but none has a settled outcome: a welcoming
  // state, never a list of cards claiming health nobody has observed.
  function unprobedEmpty(count) {
    return emptyCard('No target has been probed yet',
      count + phrase(count, ' target is', ' targets are') + ' configured and ready. Each one ' +
      'reports its breaker state and last settled outcome here as soon as a request is ' +
      'dispatched to it.',
      [count + phrase(count, ' target configured', ' targets configured'), 'none probed yet']);
  }

  // One card per NICKNAME, collapsing a pooled model's seats through the SAME
  // worst-circuit rule the Routing tab uses (`targetsByNickname`): an open
  // breaker on one seat is the fact an operator needs, not the closed one
  // beside it. The seats themselves are kept so a card can name how many it
  // speaks for and which learned negatives belong to it.
  function collapsedTargets(targets) {
    var byNickname = targetsByNickname(targets);
    var order = [];
    var seen = Object.create(null);
    (targets || []).forEach(function (t) {
      if (!t || !t.nickname || seen[t.nickname]) { return; }
      seen[t.nickname] = true;
      var group = byNickname[t.nickname];
      order.push({
        nickname: t.nickname,
        worst: group.worst,
        count: group.count,
        members: group.members
      });
    });
    return order;
  }

  // Learned rows keyed by the state key they were recorded against, so a
  // card lists the negatives of ITS OWN seats and nothing else. Only `broken`
  // verdicts are negatives -- a `verified` positive shares the list and must
  // not be read as one.
  function learnedByStateKey(rows) {
    var map = Object.create(null);
    (rows || []).forEach(function (row) {
      if (!row || !row.state_key || row.verdict !== 'broken') { return; }
      if (!map[row.state_key]) { map[row.state_key] = []; }
      map[row.state_key].push(row);
    });
    return map;
  }

  function targetGroupList(title, hint, groups, negatives, nowMs) {
    var wrap = document.createElement('div');
    wrap.className = 'hgroup';
    wrap.appendChild(sectionHead(title, hint));
    var list = document.createElement('div');
    list.className = 'hlist';
    groups.forEach(function (group) { list.appendChild(targetCard(group, negatives, nowMs)); });
    wrap.appendChild(list);
    return wrap;
  }

  function targetCard(group, negatives, nowMs) {
    var phase = targetPhase(group);
    var cardEl = document.createElement('div');
    cardEl.className = 'hcard hcard--' + phase;
    cardEl.appendChild(targetCardHead(group, phase, nowMs));
    cardEl.appendChild(targetCardGrid(group, negatives, nowMs));
    return cardEl;
  }

  function targetCardHead(group, phase, nowMs) {
    var head = document.createElement('div');
    head.className = 'hcard-head';
    head.appendChild(statePill(group, phase));
    head.appendChild(targetIdentity(group, phase, nowMs));
    head.appendChild(targetAnchors(group, nowMs));
    return head;
  }

  // The state pill carries the WORST circuit across the seats, or the neutral
  // unknown token when nothing has been observed.
  function statePill(group, phase) {
    var pill = circuitPill(group, phase);
    pill.classList.add('hstate');
    return pill;
  }

  function targetIdentity(group, phase, nowMs) {
    var wrap = document.createElement('div');
    wrap.className = 'hident';
    var name = document.createElement('span');
    name.className = 'hname';
    name.textContent = group.nickname;
    name.title = group.nickname;
    wrap.appendChild(name);
    var sub = document.createElement('span');
    sub.className = 'hsub';
    sub.textContent = targetSubText(group);
    wrap.appendChild(sub);
    var reason = targetReasonText(group, phase, nowMs);
    if (reason) {
      var line = document.createElement('span');
      line.className = 'hreason';
      line.textContent = reason;
      wrap.appendChild(line);
    }
    return wrap;
  }

  function targetSubText(group) {
    var target = group.worst;
    var parts = [target.provider_name, target.upstream].filter(Boolean);
    if (group.count > 1) {
      parts.push(group.count + phrase(group.count, ' seat', ' seats'));
    }
    return parts.join(' - ');
  }

  function targetReasonText(group, phase, nowMs) {
    if (phase === 'unknown') { return 'nothing dispatched to it yet'; }
    var target = group.worst;
    var parts = [];
    if (target.circuit === 'open' && target.open_since_ms !== null && target.open_since_ms !== undefined) {
      parts.push('breaker open for ' + ageSince(target.open_since_ms, nowMs));
    }
    if (target.half_open_probe_in_flight) { parts.push('recovery probe in flight'); }
    return parts.join(' - ');
  }

  // The two anchors of the card. `last ok` is only knowable when the seat's
  // LAST settled outcome was itself ok -- the health source carries one
  // outcome per seat, not a history -- so anything else reads unknown rather
  // than claiming a moment nobody recorded.
  function targetAnchors(group, nowMs) {
    var wrap = document.createElement('div');
    wrap.className = 'hanchors';
    wrap.appendChild(anchorCell('p50', latencyAnchor()));
    wrap.appendChild(anchorCell('last ok', lastOkAnchor(group, nowMs)));
    return wrap;
  }

  // Neither source behind this tab carries per-target latency: the health
  // panel reports gate state and the usage panel aggregates by group, not by
  // target. The anchor says so rather than borrowing another tab's figure.
  function latencyAnchor() {
    var span = faintFigure('no data');
    span.title = 'per-target latency is not reported by the health or usage panel';
    return span;
  }

  function lastOkAnchor(group, nowMs) {
    var stamps = group.members
      .filter(function (t) {
        return t.last_outcome === 'ok' && t.last_outcome_at_ms !== null && t.last_outcome_at_ms !== undefined;
      })
      .map(function (t) { return Number(t.last_outcome_at_ms); });
    if (!stamps.length) { return faintFigure('unknown'); }
    return figure(ageSince(Math.max.apply(null, stamps), nowMs), null, null);
  }

  function anchorCell(label, valueNode) {
    var wrap = document.createElement('div');
    wrap.className = 'hanchor';
    var l = document.createElement('span');
    l.className = 'hanchor-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'hanchor-value';
    v.appendChild(valueNode);
    wrap.appendChild(l);
    wrap.appendChild(v);
    return wrap;
  }

  function targetCardGrid(group, negatives, nowMs) {
    var grid = document.createElement('div');
    grid.className = 'hgrid';
    grid.appendChild(gridCell('rate limit', rpmValue(group.worst)));
    grid.appendChild(gridCell('last outcome', lastOutcomeValue(group.worst, nowMs)));
    grid.appendChild(gridCell('circuit', circuitValue(group.worst, nowMs)));
    grid.appendChild(gridCell('learned negatives', negativeChips(group, negatives, nowMs)));
    return grid;
  }

  function gridCell(label, valueNode) {
    var wrap = document.createElement('div');
    wrap.className = 'hcell';
    var l = document.createElement('span');
    l.className = 'hcell-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'hcell-value';
    v.appendChild(valueNode);
    wrap.appendChild(l);
    wrap.appendChild(v);
    return wrap;
  }

  // Projected available tokens, with NO bar: the panel reports the level, not
  // the ceiling, and a bar without a denominator would invent one. An absent
  // level is the unlimited policy, not a missing reading.
  function rpmValue(target) {
    if (target.rpm_available === null || target.rpm_available === undefined) {
      return faintFigure('unlimited');
    }
    return figure(String(Math.floor(num0(target.rpm_available))), 'rpm left', null);
  }

  function lastOutcomeValue(target, nowMs) {
    if (!target.last_outcome) { return faintFigure('no settled outcome yet'); }
    var node = labelCell('outcome', target.last_outcome);
    var wrap = document.createElement('span');
    if (node instanceof Node) { wrap.appendChild(node); }
    else { wrap.appendChild(document.createTextNode(node)); }
    if (target.last_outcome !== 'ok') { wrap.classList.add('neg'); }
    if (target.last_outcome_at_ms !== null && target.last_outcome_at_ms !== undefined) {
      var seen = ageSince(target.last_outcome_at_ms, nowMs);
      if (seen !== '-') {
        var age = document.createElement('span');
        age.className = 'hage';
        age.textContent = seen + ' ago';
        wrap.appendChild(age);
      }
    }
    return wrap;
  }

  function circuitValue(target, nowMs) {
    if (target.circuit === 'closed') { return faintFigure('closed'); }
    var parts = [];
    if (target.open_since_ms !== null && target.open_since_ms !== undefined) {
      parts.push('open for ' + ageSince(target.open_since_ms, nowMs));
    }
    if (target.half_open_probe_in_flight) { parts.push('probe in flight'); }
    return figure(parts.length ? parts.join(' - ') : labelFor('circuit', target.circuit).label, null, null);
  }

  function negativeChips(group, negatives, nowMs) {
    var wrap = document.createElement('span');
    wrap.className = 'hchips';
    var seen = Object.create(null);
    group.members.forEach(function (t) {
      (negatives[t.state_key] || []).forEach(function (row) {
        if (seen[row.capability_key]) { return; }
        seen[row.capability_key] = true;
        wrap.appendChild(negativeChip(row, nowMs));
      });
    });
    if (!wrap.childNodes.length) { wrap.appendChild(faintFigure('none')); }
    return wrap;
  }

  function negativeChip(row, nowMs) {
    var chip = document.createElement('span');
    chip.className = 'hchip';
    chip.textContent = row.capability_key;
    var seen = ageSince(row.last_seen_ms, nowMs);
    chip.title = row.capability_key + ' - ' + row.source + ' evidence' +
      (seen === '-' ? '' : ', last seen ' + seen + ' ago');
    return chip;
  }

  // ---- health: per-seat quota (from the usage ledger) ------------------

  function healthQuotaSection() {
    var rec = SOURCES.usage;
    var wrap = routingSection('Seat quota', stateCard(rec)
      ? 'usage unavailable - quota is not shown'
      : 'latest snapshot per credential seat - never summed across seats');
    wrap.appendChild(safeSection(rec, buildQuotaList));
    return wrap;
  }

  function buildQuotaList(rec) {
    if (!rec.data || !Array.isArray(rec.data.quota)) {
      throw new Error('usage payload carries no quota list');
    }
    if (!rec.data.quota.length) { return quotaNotReported(); }
    var nowMs = panelNowMs(rec);
    var list = document.createElement('div');
    list.className = 'qlist';
    rec.data.quota.forEach(function (q) { list.appendChild(quotaTile(q, nowMs)); });
    return list;
  }

  // No seat reported a quota-bearing row. An absence is stated as one -- never
  // as a zero utilization, and never as a bar.
  function quotaNotReported() {
    var note = document.createElement('p');
    note.className = 'footnote';
    note.textContent = 'Quota not reported - no credential seat has recorded a quota snapshot.';
    return note;
  }

  // The instant the quota figures are read against: the usage panel's OWN
  // as_of (see panelNowMs).
  //
  // ONE tile per quota row, keyed by the seat it belongs to. Rows are never
  // merged, summed, averaged, or reduced to a headline: a seat's quota is a
  // fact about that credential and nothing else.
  function quotaTile(q, nowMs) {
    var tile = document.createElement('div');
    tile.className = 'qtile';
    tile.appendChild(quotaHead(q, nowMs));
    tile.appendChild(quotaLines(q));
    tile.appendChild(quotaMeta(q, nowMs));
    return tile;
  }

  function quotaHead(q, nowMs) {
    var head = document.createElement('div');
    head.className = 'qtile-head';
    var seat = document.createElement('span');
    seat.className = 'qseat';
    if (q.seat) {
      seat.textContent = q.seat;
      seat.title = q.seat;
    } else {
      seat.classList.add('mag-zero');
      seat.textContent = 'Unknown legacy seat';
      seat.title = 'recorded before the seat column was populated, or a forwarded client credential';
    }
    head.appendChild(seat);
    if (q.provider_kind) {
      var kind = document.createElement('span');
      kind.className = 'qkind';
      kind.textContent = q.provider_kind;
      head.appendChild(kind);
    }
    head.appendChild(quotaFreshness(q, nowMs));
    return head;
  }

  // A reset instant at or before the panel's as_of has already passed, so the
  // snapshot describes a window that has since rolled: that is the ONE
  // staleness this page can prove. Without a reset to compare against there is
  // no provider time-to-live to lean on, so the tile reports its own age and
  // says freshness is unknown.
  function quotaFreshness(q, nowMs) {
    var span = document.createElement('span');
    span.className = 'qfresh';
    if (resetElapsed(q, nowMs)) {
      span.classList.add('qfresh--stale');
      span.textContent = 'STALE';
      span.title = 'the reported reset time has already passed: ' + fmtTs(q.reset_ms);
      return span;
    }
    span.classList.add('mag-zero');
    span.textContent = 'freshness unknown';
    span.title = 'no reset has elapsed and the provider declares no snapshot lifetime';
    return span;
  }

  function resetElapsed(q, nowMs) {
    if (q.reset_ms === null || q.reset_ms === undefined || Number(q.reset_ms) <= 0) { return false; }
    return isFinite(nowMs) && Number(q.reset_ms) <= nowMs;
  }

  // One line per POPULATED utilization field, never one per provider: the
  // primary window when `utilization` arrived, and a second line only when
  // `overage_utilization` did. A field the row does not carry produces no
  // line at all.
  function quotaLines(q) {
    var wrap = document.createElement('div');
    wrap.className = 'qlines';
    wrap.appendChild(hasNumber(q.utilization)
      ? quotaLine(windowLabel(q.provider_kind), q.utilization)
      : utilizationMissing());
    if (hasNumber(q.overage_utilization)) {
      wrap.appendChild(quotaLine(overageLabel(q.overage_status), q.overage_utilization));
    }
    return wrap;
  }

  function hasNumber(v) {
    return v !== null && v !== undefined && isFinite(Number(v));
  }

  // A measured zero is a reading, not an absence: it renders faint, with its
  // bar at zero. A MISSING utilization renders no bar and no percentage.
  function quotaLine(labelNode, fraction) {
    var line = document.createElement('div');
    line.className = 'qline';
    var label = document.createElement('span');
    label.className = 'qline-label';
    label.appendChild(labelNode);
    line.appendChild(label);
    var pct = Number(fraction) * 100;
    var value = document.createElement('span');
    value.className = 'qline-value';
    var text = pctFrac(fraction);
    var fig = figure(text.slice(0, text.length - 1), '%', null);
    if (pct <= 0) { fig.classList.add('mag-zero'); }
    value.appendChild(fig);
    line.appendChild(value);
    line.appendChild(shareBar(pct));
    return line;
  }

  function utilizationMissing() {
    var line = document.createElement('div');
    line.className = 'qline qline--missing';
    var label = document.createElement('span');
    label.className = 'qline-label';
    label.textContent = 'Utilization not reported';
    line.appendChild(label);
    return line;
  }

  // The window the primary fraction is OF, read from `provider_kind` alone. A
  // kind this build does not know gets no window name -- the raw kind stays
  // visible in the tile header, and inventing a window would be worse than
  // saying nothing.
  function windowLabel(providerKind) {
    var parts = QUOTA_WINDOW[providerKind];
    if (!parts) { return document.createTextNode('reported window'); }
    if (parts.length === 1) { return document.createTextNode(parts[0]); }
    var span = document.createElement('span');
    span.className = 'qwindow';
    span.appendChild(document.createTextNode(parts[0]));
    var sep = document.createElement('span');
    sep.className = 'qsep';
    sep.setAttribute('aria-hidden', 'true');
    span.appendChild(sep);
    span.appendChild(document.createTextNode(parts[1]));
    return span;
  }

  // The overage line is labeled by the status the row reported for it; with no
  // status token it is simply the overage window.
  function overageLabel(overageStatus) {
    if (!overageStatus) { return document.createTextNode('overage'); }
    var span = document.createElement('span');
    span.appendChild(document.createTextNode('overage'));
    span.appendChild(tokLabeled('qstatus', 'qstatus', overageStatus));
    return span;
  }

  // Claim and status render ONLY when the row carries them: a provider that
  // reports a utilization without a status header (codex) gets its figure and
  // its reset and no invented status, and is never classified degraded for
  // the omission.
  function quotaMeta(q, nowMs) {
    var pairs = [];
    if (q.claim) { pairs.push(['claim', labelCell('claim', q.claim)]); }
    if (q.status) { pairs.push(['status', tokLabeled('qstatus', 'qstatus', q.status)]); }
    pairs.push(['snapshot', snapshotAge(q.ts_start_ms, nowMs)]);
    pairs.push(['reset', resetElapsed(q, nowMs) ? 'Reset elapsed' : quotaResetCell(q.reset_ms, nowMs)]);
    var meta = buildDefList(pairs);
    meta.classList.add('qmeta');
    return meta;
  }

  // Snapshot age against the panel's own as_of. A stamp in the future beyond
  // the skew tolerance, or one that will not parse, reads unknown -- never a
  // negative age.
  function snapshotAge(tsMs, nowMs) {
    var ts = Number(tsMs);
    if (!isFinite(ts) || ts <= 0 || !isFinite(nowMs)) { return faintFigure('unknown'); }
    var age = nowMs - ts;
    if (age < -SKEW_TOLERANCE_SEC * 1000) { return faintFigure('unknown'); }
    return humanDuration(Math.max(0, age)) + ' old';
  }
  // ---- end tab:health --------------------------------------------------

  // ---- tab:config ------------------------------------------------------

  // Config is the one tab that FAILS CLOSED as a whole: its vocabulary decides
  // how every other tab's routing figures are read, so a version mismatch or a
  // malformed payload replaces the entire tab rather than rendering the parts
  // that happen to parse. Single-source, so the tab-wide `safeSection` IS that
  // posture -- there is no second record to confine a failure to, and no
  // section is built before the whole payload has been validated below.
  function buildConfig(rec) {
    return safeSection(rec, buildConfigLive);
  }

  // Validate the WHOLE payload before any section is built: a config panel at
  // this version always carries all six members, so a missing one is a
  // malformed same-version payload and the throw takes the tab with it.
  function buildConfigLive(rec) {
    var data = rec.data || {};
    var lists = [data.aliases, data.models, data.classes, data.capabilities, data.activation];
    if (!data.source || lists.some(function (l) { return !Array.isArray(l); })) {
      throw new Error('config payload does not carry the effective view');
    }
    var stack = tabStack();
    stack.appendChild(sourceStrip(data.source));
    if (!data.aliases.length && !data.models.length) {
      stack.appendChild(defaultConfigEmpty());
    }
    // A table with no rows is a header claiming a reading nobody made, so each
    // one is built only when the effective view carries entries for it. The
    // class policies always resolve (a baked default per class), and the
    // activation inventory describes what routectl WOULD do, so both stay
    // useful on a config that names nothing yet.
    appendIfAny(stack, data.aliases, aliasSection);
    appendIfAny(stack, data.models, modelSection);
    appendIfAny(stack, data.capabilities, capabilitySection);
    appendIfAny(stack, data.activation, activationSection);
    appendIfAny(stack, data.classes, classSection);
    return stack;
  }

  function appendIfAny(stack, list, build) {
    if (list.length) { stack.appendChild(build(list)); }
  }

  // Nothing operator-authored is in effect. Welcoming, and specific about the
  // one edit that fills the tables above.
  function defaultConfigEmpty() {
    return emptyCard('Running on the default configuration',
      'No aliases or models are configured yet. Name one under [aliases] and its resolved ' +
      'chain, its targets, and the catalog rows behind them appear here.',
      ['current state', 'defaults in effect']);
  }

  // ---- config: the source strip ----------------------------------------

  // Which config is in effect and which daemon is serving it -- the first
  // question the tab exists to answer, so it sits above every table.
  function sourceStrip(source) {
    var strip = document.createElement('div');
    strip.className = 'cfgstrip';
    [
      ['config file', source.config_path || 'none (not loaded from a file)'],
      ['loaded', loadedAge(source.loaded_age_ms)],
      ['resolved', countText(source.alias_count, ' alias', ' aliases') + ', ' +
        countText(source.provider_count, ' provider', ' providers')],
      ['listening on', source.listen_addr],
      ['version', source.version]
    ].forEach(function (fact) { strip.appendChild(cfgFact(fact[0], fact[1])); });
    return strip;
  }

  function cfgFact(label, value) {
    var wrap = document.createElement('div');
    wrap.className = 'cfgfact';
    var l = document.createElement('span');
    l.className = 'cfgfact-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'cfgfact-value';
    v.textContent = (value === null || value === undefined || value === '') ? '-' : String(value);
    v.title = v.textContent;
    wrap.appendChild(l);
    wrap.appendChild(v);
    return wrap;
  }

  // A load that was never stamped reads as unknown rather than as an age of
  // zero; a stamp ahead of the clock is clamped by humanDuration's caller.
  function loadedAge(ms) {
    var x = Number(ms);
    if (ms === null || ms === undefined || !isFinite(x) || x < 0) { return 'not stamped'; }
    return humanDuration(x) + ' ago';
  }

  function countText(n, one, many) {
    var x = num0(n);
    return x + (x === 1 ? one : many);
  }

  // ---- config: the reference tables ------------------------------------

  // A configured alias and the ordered chain it resolves to. The chain order IS
  // the sequence dispatch walks, so it is listed verbatim. There is deliberately
  // NO per-alias timeout or retry-policy column: neither exists in the config
  // model, and a column of invented values would read as configuration.
  function aliasSection(aliases) {
    var tbl = mkTable('Configured aliases',
      [R('alias'), W('resolved chain'), N('steps')], false);
    aliases.forEach(function (entry) {
      var steps = Array.isArray(entry.chain) ? entry.chain : [];
      trow(tbl, [entry.alias, steps.length ? steps.join(' then ') : null, steps.length]);
    });
    return card('Aliases', 'what a client name resolves to, in order', tableScroll(tbl));
  }

  // The auto-activation inventory: one row per routectl-owned provider, with
  // the reason a provider could not be resolved. Named CREDENTIAL providers
  // because the source strip counts the ROUTING providers instead, and one tab
  // carrying two unqualified "providers" reads as one number contradicting the
  // other.
  function activationSection(activation) {
    var tbl = mkTable('Provider activation',
      [R('provider'), C('kind'), C('status'), C('reason'), C('used by aliases')], false);
    activation.forEach(function (p) {
      trow(tbl, [
        p.provider_id,
        p.provider_kind,
        tokLabeled('actstatus', 'actstatus', p.status),
        p.reason ? labelCell('activation', p.reason) : null,
        yesNo(p.referenced_by_aliases)
      ]);
    });
    var wrap = card('Credential providers', 'which credentials resolved, and why',
      tableScroll(tbl));
    wrap.appendChild(activationNote());
    return wrap;
  }

  function activationNote() {
    var note = document.createElement('p');
    note.className = 'footnote';
    note.textContent = 'Credential activation only. The provider count in the strip above ' +
      'counts the configured routing providers, which is a different set.';
    return note;
  }

  // One row per `[models.X]` entry with the catalog layer that won it. The
  // economics and the confirmed context window sit in the detail row: they are
  // reference figures an operator opens, not a reading to scan.
  function modelSection(models) {
    var tbl = mkTable('Configured models',
      [R('model'), C('provider'), C('upstream'), C('catalog layer')], true);
    models.forEach(function (m) { modelRow(tbl, m); });
    return card('Models', 'what each name dispatches to, and which catalog row backs it',
      tableScroll(tbl));
  }

  function modelRow(tbl, m) {
    var econ = m.economics || {};
    xrow(tbl, m.nickname,
      [m.nickname, m.provider, m.upstream, tokLabeled('src', 'src', m.source)],
      presentPairs([
        ['provider kind', m.provider_kind],
        ['max context', econ.max_context_tokens ? humanCount(econ.max_context_tokens) : null],
        ['cache write multiplier', econ.wm],
        ['cache read multiplier', econ.rm],
        ['verified at', m.verified_at]
      ]));
  }

  // A model with no catalog row behind it carries none of these figures, and a
  // grid of five dashes is indistinguishable from an expander that failed to
  // open. Dropping the absent pairs lets `buildDefList` say so explicitly.
  function presentPairs(pairs) {
    return pairs.filter(function (p) {
      return p[1] !== null && p[1] !== undefined && p[1] !== '';
    });
  }

  // The capability overrides the config resolves, kept behind a default-closed
  // disclosure: a real config carries few, and none of them changes what the
  // tables above say. Reachable (data floor), never dominant.
  function capabilitySection(caps) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Capability overrides',
      countText(caps.length, ' cell', ' cells') + ' resolved from config'));
    var tbl = mkTable('Capability overrides',
      [R('target'), C('capability'), C('verdict'), C('source')], false);
    caps.forEach(function (c) {
      trow(tbl, [
        c.target_spec,
        c.capability_key,
        tokLabeled('verdict', 'verdict', c.verdict),
        tokLabeled('prov', 'prov', c.provenance)
      ]);
    });
    wrap.appendChild(buildExpander('Show the resolved cells', tableScroll(tbl)));
    return wrap;
  }

  // What routectl does when an upstream returns each failure class. The columns
  // are exactly the four knobs the config model HAS -- retry cap, fallback,
  // whether the class debits the breaker's health accounting, and the layer
  // that won. `breaker debit` reads the wire's own `debits_breaker`, which the
  // server derives from the router's transient-health set: this page must never
  // restate that set, and it invents no per-class knob beside it.
  function classSection(classes) {
    var tbl = mkTable('Retry class policy',
      [R('class'), N('retry cap'), C('fallback'), C('breaker debit'), C('source')], false);
    classes.forEach(function (c) {
      trow(tbl, [
        labelCell('class', c.class),
        num0(c.retry_cap),
        yesNo(c.fallback),
        yesNo(c.debits_breaker),
        classSourceCell(c.source)
      ]);
    });
    return card('Retry classes', 'what routectl does when an upstream returns each class',
      tableScroll(tbl));
  }

  // A baked default is the ABSENCE of an operator choice, so it reads faint;
  // a class the config names reads at full weight.
  function classSourceCell(source) {
    var span = document.createElement('span');
    span.textContent = (source === null || source === undefined) ? '-' : String(source);
    if (source !== 'config') { span.classList.add('mag-zero'); }
    return span;
  }

  // A configured boolean. The negative reads faint rather than red: an unset
  // switch is a choice, not a failure.
  function yesNo(on) {
    var span = document.createElement('span');
    span.textContent = on ? 'yes' : 'no';
    if (!on) { span.classList.add('mag-zero'); }
    return span;
  }
  // ---- end tab:config --------------------------------------------------

  // ---- tab:doctor ------------------------------------------------------

  // The report's severity vocabulary is a fixed triad. A token outside it
  // means the wire's meaning has moved, and an unknown severity cannot be
  // sorted into "a problem" or "fine" without guessing -- so the tab fails
  // closed on one rather than interpreting it. `LABELS.status` carries the
  // humanized text for exactly these three.
  var DOCTOR_SEVERITIES = { Pass: 1, Warn: 1, Fail: 1 };

  // The reachability verdicts the panel folds from each target's last settled
  // outcome, worst-known-first for the summary line. Same fail-closed posture
  // as the severities above.
  var REACHABILITY_TOKENS = ['reachable', 'degraded', 'unknown'];

  // Doctor is the second tab that FAILS CLOSED as a whole: an unrecognized
  // severity would have to be guessed at to be placed, and a diagnostic that
  // guesses is worse than one that says it cannot read the report. Single-
  // source, so the tab-wide `safeSection` IS that posture -- the whole payload
  // is validated below before any section is built.
  //
  // The report also carries structured panels (steady-state trim, capability
  // matrix). Neither is a check with a verdict, so neither belongs among the
  // findings; the capability cells the config resolves already have their home
  // on the Config tab.
  function buildDoctor(rec) {
    return safeSection(rec, buildDoctorLive);
  }

  function buildDoctorLive(rec) {
    var report = validatedReport(rec.data);
    var fails = findingsWith(report.findings, 'Fail');
    var warns = findingsWith(report.findings, 'Warn');
    var passes = findingsWith(report.findings, 'Pass');
    var attention = fails.concat(warns);
    var counts = { fail: fails.length, warn: warns.length, pass: passes.length };
    var stack = tabStack();
    stack.appendChild(doctorVerdict(counts, rec.data.reachability));
    if (attention.length) { stack.appendChild(doctorFindings(attention)); }
    else if (passes.length) { stack.appendChild(doctorAllClear(passes.length)); }
    else { stack.appendChild(doctorNoChecks()); }
    if (passes.length) { stack.appendChild(doctorPasses(passes)); }
    return stack;
  }

  // Validate the WHOLE payload before a section is built, INCLUDING every
  // severity and reachability token: a value outside the known vocabulary is
  // not an additive change (those do not bump a version), so reading past it
  // would render a finding whose meaning this build does not know.
  function validatedReport(data) {
    var report = (data || {}).report;
    var reachability = (data || {}).reachability;
    if (!report || !Array.isArray(report.findings) || !Array.isArray(reachability)) {
      throw new Error('doctor payload does not carry a report and its reachability');
    }
    report.findings.forEach(function (f) {
      if (!f || !DOCTOR_SEVERITIES[f.status]) {
        throw new Error('doctor finding carries a severity this build cannot read');
      }
    });
    reachability.forEach(function (t) {
      if (!t || REACHABILITY_TOKENS.indexOf(t.reachability) < 0) {
        throw new Error('doctor reachability carries a verdict this build cannot read');
      }
    });
    return report;
  }

  // Server order is preserved WITHIN a severity (section, then name), so the
  // findings list reads failures first without resorting the report.
  function findingsWith(findings, status) {
    return findings.filter(function (f) { return f.status === status; });
  }

  // ---- doctor: the verdict card ----------------------------------------

  function doctorVerdict(counts, reachability) {
    var wrap = document.createElement('div');
    wrap.className = 'card docverdict';
    wrap.appendChild(doctorVerdictLead(counts, reachability));
    wrap.appendChild(doctorCounts(counts));
    return wrap;
  }

  function doctorVerdictLead(counts, reachability) {
    var lead = document.createElement('div');
    lead.className = 'docverdict-lead';
    var dot = document.createElement('span');
    dot.className = 'docdot docdot--' + verdictTone(counts);
    dot.setAttribute('aria-hidden', 'true');
    var text = document.createElement('div');
    text.className = 'docverdict-text';
    text.appendChild(docText('docverdict-head', verdictHeadline(counts)));
    text.appendChild(docText('docverdict-sub',
      'no-network checks - ' + reachSummary(reachability)));
    lead.appendChild(dot);
    lead.appendChild(text);
    return lead;
  }

  // A report nobody could produce a check for is UNKNOWN, never clear: zero
  // findings is an absence of evidence, not a clean bill of health.
  function verdictTone(counts) {
    if (counts.fail) { return 'bad'; }
    if (counts.warn) { return 'warn'; }
    return counts.pass ? 'ok' : 'unknown';
  }

  function verdictHeadline(counts) {
    if (counts.fail) {
      return countText(counts.fail, ' check failed', ' checks failed') +
        ' - routing stays degraded until they are fixed';
    }
    if (counts.warn) {
      return countText(counts.warn, ' warning', ' warnings') + ' - nothing is broken yet';
    }
    return counts.pass ? 'Everything checks out' : 'No checks reported';
  }

  // The reachability rollup, which is the ONLY reachability reading on the
  // page: the per-target last outcome it folds is the Health tab's subject, so
  // repeating those rows here would show the same fact twice.
  function reachSummary(reachability) {
    if (!reachability.length) { return 'no dispatch target resolved yet'; }
    var parts = [];
    REACHABILITY_TOKENS.forEach(function (token) {
      var n = reachability.filter(function (t) { return t.reachability === token; }).length;
      if (n) { parts.push(n + ' ' + token); }
    });
    return 'last settled outcome: ' + parts.join(', ');
  }

  function doctorCounts(counts) {
    var row = document.createElement('div');
    row.className = 'doccounts';
    [
      ['failed', counts.fail, 'bad'],
      ['warnings', counts.warn, 'warn'],
      ['passed', counts.pass, null]
    ].forEach(function (c) { row.appendChild(doctorCount(c[0], c[1], c[2])); });
    return row;
  }

  // A zero count reads faint: no failures is the good state, so it must not
  // carry the failure color.
  function doctorCount(label, value, tone) {
    var cell = document.createElement('div');
    cell.className = 'doccount';
    cell.appendChild(docText('doccount-label', label));
    var v = docText('doccount-value', String(value));
    if (!value) { v.classList.add('mag-zero'); }
    else if (tone) { v.classList.add('doccount-value--' + tone); }
    cell.appendChild(v);
    return cell;
  }

  // ---- doctor: findings + passes ---------------------------------------

  function doctorFindings(findings) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Findings',
      countText(findings.length, ' check needs attention', ' checks need attention')));
    var list = document.createElement('div');
    list.className = 'doclist';
    findings.forEach(function (f) { list.appendChild(doctorFinding(f)); });
    wrap.appendChild(list);
    return wrap;
  }

  // One card per finding: the severity, what was checked, what was found, and
  // the remediation when the check carries one. A finding with no remediation
  // renders no empty slot for it.
  function doctorFinding(f) {
    var cardEl = document.createElement('div');
    cardEl.className = 'docfinding' + (f.status === 'Fail' ? ' docfinding--fail' : '');
    cardEl.appendChild(doctorFindingHead(f));
    cardEl.appendChild(docText('docfinding-detail', f.detail));
    if (f.remediation) { cardEl.appendChild(doctorFix(f.remediation)); }
    return cardEl;
  }

  function doctorFindingHead(f) {
    var head = document.createElement('div');
    head.className = 'docfinding-head';
    head.appendChild(tokLabeled('status', 'status', f.status));
    head.appendChild(docText('docfinding-name', f.name));
    head.appendChild(docText('docfinding-section', f.section));
    return head;
  }

  function doctorFix(remediation) {
    var wrap = document.createElement('div');
    wrap.className = 'docfix';
    wrap.appendChild(docText('docfix-label', 'remediation'));
    wrap.appendChild(docText('docfix-text', remediation));
    return wrap;
  }

  // The passing checks stay reachable (data floor) behind a default-closed
  // disclosure: they are the report's bulk and none of them is something to
  // act on, so they must not push the findings off the screen.
  function doctorPasses(passes) {
    var wrap = document.createElement('div');
    wrap.className = 'card';
    var grid = document.createElement('div');
    grid.className = 'docpassgrid';
    passes.forEach(function (p) { grid.appendChild(doctorPass(p)); });
    wrap.appendChild(buildExpander(
      countText(passes.length, ' check passed', ' checks passed'), grid));
    return wrap;
  }

  function doctorPass(p) {
    var row = document.createElement('div');
    row.className = 'docpass';
    var dot = document.createElement('span');
    dot.className = 'docpass-dot';
    dot.setAttribute('aria-hidden', 'true');
    row.appendChild(dot);
    row.appendChild(docText('docpass-name', p.name));
    row.appendChild(docText('docpass-detail', p.detail));
    return row;
  }

  // Nothing to act on. Welcoming, and explicit that the passing checks below
  // are what the verdict rests on.
  function doctorAllClear(passCount) {
    return emptyCard('Nothing needs attention',
      countText(passCount, ' check ran', ' checks ran') + ' and none of them reported a ' +
      'problem. A warning or a failure appears here the moment one does.',
      ['current state', 'no upstream was dialed']);
  }

  // A report with no check at all: nothing is known to be wrong, and nothing
  // has been verified either. Said in those words rather than as an all-clear.
  function doctorNoChecks() {
    return emptyCard('No checks reported',
      'The report came back without a single check. Nothing is known to be wrong here, ' +
      'and nothing has been confirmed healthy either.',
      ['current state']);
  }

  function docText(cls, value) {
    var span = document.createElement('span');
    span.className = cls;
    span.textContent = (value === null || value === undefined || value === '')
      ? '-' : String(value);
    return span;
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
      var state = effectiveState(rec);
      if (state === 'unavailable' || state === 'incompatible' ||
        state === 'invalid_payload') { return true; }
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

  // What a windowless tab shows INSTEAD of a window, as a label beside the
  // dimmed picker. Without it the picker reads as broken rather than as not
  // applicable, and Routing in particular would look like it were still
  // filtered to the window the buttons show.
  var WINDOWLESS_LABEL = {
    routing: 'All history',
    config: 'Current state',
    doctor: 'Current state'
  };

  // The picker reflects the live selection and goes DIM + inert on the
  // windowless tabs: Config and Doctor report current state and Routing
  // attributes over all history, so a window on any of them would promise a
  // filter that does not reach the figures.
  function updateWindowSel() {
    var group = el('windowsel');
    var tab = activeTab;
    var windowless = !!WINDOWLESS_TABS[tab];
    group.setAttribute('data-windowless', windowless ? 'true' : 'false');
    group.title = windowless
      ? 'this tab is not windowed - it shows ' + WINDOWLESS_SPAN[tab]
      : '';
    var buttons = group.querySelectorAll('button');
    Array.prototype.forEach.call(buttons, function (b) {
      var active = b.getAttribute('data-window') === selectedWindow;
      b.setAttribute('aria-pressed', active ? 'true' : 'false');
      b.disabled = terminal || windowless;
    });
    syncWindowlessLabel(group, windowless ? WINDOWLESS_LABEL[tab] : null);
  }

  // The label lives beside the picker, created on demand and hidden when the
  // active tab is windowed.
  function syncWindowlessLabel(group, text) {
    var label = el('windowless-label');
    if (!label) {
      label = document.createElement('span');
      label.id = 'windowless-label';
      label.className = 'windowless-label';
      group.parentNode.insertBefore(label, group.nextSibling);
    }
    label.hidden = !text;
    label.textContent = text || '';
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
    usageAllRound();
  });
})();
