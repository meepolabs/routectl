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
  //            routing sections under them guard their own sources). The
  //            FIRST QUERY source listed, primary or not, is the one request
  //            the view issues (viewQuerySource); a view lists at most one.
  //   span     null for a windowed view. Otherwise what the view's figures
  //            span instead of a window: the picker dims and goes inert on
  //            it rather than promising a filter that does not reach the
  //            figures, and the span is shown beside it.
  //   build    build(primaryRec) -> Node, per the contract in dash_50_dom.js;
  //            every section it composes goes through viewSection, which
  //            hands that section's builder its own source's record.
  var VIEWS = [
    {
      id: 'now',
      label: 'Now',
      sources: ['attention', 'accounts', QUERY_SERIES_SOURCE, 'config'],
      span: null,
      build: buildNow
    },
    {
      id: 'accounts',
      label: 'Accounts',
      sources: ['accounts', 'health'],
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
      build: buildConfigView
    },
    {
      id: 'doctor',
      label: 'Doctor',
      sources: ['doctor'],
      span: 'current state',
      build: buildDoctorView
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

  // Each view composes its sections through viewSection (dash_40_render.js),
  // naming the source each section reads: the host, not the section, owns
  // that section's panel states, its fault boundary and its stale and
  // pending marks. Controls over the page selection sit beside the sections,
  // so a section showing a state card never takes them with it.

  // What needs attention, from the daemon's list, then how each subscription
  // account is doing, then where traffic is going: the windowed QUERY's
  // provider cards and tiles. The scope reset line stands in for the provider
  // cards when a scoped query has nothing renderable.
  function buildNow() {
    var stack = tabStack();
    stack.appendChild(viewSection('attention', attentionSection, 'Needs attention'));
    stack.appendChild(viewSection('accounts', accountsGlanceSection, 'Accounts'));
    if (selectedProvider && stateCard(SOURCES[QUERY_SERIES_SOURCE])) {
      stack.appendChild(scopeRecovery());
    }
    stack.appendChild(viewSection(QUERY_SERIES_SOURCE, buildOverview));
    return stack;
  }

  // Every configured credential by pool, from the accounts panel, with the
  // pool headers' usable counts read from health inside it; then the two
  // background drivers that keep the logins and readings fresh.
  function buildAccounts() {
    var stack = tabStack();
    stack.appendChild(viewSection('accounts', accountsPoolSection, 'Accounts by pool'));
    stack.appendChild(viewSection('accounts', accountsDriverSection, 'Background seat drivers'));
    return stack;
  }

  // What was served in the window, then how the configured chains carried
  // it: the chains from config, their live state from health and the
  // estimate from the all-history ledger.
  function buildTraffic() {
    var stack = tabStack();
    stack.appendChild(usageControls());
    stack.appendChild(viewSection(QUERY_SOURCE, buildUsage));
    stack.appendChild(viewSection('config', routingChainsSection, 'Fallback chains'));
    var chains = liveChains(SOURCES.config);
    if (!chains.length) { return stack; }
    stack.appendChild(viewSection('health', function (rec) {
      return targetStateSection(rec, chains);
    }, 'Live target state'));
    stack.appendChild(viewSection(USAGE_ALL_SOURCE, function (rec) {
      return attributionSection(rec, chains);
    }, 'Estimated step attribution'));
    return stack;
  }

  function buildModels() {
    return viewSection('health', healthTargetSection, 'Dispatch targets');
  }

  function buildConfigView() {
    return viewSection('config', buildConfig);
  }

  function buildDoctorView() {
    return viewSection('doctor', buildDoctor);
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

  // The QUERY source a view issues its request for, or null when the view
  // reads no QUERY: the first QUERY read in its sources, which need not be
  // its primary (a GET-led view can still carry a ranged roll-up).
  function viewQuerySource(view) {
    for (var i = 0; i < view.sources.length; i++) {
      if (QUERY_SOURCES.indexOf(view.sources[i]) >= 0) { return view.sources[i]; }
    }
    return null;
  }

  // A span as the label beside the dimmed picker. Without it the picker reads
  // as broken rather than as not applicable.
  function spanLabel(span) {
    return span.charAt(0).toUpperCase() + span.slice(1);
  }
  // ---- end views -------------------------------------------------------
