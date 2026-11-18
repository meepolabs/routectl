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
  //            first (Routing is about the CONFIGURED chains; the ledger and
  //            health only describe them). A primary that is a QUERY source
  //            is also the request the view issues (viewQuerySource).
  //   span     null for a windowed view. Otherwise what the view's figures
  //            span instead of a window: the picker dims and goes inert on
  //            it rather than promising a filter that does not reach the
  //            figures, and the span is shown beside it.
  //   build    build(primaryRec) -> Node, per the contract in dash_50_dom.js.
  var VIEWS = [
    {
      id: 'overview',
      label: 'Overview',
      sources: [QUERY_SERIES_SOURCE, 'usage'],
      span: null,
      build: buildOverview
    },
    {
      id: 'usage',
      label: 'Usage',
      sources: [QUERY_SOURCE],
      span: null,
      build: buildUsage
    },
    {
      id: 'routing',
      label: 'Routing',
      sources: ['config', USAGE_ALL_SOURCE, 'health'],
      span: 'all history',
      build: buildRouting
    },
    {
      id: 'health',
      label: 'Health',
      sources: ['health', 'usage'],
      span: null,
      build: buildHealth
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

  // The registered view with this id, or null.
  function viewFor(id) {
    for (var i = 0; i < VIEWS.length; i++) {
      if (VIEWS[i].id === id) { return VIEWS[i]; }
    }
    return null;
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
