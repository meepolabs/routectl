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

  // What was served in the selected range: the scope strip and the grouping
  // picker, then one sortable table with a row per group.
  function buildTraffic() {
    var stack = tabStack();
    stack.appendChild(trafficControls());
    var section = viewSection(QUERY_SOURCE, buildTrafficGroups, GROUP_BY_TITLES[selectedGroupBy]);
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
      controls.appendChild(scopeStrip('usage', 'every figure below is this provider only'));
    }
    controls.appendChild(groupByStrip());
    return controls;
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
    return card(GROUP_BY_TITLES[selectedGroupBy],
      trafficHint(ranked.length, view.groups.length - live.length),
      tableScroll(trafficTable(ranked)));
  }

  function trafficTable(groups) {
    var tbl = mkTable('traffic-groups', 'Traffic ' + GROUP_BY_TITLES[selectedGroupBy].toLowerCase(), [
      sortBy(R('group'), 'group', function (g) { return g.label; }),
      sortBy(N('requests'), 'requests', function (g) { return num0(g.metrics.requests); }),
      sortBy(N('error rate'), 'errors', function (g) { return errorRate(g.metrics); }),
      sortBy(N('TTFT p50'), 'ttft', function (g) { return g.metrics.ttft_p50_ms; }),
      sortBy(N('latency p50'), 'latency', function (g) { return g.metrics.latency_p50_ms; }),
      sortBy(N('cost'), 'cost', function (g) { return pricedCost(g.metrics); }),
      sortBy(N('At API prices'), 'equivalent', function (g) { return g.metrics.equivalent_cost_usd; }),
      sortBy(N('cache hit'), 'cache', function (g) { return g.metrics.cache_hit_pct; })
    ], { expandable: true, sortable: true, rowKey: function (g) { return g.label; } });
    fillRows(tbl, groups, function (g, key) { trafficRow(tbl, g, key); });
    return tbl;
  }

  function trafficRow(tbl, g, key) {
    var m = g.metrics;
    xrow(tbl, key, [
      g.label,
      magSpan(m.requests),
      errorRateFigure(m),
      msFigure(m.ttft_p50_ms),
      msFigure(m.latency_p50_ms),
      costFigure(m),
      equivalentFigure(m.equivalent_cost_usd),
      cacheHitFigure(m.cache_hit_pct)
    ], trafficDetail(g));
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
    if (rate > 0) { fig.classList.add('neg'); }
    return fig;
  }

  function msFigure(ms) {
    var p = msParts(ms);
    return figure(p.v, p.u || null, null);
  }

  // The dollar figure a cost column sorts by: present only where rows were
  // priced, so an unpriced or subscription group sorts with the absent.
  function pricedCost(m) {
    var priced = m.cost_status === 'priced' || m.cost_status === 'partial';
    return priced ? m.cost_usd : null;
  }

  // An equivalent is what subscription traffic would have cost at API
  // prices, never a bill: the tilde says so.
  function equivalentFigure(v) {
    if (v === null || v === undefined) { return figure('-', null, null); }
    return figure(money(v), null, '~$');
  }

  function cacheHitFigure(pct) {
    var text = cacheHitText(pct);
    return text === '-' ? figure('-', null, null) : figure(text.slice(0, text.length - 1), '%', null);
  }

  // ---- traffic: the expanded row ---------------------------------------

  // The tail percentiles, then what went wrong, what the cache-write figure
  // covers, what could not be priced and, for an alias, how its chain
  // carried the group.
  function trafficDetail(g) {
    var m = g.metrics;
    return [
      ['TTFT p90, p99', msText(m.ttft_p90_ms) + ', ' + msText(m.ttft_p99_ms)],
      ['latency p90, p99', msText(m.latency_p90_ms) + ', ' + msText(m.latency_p99_ms)],
      ['error classes', errorClassText(m)],
      ['cache write', cacheWriteStatement(m)]
    ].concat(missingPricePairs(m.missing_prices), aliasStepPairs(g));
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
    var share = written > 0 ? ' (' + Math.round(unknown / written * 100) + '%)' : '';
    return covers + '; ' + humanCount(unknown) + share + ' of unknown lifetime';
  }

  // One pair per (model, dimension) with no rate. The configuration key is
  // the server's text, shown as it arrived.
  function missingPricePairs(prices) {
    return prices.map(function (p) {
      var value = document.createElement('span');
      value.appendChild(document.createTextNode(p.model + ' ' + p.dimension + ' - '));
      if (p.config_key === null) {
        value.appendChild(document.createTextNode('no configuration key prices it'));
      } else {
        value.appendChild(document.createTextNode('set '));
        var key = document.createElement('code');
        key.className = 'traffic-key';
        key.textContent = p.config_key;
        value.appendChild(key);
      }
      return ['no price', value];
    });
  }

  // Each top-level chain step's share of the group, then the requests served
  // off the chain and the ones never dispatched: together they are the
  // group's requests. A step that names another alias reads as that alias.
  function aliasStepPairs(g) {
    if (!g.steps) { return []; }
    var total = num0(g.metrics.requests);
    if (!g.steps.length) {
      return [['chain', 'this name resolves to no configured chain']].concat(remainderPairs(g, total));
    }
    return g.steps.map(function (s, i) {
      return ['step ' + (i + 1), stepShare(s, total)];
    }).concat(remainderPairs(g, total));
  }

  function remainderPairs(g, total) {
    return [
      ['off chain', countShare(g.off_chain, total)],
      ['not dispatched', countShare(g.not_dispatched, total)]
    ];
  }

  function stepShare(s, total) {
    var span = document.createElement('span');
    span.className = 'traffic-step';
    var name = document.createElement('span');
    name.className = 'traffic-stepname';
    name.textContent = s.kind === ALIAS_STEP_KIND ? 'via alias ' + s.step : s.step;
    span.appendChild(name);
    span.appendChild(countShare(s.requests, total));
    return span;
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
      span.appendChild(figure(String(Math.round(num0(n) / total * 100)), '%', null));
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
