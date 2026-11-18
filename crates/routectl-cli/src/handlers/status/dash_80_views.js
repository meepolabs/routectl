  // ---- views -----------------------------------------------------------

  // The one view registry. Array order is tab order, and `id` names the
  // view's markup: `tab-<id>`, `badge-<id>`, `pane-<id>`, `status-<id>` and
  // `body-<id>` in dashboard.html, in this order (page.rs checks both).
  //
  //   sources  every source the view reads. The poll loop re-renders a view
  //            only when one of ITS sources lands, so a dead QUERY degrades
  //            the query-backed views and leaves the GET-backed ones alone.
  //            The FIRST entry is the view's primary source: it drives the
  //            freshness line, the tab badge and the whole-view state card,
  //            so a multi-source view names the source that owns its subject
  //            first (Traffic leads with the QUERY its usage cards read; the
  //            routing sections under them guard their own sources). A
  //            primary that is a QUERY source is also the request the view
  //            issues (viewQuerySource).
  //   span     null for a windowed view. Otherwise what the view's figures
  //            span instead of a window: the picker dims and goes inert on
  //            it rather than promising a filter that does not reach the
  //            figures, and the span is shown beside it.
  //   build    build(primaryRec) -> Node, per the contract in dash_50_dom.js.
  var VIEWS = [
    {
      id: 'now',
      label: 'Now',
      sources: [QUERY_SERIES_SOURCE, 'usage'],
      span: null,
      build: buildOverview
    },
    {
      id: 'accounts',
      label: 'Accounts',
      sources: ['health', 'usage'],
      span: 'current state',
      build: buildAccounts
    },
    {
      id: 'traffic',
      label: 'Traffic',
      sources: [QUERY_SOURCE, 'config', USAGE_ALL_SOURCE, 'health'],
      span: null,
      build: buildTraffic
    },
    {
      id: 'models',
      label: 'Models',
      sources: ['health'],
      span: 'current state',
      build: buildModels
    },
    {
      id: 'config',
      label: 'Config',
      sources: ['config'],
      span: 'current state',
      build: buildConfig
    },
    {
      id: 'doctor',
      label: 'Doctor',
      sources: ['doctor'],
      span: 'current state',
      build: buildDoctor
    }
  ];

  // Tab tokens from before the six views, kept so a saved or shared link
  // still opens the view that took over its content.
  var LEGACY_VIEW_IDS = {
    overview: 'now',
    usage: 'traffic',
    routing: 'traffic',
    health: 'accounts'
  };

  // Seats and quota: the pools a request can dispatch to, the driver keeping
  // their logins fresh, and the latest quota snapshot per seat. The quota
  // section reads the usage ledger and guards itself, so a dark ledger costs
  // the tiles alone.
  function buildAccounts(rec) {
    var stack = tabStack();
    stack.appendChild(healthPoolSection(rec));
    stack.appendChild(healthDriverSection(rec));
    stack.appendChild(healthQuotaSection());
    return stack;
  }

  // What was served in the window, then how the configured chains carried
  // it. The routing sections read config, health and the all-history ledger,
  // each behind its own safeSection; buildRouting takes config as the record
  // it is built around.
  function buildTraffic(rec) {
    var stack = tabStack();
    stack.appendChild(buildUsage(rec));
    stack.appendChild(buildRouting(SOURCES.config));
    return stack;
  }

  function buildModels(rec) {
    return healthTargetSection(rec);
  }

  // The registered view with this id, or null.
  function viewFor(id) {
    for (var i = 0; i < VIEWS.length; i++) {
      if (VIEWS[i].id === id) { return VIEWS[i]; }
    }
    return null;
  }

  // The view a hash token names: a registered id as is, an old tab token as
  // its successor, anything else as the first view.
  function canonicalViewId(token) {
    if (Object.prototype.hasOwnProperty.call(LEGACY_VIEW_IDS, token)) {
      return LEGACY_VIEW_IDS[token];
    }
    return viewFor(token) ? token : VIEWS[0].id;
  }

  // The QUERY source a view issues its request for, or null when the view is
  // GET-backed: its primary source when that is one of the two QUERY reads.
  function viewQuerySource(view) {
    var primary = view.sources[0];
    if (primary === QUERY_SERIES_SOURCE || primary === QUERY_SOURCE) { return primary; }
    return null;
  }

  // A span as the label beside the dimmed picker. Without it the picker reads
  // as broken rather than as not applicable.
  function spanLabel(span) {
    return span.charAt(0).toUpperCase() + span.slice(1);
  }
  // ---- end views -------------------------------------------------------
