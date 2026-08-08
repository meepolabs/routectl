
  // ---- chrome: age ticker ----------------------------------------------

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
    // The seat modal belongs to the Overview provider row: leaving the tab
    // dismisses it rather than leaving it to reappear on return.
    if (changed) { seatModalProvider = null; }
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
    document.addEventListener('keydown', function (e) {
      if (e.key === 'Escape') { closeSeatModal(); }
    });
    tick();
    usageAllRound();
  });
})();
