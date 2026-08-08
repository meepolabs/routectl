  // ---- tab:routing -----------------------------------------------------

  // Circuit phases ranked worst-first. One chain member can resolve to several
  // health targets (one per seat), and the member's live state is the worst of
  // them: an open breaker on one seat is the fact an operator needs, not the
  // closed one beside it.
  var CIRCUIT_RANK = { open: 3, half_open_ready: 2, closed: 1 };

  var CIRCUIT_DOT = {
    closed: 'tdot--ok',
    open: 'tdot--bad',
    half_open_ready: 'tdot--warn'
  };

  // The first multi-source tab: the configured chains come from config, the
  // step attribution from the usage ledger, and the live target state from
  // health. Each section is wrapped in its OWN `safeSection` against its OWN
  // source record, so health going dark costs the state list alone and a
  // ledger outage costs the estimate alone -- the chains keep rendering
  // either way. `rec` is the config record (the tab's primary source): with
  // no chains there is nothing for the other two sections to describe, so
  // they are not built at all rather than rendered empty.
  function buildRouting(rec) {
    var stack = tabStack();
    stack.appendChild(routingHead());
    stack.appendChild(safeSection(rec, buildChainList));
    var chains = liveChains(rec);
    if (!chains.length) { return stack; }
    stack.appendChild(targetStateSection(chains));
    stack.appendChild(attributionSection(chains));
    return stack;
  }

  // The configured chains, or an empty list when config carries none to
  // describe. Keyed on the RETAINED payload rather than on `live`, so a source
  // marked stale (503, last-known values kept) still gets its derived sections
  // -- the whole pane is already dimmed in that state. The chain section
  // itself reports the malformed case, so this only decides whether the
  // derived sections have anything to describe.
  function liveChains(rec) {
    if (stateCard(rec) || !rec.data || !Array.isArray(rec.data.aliases)) { return []; }
    return rec.data.aliases;
  }

  // The span the estimate covers comes from the all-history panel's OWN window
  // token, never from the picker: the panel decides the window it aggregated,
  // and claiming any other span is the one thing this tab must not do. The
  // read is issued at window=all, so a token that is anything else means the
  // page is looking at a payload it did not ask for -- it says so rather than
  // relabeling it.
  function attributedSpan() {
    var rec = SOURCES[USAGE_ALL_SOURCE];
    if (stateCard(rec) || !rec.data) { return 'the recorded history'; }
    return WINDOW_SPAN[rec.data.window] || 'the recorded window';
  }

  // The tab header. The approximation is stated HERE, in the header and in a
  // persistent note -- never behind a hover -- and the note names exactly what
  // the derivation cannot know. The span is ALL HISTORY on this tab and the
  // window picker is inert here, so the header says which span the figures
  // below actually cover rather than letting the picker imply another.
  function routingHead() {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead('Fallback chains',
      'Approximate attribution - ' + attributedSpan()));
    var lead = document.createElement('p');
    lead.className = 'footnote';
    lead.textContent = 'Each alias tries its steps in order; a step is used only when every step before it failed.';
    wrap.appendChild(lead);
    var note = document.createElement('p');
    note.className = 'footnote';
    note.textContent = 'Derived from recorded alias, served target, and fallback counts; ' +
      'does not identify the exact attempted step for every request. ' +
      'Attribution covers all recorded history and does not follow the window picker.';
    wrap.appendChild(note);
    return wrap;
  }

  // A titled section whose BODY is filled by the caller. The head sits outside
  // the section boundary so a source that is down still says which reading is
  // missing instead of showing a bare error box.
  function routingSection(title, hint) {
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead(title, hint));
    return wrap;
  }

  // ---- routing: the configured chains (authoritative, from config) -----

  function buildChainList(rec) {
    var chains = (rec.data || {}).aliases;
    if (!Array.isArray(chains)) {
      throw new Error('config payload carries no alias chains');
    }
    if (!chains.length) {
      return emptyCard('No fallback chains configured',
        'An alias with more than one target gets a fallback chain. Configure one and its ' +
        'steps -- and the traffic attributed to them -- appear here.',
        ['configured order']);
    }
    var providers = providersByNickname(rec.data.models);
    var list = document.createElement('div');
    list.className = 'chainlist';
    chains.forEach(function (entry) { list.appendChild(chainCard(entry, providers)); });
    return list;
  }

  // Nickname -> provider, from the SAME config payload the chains came from, so
  // a step's provider is as authoritative as its position.
  function providersByNickname(models) {
    var map = Object.create(null);
    (models || []).forEach(function (m) {
      if (m && m.nickname) { map[m.nickname] = m.provider; }
    });
    return map;
  }

  function chainCard(entry, providers) {
    var steps = Array.isArray(entry.chain) ? entry.chain : [];
    var cardEl = document.createElement('div');
    cardEl.className = 'chain';

    var head = document.createElement('div');
    head.className = 'chain-head';
    var alias = document.createElement('span');
    alias.className = 'chain-alias';
    alias.textContent = entry.alias;
    alias.title = entry.alias;
    head.appendChild(alias);
    var meta = document.createElement('span');
    meta.className = 'chain-meta';
    meta.textContent = steps.length === 1
      ? 'one target - no fallback configured'
      : steps.length + ' steps in order';
    head.appendChild(meta);
    cardEl.appendChild(head);

    var row = document.createElement('div');
    row.className = 'chain-steps';
    steps.forEach(function (model, i) {
      if (i > 0) { row.appendChild(stepSep()); }
      row.appendChild(stepCard(i, model, providers[model]));
    });
    cardEl.appendChild(row);
    return cardEl;
  }

  function stepSep() {
    var sep = document.createElement('span');
    sep.className = 'step-sep';
    sep.textContent = 'then';
    return sep;
  }

  function stepCard(index, model, provider) {
    var step = document.createElement('div');
    step.className = 'chain-step';
    var n = document.createElement('span');
    n.className = 'step-n';
    n.textContent = String(index + 1);
    var name = document.createElement('span');
    name.className = 'step-model';
    name.textContent = model;
    name.title = model;
    var prov = document.createElement('span');
    prov.className = 'step-prov';
    if (provider) {
      prov.textContent = provider;
    } else {
      prov.classList.add('mag-zero');
      prov.textContent = 'no catalog row';
    }
    step.appendChild(n);
    step.appendChild(name);
    step.appendChild(prov);
    return step;
  }

  // ---- routing: live target state (authoritative, from health) ---------

  // Deliberately its OWN section rather than a dot inside each step card: the
  // live state is a health reading, and confining it here is what lets a dark
  // health source degrade to one state card while the chains and the estimate
  // keep rendering. The hint follows what `safeSection` will actually do with
  // the record, so it never promises a list that a state card replaced (nor
  // cries unavailable over retained last-known values).
  function targetStateSection(chains) {
    var rec = SOURCES.health;
    var wrap = routingSection('Live target state', stateCard(rec)
      ? 'health unavailable - live target state is not shown'
      : 'reported now - never part of the estimate below');
    wrap.appendChild(safeSection(rec, function (live) {
      return targetStateList(live, chains);
    }));
    return wrap;
  }

  function targetStateList(rec, chains) {
    if (!rec.data) { throw new Error('health payload carries no targets'); }
    var nowMs = panelNowMs(rec);
    var byNickname = targetsByNickname(rec.data.targets);
    var list = document.createElement('div');
    list.className = 'statelist';
    chainMembers(chains).forEach(function (model) {
      list.appendChild(targetStateRow(model, byNickname[model], nowMs));
    });
    return list;
  }

  // Every distinct chain member, in first-appearance order. Deduped: a model
  // shared by two aliases is one target with one state, and listing it twice
  // would show the same fact twice.
  function chainMembers(chains) {
    var seen = Object.create(null);
    var out = [];
    chains.forEach(function (entry) {
      (Array.isArray(entry.chain) ? entry.chain : []).forEach(function (model) {
        if (seen[model]) { return; }
        seen[model] = true;
        out.push(model);
      });
    });
    return out;
  }

  // Seats collapsed per nickname: the worst circuit across them, how many they
  // are, and the seats themselves. The members are carried because the phase
  // rule reads every seat's settled outcome, not just the worst one's.
  function targetsByNickname(targets) {
    var map = Object.create(null);
    (targets || []).forEach(function (t) {
      if (!t || !t.nickname) { return; }
      var found = map[t.nickname];
      if (!found) {
        map[t.nickname] = { worst: t, count: 1, members: [t] };
        return;
      }
      found.count += 1;
      found.members.push(t);
      if (circuitRank(t.circuit) > circuitRank(found.worst.circuit)) { found.worst = t; }
    });
    return map;
  }

  function circuitRank(circuit) {
    return CIRCUIT_RANK[circuit] || 0;
  }

  // The ONE target-state rule, shared by the Routing state list and the Health
  // target cards so the two tabs can never disagree about the same target. A
  // target with no settled outcome anywhere is `unknown`, NEVER healthy: no
  // observation is not evidence of health. Healthy means a closed breaker AND
  // an ok last outcome on every seat that has reported one.
  function targetPhase(group) {
    if (group.worst.circuit !== 'closed') { return 'attention'; }
    var settled = group.members.filter(function (t) { return !!t.last_outcome; });
    if (!settled.length) { return 'unknown'; }
    var failing = settled.some(function (t) { return t.last_outcome !== 'ok'; });
    return failing ? 'attention' : 'healthy';
  }

  // The circuit pill both tabs render: the neutral unknown token when nothing
  // has been observed, the worst seat's circuit otherwise.
  function circuitPill(group, phase) {
    return phase === 'unknown'
      ? tok('circuit', 'unknown')
      : tokLabeled('circuit', 'circuit', group.worst.circuit);
  }

  var PHASE_DOT = { attention: 'tdot--bad', healthy: 'tdot--ok', unknown: 'tdot--unknown' };

  // The dot follows the PHASE, not the raw circuit: a closed breaker nobody has
  // probed is unknown, and a green dot beside it would contradict Health.
  function phaseDot(group, phase) {
    if (phase === 'unknown') { return PHASE_DOT.unknown; }
    return CIRCUIT_DOT[group.worst.circuit] || PHASE_DOT[phase] || 'tdot--unknown';
  }

  // A configured member health has never dispatched to is `unknown`, never
  // healthy: no observation is not evidence of health. The dot, the pill, and
  // the reason all read the SHARED `targetPhase`, so this row and the Health
  // card for the same target can never disagree.
  function targetStateRow(model, found, nowMs) {
    var row = document.createElement('div');
    row.className = 'staterow';
    var phase = found ? targetPhase(found) : 'unknown';
    var dot = document.createElement('span');
    dot.className = 'tdot ' + (found ? phaseDot(found, phase) : 'tdot--unknown');
    dot.setAttribute('aria-hidden', 'true');
    row.appendChild(dot);

    var name = document.createElement('span');
    name.className = 'state-name';
    name.textContent = model;
    name.title = model;
    row.appendChild(name);

    var meta = document.createElement('span');
    meta.className = 'state-meta';
    if (!found) {
      meta.classList.add('mag-zero');
      meta.textContent = 'unknown - nothing dispatched to it yet';
      row.appendChild(meta);
      return row;
    }
    row.appendChild(circuitPill(found, phase));
    meta.textContent = targetStateText(found, phase, nowMs);
    row.appendChild(meta);
    return row;
  }

  function targetStateText(found, phase, nowMs) {
    var target = found.worst;
    var parts = [];
    if (found.count > 1) { parts.push(found.count + ' targets'); }
    if (phase === 'unknown') {
      parts.push('nothing dispatched to it yet');
    } else {
      parts.push(target.last_outcome
        ? 'last ' + labelFor('outcome', target.last_outcome).label
        : 'no settled outcome yet');
    }
    if (target.circuit === 'open' && target.open_since_ms !== null && target.open_since_ms !== undefined) {
      parts.push('open for ' + ageSince(target.open_since_ms, nowMs));
    }
    return parts.join(' - ');
  }

  // ---- routing: estimated step attribution (from the usage ledger) -----

  function attributionSection(chains) {
    var rec = SOURCES[USAGE_ALL_SOURCE];
    var wrap = routingSection('Estimated step attribution', attributionHint(rec));
    wrap.appendChild(safeSection(rec, function (live) {
      return buildAttribution(live, chains);
    }));
    return wrap;
  }

  function attributionHint(rec) {
    if (stateCard(rec)) {
      return 'usage unavailable - no traffic to attribute';
    }
    return 'estimated, not measured - over ' + attributedSpan();
  }

  function buildAttribution(rec, chains) {
    if (!rec.data) { throw new Error('usage payload carries no groups'); }
    var derived = deriveStepTraffic(rec.data.groups, chains);
    if (derived.recorded <= 0) {
      return emptyCard('No traffic recorded yet',
        'These chains are configured and ready. Each step gets its share here as soon as ' +
        'requests are routed through the alias that owns it.',
        ['chains configured', 'nothing to attribute']);
    }
    var wrap = document.createElement('div');
    wrap.className = 'attr';
    wrap.appendChild(laterStepHeadline(derived));
    var list = document.createElement('div');
    list.className = 'attrlist';
    derived.aliases.forEach(function (entry) { list.appendChild(attrAlias(entry)); });
    wrap.appendChild(list);
    if (derived.unconfigured.aliases > 0) {
      wrap.appendChild(unconfiguredNote(derived.unconfigured));
    }
    return wrap;
  }

  // The ONE step-traffic derivation on this tab; no other render path
  // recomputes it. The ledger records which target SERVED a request, never
  // which step of the chain was attempted, so step traffic is attributed by
  // matching a group's served model back onto the configured chain. A
  // write-time step-index column on the usage ledger would make this exact and
  // retire the heuristic entirely; that column does not exist yet, so this is
  // the honest approximation, and keeping it in one function is what stops a
  // second, disagreeing one from growing.
  //
  // Returns, per alias: the per-step counts, the recorded models that match NO
  // chain member (the chain was edited after those rows were written -- never
  // dropped, never folded into step 0), and the attributed / later-step split.
  // Traffic recorded against an alias with no configured chain at all is
  // counted separately rather than discarded. Every figure this tab shows,
  // including the later-step headline, comes out of this record.
  function deriveStepTraffic(groups, chains) {
    var byAlias = Object.create(null);
    var order = [];
    chains.forEach(function (entry) {
      if (byAlias[entry.alias]) { return; }
      byAlias[entry.alias] = {
        alias: entry.alias,
        steps: (Array.isArray(entry.chain) ? entry.chain : []).map(function (model) {
          return { model: model, requests: 0 };
        }),
        offChain: [],
        attributed: 0,
        later: 0
      };
      order.push(entry.alias);
    });

    var unconfigured = { aliases: 0, requests: 0 };
    var seenUnconfigured = Object.create(null);
    var offChainTotal = 0;
    (groups || []).forEach(function (group) {
      var requests = num0(group.requests);
      var entry = byAlias[group.alias];
      if (!entry) {
        if (!seenUnconfigured[group.alias]) {
          seenUnconfigured[group.alias] = true;
          unconfigured.aliases += 1;
        }
        unconfigured.requests += requests;
        return;
      }
      var at = stepIndexOf(entry.steps, group.model);
      if (at < 0) {
        addOffChain(entry.offChain, group.model, requests);
        offChainTotal += requests;
        return;
      }
      entry.steps[at].requests += requests;
      entry.attributed += requests;
      if (at > 0) { entry.later += requests; }
    });

    var attributed = 0;
    var later = 0;
    order.forEach(function (alias) {
      attributed += byAlias[alias].attributed;
      later += byAlias[alias].later;
    });
    return {
      aliases: order.map(function (alias) { return byAlias[alias]; }),
      attributed: attributed,
      later: later,
      unconfigured: unconfigured,
      recorded: attributed + offChainTotal + unconfigured.requests
    };
  }

  // The FIRST chain position serving this model. A chain may legitimately
  // repeat a member, and the ledger cannot say which attempt served the
  // request, so the traffic lands on the first position and the duplicate
  // reads as unused rather than inventing a split.
  function stepIndexOf(steps, model) {
    for (var i = 0; i < steps.length; i++) {
      if (steps[i].model === model) { return i; }
    }
    return -1;
  }

  function addOffChain(list, model, requests) {
    for (var i = 0; i < list.length; i++) {
      if (list[i].model === model) {
        list[i].requests += requests;
        return;
      }
    }
    list.push({ model: model, requests: requests });
  }

  // An estimated count: the `~` says the figure was attributed rather than
  // measured.
  function approxCount(n) {
    return '~' + humanCount(n);
  }

  // An estimated share at WHOLE precision. A decimal place would imply an
  // accuracy this derivation does not have.
  function approxPctParts(part, total) {
    if (total <= 0) { return { v: '-', u: null }; }
    return { v: '~' + Math.round(num0(part) / total * 100), u: '%' };
  }

  // The headline reads off the STEP DISTRIBUTION above, not the ledger's own
  // fallback counter: the two disagree by construction, and this tab shows one
  // derivation and one number.
  function laterStepHeadline(derived) {
    var pct = approxPctParts(derived.later, derived.attributed);
    var head = document.createElement('div');
    head.className = 'attr-headline';
    var value = document.createElement('div');
    value.className = 'attr-figure';
    var fig = figure(pct.v, pct.u, null);
    if (derived.later <= 0) { fig.classList.add('mag-zero'); }
    value.appendChild(fig);
    head.appendChild(value);
    var note = document.createElement('div');
    note.className = 'attr-headnote';
    note.textContent = laterStepNote(derived);
    head.appendChild(note);
    return head;
  }

  function laterStepNote(derived) {
    if (derived.attributed <= 0) {
      return 'no recorded request could be attributed to a step of these chains';
    }
    if (derived.later <= 0) {
      return 'the first step served every attributed request';
    }
    return approxCount(derived.later) + ' of ' + approxCount(derived.attributed) +
      ' attributed requests took a later step';
  }

  function attrAlias(entry) {
    var wrap = document.createElement('div');
    wrap.className = 'attr-alias';
    var head = document.createElement('div');
    head.className = 'attr-aliashead';
    var name = document.createElement('span');
    name.className = 'attr-aliasname';
    name.textContent = entry.alias;
    name.title = entry.alias;
    head.appendChild(name);
    var total = document.createElement('span');
    total.className = 'attr-aliastotal';
    total.textContent = entry.attributed > 0
      ? approxCount(entry.attributed) + ' req attributed'
      : 'no traffic recorded';
    if (entry.attributed <= 0) { total.classList.add('mag-zero'); }
    head.appendChild(total);
    wrap.appendChild(head);
    // A chain with no traffic gets the welcoming line above and nothing else:
    // its steps are already listed as configured facts, and a column of zero
    // bars would read as a measurement.
    if (entry.attributed > 0) {
      entry.steps.forEach(function (step, i) {
        wrap.appendChild(attrStep(step, i, entry.attributed));
      });
    }
    entry.offChain.forEach(function (row) { wrap.appendChild(offChainNote(row)); });
    return wrap;
  }

  function attrStep(step, index, total) {
    var row = document.createElement('div');
    row.className = 'attr-step';
    var n = document.createElement('span');
    n.className = 'attr-stepn';
    n.textContent = 'step ' + (index + 1);
    row.appendChild(n);
    var model = document.createElement('span');
    model.className = 'attr-model';
    model.textContent = step.model;
    model.title = step.model;
    row.appendChild(model);
    var count = document.createElement('span');
    count.className = 'attr-count';
    var share = document.createElement('span');
    share.className = 'attr-pct';
    if (step.requests > 0) {
      var pct = approxPctParts(step.requests, total);
      count.textContent = approxCount(step.requests);
      share.appendChild(figure(pct.v, pct.u, null));
      row.appendChild(count);
      row.appendChild(share);
      row.appendChild(shareBar(num0(step.requests) / total * 100));
      return row;
    }
    count.classList.add('mag-zero');
    count.textContent = 'never used';
    row.appendChild(count);
    row.appendChild(share);
    return row;
  }

  // A recorded model that is not a member of the current chain. Surfaced as
  // its own line rather than dropped or folded into step 0: the traffic is
  // real, and the chain having changed since is exactly what an operator
  // needs told.
  function offChainNote(row) {
    var note = document.createElement('p');
    note.className = 'footnote attr-off';
    note.textContent = 'off chain - ' + (row.model || 'unrecorded target') + ' served ' +
      approxCount(row.requests) + ' req for this alias but is not a step of its current chain';
    return note;
  }

  function unconfiguredNote(unconfigured) {
    var note = document.createElement('p');
    note.className = 'footnote attr-off';
    note.textContent = unconfigured.aliases +
      (unconfigured.aliases === 1 ? ' alias with no configured chain carries ' : ' aliases with no configured chain carry ') +
      approxCount(unconfigured.requests) + ' recorded req, attributed to no step above';
    return note;
  }
  // ---- end tab:routing -------------------------------------------------

