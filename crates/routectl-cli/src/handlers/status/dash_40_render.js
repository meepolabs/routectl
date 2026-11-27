  // ---- render dispatch -------------------------------------------------

  // The browser tests drive per-pass fault reconciliation and the in-place
  // update (page_browser_states_tests.rs, page_browser_verdict_tests.rs,
  // page_browser_locks_tests.rs, page_browser_motion_tests.rs); what they do
  // not reach is verified BY HAND -- see dashboard-manual-checklist.md beside
  // this source, and run it before shipping a change here.

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
      liveAt: new Date(),
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
      code: 'expected ' + expectedVersion(name) + ', received ' + received,
      data: null,
      badge: { label: 'n/a', kind: 'muted' }
    });
    renderSourceChanged(name);
  }

  // A failed background refresh (503, timeout, network) never closes what the
  // source last rendered: a source holding a payload keeps it and its DOM,
  // marked stale (the section dims, the status line says since when), and
  // its badge keeps its last number, dimmed: amber on a badge means a
  // problem, and old data is not one. A source with nothing rendered yet has
  // nothing to keep, so it shows the transport error state. Siblings are
  // untouched either way.
  function markSourceTransport(name) {
    var prev = SOURCES[name];
    var kept = prev.data !== null;
    setSource(name, {
      state: kept ? 'stale' : 'dead',
      code: null,
      badge: kept ? dimmedBadge(prev.badge) : { label: 'offline', kind: 'error' }
    });
    renderSourceChanged(name);
  }

  function dimmedBadge(badge) {
    if (!badge) { return null; }
    return { label: badge.label, kind: badge.kind, title: badge.title, dimmed: true };
  }

  // Polling has stopped for good: nothing on screen is current any more, and
  // nothing will refresh it.
  function markSourceStopped(name) {
    setSource(name, {
      state: 'dead',
      code: null,
      data: null,
      badge: { label: 'offline', kind: 'error' }
    });
    renderSourceChanged(name);
  }

  // The transport kind of the last failed aggregate round. The strip reads it
  // only while the attention source is stale or dead, and on a running page
  // only such a failure leaves it so: a good round makes it current again, so
  // the kind it left behind is never read.
  var aggregateFailureKind = null;

  // An unreachable daemon puts up no banner: the verdict strip already says
  // the daemon cannot be reached, and a banner left by an earlier overload or
  // timeout would name the wrong cause.
  function markTransport(kind) {
    aggregateFailureKind = kind;
    GET_SOURCES.forEach(function (n) { markSourceTransport(n); });
    if (kind === 'overloaded') {
      setBanner('warn', 'status overloaded: showing last known values, backing off');
    } else if (kind === 'timeout') {
      setBanner('error', 'status request timed out, backing off');
    } else {
      clearBanner();
    }
  }

  function getPanel(agg, name) {
    return (agg && agg.panels) ? agg.panels[name] : undefined;
  }

  // A source landed (fresh, unavailable, or down). Repaint the active tab if
  // it reads that source, and refresh the chrome that summarizes every
  // source. A tab that does NOT read the source is left alone -- this is
  // what keeps a dead QUERY confined to Overview and Usage.
  //
  // Every answer that applies to a source lands here, so this is also where a
  // pending source stops being pending: the answer (figures, a refusal or a
  // failure) is what the view shows next.
  function renderSourceChanged(name) {
    settlePending(name);
    if (viewFor(activeTab).sources.indexOf(name) >= 0) { renderActiveTab(); }
    renderVerdict();
    syncTabBadges();
  }

  // ---- pending view changes --------------------------------------------

  // Sources reset by the operator's own range, scope or group change and not
  // yet answered for the new selection. A pane reading one of them is marked
  // busy until that answer applies, so nothing in it reads as the current
  // answer for the selection its controls now show.
  var PENDING_SOURCES = Object.create(null);

  function markPending(names) {
    names.forEach(function (name) { PENDING_SOURCES[name] = true; });
    paintPending();
  }

  function settlePending(name) {
    if (!PENDING_SOURCES[name]) { return; }
    delete PENDING_SOURCES[name];
    paintPending();
  }

  // Only the active pane is marked: a source a hidden view reads stays
  // pending until that view is opened and its own round answers, and the mark
  // follows it there rather than sitting on a pane nobody can see.
  function paintPending() {
    VIEWS.forEach(function (view) {
      var pane = el('pane-' + view.id);
      if (!pane) { return; }
      var busy = view.id === activeTab &&
        view.sources.some(function (name) { return !!PENDING_SOURCES[name]; });
      pane.classList.toggle('pane--pending', busy);
      if (busy) { pane.setAttribute('aria-busy', 'true'); }
      else { pane.removeAttribute('aria-busy'); }
    });
  }

  // ---- tab rendering ---------------------------------------------------

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
    var view = viewFor(tab);
    var primary = SOURCES[view.sources[0]];
    var pass = { faults: Object.create(null), touched: Object.create(null) };
    var marksChanges = paintedView === tab;
    paintedView = tab;
    renderPass = pass;
    try {
      morphPanel(body, view.build(primary), marksChanges);
    } catch (e) {
      pass.faults[primary.name] = true;
      pass.touched[primary.name] = true;
      morphPanel(body, errorCard('invalid_payload',
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
    renderSectionStatus(status, primary);
    // A fault raised or cleared by THIS pass changes what the chrome should say
    // about its source. Neither of these re-enters the tab builder, so there is
    // no recursion back into a section that just rendered.
    if (changed) {
      renderVerdict();
      syncTabBadges();
    }
  }

  // The one way a pane body is updated: `parent`'s children
  // are brought in line with `node` in place, so an element the rebuild keeps
  // (matched by id, else by tag) is the same node afterward and focus on it
  // survives. A node only, never markup: the morph library parses a string
  // argument as HTML, and every text on this page is wire data.
  //
  // A refresh unit holding a non-collapsed text selection is left exactly as
  // it is (morphGate); it catches up on the first refresh after the selection
  // collapses. Page and table scroll positions are put back after the morph.
  // With `marksChanges`, a figure whose value moved on a kept node is marked.
  function morphPanel(parent, node, marksChanges) {
    if (!(node instanceof Node)) {
      throw new TypeError('morphPanel takes a DOM node, never markup');
    }
    var held = selectedUnits(parent);
    var scroll = heldScroll(parent);
    Idiomorph.morph(parent, node, {
      morphStyle: 'innerHTML',
      callbacks: { beforeNodeMorphed: morphGate(held, marksChanges) }
    });
    restoreScroll(parent, scroll);
  }

  // The morph's per-node veto: carry a kept disclosure's open state and mark a
  // changed value first, then refuse to touch a held unit paired with its own
  // rebuild, so neither its text nor its place changes under the selection. A
  // held unit paired with a different section (or with none) is morphed: its
  // section left.
  function morphGate(held, marksChanges) {
    return function (oldNode, freshNode) {
      keepOpenState(oldNode, freshNode);
      if (marksChanges) { markChangedValue(oldNode, freshNode); }
      return !(held.indexOf(oldNode) >= 0 && sameRefreshUnit(oldNode, freshNode));
    };
  }

  function sameRefreshUnit(oldNode, freshNode) {
    return freshNode.nodeType === 1 &&
      freshNode.getAttribute(REFRESH_UNIT_ATTR) === oldNode.getAttribute(REFRESH_UNIT_ATTR);
  }

  // The refresh units in `parent` that hold an end of a non-collapsed text
  // selection: the nearest marked ancestor of each end point. A selection
  // inside one unit holds that unit alone.
  function selectedUnits(parent) {
    var sel = window.getSelection();
    if (!sel || sel.isCollapsed) { return []; }
    var units = [];
    [sel.anchorNode, sel.focusNode].forEach(function (end) {
      var unit = refreshUnitOf(end);
      if (unit && parent.contains(unit) && units.indexOf(unit) < 0) { units.push(unit); }
    });
    return units;
  }

  function refreshUnitOf(node) {
    var elem = node && (node.nodeType === 1 ? node : node.parentElement);
    return elem ? elem.closest('[' + REFRESH_UNIT_ATTR + ']') : null;
  }

  // The page scroll and the scroll of every keyed table scroller in `parent`.
  // A morph that shortens the page for a moment, or rebuilds a scroller,
  // would otherwise move the operator's reading position.
  function heldScroll(parent) {
    var tables = [];
    Array.prototype.forEach.call(parent.querySelectorAll('.tablewrap[id]'), function (wrap) {
      tables.push({ id: wrap.id, left: wrap.scrollLeft, top: wrap.scrollTop });
    });
    return { x: window.scrollX, y: window.scrollY, tables: tables };
  }

  function restoreScroll(parent, held) {
    held.tables.forEach(function (t) {
      var wrap = document.getElementById(t.id);
      if (!wrap || !parent.contains(wrap)) { return; }
      if (wrap.scrollLeft !== t.left) { wrap.scrollLeft = t.left; }
      if (wrap.scrollTop !== t.top) { wrap.scrollTop = t.top; }
    });
    if (window.scrollX !== held.x || window.scrollY !== held.y) {
      window.scrollTo(held.x, held.y);
    }
  }

  // A rebuilt <details> is always closed, and the morph removes every
  // attribute the fresh node lacks, so without this a disclosure the operator
  // opened would shut on the next refresh. The kept node's live state wins.
  function keepOpenState(oldNode, freshNode) {
    if (oldNode instanceof HTMLDetailsElement && freshNode instanceof HTMLDetailsElement) {
      freshNode.open = oldNode.open;
    }
  }

  // The sections built during the current render pass: which sources they read
  // and which of them threw. Non-null only while a tab body is being built.
  var renderPass = null;

  // The view the pane bodies were last painted for. Only a repaint of that
  // same view is a refresh whose moved values are marked: a first paint or a
  // view switch shows values the operator has not been watching change.
  var paintedView = null;

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
  // view's `sources` entry goes through this helper, with that entry's own record.
  // The tab-level try/catch replaces the whole tab, so a secondary source's
  // throw reaching it would blank content the primary source rendered
  // correctly while the status line still reported the primary live.
  function safeSection(rec, build) {
    return guardedContent(rec, build).node;
  }

  // safeSection's node, and whether it is a card drawn in the builder's place.
  function guardedContent(rec, build, failedText) {
    var pending = stateCard(rec, failedText);
    if (pending) { return { node: pending, isCard: true }; }
    try {
      var node = build(rec);
      markRenderAttempt(rec.name);
      return { node: node, isCard: false };
    } catch (e) {
      recordRenderFault(rec.name);
      var fault = errorCard('invalid_payload', 'this section could not be rendered from the payload it received');
      return { node: fault, isCard: true };
    }
  }

  // The host's boundary around ONE section a view composes, bound to the
  // source that section reads (see VIEWS). The binding is written on the
  // section, so the panel states are drawn per section rather than per pane:
  // a section whose source is serving retained values after a failed refresh
  // is marked stale while its siblings stay current. The section's content
  // goes through safeSection, so the host, not the builder, draws the
  // loading and failure cards, contains a throw and records the attempt that
  // lets a recovered section clear its source's fault. `build` is called
  // only for a renderable record, and receives it. A builder that guards
  // itself as well is safe here: its own card is a successful build to the
  // host, and the fault it records is not undone by the host's attempt mark.
  // A control that must stay usable while the section shows a card (a
  // scope reset, a grouping picker) is composed beside the section, not in it.
  // A section that titles itself passes its `title`, so every card the host
  // draws in its place (a state card or a builder fault) still says which
  // reading is missing; a successful build draws its own head instead.
  // A section that passes `failedText` leads its failed-read card with that
  // plain sentence, the technical code under it.
  var SECTION_SOURCE_ATTR = 'data-source';

  function viewSection(name, build, title, failedText) {
    var rec = SOURCES[name];
    var wrap = document.createElement('div');
    wrap.className = 'view-section';
    wrap.setAttribute(SECTION_SOURCE_ATTR, name);
    if (rec.state === 'stale') { wrap.classList.add('section--stale'); }
    if (PENDING_SOURCES[name]) { wrap.classList.add('section--pending'); }
    var content = guardedContent(rec, build, failedText);
    if (title && content.isCard) { wrap.appendChild(sectionHead(title, null)); }
    wrap.appendChild(content.node);
    return wrap;
  }

  // The pane's status line reports a source that is NOT current, and nothing
  // else. A live source says nothing here: the verdict strip already carries the
  // as_of age of the visible tab's data, and repeating it per pane would show
  // the same fact twice.
  function renderSectionStatus(status, rec) {
    if (!status) { return; }
    var state = effectiveState(rec);
    status.replaceChildren();
    if (state === 'loading') {
      status.appendChild(document.createTextNode('loading'));
      return;
    }
    if (state === 'stale' || state === 'dead') {
      var note = rec.liveAt
        ? 'last success at ' + rec.liveAt.toLocaleTimeString()
        : 'no successful poll yet';
      var lead = state === 'stale' ? 'stale: ' : 'no current data: ';
      status.appendChild(makeLiveDot());
      status.appendChild(document.createTextNode(lead + note));
      return;
    }
    if (state === 'invalid_payload') {
      status.appendChild(makeLiveDot());
      status.appendChild(document.createTextNode(
        'invalid payload: a section could not be rendered'));
    }
  }

  // The small round indicator beside a not-current status line.
  function makeLiveDot() {
    var dot = document.createElement('span');
    dot.className = 'live-dot';
    dot.setAttribute('aria-hidden', 'true');
    return dot;
  }

  // ---- per-source state presentation -----------------------------------

  // The state a builder must render instead of content, or null when the
  // source is live. Every buildX starts by consulting this, so no tab has to
  // reinvent its loading / empty / failure shapes. `failedText`, when given,
  // leads the card of a read that failed.
  function stateCard(rec, failedText) {
    var state = rec.state;
    if (state === 'loading') { return skeletonCard(); }
    if (state === 'unavailable') {
      return failedReadCard(failedText, rec, rec.code, 'this source is not answering right now');
    }
    if (state === 'incompatible') { return incompatibleCard(rec.code); }
    if (state === 'invalid_payload') {
      return errorCard('invalid_payload', 'the payload did not match its declared shape');
    }
    if (state === 'dead') { return failedReadCard(failedText, rec, 'transport failure', null); }
    return null;
  }

  // The plain sentence, what happens next, and the time of the source's last
  // good read when it had one, above the technical code, which stays as a
  // faint second line. A failing read backs off rather than keeping the 5 s
  // cadence, so the card promises a retry, not its interval; a page that
  // stopped polling promises none.
  function failedReadCard(failedText, rec, code, extra) {
    if (!failedText) { return errorCard(code, extra); }
    var lead = failedText + (terminal ? '; polling has stopped' : '; retrying automatically');
    if (rec.liveAt) { lead += ' - last good read at ' + rec.liveAt.toLocaleTimeString(); }
    var codeLine = document.createElement('span');
    codeLine.className = 'unavailable-code';
    codeLine.textContent = unavailableText(code, extra);
    var div = document.createElement('div');
    div.className = 'unavailable-msg';
    div.appendChild(document.createTextNode(lead));
    div.appendChild(codeLine);
    return div;
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
    div.textContent = unavailableText(code, extra);
    return div;
  }

  function unavailableText(code, extra) {
    return 'unavailable: ' + code + (extra ? ' (' + extra + ')' : '');
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

  function enterTerminal() {
    terminal = true;
    clearTimeout(timer);
    clearTimeout(queryTimer);
    clearInterval(ageTimer);
    invalidateEveryFetch();
    setBanner('terminal',
      'This dashboard must be opened via the loopback or bound address ' +
      '(for example http://127.0.0.1:8787). Polling has stopped.');
    ALL_SOURCES.forEach(markSourceStopped);
    updateWindowSel();
    syncRefreshBtn();
    renderVerdict();
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

  // The plain-language verdict: ONE sentence the daemon wrote about whether
  // routing works (attention.verdict), a link to the view that shows its
  // cause, the window's req/span figures and the poll indicator. The page
  // adds no wording of its own to the sentence. The favicon follows the
  // verdict, so it is repainted here too.
  // Deliberately carries NO cost figure -- cost is a Now/Traffic reading,
  // and repeating it here would show the same fact twice.
  function renderVerdict() {
    var current = currentVerdict();
    if (current) { lastVerdict = current; }
    var shown = shownVerdict(current);
    var strip = el('verdict');
    strip.classList.remove('verdict--ok', 'verdict--warn', 'verdict--bad', 'verdict--idle');
    strip.classList.add('verdict--' + shown.kind);
    strip.classList.toggle('verdict--stale', !!shown.note);
    el('verdict-text').textContent = shown.text;
    var note = el('verdict-note');
    note.hidden = !shown.note;
    note.textContent = shown.note || '';
    syncVerdictLink(shown.target);
    el('verdict-stats').textContent = verdictStats();
    renderPollIndicator();
    updateFavicon();
  }

  var VERDICT_KIND = { ok: 'ok', degraded: 'warn', broken: 'bad' };

  // The last current verdict the strip showed, kept so an outage or a panel
  // that stops answering never returns the strip to the start-up text.
  var lastVerdict = null;

  // The verdict of a CURRENT attention payload, or null when attention is
  // not current (no payload yet, a refusal, a failed refresh) or its verdict
  // is not one this page can read.
  function currentVerdict() {
    var rec = SOURCES.attention;
    if (effectiveState(rec) !== 'live' || !isPlainObject(rec.data)) { return null; }
    var v = rec.data.verdict;
    var known = isPlainObject(v) && Object.prototype.hasOwnProperty.call(VERDICT_KIND, v.severity) &&
      typeof v.text === 'string' && (v.target === null || typeof v.target === 'string');
    if (!known) { return null; }
    return { severity: v.severity, text: v.text, target: v.target, as_of: rec.asOf };
  }

  // What the strip shows: the current verdict; else, once one has been
  // shown, that last verdict marked with its time and why it is not current;
  // only before any verdict, the start-up text. A stopped page says so.
  function shownVerdict(current) {
    if (terminal) {
      return { kind: 'bad', text: 'Polling stopped - open this page on the bound address', target: null, note: null };
    }
    var known = current || lastVerdict;
    if (!known) {
      var why = lastRefreshFailed() ? refreshFailureNote() : null;
      return { kind: 'idle', text: 'Checking routing state', target: null, note: why };
    }
    return {
      kind: VERDICT_KIND[known.severity],
      text: known.text,
      target: known.target,
      note: current ? null : staleVerdictNote(known.as_of)
    };
  }

  var REFRESH_FAILURE_NOTES = {
    overloaded: 'the daemon is overloaded',
    timeout: 'the status request timed out'
  };
  var UNREACHABLE_NOTE = 'the daemon cannot be reached';

  // Whether the attention source's last refresh failed, leaving it stale or
  // dead. Every failure counts (no answer, an overload, a timeout).
  function lastRefreshFailed() {
    var state = SOURCES.attention.state;
    return state === 'stale' || state === 'dead';
  }

  // Any state other than a failed refresh means the daemon answered without
  // a current verdict.
  function staleVerdictNote(asOf) {
    var why = lastRefreshFailed() ? refreshFailureNote() : 'verdict not current';
    return 'as of ' + verdictTime(asOf) + ' - ' + why;
  }

  // Why that refresh failed, from its transport kind: an overload and a
  // timeout name themselves, and any other failure reads as no answer.
  function refreshFailureNote() {
    return Object.prototype.hasOwnProperty.call(REFRESH_FAILURE_NOTES, aggregateFailureKind)
      ? REFRESH_FAILURE_NOTES[aggregateFailureKind] : UNREACHABLE_NOTE;
  }

  function verdictTime(asOf) {
    var then = asOf ? new Date(asOf) : null;
    return (then && !isNaN(then.getTime())) ? then.toLocaleTimeString() : 'an unknown time';
  }

  // The link to the view a verdict points at, labelled from that view. A
  // token naming no registered view gets no link, and neither does the view
  // already open. A link already showing the same view is kept, so keyboard
  // focus on it survives the refresh.
  function syncVerdictLink(target) {
    var slot = el('verdict-link');
    var view = (typeof target === 'string') ? viewFor(target) : null;
    if (view && view.id === activeTab) { view = null; }
    var kept = slot.firstElementChild;
    if (kept && (!view || kept.getAttribute('data-arg') !== view.id)) {
      slot.removeChild(kept);
      kept = null;
    }
    if (!view) { return; }
    if (!kept) {
      kept = viewLink(view);
      slot.appendChild(kept);
    }
    kept.setAttribute('href', viewLinkHash(view.id));
  }

  // req/span for the ACTIVE tab. A count is shown only beside the span it
  // covers: a query-backed view's count is its own QUERY's totals, read for
  // the selection the picker shows. Until that answer applies, and on a view
  // whose span is not a window (whatever QUERY it reads for its own use), the
  // span stands alone; no other read stands in for it, since none covers the
  // same span and scope.
  function verdictStats() {
    var view = viewFor(activeTab);
    var span = view.span || WINDOW_SPAN[selectedWindow];
    var source = view.span ? null : viewQuerySource(view);
    var answer = source ? queryViewOf(source) : null;
    return answer ? humanCount(answer.totals.requests) + ' req ' + span : span;
  }

  // The poll indicator reports the AGE of the data on screen, not a countdown to
  // the next round: how stale the figures are is the fact an operator needs, and
  // the cadence is a fixed 5s they cannot change. The age comes from the ACTIVE
  // tab's primary source, so it describes the numbers actually visible; the 1s
  // countdown interval re-renders it, so it advances rather than sitting frozen
  // between rounds.
  function renderPollIndicator() {
    var poll = el('poll');
    poll.classList.remove('poll--warn', 'poll--dead', 'poll--idle');
    var label;
    if (terminal) {
      poll.classList.add('poll--dead');
      label = 'polling stopped';
    } else if (backingOff) {
      poll.classList.add('poll--warn');
      label = 'reconnecting - stale ' + activeAgeText();
    } else if (lastSuccess === null) {
      poll.classList.add('poll--idle');
      label = 'polling';
    } else {
      label = 'live - ' + activeAgeText();
    }
    el('poll-label').textContent = label;
  }

  // The as_of age of the active tab's primary source as a humane phrase, or a
  // word when there is no usable stamp to age against. A stamp ahead of the
  // local clock beyond the skew tolerance says so rather than reading as a
  // negative age.
  function activeAgeText() {
    var asOf = SOURCES[viewFor(activeTab).sources[0]].asOf;
    if (!asOf) { return 'age unknown'; }
    var then = new Date(asOf);
    if (isNaN(then.getTime())) { return 'age unknown'; }
    var ageSec = Math.round((Date.now() - then.getTime()) / 1000);
    if (ageSec < -SKEW_TOLERANCE_SEC) { return 'clock skew'; }
    return relAge(Math.max(0, ageSec));
  }

  // ---- tab badges ------------------------------------------------------

  // The badge for a source, computed ONLY from a fresh successful render.
  // Counts a genuine problem: health targets with a non-closed
  // circuit or an eligibility reason (once each), model lanes whose state is
  // not ok, the daemon's active
  // attention items (broken + degraded; recovered ones never count). Config
  // carries no error concept, so it never badges. A zero count returns null
  // so a clean source shows nothing.
  function sourceBadge(name, data) {
    if (name === 'health') {
      var open = (data.targets || []).filter(function (t) {
        return (t.circuit && t.circuit !== 'closed') || !!(t.eligibility && t.eligibility.reason);
      }).length;
      return open > 0 ? { label: String(open), kind: 'warn' } : null;
    }
    if (name === 'models') {
      var lanes = modelProblemLanes(data.rows);
      return lanes > 0 ? { label: String(lanes), kind: 'warn' } : null;
    }
    if (name === 'attention') {
      var counts = attentionTally(data.counts);
      var active = counts.broken + counts.degraded;
      if (active === 0) { return null; }
      return { label: String(active), kind: counts.broken > 0 ? 'error' : 'warn' };
    }
    return null;
  }

  // The model lanes whose state is not ok: the Models badge, and the lane
  // figure Doctor shows beside its problem count, so the two never differ.
  function modelProblemLanes(rows) {
    return (rows || []).filter(function (row) { return row && row.state !== 'ok'; }).length;
  }

  var ATTENTION_COUNT_KEYS = ['broken', 'degraded', 'recovered', 'doctor_fail'];

  // The attention panel's tallies, each a whole count the daemon always
  // sends. A tally that is missing or not a count throws, so the payload
  // lands as invalid_payload: a hidden badge would read as nothing wrong.
  function attentionTally(counts) {
    if (!isPlainObject(counts)) { throw new Error('attention payload carries no counts'); }
    ATTENTION_COUNT_KEYS.forEach(function (key) {
      var n = counts[key];
      if (!isFiniteNumber(n) || n < 0 || Math.floor(n) !== n) {
        throw new Error('attention count ' + key + ' is not a count');
      }
    });
    return counts;
  }

  // Surface each tab's badge from its badge source (its `badge`, else its
  // primary), so a problem is visible without switching to that tab. A
  // source whose section failed to render badges as unusable rather than
  // carrying the badge computed from the last payload that did render.
  function syncTabBadges() {
    VIEWS.forEach(function (view) {
      var rec = SOURCES[view.badge || view.sources[0]];
      setTabBadge(view.id, effectiveState(rec) === 'invalid_payload'
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
    badge.className = 'tab-badge tab-badge--' + state.kind + (state.dimmed ? ' tab-badge--dimmed' : '');
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

