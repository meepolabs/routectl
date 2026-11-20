  // ---- tab:health ------------------------------------------------------

  // The Models view's dispatch target cards, wrapped in their own
  // `safeSection` against the health record. The view that composes them is
  // in dash_80_views.js.

  function healthTargetSection(rec) {
    var wrap = routingSection('Dispatch targets', stateCard(rec)
      ? 'Health data unavailable - target state is not shown'
      : 'reported now - current state, not a windowed aggregate');
    wrap.appendChild(safeSection(rec, buildTargetHealth));
    return wrap;
  }

  function buildTargetHealth(rec) {
    if (!rec.data || !Array.isArray(rec.data.targets)) {
      throw new Error('health payload carries no targets');
    }
    var groups = collapsedTargets(rec.data.targets);
    if (!groups.length) {
      return emptyCard('No dispatch targets configured',
        'A configured model becomes a dispatch target. Add one and its breaker state, ' +
        'rate-limit headroom, and last settled outcome appear here.',
        ['current state']);
    }
    var nowMs = panelNowMs(rec);
    var negatives = learnedByLane(rec.data.learned_negatives);
    var attention = groups.filter(function (g) { return targetPhase(g) === 'attention'; });
    var healthy = groups.filter(function (g) { return targetPhase(g) === 'healthy'; });
    var unknown = groups.filter(function (g) { return targetPhase(g) === 'unknown'; });
    if (!attention.length && !healthy.length) { return unprobedEmpty(unknown.length); }
    var stack = tabStack();
    if (attention.length) {
      stack.appendChild(targetGroupList('Needs attention',
        attention.length + phrase(attention.length, ' target', ' targets') + ' not serving normally',
        attention, negatives, nowMs));
    }
    if (healthy.length) {
      stack.appendChild(targetGroupList('Healthy',
        healthy.length + phrase(healthy.length, ' target', ' targets') + ' with a closed breaker and an ok last outcome',
        healthy, negatives, nowMs));
    }
    if (unknown.length) {
      stack.appendChild(targetGroupList('Not observed yet',
        'configured, nothing dispatched to them yet - unknown, never healthy',
        unknown, negatives, nowMs));
    }
    return stack;
  }

  function phrase(n, one, many) { return n === 1 ? one : many; }

  // Every target is configured but none has a settled outcome: a welcoming
  // state, never a list of cards claiming health nobody has observed.
  function unprobedEmpty(count) {
    return emptyCard('No target has been probed yet',
      count + phrase(count, ' target is', ' targets are') + ' configured and ready. Each one ' +
      'reports its breaker state and last settled outcome here as soon as a request is ' +
      'dispatched to it.',
      [count + phrase(count, ' target configured', ' targets configured'), 'none probed yet']);
  }

  // One card per NICKNAME, collapsing a pooled model's seats through the SAME
  // worst-circuit rule Traffic's routing chains use (`targetsByNickname`): an open
  // breaker on one seat is the fact an operator needs, not the closed one
  // beside it. The seats themselves are kept so a card can name how many it
  // speaks for and which learned negatives belong to it.
  function collapsedTargets(targets) {
    var byNickname = targetsByNickname(targets);
    var order = [];
    var seen = Object.create(null);
    (targets || []).forEach(function (t) {
      if (!t || !t.nickname || seen[t.nickname]) { return; }
      seen[t.nickname] = true;
      var group = byNickname[t.nickname];
      order.push({
        nickname: t.nickname,
        worst: group.worst,
        count: group.count,
        members: group.members
      });
    });
    return order;
  }

  // Learned rows keyed by the lane they were recorded against
  // (`provider#upstream`). A target joins them through its `learned_lane`,
  // never its runtime `state_key`: every nickname on one lane shares the
  // lane's negatives, and a card lists those of ITS OWN seats' lanes and
  // nothing else. Only `broken` verdicts are negatives -- a `verified`
  // positive shares the list and must not be read as one.
  function learnedByLane(rows) {
    var map = Object.create(null);
    (rows || []).forEach(function (row) {
      if (!row || !row.state_key || row.verdict !== 'broken') { return; }
      if (!map[row.state_key]) { map[row.state_key] = []; }
      map[row.state_key].push(row);
    });
    return map;
  }

  function targetGroupList(title, hint, groups, negatives, nowMs) {
    var wrap = document.createElement('div');
    wrap.className = 'hgroup';
    wrap.appendChild(sectionHead(title, hint));
    var list = document.createElement('div');
    list.className = 'hlist';
    groups.forEach(function (group) {
      var card = targetCard(group, negatives, nowMs);
      card.id = 'health-target-' + idKey(group.nickname);
      list.appendChild(card);
    });
    wrap.appendChild(list);
    return wrap;
  }

  function targetCard(group, negatives, nowMs) {
    var phase = targetPhase(group);
    var cardEl = document.createElement('div');
    cardEl.className = 'hcard hcard--' + phase;
    cardEl.appendChild(targetCardHead(group, phase, nowMs));
    cardEl.appendChild(targetCardGrid(group, negatives, nowMs));
    return cardEl;
  }

  function targetCardHead(group, phase, nowMs) {
    var head = document.createElement('div');
    head.className = 'hcard-head';
    head.appendChild(statePill(group, phase));
    head.appendChild(targetIdentity(group, phase, nowMs));
    head.appendChild(targetAnchors(group, nowMs));
    return head;
  }

  // The state pill carries the WORST circuit across the seats, or the neutral
  // unknown token when nothing has been observed.
  function statePill(group, phase) {
    var pill = circuitPill(group, phase);
    pill.classList.add('hstate');
    return pill;
  }

  function targetIdentity(group, phase, nowMs) {
    var wrap = document.createElement('div');
    wrap.className = 'hident';
    var name = document.createElement('span');
    name.className = 'hname';
    name.textContent = group.nickname;
    name.title = group.nickname;
    wrap.appendChild(name);
    var sub = document.createElement('span');
    sub.className = 'hsub';
    sub.textContent = targetSubText(group);
    wrap.appendChild(sub);
    var reason = targetReasonText(group, phase, nowMs);
    if (reason) {
      var line = document.createElement('span');
      line.className = 'hreason';
      line.textContent = reason;
      wrap.appendChild(line);
    }
    return wrap;
  }

  function targetSubText(group) {
    var target = group.worst;
    var parts = [target.provider_name, target.upstream].filter(Boolean);
    if (group.count > 1) {
      parts.push(group.count + phrase(group.count, ' seat', ' seats'));
    }
    return parts.join(' - ');
  }

  function targetReasonText(group, phase, nowMs) {
    var blocked = blockedSeats(group);
    if (blocked.length) {
      return blocked.length + ' of ' + group.count + phrase(group.count, ' seat', ' seats') +
        ' not eligible';
    }
    if (phase === 'unknown') { return 'nothing dispatched to it yet'; }
    var target = group.worst;
    var parts = [];
    if (target.circuit === 'open' && target.open_since_ms !== null && target.open_since_ms !== undefined) {
      parts.push('breaker open for ' + ageSince(target.open_since_ms, nowMs));
    }
    if (target.half_open_probe_in_flight) { parts.push('recovery probe in flight'); }
    return parts.join(' - ');
  }

  // The two anchors of the card. `last ok` is only knowable when the seat's
  // LAST settled outcome was itself ok -- the health source carries one
  // outcome per seat, not a history -- so anything else reads unknown rather
  // than claiming a moment nobody recorded.
  function targetAnchors(group, nowMs) {
    var wrap = document.createElement('div');
    wrap.className = 'hanchors';
    wrap.appendChild(anchorCell('p50', latencyAnchor()));
    wrap.appendChild(anchorCell('last ok', lastOkAnchor(group, nowMs)));
    return wrap;
  }

  // Neither source behind this tab carries per-target latency: the health
  // panel reports gate state and the usage panel aggregates by group, not by
  // target. The anchor says so rather than borrowing another tab's figure.
  function latencyAnchor() {
    var span = faintFigure('no data');
    span.title = 'per-target latency is not reported by the health or usage panel';
    return span;
  }

  function lastOkAnchor(group, nowMs) {
    var stamps = group.members
      .filter(function (t) {
        return t.last_outcome === 'ok' && t.last_outcome_at_ms !== null && t.last_outcome_at_ms !== undefined;
      })
      .map(function (t) { return Number(t.last_outcome_at_ms); });
    if (!stamps.length) { return faintFigure('unknown'); }
    return figure(ageSince(Math.max.apply(null, stamps), nowMs), null, null);
  }

  function anchorCell(label, valueNode) {
    var wrap = document.createElement('div');
    wrap.className = 'hanchor';
    var l = document.createElement('span');
    l.className = 'hanchor-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'hanchor-value';
    v.appendChild(valueNode);
    wrap.appendChild(l);
    wrap.appendChild(v);
    return wrap;
  }

  function targetCardGrid(group, negatives, nowMs) {
    var grid = document.createElement('div');
    grid.className = 'hgrid';
    grid.appendChild(gridCell('rate limit', rpmValue(group.worst)));
    grid.appendChild(gridCell('last outcome', lastOutcomeValue(group.worst, nowMs)));
    grid.appendChild(gridCell('circuit', circuitValue(group.worst, nowMs)));
    grid.appendChild(gridCell('learned negatives', negativeChips(group, negatives, nowMs)));
    var elig = gridCell('eligibility', eligibilityCell(group, nowMs));
    if (blockedSeats(group).length) { elig.classList.add('hcell--wide'); }
    grid.appendChild(elig);
    return grid;
  }

  // One line per seat that cannot be dispatched to: its reason, the cause
  // of an unusable credential, how long ago it began, and the fix to run.
  function eligibilityCell(group, nowMs) {
    var blocked = blockedSeats(group);
    if (!blocked.length) { return faintFigure('eligible'); }
    var wrap = document.createElement('span');
    wrap.className = 'helig';
    blocked.forEach(function (t) { wrap.appendChild(eligibilityRow(t, group.count > 1, nowMs)); });
    return wrap;
  }

  function eligibilityRow(t, named, nowMs) {
    var e = t.eligibility;
    var line = document.createElement('span');
    line.className = 'helig-row neg';
    var parts = [];
    if (named) { parts.push(t.seat_label || t.state_key); }
    parts.push(e.cause ? e.reason + ' (' + e.cause + ')' : e.reason);
    if (e.since_ms !== null && e.since_ms !== undefined) {
      var since = ageSince(e.since_ms, nowMs);
      if (since !== '-') { parts.push('since ' + since + ' ago'); }
    }
    line.appendChild(document.createTextNode(parts.join(' - ')));
    if (e.fix) {
      line.appendChild(copyableCode('helig-fix', e.fix));
    }
    return line;
  }

  function gridCell(label, valueNode) {
    var wrap = document.createElement('div');
    wrap.className = 'hcell';
    var l = document.createElement('span');
    l.className = 'hcell-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'hcell-value';
    v.appendChild(valueNode);
    wrap.appendChild(l);
    wrap.appendChild(v);
    return wrap;
  }

  // Projected available tokens, with NO bar: the panel reports the level, not
  // the ceiling, and a bar without a denominator would invent one. An absent
  // level is the unlimited policy, not a missing reading.
  function rpmValue(target) {
    if (target.rpm_available === null || target.rpm_available === undefined) {
      return faintFigure('unlimited');
    }
    return figure(String(Math.floor(num0(target.rpm_available))), 'rpm left', null);
  }

  function lastOutcomeValue(target, nowMs) {
    if (!target.last_outcome) { return faintFigure('no settled outcome yet'); }
    var node = labelCell('outcome', target.last_outcome);
    var wrap = document.createElement('span');
    if (node instanceof Node) { wrap.appendChild(node); }
    else { wrap.appendChild(document.createTextNode(node)); }
    if (target.last_outcome !== 'ok') { wrap.classList.add('neg'); }
    if (target.last_outcome_at_ms !== null && target.last_outcome_at_ms !== undefined) {
      var seen = ageSince(target.last_outcome_at_ms, nowMs);
      if (seen !== '-') {
        var age = document.createElement('span');
        age.className = 'hage';
        age.textContent = seen + ' ago';
        wrap.appendChild(age);
      }
    }
    return wrap;
  }

  function circuitValue(target, nowMs) {
    if (target.circuit === 'closed') { return faintFigure('closed'); }
    var parts = [];
    if (target.open_since_ms !== null && target.open_since_ms !== undefined) {
      parts.push('open for ' + ageSince(target.open_since_ms, nowMs));
    }
    if (target.half_open_probe_in_flight) { parts.push('probe in flight'); }
    return figure(parts.length ? parts.join(' - ') : labelFor('circuit', target.circuit).label, null, null);
  }

  function negativeChips(group, negatives, nowMs) {
    var wrap = document.createElement('span');
    wrap.className = 'hchips';
    var seen = Object.create(null);
    group.members.forEach(function (t) {
      ((t.learned_lane && negatives[t.learned_lane]) || []).forEach(function (row) {
        if (seen[row.capability_key]) { return; }
        seen[row.capability_key] = true;
        wrap.appendChild(negativeChip(row, nowMs));
      });
    });
    if (!wrap.childNodes.length) { wrap.appendChild(faintFigure('none')); }
    return wrap;
  }

  function negativeChip(row, nowMs) {
    var chip = document.createElement('span');
    chip.className = 'hchip';
    chip.textContent = row.capability_key;
    var seen = ageSince(row.last_seen_ms, nowMs);
    chip.title = row.capability_key + ' - ' + row.source + ' evidence' +
      (seen === '-' ? '' : ', last seen ' + seen + ' ago') + ', value ' + (row.value || '-');
    return chip;
  }
  // ---- end tab:health --------------------------------------------------

