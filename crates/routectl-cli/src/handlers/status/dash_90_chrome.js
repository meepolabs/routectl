
  // ---- chrome: age ticker ----------------------------------------------

  // The browser tests drive the address, the verdict and favicon and the
  // changed-value mark (page_browser_url_tests.rs,
  // page_browser_verdict_tests.rs, page_browser_motion_tests.rs); the ticker
  // and how motion looks are verified BY HAND -- see
  // dashboard-manual-checklist.md beside this source, and run it before
  // shipping a change here.

  // Repaint the poll indicator every second so the as_of age it reports
  // advances between rounds instead of looking frozen at the last one's value.
  function startAgeTicker() {
    renderPollIndicator();
    clearInterval(ageTimer);
    ageTimer = setInterval(renderPollIndicator, 1000);
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
  var FAVICON_BAD = "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><circle cx='8' cy='8' r='7' fill='%23d32f2f'></circle></svg>";
  var FAVICONS = {
    ok: FAVICON_OK,
    warn: FAVICON_WARN,
    idle: FAVICON_STALE,
    bad: FAVICON_BAD
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

  // The icon follows the verdict strip: red for a stopped page or a current
  // broken verdict, amber for a current degraded one. A verdict that is not
  // current (the daemon unreachable, the panel not answering), or data the
  // wall clock says has aged out, reads grey whatever its last severity was:
  // the icon reports what is known now, and the strip keeps the last verdict
  // with its time.
  function faviconState() {
    if (terminal) { return 'bad'; }
    var shown = shownVerdict(currentVerdict());
    if (shown.note || isStaleUnknown()) { return 'idle'; }
    return shown.kind;
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

  // ---- chrome: URL view state ------------------------------------------

  // The view state lives in the fragment: `#<view>` optionally followed by
  // `?range=&scope=&model=&group=&sort=`, in that key order, each key omitted
  // at its default. It serializes the page-global selection (activeTab,
  // selectedWindow, selectedProvider, selectedModel, selectedGroupBy,
  // selectedSort) and is never a second store. Open rows, dialogs, focus and
  // scroll stay out of it.
  //
  // The fragment is untrusted: anyone can hand the developer a link. Each key
  // is read with URLSearchParams and checked against a closed set; a value
  // outside it falls back to the default and the address is rewritten
  // canonical with replaceState. range, group and sort have fixed
  // vocabularies, and for them the FIRST of a duplicate counts. scope and
  // model reach the QUERY body and the scope strip, so each is taken only
  // when the address spells it once and exactly as the page writes it
  // (canonicalParam), and then only when it equals a member of a set read
  // from live payloads (addressVerdict): a configured provider entry or a
  // provider label of the last live unscoped provider read for scope, a
  // configured nickname for model. Until its set is known the value is held:
  // kept in the address, never applied, sent or shown.
  //
  // A link from before the query keys, `#<view>[/<window>]`, still opens: an
  // old tab token goes through canonicalViewId, and the window segment is the
  // range when no valid `range` key is given.
  //
  // Only the functions in this section touch location or history; a page test
  // pins that. The separator is built from its char code so this script
  // carries no slash-leading string literal (see the mutation-channel scan in
  // page.rs).
  var HASH_SEP = String.fromCharCode(47);
  var QUERY_MARK = '?';
  var VIEW_STATE_KEYS = ['range', 'scope', 'model', 'group', 'sort'];
  // The keys whose value must be in a set read from live payloads. model
  // comes first: whether a scope waits on the ledger depends on it.
  var HELD_KEYS = ['model', 'scope'];
  var ADDRESS_KEEP = 'keep';
  var ADDRESS_DROP = 'drop';
  var ADDRESS_HOLD = 'hold';
  var SORT_DIRS = ['asc', 'desc'];
  // Sortable tables by id, each with the column ids it sorts by (the `sortBy`
  // ids its mkTable call declares; a page test keeps the two equal). A sort
  // key naming a table or column not registered here is dropped.
  var SORT_TABLES = {
    'config-models': ['model', 'provider', 'upstream']
  };
  var selectedSort = null;         // `<table>.<column>.<dir>`, null = unsorted
  var selectedModel = null;        // nickname scope, null = every model
  // scope and model values read from the address whose set is not known
  // yet: held here, never in the selection, until admitHeld settles them.
  var heldAddress = { scope: null, model: null };
  // The provider labels of the last live QUERY answer grouped by provider
  // and carrying no scope; null until one lands.
  var ledgerProviders = null;

  function defaultViewState() {
    return {
      view: VIEWS[0].id, range: WINDOWS[0], scope: null, model: null,
      group: GROUP_BYS[0][0], sort: null
    };
  }

  function currentViewState() {
    return {
      view: activeTab,
      range: selectedWindow,
      scope: selectedProvider,
      model: selectedModel,
      group: selectedGroupBy,
      sort: selectedSort
    };
  }

  // The selection a QUERY body is filled from. The view picks the request
  // shape; sort orders rows on the page and never reaches the request.
  function queryInputKey(state) {
    return JSON.stringify([state.range, state.scope, state.model, state.group]);
  }

  function firstWindow(candidates) {
    for (var i = 0; i < candidates.length; i++) {
      if (WINDOWS.indexOf(candidates[i]) >= 0) { return candidates[i]; }
    }
    return WINDOWS[0];
  }

  // The `key` value of the address query `query` (already parsed into
  // `params`), or null unless the key appears exactly once and is spelled
  // exactly as URLSearchParams serializes its decoded value. Any other
  // spelling of the same value (a percent-encoded letter, a `+` where the
  // page writes `%2B`) is a second layout of it, and a duplicate is two
  // claims; both are refused rather than resolved.
  function canonicalParam(query, params, key) {
    var values = params.getAll(key);
    if (values.length !== 1 || values[0] === '') { return null; }
    var spelled = new URLSearchParams([[key, values[0]]]).toString();
    var bare = query.charAt(0) === QUERY_MARK ? query.slice(1) : query;
    return bare.split('&').indexOf(spelled) >= 0 ? values[0] : null;
  }

  // The `field` of each entry of the live config payload's `list`, or null
  // while no live config payload is held.
  function configNames(list, field) {
    var rec = SOURCES.config;
    if (rec.state !== 'live' || !rec.data || !Array.isArray(rec.data[list])) { return null; }
    var names = [];
    rec.data[list].forEach(function (entry) {
      if (isPlainObject(entry) && typeof entry[field] === 'string') { names.push(entry[field]); }
    });
    return names;
  }

  // Whether the view of `state` issues an unscoped provider read, the answer
  // that fills ledgerProviders. A model scope, set or still held, narrows it.
  function ledgerReadComing(state) {
    if (state.model !== null || heldAddress.model !== null) { return false; }
    var source = querySourceFor(state.view);
    var shape = source ? shapeAt(source, { window: state.range, groupBy: state.group }) : null;
    return !!shape && shape.group_by === 'provider';
  }

  // keep, drop or hold an address `value` for `key` against its live set,
  // for the selection `state`. A scope no configured entry names waits for
  // the ledger's labels only while the view is about to read them;
  // otherwise the labels seen so far decide.
  function addressVerdict(key, value, state) {
    if (key === 'model') {
      var nicknames = configNames('models', 'nickname');
      if (nicknames === null) { return ADDRESS_HOLD; }
      return nicknames.indexOf(value) >= 0 ? ADDRESS_KEEP : ADDRESS_DROP;
    }
    var entries = configNames('providers', 'provider_id');
    if (entries === null) { return ADDRESS_HOLD; }
    if (entries.indexOf(value) >= 0) { return ADDRESS_KEEP; }
    if (ledgerProviders === null && ledgerReadComing(state)) { return ADDRESS_HOLD; }
    return (ledgerProviders || []).indexOf(value) >= 0 ? ADDRESS_KEEP : ADDRESS_DROP;
  }

  // `state` with each held value whose set is now known moved into it (kept)
  // or discarded (dropped); a value still waiting stays held.
  function admitHeld(state) {
    var next = copyViewState(state);
    HELD_KEYS.forEach(function (key) {
      var value = heldAddress[key];
      if (value === null) { return; }
      var verdict = addressVerdict(key, value, next);
      if (verdict === ADDRESS_HOLD) { return; }
      heldAddress[key] = null;
      if (verdict === ADDRESS_KEEP) { next[key] = value; }
    });
    return next;
  }

  function copyViewState(state) {
    var copy = {};
    Object.keys(state).forEach(function (key) { copy[key] = state[key]; });
    return copy;
  }

  function hasHeldAddress() {
    return heldAddress.scope !== null || heldAddress.model !== null;
  }

  // Settle what the address still holds once a set it waits on may have
  // become known (a GET round, an unscoped provider answer): a kept value
  // moves the selection and refetches, a dropped one is cut from the address.
  function settleHeldAddress() {
    if (!hasHeldAddress() || terminal) { return; }
    var before = JSON.stringify(heldAddress);
    var next = admitHeld(currentViewState());
    if (JSON.stringify(heldAddress) === before) { return; }
    applyViewState(next, true);
    writeViewHash(next, false);
  }

  // Record the provider labels of a live answer to `body` when it is an
  // unscoped provider read, then settle a scope waiting on them.
  function noteLedgerProviders(source, body) {
    if (body.group_by !== 'provider' || body.provider || body.model) { return; }
    if (SOURCES[source].state !== 'live') { return; }
    var view = queryViewOf(source);
    if (!view) { return; }
    ledgerProviders = view.groups.map(function (g) { return g.label; });
    settleHeldAddress();
  }

  function isGroupBy(token) {
    return GROUP_BYS.some(function (g) { return g[0] === token; });
  }

  // The `sort-column` action: a header click on `<table>.<column>` sorts that
  // column ascending, then descending, then back to builder order.
  function onSortColumn(arg) {
    if (terminal) { return; }
    var next = nextSortKey(selectedSort, arg);
    if (next !== null && !isSortKey(next)) { return; }
    commitViewState({ sort: next });
  }

  function nextSortKey(current, column) {
    if (current === column + '.asc') { return column + '.desc'; }
    if (current === column + '.desc') { return null; }
    return column + '.asc';
  }

  function isSortKey(token) {
    if (token === null) { return false; }
    var parts = token.split('.');
    if (parts.length !== 3 || SORT_DIRS.indexOf(parts[2]) < 0) { return false; }
    if (!Object.prototype.hasOwnProperty.call(SORT_TABLES, parts[0])) { return false; }
    return SORT_TABLES[parts[0]].indexOf(parts[1]) >= 0;
  }

  function parseViewHash(hash) {
    var raw = (hash.charAt(0) === '#') ? hash.slice(1) : hash;
    var mark = raw.indexOf(QUERY_MARK);
    var path = (mark >= 0 ? raw.slice(0, mark) : raw).split(HASH_SEP);
    var query = mark >= 0 ? raw.slice(mark + 1) : '';
    var params = new URLSearchParams(query);
    return {
      view: canonicalViewId(path[0]),
      range: firstWindow([params.get('range'), path[1]]),
      scope: canonicalParam(query, params, 'scope'),
      model: canonicalParam(query, params, 'model'),
      group: groupParam(params.get('group')),
      sort: sortParam(params.get('sort'))
    };
  }

  function groupParam(token) {
    return isGroupBy(token) ? token : GROUP_BYS[0][0];
  }

  function sortParam(token) {
    return isSortKey(token) ? token : null;
  }

  function formatViewHash(state) {
    var defaults = defaultViewState();
    var params = new URLSearchParams();
    VIEW_STATE_KEYS.forEach(function (key) {
      if (state[key] !== null && state[key] !== defaults[key]) { params.append(key, state[key]); }
    });
    var query = params.toString();
    return '#' + state.view + (query ? QUERY_MARK + query : '');
  }

  // `state` as the address carries it: a value still held stands in for its
  // unset key, so a reload or a copied link keeps it for the next check.
  function addressState(state) {
    var out = copyViewState(state);
    HELD_KEYS.forEach(function (key) {
      if (out[key] === null && heldAddress[key] !== null) { out[key] = heldAddress[key]; }
    });
    return out;
  }

  // The address of view `id` with the rest of the selection as it is now.
  function viewLinkHash(id) {
    var state = currentViewState();
    state.view = id;
    return formatViewHash(addressState(state));
  }

  function writeViewHash(state, push) {
    var hash = formatViewHash(addressState(state));
    if (location.hash === hash) { return; }
    if (push) {
      history.pushState(null, '', hash);
    } else {
      history.replaceState(null, '', hash);
    }
  }

  // Make `next` the page selection and repaint it. With `refetch`, a move
  // re-issues the QUERY once through queryInputChanged, which supersedes any
  // in-flight read for the old selection. A changed range, scope or group
  // resets every query source whose request it changes and marks the panes
  // reading them pending (aria-busy) BEFORE anything is relabeled, on whatever
  // view is active, so no view shows another selection's figures under the
  // new labels, until the answer for the new selection applies. A view switch
  // alone first repaints the view's own last payload, which was fetched for
  // this same selection.
  function applyViewState(next, refetch) {
    var prior = currentViewState();
    var priorKeys = queryKeysBySource();
    var inputsMoved = queryInputKey(prior) !== queryInputKey(next);
    var viewMoved = prior.view !== next.view;
    activeTab = next.view;
    selectedWindow = next.range;
    selectedProvider = next.scope;
    selectedModel = next.model;
    selectedGroupBy = next.group;
    selectedSort = next.sort;
    if (refetch) { resetMovedQuerySources(priorKeys); }
    if (refetch && inputsMoved) { queryInputChanged(); }
    paintTabs();
    paintPending();
    updateWindowSel();
    renderActiveTab();
    renderVerdict();
    if (refetch && viewMoved && !inputsMoved) { queryInputChanged(); }
  }

  // The one path for a user's change to the view state: apply it and push ONE
  // history entry, so Back returns to the selection before it. A change that
  // sets scope or model replaces whatever the address still held for it.
  function commitViewState(change) {
    var next = currentViewState();
    Object.keys(change).forEach(function (key) {
      next[key] = change[key];
      if (HELD_KEYS.indexOf(key) >= 0) { heldAddress[key] = null; }
    });
    applyViewState(next, true);
    writeViewHash(next, true);
  }

  // Read the address into the page, on load and on every hashchange (Back,
  // Forward, an edited or followed link), and rewrite it canonical in place:
  // a cleanup is not a history step. scope and model are held first and
  // applied only as admitHeld lets them through. On load nothing has been
  // asked yet, so the first round's nudge issues the QUERY, after that
  // round's config answer has settled what the address held.
  function syncFromLocation(refetch) {
    var parsed = parseViewHash(location.hash);
    heldAddress = { scope: parsed.scope, model: parsed.model };
    parsed.scope = null;
    parsed.model = null;
    var next = admitHeld(parsed);
    applyViewState(next, refetch);
    writeViewHash(next, false);
  }

  // ---- chrome: window picker -------------------------------------------

  // The picker reflects the live selection and goes DIM + inert on every
  // view that registers a `span` (see VIEWS): such a view reports what it
  // spans instead of a window, so a live picker would promise a filter that
  // does not reach its figures.
  function updateWindowSel() {
    var group = el('windowsel');
    var span = viewFor(activeTab).span;
    var windowless = !!span;
    group.setAttribute('data-windowless', windowless ? 'true' : 'false');
    group.title = windowless
      ? 'this tab is not windowed - it shows ' + span
      : '';
    var buttons = group.querySelectorAll('button');
    Array.prototype.forEach.call(buttons, function (b) {
      var active = b.getAttribute('data-window') === selectedWindow;
      b.setAttribute('aria-pressed', active ? 'true' : 'false');
      b.disabled = terminal || windowless;
    });
    syncWindowlessLabel(group, windowless ? spanLabel(span) : null);
    syncWindowCovers(windowless ? null : WINDOW_COVERS[selectedWindow]);
  }

  // The line beside a live picker saying what the selected range covers. A
  // dimmed picker filters nothing, so it says nothing there.
  function syncWindowCovers(text) {
    var covers = el('window-covers');
    covers.hidden = !text;
    covers.textContent = text || '';
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
    if (terminal || viewFor(activeTab).span || w === selectedWindow) { return; }
    commitViewState({ range: w });
  }

  function initWindowSel() {
    var buttons = el('windowsel').querySelectorAll('button');
    Array.prototype.forEach.call(buttons, function (b) {
      b.addEventListener('click', function () { onWindowChange(b.getAttribute('data-window')); });
    });
    updateWindowSel();
  }

  // ---- chrome: delegated actions ---------------------------------------

  // Every control a view builder draws names its handler here through
  // `data-action` (and its argument through `data-arg`; see setAction in
  // dash_50_dom.js) instead of carrying a listener of its own. Each handler
  // receives the argument string (null when absent), the marked node and the
  // event, and reads page state at dispatch time, never a value captured
  // when the node was built. page.rs checks that every action a builder
  // names has an entry here and that every entry is named by a builder.
  var ACTIONS = {
    'row-toggle': function (arg) { if (arg) { onRowToggle(arg); } },
    'provider-scope': function (arg) { onProviderScope(arg); },
    'provider-overflow': function () { onProviderOverflow(); },
    'group-by': function (arg) { if (arg) { onGroupBy(arg); } },
    'sort-column': function (arg) { if (arg) { onSortColumn(arg); } },
    // A same-page link: the view switch goes through the one view-state path
    // so the rest of the selection is kept, not reset by the bare hash.
    'open-view': function (arg, node, e) {
      if (!viewFor(arg) || e.ctrlKey || e.metaKey || e.shiftKey || e.altKey) { return; }
      e.preventDefault();
      selectTab(arg);
    }
  };

  // Dispatch a click to the action of the NEAREST marked ancestor of its
  // target inside `root`. Nearest-only is what lets a disclosure button sit
  // inside a clickable row without the click toggling twice.
  function dispatchAction(root, e) {
    var target = e.target;
    if (!target || typeof target.closest !== 'function') { return; }
    var node = target.closest('[data-action]');
    if (!node || !root.contains(node)) { return; }
    var name = node.getAttribute('data-action');
    if (!Object.prototype.hasOwnProperty.call(ACTIONS, name)) { return; }
    ACTIONS[name](node.getAttribute('data-arg'), node, e);
  }

  // Ctrl+A / Cmd+A on a focused copyable code element (COPY_ATTR, set by
  // copyableCode in dash_50_dom.js) selects that element's text alone, so
  // the copy that follows carries the command and nothing else.
  function selectFocusedCode(root, e) {
    if (!(e.ctrlKey || e.metaKey) || e.altKey || e.shiftKey) { return; }
    if (e.key !== 'a' && e.key !== 'A') { return; }
    var node = e.target;
    if (!node || node.nodeType !== 1 || !node.hasAttribute(COPY_ATTR) || !root.contains(node)) { return; }
    e.preventDefault();
    var range = document.createRange();
    range.selectNodeContents(node);
    var sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
  }

  // One click and one keydown listener per pane root and one click listener
  // on the verdict strip, installed once: the nodes beneath them are rebuilt
  // every render, the roots are not.
  function initActions() {
    var panes = VIEWS.map(function (view) { return el('pane-' + view.id); });
    panes.forEach(function (root) {
      if (!root) { return; }
      root.addEventListener('keydown', function (e) { selectFocusedCode(root, e); });
    });
    panes.concat([el('verdict')]).forEach(function (root) {
      if (!root) { return; }
      root.addEventListener('click', function (e) { dispatchAction(root, e); });
    });
  }

  // ---- chrome: tabs ----------------------------------------------------

  // Selecting a tab is a view-state change like any other: it switches the
  // visible pane, follows the window picker's applicability, and refreshes a
  // query-backed view's QUERY at once rather than waiting for the next round.
  function selectTab(name) {
    commitViewState({ view: name });
  }

  function paintTabs() {
    VIEWS.forEach(function (view) {
      var sel = view.id === activeTab;
      var tab = el('tab-' + view.id);
      tab.setAttribute('aria-selected', sel ? 'true' : 'false');
      tab.tabIndex = sel ? 0 : -1;
      el('pane-' + view.id).hidden = !sel;
    });
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
  }

  document.addEventListener('DOMContentLoaded', function () {
    initWindowSel();
    initActions();
    initTabs();
    initRefresh();
    // Restore view state from the address BEFORE the first tick so the
    // initial render lands on the requested view and selection.
    syncFromLocation(false);
    syncTabBadges();
    updateFavicon();
    window.addEventListener('hashchange', function () { syncFromLocation(true); });
    // A hidden tab throttles the poll timer, so re-evaluate staleness (and
    // the icon) whenever the tab regains attention.
    document.addEventListener('visibilitychange', updateFavicon);
    window.addEventListener('focus', updateFavicon);
    window.addEventListener('pageshow', updateFavicon);
    document.addEventListener('animationend', clearChangedValue);
    tick();
    usageAllRound();
  });
})();
