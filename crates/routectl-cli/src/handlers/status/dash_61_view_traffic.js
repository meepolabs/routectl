  // ---- view:traffic ----------------------------------------------------

  var GROUP_BY_TITLES = {
    model: 'By model',
    alias: 'By alias',
    provider: 'By provider',
    seat: 'By seat'
  };

  // The height the group section last drew at, held while the section waits
  // for the answer to a new selection, so a loading section never collapses
  // and drops what sits under it. Zero until a table has been drawn.
  var trafficSectionPx = 0;

  // An error rate colors only once it marks a state: degraded from this share
  // of a group's requests, broken from the next. Below both it is plain text,
  // and a zero is faint.
  var ERROR_RATE_DEGRADED_PCT = 5;
  var ERROR_RATE_BROKEN_PCT = 50;

  // The two timing columns are read over different requests, so a failing
  // group's latency can sit under its time to first token.
  var TTFT_POPULATION = 'over streamed successes';
  var LATENCY_POPULATION = 'over every request, failures included';

  var TRAFFIC_READ_FAILED = "Couldn't read traffic from routectl";

  // The missing-price dimension the server names for cache writes of unknown
  // lifetime. No configuration key prices it at any rate, and the remainder
  // sentence already says so, so the No price block never repeats it there;
  // a page.rs test pins it to the server's label.
  var UNKNOWN_LIFETIME_DIMENSION = 'cache write (unknown lifetime)';

  // What was served in the selected range: the scope strip and the grouping
  // picker, then one sortable table with a row per group.
  function buildTraffic() {
    var stack = tabStack();
    stack.appendChild(trafficControls());
    var section = viewSection(QUERY_SOURCE, buildTrafficGroups, GROUP_BY_TITLES[selectedGroupBy],
      TRAFFIC_READ_FAILED);
    reserveTrafficHeight(section);
    stack.appendChild(section);
    return stack;
  }

  // The picker and the scope strip sit OUTSIDE the section boundary: a
  // selection whose query comes back unavailable or refused must stay
  // reversible, and the state card that replaces the content carries no
  // affordance.
  function trafficControls() {
    var controls = document.createDocumentFragment();
    if (hasQueryScope()) {
      controls.appendChild(scopeStrip('usage', trafficScopeHint()));
    }
    controls.appendChild(groupByStrip());
    return controls;
  }

  function trafficScopeHint() {
    if (selectedModel && selectedProvider) {
      return 'every figure below is this model on this provider only';
    }
    return selectedModel ? 'every figure below is this model only' : 'every figure below is this provider only';
  }

  // Measured on the section the page shows now, before the rebuild replaces
  // it; only a drawn table on a shown pane counts, so a reserve never feeds
  // on itself.
  function reserveTrafficHeight(section) {
    var shown = el('body-traffic');
    var current = shown && shown.querySelector('[data-source="' + QUERY_SOURCE + '"]');
    if (current && current.querySelector('#traffic-groups') && current.offsetHeight > 0) {
      trafficSectionPx = current.offsetHeight;
    }
    if (SOURCES[QUERY_SOURCE].state === 'loading' && trafficSectionPx > 0) {
      section.style.minHeight = trafficSectionPx + 'px';
    }
  }

  function buildTrafficGroups(rec) {
    return safeSection(rec, buildTrafficLive);
  }

  // Groups with no request in the range are left out rather than listed as
  // rows of zeroes; the hint says how many. The builder order is busiest
  // first, which is the order an unsorted table keeps.
  function buildTrafficLive() {
    var view = queryView();
    if (!view) { throw new Error('usage query payload is not live'); }
    var live = view.groups.filter(function (g) { return num0(g.metrics.requests) > 0; });
    if (num0(view.totals.requests) <= 0 || live.length === 0) { return trafficEmpty(); }
    var ranked = live.slice().sort(function (a, b) {
      return num0(b.metrics.requests) - num0(a.metrics.requests);
    });
    var body = document.createDocumentFragment();
    body.appendChild(tableScroll(trafficTable(ranked)));
    body.appendChild(trafficNotes());
    return card(GROUP_BY_TITLES[selectedGroupBy],
      trafficHint(ranked.length, view.groups.length - live.length),
      body);
  }

  // What the columns whose meaning is not in their label measure, on the
  // card itself rather than only in a hover title.
  function trafficNotes() {
    var notes = document.createElement('div');
    notes.className = 'traffic-notes';
    [
      'At API prices: ' + API_PRICE_NOTE,
      'Latency ' + LATENCY_POPULATION + '; time to first token ' + TTFT_POPULATION
    ].forEach(function (text) {
      var line = document.createElement('p');
      line.className = 'traffic-note';
      line.textContent = text;
      notes.appendChild(line);
    });
    return notes;
  }

  function trafficTable(groups) {
    var tbl = mkTable('traffic-groups', 'Traffic ' + GROUP_BY_TITLES[selectedGroupBy].toLowerCase(), [
      sortBy(R('group'), 'group', function (g) { return g.label; }),
      sortBy(N('requests'), 'requests', function (g) { return num0(g.metrics.requests); }),
      sortBy(N('error rate'), 'errors', function (g) { return errorRate(g.metrics); }),
      sortBy(titled(N('TTFT p50'), 'median time to first token ' + TTFT_POPULATION), 'ttft',
        function (g) { return g.metrics.ttft_p50_ms; }),
      sortBy(titled(N('latency p50'), 'median latency ' + LATENCY_POPULATION), 'latency',
        function (g) { return g.metrics.latency_p50_ms; }),
      sortBy(N('cost'), 'cost', function (g) { return pricedCost(g.metrics); }),
      sortBy(titled(N('At API prices'), API_PRICE_NOTE), 'equivalent',
        function (g) { return g.metrics.equivalent_cost_usd; }),
      sortBy(N('cache hit'), 'cache', function (g) { return g.metrics.cache_hit_pct; })
    ], { expandable: true, sortable: true, rowKey: function (g) { return g.label; } });
    fillRows(tbl, groups, function (g, key) { trafficRow(tbl, g, key); });
    return tbl;
  }

  function titled(col, title) {
    col.title = title;
    return col;
  }

  function trafficRow(tbl, g, key) {
    var m = g.metrics;
    xrow(tbl, key, [
      g.label,
      magSpan(m.requests),
      errorRateFigure(m),
      msFigure(m.ttft_p50_ms),
      msFigure(m.latency_p50_ms),
      trafficCostFigure(m),
      equivalentFigure(m),
      cacheHitFigure(m.cache_hit_pct)
    ], trafficTimingPairs(m));
    appendDetailBlocks(tbl, [
      unpricedRemainderLine(m), aliasChainBlock(g), missingPriceBlock(trafficMissingPrices(m))
    ]);
  }

  function trafficHint(shown, omitted) {
    var hint = shown + (shown === 1 ? ' group - ' : ' groups - ') + WINDOW_SPAN[selectedWindow];
    if (omitted > 0) {
      hint += ' - ' + omitted + (omitted === 1 ? ' group' : ' groups') + ' with no requests not listed';
    }
    return hint;
  }

  function trafficEmpty() {
    return emptyCard('No requests in this window',
      'Nothing was routed ' + WINDOW_SPAN[selectedWindow] +
      '. Pick a wider window, or send a request through the proxy.',
      ['grouped by ' + selectedGroupBy, WINDOW_SPAN[selectedWindow]]);
  }

  // ---- traffic: row figures --------------------------------------------

  function errorRate(m) {
    var req = num0(m.requests);
    return req > 0 ? num0(m.errors) / req * 100 : null;
  }

  // One decimal, so a rate under one percent still reads as a rate; a rate
  // above zero that rounds to nothing says so rather than reading as zero.
  function errorRateFigure(m) {
    var rate = errorRate(m);
    if (rate === null) { return figure('-', null, null); }
    var text = rate > 0 && rate < 0.05 ? '<0.1' : rate.toFixed(1);
    var fig = figure(rate === 0 ? '0' : text, '%', null);
    var state = errorRateState(rate);
    if (state) { fig.classList.add('traffic-rate--' + state); }
    return fig;
  }

  function errorRateState(rate) {
    if (rate >= ERROR_RATE_BROKEN_PCT) { return 'broken'; }
    return rate >= ERROR_RATE_DEGRADED_PCT ? 'degraded' : null;
  }

  function msFigure(ms) {
    var p = msParts(ms);
    return figure(p.v, p.u || null, null);
  }

  // A subtotal of the priced rows carries its mark in the cell, so it never
  // reads as the group's whole cost.
  function trafficCostFigure(m) {
    var fig = costFigure(m);
    if (isPricedSubtotal(m)) { fig.appendChild(costMark(PARTIAL_COST_NOTE)); }
    return fig;
  }

  // An equivalent is what subscription traffic would have cost at API
  // prices, never a bill: the tilde says so. One that leaves some of that
  // traffic out carries the mark a priced subtotal does, and subscription
  // traffic none of whose requests could be priced says so in a word.
  function equivalentFigure(m) {
    var coverage = equivalentCoverage(m);
    if (coverage === 'unpriced') { return unpricedEquivalentFigure(); }
    if (coverage === 'none') { return figure('-', null, null); }
    var fig = figure(money(m.equivalent_cost_usd), null, '~$');
    if (coverage === 'partial') { fig.appendChild(costMark(EQUIVALENT_PARTIAL_TITLE)); }
    return fig;
  }

  function cacheHitFigure(pct) {
    var text = cacheHitText(pct);
    return text === '-' ? figure('-', null, null) : figure(text.slice(0, text.length - 1), '%', null);
  }

  // ---- traffic: the expanded row ---------------------------------------

  // The tail percentiles, each naming the requests it is read over, then what
  // went wrong and what the cache-write figure covers.
  function trafficTimingPairs(m) {
    return [
      ['TTFT p90, p99 ' + TTFT_POPULATION, msText(m.ttft_p90_ms) + ', ' + msText(m.ttft_p99_ms)],
      ['latency p90, p99 ' + LATENCY_POPULATION,
        msText(m.latency_p90_ms) + ', ' + msText(m.latency_p99_ms)],
      ['error classes', errorClassText(m)],
      ['cache write', cacheWriteStatement(m)]
    ];
  }

  // The blocks that read as lists rather than label:value pairs sit under the
  // pairs, each a block of its own. xrow appends a row's detail row last.
  function appendDetailBlocks(tbl, blocks) {
    var cell = tbl.tb.lastChild.firstChild;
    blocks.forEach(function (block) { if (block) { cell.appendChild(block); } });
  }

  function detailBlock(className, title) {
    var block = document.createElement('div');
    block.className = 'traffic-block ' + className;
    var head = document.createElement('h3');
    head.className = 'traffic-blockhead';
    head.textContent = title;
    block.appendChild(head);
    return block;
  }

  // Most frequent class first, ties by token, each in its humanized words.
  function errorClassText(m) {
    var classes = m.errors_by_class;
    var tokens = Object.keys(classes).filter(function (k) { return num0(classes[k]) > 0; });
    if (!tokens.length) { return 'none'; }
    tokens.sort(function (a, b) {
      return (num0(classes[b]) - num0(classes[a])) || (a < b ? -1 : (a > b ? 1 : 0));
    });
    return tokens.map(function (k) {
      return labelFor('class', k).label + ' ' + humanCount(classes[k]);
    }).join(', ');
  }

  // What the write figure covers, in the CLI legend's terms, and how much of
  // it had no reported lifetime.
  function cacheWriteStatement(m) {
    var written = cacheWritten(m);
    if (written === null) { return 'no cache write reported'; }
    var unknown = num0(m.cache_write_unknown_tokens);
    var covers = humanCount(written) + ' tokens, every lifetime and both ledger eras, already in input';
    if (unknown <= 0) { return covers + '; none of unknown lifetime'; }
    var share = written > 0 ? ' (' + shareText(unknown, written) + '%)' : '';
    return covers + '; ' + humanCount(unknown) + share + ' of unknown lifetime';
  }

  // Written tokens of unknown lifetime are never priced at any lifetime's
  // rate, so every dollar figure of the group leaves them out.
  function unpricedRemainderLine(m) {
    var unknown = num0(m.cache_write_unknown_tokens);
    if (unknown <= 0) { return null; }
    var line = document.createElement('p');
    line.className = 'traffic-remainder';
    line.textContent = humanCount(unknown) +
      phrase(unknown, ' cache-write token of unknown lifetime has', ' cache-write tokens of unknown lifetime have') +
      ' no price; the cost and At API prices figures leave ' + phrase(unknown, 'it', 'them') + ' out.';
    return line;
  }

  // The missing prices the No price block lists: the unknown-lifetime
  // dimension is left out wherever the remainder sentence explains it.
  function trafficMissingPrices(m) {
    if (num0(m.cache_write_unknown_tokens) <= 0) { return m.missing_prices; }
    return m.missing_prices.filter(function (p) { return p.dimension !== UNKNOWN_LIFETIME_DIMENSION; });
  }

  // A whole-percent share; a part above zero that rounds to nothing reads
  // "<1", never a zero beside its own count.
  function shareText(part, whole) {
    var pct = num0(part) / whole * 100;
    return pct > 0 && Math.round(pct) === 0 ? '<1' : String(Math.round(pct));
  }

  // One line per model with no rate for some dimension, in the server's
  // order: the dimensions, then each configuration key that would price one,
  // as the server's text.
  function missingPriceBlock(prices) {
    if (!prices.length) { return null; }
    var block = detailBlock('traffic-prices', 'No price');
    missingPricesByModel(prices).forEach(function (entry) {
      block.appendChild(missingPriceLine(entry.model, entry.prices));
    });
    return block;
  }

  function missingPricesByModel(prices) {
    var byModel = [];
    var at = Object.create(null);
    prices.forEach(function (p) {
      if (!(p.model in at)) {
        at[p.model] = byModel.length;
        byModel.push({ model: p.model, prices: [] });
      }
      byModel[at[p.model]].prices.push(p);
    });
    return byModel;
  }

  function missingPriceLine(model, prices) {
    var line = document.createElement('p');
    line.className = 'traffic-line traffic-priceline';
    var name = document.createElement('span');
    name.className = 'traffic-stepname';
    name.textContent = model;
    line.appendChild(name);
    line.appendChild(document.createTextNode(': ' + prices.map(function (p) {
      return p.dimension;
    }).join(', ')));
    var keyed = prices.filter(function (p) { return p.config_key !== null; });
    if (keyed.length) {
      line.appendChild(document.createTextNode(' - set'));
      keyed.forEach(function (p) {
        line.appendChild(document.createTextNode(' '));
        var key = document.createElement('code');
        key.className = 'traffic-key';
        key.textContent = p.config_key;
        line.appendChild(key);
      });
    }
    var unkeyed = prices.filter(function (p) {
      return p.config_key === null && p.dimension !== UNKNOWN_LIFETIME_DIMENSION;
    });
    if (unkeyed.length) {
      line.appendChild(document.createTextNode(' - no configuration key prices ' +
        unkeyed.map(function (p) { return p.dimension; }).join(', ')));
    }
    return line;
  }

  // An alias group's chain in order, one line per top-level step with its
  // share of the group, then the requests served off the chain and the ones
  // never dispatched: together they are the group's requests. A step that
  // names another alias reads as that alias.
  function aliasChainBlock(g) {
    if (!g.steps) { return null; }
    var total = num0(g.metrics.requests);
    var block = detailBlock('traffic-chain', 'Alias chain');
    if (!g.steps.length) {
      block.appendChild(chainLine(null, 'this name resolves to no configured chain', null, total));
    }
    g.steps.forEach(function (s, i) {
      var name = s.kind === ALIAS_STEP_KIND ? 'via alias ' + s.step : s.step;
      block.appendChild(chainLine('Step ' + (i + 1), name, s.requests, total));
    });
    block.appendChild(chainLine('off chain', null, g.off_chain, total));
    block.appendChild(chainLine('not dispatched', null, g.not_dispatched, total));
    return block;
  }

  function chainLine(label, name, count, total) {
    var line = document.createElement('p');
    line.className = 'traffic-line traffic-step';
    if (label !== null) {
      var term = document.createElement('span');
      term.className = 'traffic-steplabel';
      term.textContent = label;
      line.appendChild(term);
    }
    if (name !== null) {
      var nameEl = document.createElement('span');
      nameEl.className = 'traffic-stepname';
      nameEl.textContent = name;
      line.appendChild(nameEl);
    }
    if (count !== null) { line.appendChild(countShare(count, total)); }
    return line;
  }

  // An exact count, so the step lines add up to the group on screen, and its
  // whole-percent share of the group.
  function countShare(n, total) {
    var span = document.createElement('span');
    span.className = 'traffic-count';
    var count = figure(String(num0(n)), ' req', null);
    count.classList.add('traffic-req');
    span.appendChild(count);
    if (total > 0) {
      span.appendChild(figure(shareText(n, total), '%', null));
    }
    return span;
  }

  // ---- traffic: the grouping picker ------------------------------------

  // Switch the grouping dimension. A view-state change: commitViewState
  // re-issues the query through the standard input-changed path (which aborts
  // the in-flight request and bumps the generation, so a late response for the
  // previous dimension cannot repaint this one) and records it in the address.
  function onGroupBy(dim) {
    if (terminal || dim === selectedGroupBy) { return; }
    commitViewState({ group: dim });
  }

  function groupByStrip() {
    var wrap = document.createElement('div');
    wrap.className = 'gbstrip';
    var label = document.createElement('span');
    label.className = 'gblabel';
    label.textContent = 'Group by';
    wrap.appendChild(label);
    var seg = document.createElement('div');
    seg.className = 'gbseg';
    GROUP_BYS.forEach(function (dim) { seg.appendChild(groupByButton(dim[0], dim[1])); });
    wrap.appendChild(seg);
    return wrap;
  }

  function groupByButton(dim, label) {
    var on = selectedGroupBy === dim;
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.id = 'usage-groupby-' + idKey(dim);
    btn.className = 'gbbtn' + (on ? ' gbbtn--on' : '');
    btn.setAttribute('aria-pressed', on ? 'true' : 'false');
    btn.textContent = label;
    btn.disabled = terminal;
    setAction(btn, 'group-by', dim);
    return btn;
  }
  // ---- end view:traffic ------------------------------------------------
