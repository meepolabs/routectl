  // ---- view:models -----------------------------------------------------

  // The Models view: one row per model lane from the models panel, in the
  // panel's order (problems first). Rows the needs-attention list names under
  // one cause sit under one cause line carrying that item's words; a row
  // whose problem has cleared shows how long ago, from the list's recovered
  // item that named it. Once the seven-day per-model read has answered
  // (live, or kept as stale), lanes none of whose nicknames carried a request
  // collapse to one line. Every string is set as text.
  //
  // The attention list and the seven-day read are each guarded the way
  // acctPoolCounts guards health: either one missing costs its own facts
  // (the cause words, the collapse), never the rows and their raw state.

  function buildModels() {
    return viewSection('models', modelsSection, 'Model lanes');
  }

  // State leads so a narrow window still shows it before the table
  // scrolls. The Model column links each nickname to Traffic narrowed to it
  // on the lane's own provider entry.
  var MODELS_COLUMNS = [C('State'), R('Model'), C('Provider'), C('Upstream'), C('Account'),
    C('Refuses'), C('Changed in transit')];

  function modelsSection(rec) {
    var rows = rec.data.rows;
    if (!Array.isArray(rows)) { throw new Error('models payload carries no rows'); }
    rows.forEach(modelsCheckRow);
    var startText = fmtTs(rec.data.started_ms);
    if (!rows.length) {
      var wrap = markRefreshUnit(document.createElement('div'), 'models-lanes');
      wrap.className = 'ovsection';
      wrap.appendChild(sectionHead('Model lanes', modelsHint(startText)));
      wrap.appendChild(emptyCard('No model lane yet',
        'Each configured model appears here, one row per provider entry and upstream it ' +
        'dispatches to, with its state and what it has learned to refuse.', ['current state']));
      return wrap;
    }
    var ctx = {
      nowMs: panelNowMs(rec),
      startText: startText,
      items: modelsAttentionItems(SOURCES.attention),
      used: modelsUsedNicknames(SOURCES[QUERY_MODELS_SOURCE])
    };
    return modelsCard(rows, ctx);
  }

  // The lanes in one card, as Traffic shows its table: the lanes still in
  // use in the table, the rest folded into one line under it.
  function modelsCard(rows, ctx) {
    var kept = [];
    var unused = [];
    rows.forEach(function (row) {
      (modelsIsUnused(row, ctx) ? unused : kept).push(row);
    });
    var body = document.createDocumentFragment();
    body.appendChild(modelsTable(mkTable('models-lanes', 'Model lanes', MODELS_COLUMNS,
      { expandable: true }), kept, ctx));
    if (unused.length) { body.appendChild(modelsUnusedLine(unused, ctx)); }
    return markRefreshUnit(card('Model lanes', modelsHint(ctx.startText), body), 'models-lanes');
  }

  function modelsHint(startText) {
    return 'current state - changed-in-transit counts responses since ' + startText;
  }

  // The fields every row must carry before any of it is drawn.
  function modelsCheckRow(row) {
    if (!isPlainObject(row) || typeof row.lane !== 'string' || typeof row.provider !== 'string' ||
        !Array.isArray(row.nicknames) || typeof row.state !== 'string' ||
        !Array.isArray(row.refusals) || !Array.isArray(row.capabilities) ||
        !isPlainObject(row.transit) || !Array.isArray(row.transit.top_paths)) {
      throw new Error('models payload carries a malformed row');
    }
  }

  function modelsIsProblem(row) {
    return row.state !== 'ok' || typeof row.cause_key === 'string';
  }

  // The attention items by key, or null when the list carries nothing
  // renderable.
  function modelsAttentionItems(rec) {
    if (stateCard(rec)) { return null; }
    try {
      if (!Array.isArray(rec.data.items)) { throw new Error('attention payload carries no item list'); }
      var items = rec.data.items.filter(isPlainObject);
      markRenderAttempt(rec.name);
      return items;
    } catch (e) {
      recordRenderFault(rec.name);
      return null;
    }
  }

  // The nicknames the seven-day per-model read saw a request for, or null
  // while that read holds no answer. The answer a failed refresh kept still
  // decides the fold: dropping it would unfold every collapsed lane, replace
  // their rows and lose what the operator had open.
  function modelsUsedNicknames(rec) {
    var view = queryViewOf(rec.name);
    if (!view) { return null; }
    var used = Object.create(null);
    view.groups.forEach(function (g) {
      if (num0(g.metrics.requests) > 0) { used[g.label] = true; }
    });
    markRenderAttempt(rec.name);
    return used;
  }

  // A lane collapses when the seven-day read holds an answer and none of its
  // nicknames carried a request in it. A pooled nickname's seat lanes share
  // the nickname, so they collapse or stay together. A problem, or a problem
  // that cleared recently, always stays a row of its own.
  function modelsIsUnused(row, ctx) {
    if (!ctx.used || modelsIsProblem(row) || modelsRecovered(row, ctx.items)) { return false; }
    return !row.nicknames.some(function (n) { return !!ctx.used[n]; });
  }

  // The runtime state keys a lane's targets can carry: a nickname, or a
  // pool seat's `nickname#member` (nicknames never contain the separator).
  function modelsStateKeys(row) {
    var keys = [];
    row.nicknames.forEach(function (n) {
      keys.push(String(n));
      keys.push(n + '#' + row.provider);
    });
    return keys;
  }

  // The most recent recovered attention item that named this lane: one whose
  // `targets` list one of its routes, a route item about one of its routes,
  // or the credential item of its account. Null when none did, or while the
  // list is not available.
  function modelsRecovered(row, items) {
    if (!items) { return null; }
    var keys = modelsStateKeys(row);
    var found = null;
    items.forEach(function (item) {
      if (item.state !== 'recovered' || !modelsItemNames(item, row, keys)) { return; }
      if (!found || num0(item.recovered_at_ms) > num0(found.recovered_at_ms)) { found = item; }
    });
    return found;
  }

  var MODELS_ACCOUNT_KINDS = ['account_unusable', 'account_parked'];

  function modelsItemNames(item, row, keys) {
    if (Array.isArray(item.targets)) {
      return item.targets.some(function (t) { return keys.indexOf(t) >= 0; });
    }
    if (item.kind === 'route_circuit_open') { return keys.indexOf(item.subject) >= 0; }
    return MODELS_ACCOUNT_KINDS.indexOf(item.kind) >= 0 && typeof row.account === 'string' &&
      item.account === row.account;
  }

  // The active item listed under `key`, or null.
  function modelsCauseItem(key, items) {
    if (!items) { return null; }
    for (var i = 0; i < items.length; i++) {
      if (items[i].key === key && items[i].state !== 'recovered') { return items[i]; }
    }
    return null;
  }

  // Rows in server order, each cause's rows gathered under its first row's
  // position: [{ key, rows }] with key null for a row no item names.
  function modelsCauseGroups(rows) {
    var groups = [];
    var byKey = Object.create(null);
    rows.forEach(function (row) {
      var key = typeof row.cause_key === 'string' ? row.cause_key : null;
      if (key === null) { groups.push({ key: null, rows: [row] }); return; }
      if (!byKey[key]) {
        byKey[key] = { key: key, rows: [] };
        groups.push(byKey[key]);
      }
      byKey[key].rows.push(row);
    });
    return groups;
  }

  // Fill `tbl` with `rows` and wrap it in its scroller. Two rows on one lane
  // would share a row id, so that throws and the section draws its card.
  function modelsTable(tbl, rows, ctx) {
    checkRowKeys(tbl.id, rows.map(function (row) { return { row: row, key: row.lane }; }));
    modelsCauseGroups(rows).forEach(function (group) {
      if (group.key !== null) { tbl.tb.appendChild(modelsCauseLine(group, ctx)); }
      group.rows.forEach(function (row) { modelsRow(tbl, row, ctx); });
    });
    return tableScroll(tbl);
  }

  // One line over the rows a cause names: the attention item's own words and
  // the command that fixes it, or the bare key while the list is not
  // available.
  function modelsCauseLine(group, ctx) {
    var tr = document.createElement('tr');
    tr.id = 'models-cause-' + idKey(group.key);
    tr.className = 'models-cause-row';
    var cell = document.createElement('th');
    cell.scope = 'rowgroup';
    cell.colSpan = MODELS_COLUMNS.length + 1;
    var line = document.createElement('div');
    line.className = 'models-cause';
    var item = modelsCauseItem(group.key, ctx.items);
    var count = group.rows.length + phrase(group.rows.length, ' lane', ' lanes');
    line.appendChild(attentionText('span', 'models-cause-count', count));
    if (item) {
      line.appendChild(attentionText('span', 'models-cause-text', item.cause));
      if (item.fix) { line.appendChild(copyableCode('models-cause-fix', item.fix)); }
    } else {
      line.appendChild(attentionText('span', 'models-cause-text',
        ctx.items ? 'listed under ' + group.key : 'cause not available - ' + group.key));
    }
    cell.appendChild(line);
    tr.appendChild(cell);
    return tr;
  }

  function modelsRow(tbl, row, ctx) {
    xrow(tbl, row.lane, [
      modelsStateCell(row, ctx),
      modelsNicknameLinks(row),
      row.provider,
      row.upstream,
      modelsAccountText(row),
      modelsRefusals(row),
      modelsTransitCell(row)
    ], modelsDetail(row, ctx), null, modelsRowName(row));
  }

  // The row as its cells read it: the nicknames, then the provider entry.
  function modelsRowName(row) {
    return row.nicknames.map(String).join(', ') + ' on ' + row.provider;
  }

  // One link per nickname, each to Traffic narrowed to that nickname on this
  // lane's provider entry. The address goes through the page's own codec
  // and, once followed, through its closed-set check like any other link.
  // Models has no range of its own: from Today, which is empty on most
  // mornings, the link opens the seven days the fold reads; any other range
  // the operator picked is kept.
  function modelsNicknameLinks(row) {
    var wrap = document.createElement('span');
    wrap.className = 'models-names';
    row.nicknames.forEach(function (nickname) {
      var state = currentViewState();
      state.view = 'traffic';
      state.scope = row.provider;
      state.model = String(nickname);
      if (state.range === 'today') { state.range = modelsFoldRange(); }
      var a = document.createElement('a');
      a.className = 'models-link';
      a.textContent = String(nickname);
      a.setAttribute('href', formatViewHash(state));
      a.title = 'Traffic for ' + nickname + ' on ' + row.provider;
      setAction(a, 'traffic-link', null);
      wrap.appendChild(a);
    });
    return wrap;
  }

  // The range the seven-day per-model read asks for.
  function modelsFoldRange() {
    return shapeAt(QUERY_MODELS_SOURCE, {}).window;
  }

  function modelsAccountText(row) {
    if (typeof row.account !== 'string') { return 'client credential'; }
    return typeof row.login_label === 'string' ? row.login_label : row.account;
  }

  // The lane's raw state with how long it has held, and, on a lane whose
  // problem has cleared, how long ago it did. A lane under a cause line that
  // carries the attention item's words leaves the duration to that line,
  // which already says it.
  function modelsStateCell(row, ctx) {
    var wrap = document.createElement('span');
    wrap.className = 'models-state';
    if (row.state === 'ok') {
      wrap.appendChild(attentionText('span', 'models-ok', 'ok'));
    } else {
      wrap.appendChild(tokLabeled('elig', 'elig', row.state));
      var since = modelsSinceText(row, ctx);
      if (since) { wrap.appendChild(attentionText('span', 'models-since', since)); }
    }
    var recovered = modelsIsProblem(row) ? null : modelsRecovered(row, ctx.items);
    if (recovered) {
      wrap.appendChild(attentionText('span', 'models-recovered',
        recoveredText(recovered.recovered_at_ms, ctx.nowMs)));
    }
    return wrap;
  }

  // `for <span>` since the lane's problem began, or null when its cause
  // line says it or the start is not known.
  function modelsSinceText(row, ctx) {
    if (typeof row.cause_key === 'string' && modelsCauseItem(row.cause_key, ctx.items)) { return null; }
    var at = Number(row.since_ms);
    if (row.since_ms === null || row.since_ms === undefined || !isFinite(at) || !isFinite(ctx.nowMs)) {
      return null;
    }
    return 'for ' + spanText(Math.max(0, ctx.nowMs - at));
  }

  function modelsRefusals(row) {
    var wrap = document.createElement('span');
    wrap.className = 'hchips';
    row.refusals.forEach(function (refusal) { wrap.appendChild(modelsRefusalChip(refusal)); });
    if (!wrap.childNodes.length) { wrap.appendChild(modelsAbsent('none')); }
    return wrap;
  }

  // An absent value as a faint dash, its word kept for a screen reader.
  // The wrapper is positioned so the visually hidden word stays inside a
  // table that scrolls in its card instead of widening the page.
  function modelsAbsent(word) {
    var span = markValue(document.createElement('span'), word);
    span.className = 'models-absent mag-zero';
    var dash = document.createElement('span');
    dash.setAttribute('aria-hidden', 'true');
    dash.textContent = '-';
    span.appendChild(dash);
    span.appendChild(srOnlyText(word));
    return span;
  }

  // Plain words for the verdict tokens the models panel sends whose words
  // differ from the token (routectl-core's verdicts plus the router's
  // panel-only display verdicts). Any other token, and every capability
  // key, reads as itself with its underscores spaced; the raw tokens stay
  // first in the chip's tooltip.
  var MODELS_VERDICT_WORDS = {
    verified: 'works',
    broken: 'refused',
    suspect: 'maybe ignored',
    forced_supported: 'supported by config',
    forced_unsupported: 'unsupported by config',
    unrepresentable: 'cannot be sent',
    mixed: 'differs by nickname'
  };

  function modelsSpacedToken(token) {
    return String(token).replace(/_/g, ' ');
  }

  function modelsVerdictWords(verdict) {
    var raw = String(verdict);
    return Object.prototype.hasOwnProperty.call(MODELS_VERDICT_WORDS, raw) ?
      MODELS_VERDICT_WORDS[raw] : modelsSpacedToken(raw);
  }

  function modelsChipText(capability, verdict) {
    return modelsSpacedToken(capability) + ': ' + modelsVerdictWords(verdict);
  }

  // The capability and verdict in plain words; the bare key, the one to
  // copy into `force_supported`, leads the tooltip with the verdict,
  // routing action and learned value.
  function modelsRefusalChip(refusal) {
    var chip = document.createElement('span');
    chip.className = 'hchip models-refusal';
    chip.textContent = modelsChipText(refusal.capability, refusal.verdict);
    chip.title = refusal.capability + ' - ' + refusal.verdict + ', ' + refusal.action +
      ', value ' + (refusal.value || '-');
    return chip;
  }

  // Exact counts beside the number of responses checked. A lane nothing was
  // checked on reads "unmeasured" (drawn as a faint dash), never zero.
  function modelsTransitText(transit) {
    var checked = num0(transit.checked);
    if (checked <= 0) { return 'unmeasured'; }
    return String(num0(transit.changed)) + ' of ' + String(checked) + ' responses changed';
  }

  function modelsTransitCell(row) {
    var text = modelsTransitText(row.transit);
    if (text === 'unmeasured') {
      var absent = modelsAbsent(text);
      absent.classList.add('models-transit');
      return absent;
    }
    var span = markValue(document.createElement('span'), text);
    span.className = 'models-transit';
    span.textContent = text;
    return span;
  }

  // The row's Changed in transit cell already says `unmeasured`, so the
  // expansion adds transit lines only when it has counts or paths to show.
  function modelsDetail(row, ctx) {
    var pairs = [['lane', row.lane]];
    if (num0(row.transit.checked) > 0) {
      pairs.push(['changed in transit', modelsTransitText(row.transit) + ' since ' + ctx.startText]);
    }
    if (row.transit.top_paths.length) {
      pairs.push(['most-changed fields', modelsPaths(row.transit.top_paths)]);
    }
    pairs.push(['capabilities', modelsCapabilities(row.capabilities)]);
    return pairs;
  }

  function modelsPaths(paths) {
    var wrap = document.createElement('span');
    wrap.className = 'models-paths';
    paths.forEach(function (p) {
      var line = document.createElement('span');
      line.className = 'models-path';
      var path = String(p.path);
      line.appendChild(attentionText('code', 'models-path-name',
        path === 'other' ? 'other fields' : (path === '' ? '(whole body)' : path)));
      line.appendChild(attentionText('span', 'models-path-count', String(num0(p.count))));
      wrap.appendChild(line);
    });
    return wrap;
  }

  // Every resolved capability cell: its capability and display verdict in
  // plain words, the raw tokens, tier and routing action in its tooltip. A cell resolved as unsupported is
  // marked; a verdict reads as text, never as a color alone.
  function modelsCapabilities(cells) {
    var wrap = document.createElement('span');
    wrap.className = 'hchips';
    cells.forEach(function (cell) {
      var chip = document.createElement('span');
      chip.className = 'hchip' + (cell.supported === false ? ' models-refusal' : '');
      chip.textContent = modelsChipText(cell.capability, cell.verdict);
      chip.title = cell.capability + ' - ' + cell.verdict + ', ' + cell.action +
        (cell.layer ? ', decided by ' + cell.layer : '') +
        (cell.source ? ', ' + cell.source + ' evidence' : '');
      wrap.appendChild(chip);
    });
    if (!wrap.childNodes.length) { wrap.appendChild(modelsAbsent('none')); }
    return wrap;
  }

  // The lanes with no request in the last seven days, as one line that
  // opens onto their rows.
  function modelsUnusedLine(rows, ctx) {
    var summary = rows.length + phrase(rows.length, ' lane', ' lanes') +
      ' with no request in the last 7 days';
    var det = buildExpander('models-unused', summary,
      modelsTable(mkTable('models-unused-lanes', 'Model lanes with no request in the last 7 days',
        MODELS_COLUMNS, { expandable: true }), rows, ctx));
    det.className = 'models-unused';
    return det;
  }
  // ---- end view:models -------------------------------------------------
