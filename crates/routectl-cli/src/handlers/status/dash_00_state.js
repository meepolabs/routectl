'use strict';
(function () {
  // Per-source wire versions this page was built against. See the
  // co-versioning note in the document head: same-binary, so a mismatch
  // should never occur in practice; the runtime check is recovery
  // containment for a cached page / mixed assets / a bad build, not version
  // negotiation. `query` sits alongside the GET panels because the QUERY
  // aggregate is a source of its own (see SOURCES below).
  var EXPECTED = { health: 9, config: 6, doctor: 32, query: 6, accounts: 1, attention: 1, models: 1 };

  // The panels of the aggregate round, each with its own envelope +
  // freshness. NOT the same list as the views (VIEWS in dash_80_views.js): a
  // view is what a tab shows, a source is a fetch.
  var GET_SOURCES = ['health', 'config', 'doctor', 'attention', 'accounts', 'models'];
  // The QUERY reads, each its OWN source. All speak `/status/query`, but
  // Traffic asks for a series-less roll-up and Now for a bucketed one, and the
  // payloads are not interchangeable: Now cannot draw a sparkline from a
  // payload whose `series` is null. Sharing one record made whichever view
  // polled last define what another rendered from, so a view switch painted
  // one view out of the other's payload. Two shapes are two sources.
  var QUERY_SOURCE = 'query';
  var QUERY_SERIES_SOURCE = 'query_series';
  var QUERY_SEATS_SOURCE = 'query_seats';
  var QUERY_MODELS_SOURCE = 'query_models';
  // The one registration of the QUERY reads: each source's name and the
  // QUERY_SHAPES entry it asks for under a selection `{ window, groupBy }`.
  // QUERY_SOURCES, SOURCE_PANEL, FETCH_SOURCES and shapeFor are derived from
  // it, so a new read is one entry here. The series-less read follows the
  // group-by toggle; the per-seat read is that read at seat grain whatever the
  // toggle says; the per-model read is the last seven days by model whatever
  // the range says.
  var QUERY_SOURCE_DEFS = [
    {
      name: QUERY_SOURCE,
      shape: function (sel) { return findShape(sel.window, sel.groupBy, false); }
    },
    {
      name: QUERY_SERIES_SOURCE,
      shape: function (sel) { return findShape(sel.window, 'provider', true); }
    },
    {
      name: QUERY_SEATS_SOURCE,
      shape: function (sel) { return findShape(sel.window, 'seat', false); }
    },
    {
      name: QUERY_MODELS_SOURCE,
      shape: function () { return findShape('last7d', 'model', false); }
    }
  ];
  var QUERY_SOURCES = QUERY_SOURCE_DEFS.map(function (def) { return def.name; });
  var ALL_SOURCES = GET_SOURCES.concat(QUERY_SOURCES);

  // The panel a source's envelope belongs to, where the two differ: every
  // QUERY read is the query PANEL at some request shape, so it validates
  // against the query wire version.
  var SOURCE_PANEL = {};
  QUERY_SOURCES.forEach(function (name) { SOURCE_PANEL[name] = 'query'; });

  function expectedVersion(name) {
    return EXPECTED[SOURCE_PANEL[name] || name];
  }

  // The first entry is the default range, the one an address without `range`
  // selects.
  var WINDOWS = ['today', 'last24h', 'week', 'last7d', 'month', 'all'];

  var BASE_MS = 5000;              // steady GET cadence
  // The one backoff ladder, 30s cap. GET and QUERY keep SEPARATE indexes and
  // timers (a failing QUERY must not slow healthy GET polling) but share these
  // steps, so the two schedules cannot drift apart.
  var BACKOFF_STEPS_MS = [10000, 20000, 30000];
  // Per-GET AbortController budget for the /status aggregate. The server's
  // budget for that round (a reserve for the six panels, health, config,
  // doctor, attention, accounts and models, and the response write) lands
  // inside it, so the server sheds a slow panel as unavailable instead of the
  // browser aborting the whole round. Still under the 5s cadence, so a slow
  // round cannot overlap its next attempt.
  var TIMEOUT_MS = 3500;
  // Per-QUERY AbortController budget. Like TIMEOUT_MS this is ONE abort timer
  // covering the whole exchange -- connect, send, server work, and body decode
  // (see safeRequest); the client enforces no decomposition of it.
  // /status/query is the one route that reads a client-supplied body and then
  // runs a grouped ledger scan, and those two server-side budgets (1s body
  // read + 10s read) are sized to sum below this with 1s of slack. Some server
  // time sits outside those budgets (opening the ledger, the response write),
  // so a slow surface can still abort here rather than returning a
  // served-but-unavailable panel; that is why a timeout is treated as
  // transport failure and backed off, not as a panel verdict. Longer than the
  // 5s cadence: the cadence nudge skips while a QUERY is in flight
  // (queryInFlight), so a slow QUERY is never superseded by its own top-up.
  var QUERY_TIMEOUT_MS = 12000;
  var SKEW_TOLERANCE_SEC = 2;      // future as_of within this is not clock skew
  var FRESH_SLACK_MS = 3000;       // grace past the due round before "stale"

  var selectedWindow = 'today';
  var selectedGroupBy = 'model';
  var selectedProvider = null;     // provider scope, null = every provider
  var activeTab = 'now';

  var terminal = false;            // set on 403; stops every loop permanently
  var running = false;             // one aggregate round in flight at a time
  var timer = null;
  var backoffIndex = -1;           // -1 = base cadence; 0..n = backoff step index
  var lastSuccess = null;          // wall time of the last good aggregate round
  var backingOff = false;          // reflects GET backoff state in the clock
  var nextDueAtMs = 0;             // wall-clock instant the next round is due
  var ageTimer = null;             // 1s interval advancing the as_of age label

  // Expansion state keyed by table id -> { encoded row key: true }. Survives the
  // per-poll DOM rebuild so an operator's expanded row stays open across
  // refreshes.
  var expanded = {};

  var el = function (id) { return document.getElementById(id); };

  // ---- per-source state records ----------------------------------------

  // One record per data source: a failure in one never clears a sibling.
  // `state` is one of loading | live | empty | unavailable | incompatible |
  // invalid_payload | stale | dead. `name` is carried on the record so a
  // section handed only its record can still say WHICH source it renders.
  // `liveAt` is the local wall time this source's own last valid payload
  // landed: a stale or dead section reports it, never another source's.
  function freshRecord(name) {
    return { name: name, state: 'loading', code: null, data: null, asOf: null, liveAt: null, badge: null };
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
      liveAt: prev.liveAt,
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

