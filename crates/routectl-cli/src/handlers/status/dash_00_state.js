'use strict';
(function () {
  // Per-source wire versions this page was built against. See the
  // co-versioning note in the document head: same-binary, so a mismatch
  // should never occur in practice; the runtime check is recovery
  // containment for a cached page / mixed assets / a bad build, not version
  // negotiation. `query` sits alongside the four GET panels because the
  // QUERY aggregate is a source of its own (see SOURCES below).
  var EXPECTED = { usage: 3, health: 5, config: 3, doctor: 6, query: 1 };

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
  // Per-QUERY AbortController budget. Like TIMEOUT_MS this is ONE abort timer
  // covering the whole exchange -- connect, send, server work, and body decode
  // (see safeRequest); the client enforces no decomposition of it. Larger than
  // the GET budget because /status/query is the one route that reads a
  // client-supplied body and then runs a grouped ledger scan, whose own
  // server-side budgets are sized to sum below this. Those budgets do not cover
  // queueing, so a saturated surface can still abort here rather than returning
  // a served-but-unavailable panel; that is why a timeout is treated as
  // transport failure and backed off, not as a panel verdict. Still under the 5s
  // cadence, so a slow QUERY cannot overlap its own next scheduled attempt.
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
  var ageTimer = null;             // 1s interval advancing the as_of age label

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

