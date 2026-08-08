  // ---- tab:usage -------------------------------------------------------

  // The dimensions the ledger can group by, in picker order.
  var GROUP_BYS = [
    ['model', 'Model'],
    ['alias', 'Alias'],
    ['provider', 'Provider']
  ];

  var GROUP_BY_TITLES = {
    model: 'By model',
    alias: 'By alias',
    provider: 'By provider'
  };

  // The group-by picker and the provider-scope strip render OUTSIDE the section
  // boundary: a selection whose query comes back unavailable or refused must
  // stay reversible, and the state card that replaces the content carries no
  // affordance. Usage carries no provider cards of its own, so the strip is the
  // only place its inherited scope can be read or lifted.
  function buildUsage(rec) {
    var stack = tabStack();
    if (selectedProvider) {
      stack.appendChild(scopeStrip('every figure below is this provider only'));
    }
    stack.appendChild(groupByStrip());
    stack.appendChild(safeSection(rec, buildUsageLive));
    return stack;
  }

  // One card per group, never a table: a group is a separate object an operator
  // reads on its own, and the per-group stats do not line up as columns of one
  // reading. Zero-traffic groups are omitted rather than listed as rows of
  // zeroes; the header says how many were left out.
  function buildUsageLive() {
    var view = queryView();
    if (!view) { throw new Error('usage query payload is not live'); }
    var live = view.groups.filter(function (g) { return num0(g.metrics.requests) > 0; });
    if (num0(view.totals.requests) <= 0 || live.length === 0) { return usageEmpty(); }
    var omitted = view.groups.length - live.length;
    var ranked = live.slice().sort(function (a, b) {
      return num0(b.metrics.requests) - num0(a.metrics.requests);
    });
    var wrap = document.createElement('div');
    wrap.className = 'ovsection';
    wrap.appendChild(sectionHead(GROUP_BY_TITLES[selectedGroupBy], usageHint(ranked.length, omitted)));
    var list = document.createElement('div');
    list.className = 'ugroups';
    var totalReq = num0(view.totals.requests);
    ranked.forEach(function (g) { list.appendChild(usageCard(g, totalReq)); });
    wrap.appendChild(list);
    return wrap;
  }

  function usageHint(shown, omitted) {
    var hint = shown + (shown === 1 ? ' group - ' : ' groups - ') + WINDOW_SPAN[selectedWindow];
    if (omitted > 0) {
      hint += ' - ' + omitted + (omitted === 1 ? ' group' : ' groups') + ' with no requests not listed';
    }
    return hint;
  }

  function usageEmpty() {
    return emptyCard('No requests in this window',
      'Nothing was routed ' + WINDOW_SPAN[selectedWindow] +
      '. Pick a wider window, or send a request through the proxy.',
      ['grouped by ' + selectedGroupBy, WINDOW_SPAN[selectedWindow]]);
  }

  // Switch the grouping dimension. The query is re-issued through the standard
  // input-changed path (which aborts the in-flight request and bumps the
  // generation, so a late response for the previous dimension cannot repaint
  // this one), and only this tab repaints -- no sibling tab reads the query
  // source.
  function onGroupBy(dim) {
    if (terminal || dim === selectedGroupBy) { return; }
    selectedGroupBy = dim;
    queryInputChanged();
    renderActiveTab();
    renderVerdict();
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
    btn.className = 'gbbtn' + (on ? ' gbbtn--on' : '');
    btn.setAttribute('aria-pressed', on ? 'true' : 'false');
    btn.textContent = label;
    btn.disabled = terminal;
    btn.addEventListener('click', function () { onGroupBy(dim); });
    return btn;
  }

  // The two-line group card: a headline line carrying identity, share, and the
  // three figures an operator scans for, over a hairline-separated detail line.
  function usageCard(group, totalReq) {
    var cardEl = document.createElement('div');
    cardEl.className = 'ucard';
    cardEl.appendChild(usageHeadLine(group, totalReq));
    cardEl.appendChild(usageStatLine(group.metrics));
    return cardEl;
  }

  function usageHeadLine(group, totalReq) {
    var m = group.metrics;
    var head = document.createElement('div');
    head.className = 'ucard-head';

    var id = document.createElement('div');
    id.className = 'ucard-id';
    var name = document.createElement('span');
    name.className = 'ucard-name';
    name.textContent = group.label;
    name.title = group.label;
    id.appendChild(name);
    var share = totalReq > 0 ? num0(m.requests) / totalReq * 100 : 0;
    var shareRow = document.createElement('span');
    shareRow.className = 'ucard-share';
    shareRow.appendChild(shareBar(share));
    var pct = document.createElement('span');
    pct.className = 'ucard-sharepct';
    pct.textContent = Math.round(share) + '% of requests';
    shareRow.appendChild(pct);
    id.appendChild(shareRow);
    head.appendChild(id);

    var figs = document.createElement('div');
    figs.className = 'ucard-figs';
    figs.appendChild(ufig('requests', magSpan(m.requests), null));
    figs.appendChild(ufig('errors', negCell(m.errors), null));
    figs.appendChild(ufig('est. cost', costFigure(m),
      m.cost_status === 'partial' ? 'priced subtotal only' : null));
    head.appendChild(figs);
    return head;
  }

  // The ONE column set this tab ships (the Essentials/Everything toggle is not
  // built): token volumes in and out, the weighted cache-hit share, the billed
  // cache read and write volumes, time to first token at p50 and its observed
  // peak, generation throughput, context size at its peak and mean, and how
  // many of the group's requests were streamed. Every figure is a metric the
  // ledger measured for THIS group -- nothing is derived from another column
  // and nothing is filler.
  function usageStatLine(m) {
    var line = document.createElement('div');
    line.className = 'ucard-stats';
    var ttft = msParts(m.ttft_p50_ms), peak = msParts(m.ttft_p95_ms);
    [
      ustat('tokens in / out', pairValue(magSpan(m.input_tokens), magSpan(m.output_tokens))),
      ustat('cache hit', pctFigure(m.cache_hit_pct)),
      ustat('cache read / write',
        pairValue(magSpan(m.cache_read_billed), magSpan(cacheWritten(m)))),
      ustat('ttft p50 / peak',
        pairValue(figure(ttft.v, ttft.u, null), figure(peak.v, peak.u, null))),
      ustat('throughput', throughputFigure(m.throughput_tok_s)),
      ustat('ctx peak / avg', pairValue(magSpan(m.ctx_peak), magSpan(m.ctx_avg))),
      ustat('streamed', magSpan(m.stream_count))
    ].forEach(function (stat) { line.appendChild(stat); });
    return line;
  }

  // A share that ARRIVED as a percentage, rounded by pctText and re-split so
  // the sign rides the faint unit slot instead of the numeral.
  function pctFigure(v) {
    var text = pctText(v);
    return figure(text.slice(0, text.length - 1), '%', null);
  }

  // A non-positive throughput means no generation was measured for this group
  // (the adapter coerces an absent figure to zero), so it reads as absent
  // rather than as a stalled stream.
  function throughputFigure(v) {
    var x = num0(v);
    return x > 0 ? figure(String(Math.round(x)), 'tok/s', null) : figure('-', null, null);
  }

  // Two readings of one measurement, the second de-emphasized so the pair reads
  // as primary-then-secondary rather than as two competing figures. The
  // separator is drawn by CSS: this script carries no slash-leading string
  // literal (see the mutation-channel scan in page.rs).
  function pairValue(first, second) {
    var span = document.createElement('span');
    span.className = 'upair';
    span.appendChild(first);
    var sep = document.createElement('span');
    sep.className = 'upair-sep';
    sep.setAttribute('aria-hidden', 'true');
    span.appendChild(sep);
    second.classList.add('upair-b');
    span.appendChild(second);
    return span;
  }

  // A headline figure: an uppercase label over the numeral, with an optional
  // faint qualifier under it.
  function ufig(label, valueNode, note) {
    var wrap = document.createElement('div');
    wrap.className = 'ufig';
    var l = document.createElement('span');
    l.className = 'ufig-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'ufig-value';
    v.appendChild(valueNode);
    wrap.appendChild(l);
    wrap.appendChild(v);
    if (note) {
      var n = document.createElement('span');
      n.className = 'ufig-note';
      n.textContent = note;
      wrap.appendChild(n);
    }
    return wrap;
  }

  function ustat(label, valueNode) {
    var wrap = document.createElement('div');
    wrap.className = 'ustat';
    var l = document.createElement('span');
    l.className = 'ustat-label';
    l.textContent = label;
    var v = document.createElement('span');
    v.className = 'ustat-value';
    v.appendChild(valueNode);
    wrap.appendChild(l);
    wrap.appendChild(v);
    return wrap;
  }
  // ---- end tab:usage ---------------------------------------------------

