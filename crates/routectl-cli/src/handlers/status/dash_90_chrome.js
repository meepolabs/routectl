
  // ---- chrome: age ticker ----------------------------------------------

  // NO automated runtime harness covers this part. The chrome and animation
  // surface is verified BY HAND only -- see dashboard-manual-checklist.md
  // beside this source, and run it before shipping a change here.

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

  // ---- chrome: URL view state ------------------------------------------

  // The view state lives in the fragment: `#<view>` optionally followed by
  // `?range=&scope=&group=&sort=`, in that key order, each key omitted at its
  // default. It serializes the page-global selection (activeTab,
  // selectedWindow, selectedProvider, selectedGroupBy, selectedSort) and is
  // never a second store. Open rows, dialogs, focus and scroll stay out of it.
  //
  // The fragment is untrusted: anyone can hand the developer a link. Each key
  // is read with URLSearchParams, whose `get` returns the FIRST of a
  // duplicate, and checked against a closed vocabulary; a value outside it
  // falls back to the default and the address is rewritten canonical with
  // replaceState. scope is the one open value. It reaches the QUERY body and
  // labels drawn with textContent, so it is only trimmed, bounded and
  // refused when it carries a control or format character (see scopeParam).
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
  var VIEW_STATE_KEYS = ['range', 'scope', 'group', 'sort'];
  var SCOPE_MAX_CHARS = 128;
  // The edge trim below backtracks quadratically on a long run of spaces, so
  // a raw value longer than this is dropped before it runs.
  var SCOPE_RAW_MAX_CHARS = SCOPE_MAX_CHARS * 4;
  // The server refuses a blank provider and counts U+0085 as blank, which
  // String.prototype.trim does not strip.
  var SCOPE_EDGE_SPACE = /^[\s\u0085]+|[\s\u0085]+$/g;
  // Provider names are config identifiers. A control or format character
  // (zero-width, bidi override) only makes a label read as a provider it is
  // not.
  var SCOPE_HIDDEN_CHAR = /[\p{Cc}\p{Cf}]/u;
  var SORT_DIRS = ['asc', 'desc'];
  // Sortable tables by id, each with the column ids it sorts by (the `sortBy`
  // ids its mkTable call declares; a page test keeps the two equal). A sort
  // key naming a table or column not registered here is dropped.
  var SORT_TABLES = {
    'config-models': ['model', 'provider', 'upstream']
  };
  var selectedSort = null;         // `<table>.<column>.<dir>`, null = unsorted

  function defaultViewState() {
    return { view: VIEWS[0].id, range: WINDOWS[0], scope: null, group: GROUP_BYS[0][0], sort: null };
  }

  function currentViewState() {
    return {
      view: activeTab,
      range: selectedWindow,
      scope: selectedProvider,
      group: selectedGroupBy,
      sort: selectedSort
    };
  }

  // The selection a QUERY body is filled from. The view picks the request
  // shape; sort orders rows on the page and never reaches the request.
  function queryInputKey(state) {
    return JSON.stringify([state.range, state.scope, state.group]);
  }

  function firstWindow(candidates) {
    for (var i = 0; i < candidates.length; i++) {
      if (WINDOWS.indexOf(candidates[i]) >= 0) { return candidates[i]; }
    }
    return WINDOWS[0];
  }

  function scopeParam(raw) {
    if (raw === null || raw.length > SCOPE_RAW_MAX_CHARS) { return null; }
    var scope = raw.replace(SCOPE_EDGE_SPACE, '');
    if (scope === '' || scope.length > SCOPE_MAX_CHARS) { return null; }
    return SCOPE_HIDDEN_CHAR.test(scope) ? null : scope;
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
    var params = new URLSearchParams(mark >= 0 ? raw.slice(mark + 1) : '');
    var group = params.get('group');
    var sort = params.get('sort');
    return {
      view: canonicalViewId(path[0]),
      range: firstWindow([params.get('range'), path[1]]),
      scope: scopeParam(params.get('scope')),
      group: isGroupBy(group) ? group : GROUP_BYS[0][0],
      sort: isSortKey(sort) ? sort : null
    };
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

  function writeViewHash(state, push) {
    var hash = formatViewHash(state);
    if (location.hash === hash) { return; }
    if (push) {
      history.pushState(null, '', hash);
    } else {
      history.replaceState(null, '', hash);
    }
  }

  // Make `next` the page selection and repaint it. With `refetch`, a move
  // re-issues the QUERY once through queryInputChanged, which supersedes any
  // in-flight read for the old selection and resets both query sources. A
  // changed range, scope or group marks the panes reading those sources
  // pending (aria-busy) and resets them BEFORE anything is relabeled, so the
  // view shows a busy loading state rather than another selection's figures
  // under the new labels, until the answer for the new selection applies. A
  // view switch alone first repaints the view's own last payload, which was
  // fetched for this same selection.
  function applyViewState(next, refetch) {
    var prior = currentViewState();
    var inputsMoved = queryInputKey(prior) !== queryInputKey(next);
    var viewMoved = prior.view !== next.view;
    if (refetch && inputsMoved) { markPending([QUERY_SOURCE, QUERY_SERIES_SOURCE]); }
    activeTab = next.view;
    selectedWindow = next.range;
    selectedProvider = next.scope;
    selectedGroupBy = next.group;
    selectedSort = next.sort;
    // The seat modal belongs to the Now provider row: leaving the view
    // dismisses it rather than leaving it to reappear on return.
    if (viewMoved) { seatModalProvider = null; }
    if (refetch && inputsMoved) { queryInputChanged(); }
    paintTabs();
    updateWindowSel();
    renderActiveTab();
    renderVerdict();
    if (refetch && viewMoved && !inputsMoved) { queryInputChanged(); }
  }

  // The one path for a user's change to the view state: apply it and push ONE
  // history entry, so Back returns to the selection before it.
  function commitViewState(change) {
    var next = currentViewState();
    Object.keys(change).forEach(function (key) { next[key] = change[key]; });
    applyViewState(next, true);
    writeViewHash(next, true);
  }

  // Read the address into the page, on load and on every hashchange (Back,
  // Forward, an edited or followed link), and rewrite it canonical in place:
  // a cleanup is not a history step. On load nothing has been asked yet, so
  // the first round's nudge issues the QUERY.
  function syncFromLocation(refetch) {
    var next = parseViewHash(location.hash);
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
    'seat-open': function (arg) { if (arg) { openSeatModal(arg); } },
    'seat-close': function () { closeSeatModal(); },
    // Only a click on the backdrop itself closes; a click that bubbled up
    // from inside the dialog does not.
    'seat-backdrop': function (arg, node, e) {
      if (e.target === node) { closeSeatModal(); }
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

  // One click listener per pane root and one on the modal host, installed
  // once: the nodes beneath them are rebuilt every render, the roots are not.
  function initActions() {
    var roots = VIEWS.map(function (view) { return el('pane-' + view.id); });
    roots.push(el('modal-host'));
    roots.forEach(function (root) {
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
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') { closeSeatModal(); }
    });
    tick();
    usageAllRound();
  });
})();
