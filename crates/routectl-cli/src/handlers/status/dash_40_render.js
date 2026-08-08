  // ---- render dispatch -------------------------------------------------

  // NO automated runtime harness covers this part. Per-pass fault
  // reconciliation and the state it drives are verified BY HAND only -- see
  // dashboard-manual-checklist.md beside this source, and run it before
  // shipping a change here.

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
      code: 'expected ' + expectedVersion(name) + ', received ' + received,
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
    // The overlay host is emptied here and refilled by the one section that
    // owns it, so an overlay whose owning section is no longer drawn -- another
    // tab, a state card in its place -- cannot outlive it.
    var host = el('modal-host');
    if (host) { host.replaceChildren(); }
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
      var note = lastSuccess
        ? 'last success at ' + lastSuccess.toLocaleTimeString()
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
    clearInterval(ageTimer);
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
    var asOf = SOURCES[TAB_SOURCES[activeTab][0]].asOf;
    if (!asOf) { return 'age unknown'; }
    var then = new Date(asOf);
    if (isNaN(then.getTime())) { return 'age unknown'; }
    var ageSec = Math.round((Date.now() - then.getTime()) / 1000);
    if (ageSec < -SKEW_TOLERANCE_SEC) { return 'clock skew'; }
    return relAge(Math.max(0, ageSec));
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

