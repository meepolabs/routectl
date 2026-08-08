  // ---- tab:health ------------------------------------------------------

  // The window a quota `utilization` is a fraction OF. The quota columns are
  // shared across vendors, so only `provider_kind` says which window a
  // fraction means; an unrecognized kind gets NO window name rather than an
  // assumed one. A two-part entry is joined by a CSS separator (this script
  // carries no slash-leading string literal -- see the mutation-channel scan
  // in page.rs).
  var QUOTA_WINDOW = {
    codex: ['weekly'],
    anthropic: ['5h', 'session'],
    'anthropic-api': ['5h', 'session']
  };

  // Two deliberate defaults live in the quota tiles below, both chosen rather
  // than derived, and both meant to be looked at with real provider data
  // before they harden:
  //   1. a line renders per POPULATED field -- the primary one for
  //      `utilization`, a second one only when `overage_utilization` arrived
  //      -- each labeled from `provider_kind`, never from provider identity;
  //   2. a snapshot is called STALE only when a KNOWN reset has already
  //      elapsed. With no reset to compare against, the tile reports its own
  //      age and says freshness is unknown; no provider time-to-live is
  //      invented to classify it.
  //
  // The second multi-source tab: target state comes from health, per-seat
  // quota from the usage ledger. Each section is wrapped in its OWN
  // `safeSection` against its OWN source record, so a dark health source
  // costs the target list alone and a dark ledger costs the quota tiles
  // alone.
  function buildHealth(rec) {
    var stack = tabStack();
    stack.appendChild(healthTargetSection(rec));
    stack.appendChild(healthQuotaSection());
    return stack;
  }

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
    var negatives = learnedByStateKey(rec.data.learned_negatives);
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
  // worst-circuit rule the Routing tab uses (`targetsByNickname`): an open
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

  // Learned rows keyed by the state key they were recorded against, so a
  // card lists the negatives of ITS OWN seats and nothing else. Only `broken`
  // verdicts are negatives -- a `verified` positive shares the list and must
  // not be read as one.
  function learnedByStateKey(rows) {
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
    groups.forEach(function (group) { list.appendChild(targetCard(group, negatives, nowMs)); });
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
    return grid;
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
      (negatives[t.state_key] || []).forEach(function (row) {
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
      (seen === '-' ? '' : ', last seen ' + seen + ' ago');
    return chip;
  }

  // ---- health: per-seat quota (from the usage ledger) ------------------

  function healthQuotaSection() {
    var rec = SOURCES.usage;
    var wrap = routingSection('Seat quota', stateCard(rec)
      ? 'usage unavailable - quota is not shown'
      : 'latest snapshot per credential seat - never summed across seats');
    wrap.appendChild(safeSection(rec, buildQuotaList));
    return wrap;
  }

  function buildQuotaList(rec) {
    if (!rec.data || !Array.isArray(rec.data.quota)) {
      throw new Error('usage payload carries no quota list');
    }
    if (!rec.data.quota.length) { return quotaNotReported(); }
    var nowMs = panelNowMs(rec);
    var list = document.createElement('div');
    list.className = 'qlist';
    rec.data.quota.forEach(function (q) { list.appendChild(quotaTile(q, nowMs)); });
    return list;
  }

  // No seat reported a quota-bearing row. An absence is stated as one -- never
  // as a zero utilization, and never as a bar.
  function quotaNotReported() {
    var note = document.createElement('p');
    note.className = 'footnote';
    note.textContent = 'Quota not reported - no credential seat has recorded a quota snapshot.';
    return note;
  }

  // The instant the quota figures are read against: the usage panel's OWN
  // as_of (see panelNowMs).
  //
  // ONE tile per quota row, keyed by the seat it belongs to. Rows are never
  // merged, summed, averaged, or reduced to a headline: a seat's quota is a
  // fact about that credential and nothing else.
  function quotaTile(q, nowMs) {
    var tile = document.createElement('div');
    tile.className = 'qtile';
    tile.appendChild(quotaHead(q, nowMs));
    tile.appendChild(quotaLines(q));
    tile.appendChild(quotaMeta(q, nowMs));
    return tile;
  }

  function quotaHead(q, nowMs) {
    var head = document.createElement('div');
    head.className = 'qtile-head';
    var seat = document.createElement('span');
    seat.className = 'qseat';
    if (q.seat) {
      seat.textContent = q.seat;
      seat.title = q.seat;
    } else {
      seat.classList.add('mag-zero');
      seat.textContent = 'Unknown legacy seat';
      seat.title = 'recorded before the seat column was populated, or a forwarded client credential';
    }
    head.appendChild(seat);
    if (q.provider_kind) {
      var kind = document.createElement('span');
      kind.className = 'qkind';
      kind.textContent = q.provider_kind;
      head.appendChild(kind);
    }
    head.appendChild(quotaFreshness(q, nowMs));
    return head;
  }

  // A reset instant at or before the panel's as_of has already passed, so the
  // snapshot describes a window that has since rolled: that is the ONE
  // staleness this page can prove. Without a reset to compare against there is
  // no provider time-to-live to lean on, so the tile reports its own age and
  // says freshness is unknown.
  function quotaFreshness(q, nowMs) {
    var span = document.createElement('span');
    span.className = 'qfresh';
    if (resetElapsed(q, nowMs)) {
      span.classList.add('qfresh--stale');
      span.textContent = 'STALE';
      span.title = 'the reported reset time has already passed: ' + fmtTs(q.reset_ms);
      return span;
    }
    span.classList.add('mag-zero');
    span.textContent = 'freshness unknown';
    span.title = 'no reset has elapsed and the provider declares no snapshot lifetime';
    return span;
  }

  function resetElapsed(q, nowMs) {
    if (q.reset_ms === null || q.reset_ms === undefined || Number(q.reset_ms) <= 0) { return false; }
    return isFinite(nowMs) && Number(q.reset_ms) <= nowMs;
  }

  // One line per POPULATED utilization field, never one per provider: the
  // primary window when `utilization` arrived, and a second line only when
  // `overage_utilization` did. A field the row does not carry produces no
  // line at all.
  function quotaLines(q) {
    var wrap = document.createElement('div');
    wrap.className = 'qlines';
    wrap.appendChild(hasNumber(q.utilization)
      ? quotaLine(windowLabel(q.provider_kind), q.utilization)
      : utilizationMissing());
    if (hasNumber(q.overage_utilization)) {
      wrap.appendChild(quotaLine(overageLabel(q.overage_status), q.overage_utilization));
    }
    return wrap;
  }

  function hasNumber(v) {
    return v !== null && v !== undefined && isFinite(Number(v));
  }

  // A measured zero is a reading, not an absence: it renders faint, with its
  // bar at zero. A MISSING utilization renders no bar and no percentage.
  function quotaLine(labelNode, fraction) {
    var line = document.createElement('div');
    line.className = 'qline';
    var label = document.createElement('span');
    label.className = 'qline-label';
    label.appendChild(labelNode);
    line.appendChild(label);
    var pct = Number(fraction) * 100;
    var value = document.createElement('span');
    value.className = 'qline-value';
    var text = pctFrac(fraction);
    var fig = figure(text.slice(0, text.length - 1), '%', null);
    if (pct <= 0) { fig.classList.add('mag-zero'); }
    value.appendChild(fig);
    line.appendChild(value);
    line.appendChild(shareBar(pct));
    return line;
  }

  function utilizationMissing() {
    var line = document.createElement('div');
    line.className = 'qline qline--missing';
    var label = document.createElement('span');
    label.className = 'qline-label';
    label.textContent = 'Utilization not reported';
    line.appendChild(label);
    return line;
  }

  // The window the primary fraction is OF, read from `provider_kind` alone. A
  // kind this build does not know gets no window name -- the raw kind stays
  // visible in the tile header, and inventing a window would be worse than
  // saying nothing.
  function windowLabel(providerKind) {
    var parts = QUOTA_WINDOW[providerKind];
    if (!parts) { return document.createTextNode('reported window'); }
    if (parts.length === 1) { return document.createTextNode(parts[0]); }
    var span = document.createElement('span');
    span.className = 'qwindow';
    span.appendChild(document.createTextNode(parts[0]));
    var sep = document.createElement('span');
    sep.className = 'qsep';
    sep.setAttribute('aria-hidden', 'true');
    span.appendChild(sep);
    span.appendChild(document.createTextNode(parts[1]));
    return span;
  }

  // The overage line is labeled by the status the row reported for it; with no
  // status token it is simply the overage window.
  function overageLabel(overageStatus) {
    if (!overageStatus) { return document.createTextNode('overage'); }
    var span = document.createElement('span');
    span.appendChild(document.createTextNode('overage'));
    span.appendChild(tokLabeled('qstatus', 'qstatus', overageStatus));
    return span;
  }

  // Claim and status render ONLY when the row carries them: a provider that
  // reports a utilization without a status header (codex) gets its figure and
  // its reset and no invented status, and is never classified degraded for
  // the omission.
  function quotaMeta(q, nowMs) {
    var pairs = [];
    if (q.claim) { pairs.push(['claim', labelCell('claim', q.claim)]); }
    if (q.status) { pairs.push(['status', tokLabeled('qstatus', 'qstatus', q.status)]); }
    pairs.push(['snapshot', snapshotAge(q.ts_start_ms, nowMs)]);
    pairs.push(['reset', resetElapsed(q, nowMs) ? 'Reset elapsed' : quotaResetCell(q.reset_ms, nowMs)]);
    var meta = buildDefList(pairs);
    meta.classList.add('qmeta');
    return meta;
  }

  // Snapshot age against the panel's own as_of. A stamp in the future beyond
  // the skew tolerance, or one that will not parse, reads unknown -- never a
  // negative age.
  function snapshotAge(tsMs, nowMs) {
    var ts = Number(tsMs);
    if (!isFinite(ts) || ts <= 0 || !isFinite(nowMs)) { return faintFigure('unknown'); }
    var age = nowMs - ts;
    if (age < -SKEW_TOLERANCE_SEC * 1000) { return faintFigure('unknown'); }
    return humanDuration(Math.max(0, age)) + ' old';
  }
  // ---- end tab:health --------------------------------------------------

