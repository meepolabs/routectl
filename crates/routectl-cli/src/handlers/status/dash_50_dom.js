  // ---- DOM table helpers (textContent only, never innerHTML) -----------

  // NO automated runtime harness covers this part. The drawing and DOM
  // surface is verified BY HAND only -- see dashboard-manual-checklist.md
  // beside this source, and run it before shipping a change here.

  // Column descriptors: C = text, N = numeric (right-aligned, mono,
  // tabular), R = row-header identifier column (<th scope="row">),
  // W = wrapping prose column.
  function C(label) { return { label: label }; }
  function N(label) { return { label: label, num: true }; }
  function R(label) { return { label: label, row: true }; }
  function W(label) { return { label: label, wrap: true }; }

  // ---- stable element ids and delegated actions ------------------------

  // Element ids are a contract, not decoration: a node that can hold focus,
  // be opened or be scrolled must come out of every rebuild with the same id,
  // so a later in-place update can match it. The scheme:
  //
  //   <view>-<name>                   a fixed control or table (`config-models`)
  //   <table>-<idKey(row key)>        a table row
  //   <row id>-toggle / -detail       that row's disclosure button / detail row
  //   <table>-wrap                    the table's scroll container
  //   <view>-<name>-<idKey(key)>      a control keyed by data (a provider card)
  //   <view>-<name>-<idKey(key)>...   a repeated list item (see uniqueItemId)
  //
  // Table ids are literal strings, unique across the script (page.rs checks
  // it). idKey encodes a data key into 'k' + [A-Za-z0-9_]* and is injective,
  // so two keys never share an id. An encoded key never contains '-', so the
  // last '-' of a row id splits it back into (table id, encoded key), and its
  // leading 'k' keeps it from ever equaling a fixed suffix such as `wrap`.
  // Nothing else (no counter, no render order) may enter an id.
  function idKey(raw) {
    var s = String(raw);
    var out = 'k';
    for (var i = 0; i < s.length; i++) {
      var ch = s.charAt(i);
      out += /[A-Za-z0-9]/.test(ch) ? ch : '_' + s.charCodeAt(i).toString(16) + '_';
    }
    return out;
  }

  function rowDomId(tableId, rowKey) {
    return tableId + '-' + idKey(rowKey);
  }

  // The id of one item in a repeated list (a card, a check), whose `base` is
  // built from the item's own keys through idKey. A list whose items can be
  // identical (two byte-identical checks) tells the repeats apart by their
  // order among themselves; `seen` is one fresh object per list. The ordinal suffix adds a
  // segment no idKey-built base has, so it cannot equal another item's base.
  function uniqueItemId(base, seen) {
    var n = seen[base] || 0;
    seen[base] = n + 1;
    return n ? base + '-' + n : base;
  }

  // Mark `node` as the trigger of one ACTIONS entry (dash_90_chrome.js). No
  // builder attaches a listener: the delegated listener on the pane root or
  // the modal host dispatches by these attributes, so a rebuilt or reused
  // node always acts on what it currently says.
  function setAction(node, action, arg) {
    node.setAttribute('data-action', action);
    if (arg !== null && arg !== undefined) { node.setAttribute('data-arg', String(arg)); }
  }

  // Build a table with a visually-hidden caption, `scope="col"` headers,
  // and per-column alignment metadata. `id` is the table's stable id (see the
  // scheme above). `opts.expandable` prepends a leading control column whose
  // cells carry a disclosure button.
  function mkTable(id, caption, cols, opts) {
    var expandable = !!(opts && opts.expandable);
    var t = document.createElement('table');
    t.id = id;
    var cap = document.createElement('caption');
    cap.className = 'sr-only';
    cap.textContent = caption;
    t.appendChild(cap);
    var thead = document.createElement('thead');
    var hr = document.createElement('tr');
    if (expandable) {
      var eth = document.createElement('th');
      eth.scope = 'col';
      var sr = document.createElement('span');
      sr.className = 'sr-only';
      sr.textContent = 'Details';
      eth.appendChild(sr);
      hr.appendChild(eth);
    }
    cols.forEach(function (col) {
      var th = document.createElement('th');
      th.scope = 'col';
      th.textContent = col.label;
      if (col.title) { th.title = col.title; }
      if (col.num) { th.className = 'num'; }
      hr.appendChild(th);
    });
    thead.appendChild(hr);
    t.appendChild(thead);
    var tb = document.createElement('tbody');
    t.appendChild(tb);
    return { t: t, tb: tb, cols: cols, id: id, expandable: expandable };
  }

  // A table wrapped so it scrolls INSIDE its card: the page itself never
  // scrolls sideways, down to 720 px.
  function tableScroll(tbl) {
    var wrap = document.createElement('div');
    wrap.className = 'tablewrap';
    wrap.id = tbl.id + '-wrap';
    wrap.appendChild(tbl.t);
    return wrap;
  }

  function cellNode(col, cell) {
    var node = document.createElement(col.row ? 'th' : 'td');
    if (col.row) { node.scope = 'row'; }
    if (col.num) { node.className = 'num'; }
    if (col.wrap) { node.className = 'wrap'; }
    if (cell instanceof Node) {
      node.appendChild(cell);
    } else {
      node.textContent = (cell === null || cell === undefined) ? '-' : String(cell);
    }
    return node;
  }

  // Append one plain row. Cells are parallel to the table's columns; a cell
  // may be a raw value (rendered via textContent) or a Node (a pill).
  function trow(tbl, cells) {
    var tr = document.createElement('tr');
    tbl.cols.forEach(function (col, i) { tr.appendChild(cellNode(col, cells[i])); });
    tbl.tb.appendChild(tr);
  }

  // Append an expandable row: the primary cells plus a hidden detail row
  // (a definition grid of the secondary fields) toggled by a keyboard-
  // accessible disclosure button. The WHOLE primary row is clickable (the
  // button stays the visible affordance): both carry the `row-toggle` action,
  // and the delegated dispatch acts on the NEAREST marked ancestor of a
  // click, so a button click toggles once, never twice. Expansion persists
  // across poll rebuilds via the `expanded` map keyed by (table id, encoded
  // row key). An optional `rowClass` de-emphasizes the primary row
  // (zero-traffic).
  function xrow(tbl, rowKey, cells, detailPairs, rowClass) {
    var rid = rowDomId(tbl.id, rowKey);
    var open = getExpanded(tbl.id, idKey(rowKey));
    var tr = document.createElement('tr');
    tr.id = rid;
    tr.className = 'exp-row' + (rowClass ? ' ' + rowClass : '');
    setAction(tr, 'row-toggle', rid);

    var detailRow = document.createElement('tr');
    detailRow.className = 'detail-row';
    detailRow.id = rid + '-detail';
    var dcell = document.createElement('td');
    dcell.colSpan = tbl.cols.length + 1;
    dcell.appendChild(buildDefList(detailPairs));
    detailRow.appendChild(dcell);
    detailRow.hidden = !open;

    var extd = document.createElement('td');
    extd.className = 'exp-cell';
    var btn = document.createElement('button');
    btn.type = 'button';
    btn.id = rid + '-toggle';
    btn.className = 'exp-btn';
    btn.setAttribute('aria-controls', detailRow.id);
    btn.setAttribute('aria-label', 'Toggle row details');
    setAction(btn, 'row-toggle', rid);
    paintRowToggle(btn, open);
    extd.appendChild(btn);
    tr.appendChild(extd);

    tbl.cols.forEach(function (col, i) { tr.appendChild(cellNode(col, cells[i])); });
    tbl.tb.appendChild(tr);
    tbl.tb.appendChild(detailRow);
  }

  function paintRowToggle(btn, open) {
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
    btn.textContent = open ? 'v' : '>';
  }

  // The `row-toggle` action. `rid` is the row id; its last '-' separates the
  // table id from the encoded row key (see idKey). The open state is read
  // from `expanded` now, at click time, and the row's current nodes are
  // looked up by id, so the toggle never acts on a node a rebuild replaced.
  function onRowToggle(rid) {
    var cut = rid.lastIndexOf('-');
    if (cut <= 0) { return; }
    var tableId = rid.slice(0, cut);
    var key = rid.slice(cut + 1);
    var nowOpen = !getExpanded(tableId, key);
    setExpanded(tableId, key, nowOpen);
    var btn = el(rid + '-toggle');
    var detailRow = el(rid + '-detail');
    if (btn) { paintRowToggle(btn, nowOpen); }
    if (detailRow) { detailRow.hidden = !nowOpen; }
  }

  // A definition grid of label:value pairs. Values may be humanized
  // strings or Nodes (pills). textContent only. A row whose detail carries no
  // pairs at all renders an explicit line rather than an empty grid: an
  // expander that opens onto nothing reads as a click that did not work.
  function buildDefList(pairs) {
    if (!pairs.length) { return noDetailLine(); }
    var dl = document.createElement('dl');
    dl.className = 'deflist';
    pairs.forEach(function (p) {
      var wrap = document.createElement('div');
      wrap.className = 'def';
      var dt = document.createElement('dt');
      dt.textContent = p[0];
      var dd = document.createElement('dd');
      if (p[1] instanceof Node) {
        dd.appendChild(p[1]);
      } else {
        dd.textContent = (p[1] === null || p[1] === undefined) ? '-' : String(p[1]);
      }
      wrap.appendChild(dt);
      wrap.appendChild(dd);
      dl.appendChild(wrap);
    });
    return dl;
  }

  function noDetailLine() {
    var p = document.createElement('p');
    p.className = 'nodetail';
    p.textContent = 'No additional detail for this row.';
    return p;
  }

  // Value-domain token cell: raw wire string as textContent, styled by a
  // class keyed on the exact string. `prefix + '-' + raw` plus the `.tok`
  // fallback, so an unknown token stays readable and unstyled. Families that
  // carry a colorblind-safe leading dot get an explicit `.tok-dot` marker so
  // the dot is keyed on the family, never on a substring of the raw token.
  var DOT_FAMILIES = { circuit: 1, elig: 1, reach: 1, status: 1, qstatus: 1, actstatus: 1 };
  function tok(prefix, raw) {
    var span = document.createElement('span');
    span.className = 'tok ' + prefix + '-' + raw + (DOT_FAMILIES[prefix] ? ' tok-dot' : '');
    span.textContent = (raw === null || raw === undefined) ? '-' : String(raw);
    return span;
  }

  // A `.tok` pill whose COLOR class stays keyed on the raw wire token (so
  // styling is unaffected) but whose VISIBLE text is the humanized label
  // and whose title preserves the raw token / meaning. Unknown token ->
  // raw passthrough with no tooltip, exactly like `tok`.
  function tokLabeled(prefix, family, raw) {
    var span = tok(prefix, raw);
    var lab = labelFor(family, raw);
    span.textContent = lab.label;
    if (lab.title) { span.title = lab.title; }
    return span;
  }

  // A native default-collapsed disclosure: a summary line plus arbitrary
  // content. No JS state and no mutation affordance -- used for list-shaped
  // detail that must stay reachable (data floor) without dominating a tab.
  function buildExpander(id, summaryText, contentNode) {
    var det = document.createElement('details');
    det.id = id;
    var sum = document.createElement('summary');
    sum.textContent = summaryText;
    det.appendChild(sum);
    det.appendChild(contentNode);
    return det;
  }

  // Relative age from the SERVER clock: max(0, nowMs - ms). Serves a circuit's
  // "open for" duration (from open_since_ms) and a learned negative's
  // last-observation age (from last_seen_ms). `nowMs` is the as_of of the
  // record the input came from -- passed in per section rather than read off a
  // shared render clock, because a page-global clock would age one panel's
  // figures against another panel's as_of. A null / absent input (closed /
  // post-restart, or no last_seen) or an unusable clock renders "-" -- never a
  // negative age, never invented pre-restart history.
  function ageSince(ms, nowMs) {
    if (ms === null || ms === undefined || !isFinite(nowMs)) {
      return '-';
    }
    return humanDuration(Math.max(0, nowMs - Number(ms)));
  }

  // The instant a record's figures are read against: the record's OWN as_of.
  // Never Date.now(), so client skew can neither invent an age nor un-elapse a
  // reset; never a sibling source's as_of, so one panel's clock cannot age
  // another's data.
  function panelNowMs(rec) {
    var parsed = Date.parse(rec.asOf);
    return isFinite(parsed) ? parsed : NaN;
  }

  // Quota reset cell: a reset instant at or before the panel's as_of has
  // already passed, so it renders "elapsed" (with the absolute time in the
  // title) rather than as a live countdown target. Non-positive / absent
  // -> "-".
  function quotaResetCell(resetMs, nowMs) {
    if (resetMs === null || resetMs === undefined || Number(resetMs) <= 0) { return '-'; }
    if (isFinite(nowMs) && Number(resetMs) <= nowMs) {
      var span = document.createElement('span');
      span.textContent = 'elapsed';
      span.title = 'reset time already passed: ' + fmtTs(resetMs);
      return span;
    }
    return fmtTs(resetMs);
  }

  // A humanized numeric cell that carries the error color only when the
  // value is positive (errors / negative signals), else a plain figure.
  // Renders through magSpan so its K/M/B suffix stays muted like the rest.
  function negCell(n) {
    var span = magSpan(n);
    if (num0(n) > 0) { span.classList.add('neg'); }
    return span;
  }

  // ---- quantitative render helpers -------------------------------------

  // Magnitude number with a smaller, muted K/M/B suffix in its own span.
  // Number formatting is IDENTICAL to humanCount (humanCount stays untouched
  // for deflists / footnotes); this only splits the suffix for typography and
  // de-emphasizes a zero / absent value.
  function magSpan(n) {
    var span = document.createElement('span');
    if (n === null || n === undefined || !isFinite(Number(n))) {
      span.className = 'mag-zero';
      span.textContent = '-';
      return span;
    }
    var x = Number(n), neg = x < 0, a = Math.abs(x);
    if (x === 0) { span.className = 'mag-zero'; }
    if (a < 10000) {
      span.appendChild(document.createTextNode((neg ? '-' : '') + String(Math.trunc(a))));
      return span;
    }
    var v, s;
    if (a >= 1e9) { v = a / 1e9; s = 'B'; }
    else if (a >= 1e6) { v = a / 1e6; s = 'M'; }
    else { v = a / 1e3; s = 'K'; }
    var body = v.toFixed(1);
    if (body.slice(-2) === '.0') { body = body.slice(0, -2); }
    span.appendChild(document.createTextNode((neg ? '-' : '') + body));
    var unit = document.createElement('span');
    unit.className = 'unit';
    unit.textContent = s;
    span.appendChild(unit);
    return span;
  }

  // SVG namespace for the sparklines. All sparkline SVG is built with
  // createElementNS + setAttribute ONLY -- every attribute value is numeric-
  // by-construction (viewBox, polyline points) or a static literal, so no
  // wire / user string is ever written into the DOM as markup. This keeps the
  // page's textContent-only posture (innerHTML is never used) and the
  // page.rs mutation-channel scan intact (this namespace literal opens with a
  // quote-then-letter, not a quote-then-slash, so it is not a path literal).
  var SVG_NS = 'http://www.w3.org/2000/svg';

  // Hand-rolled trend sparkline over `{t, v}` samples (a query series'
  // buckets, keyed by bucket start). The polyline splits into independent
  // segments wherever the inter-sample gap exceeds `gapMs`, so a hole in the
  // grid renders as a break, never a line drawn across it.
  function sparkline(samples, gapMs) {
    return sparkSvg([{ samples: samples, b: false, fill: false }], gapMs);
  }

  // A single-series sparkline whose area under the curve is filled. The fill
  // belongs to a series that stands alone: two series sharing one scale would
  // occlude each other, so a pair stays lines.
  function sparkArea(samples, gapMs) {
    return sparkSvg([{ samples: samples, b: false, fill: true }], gapMs);
  }

  // Two series on ONE shared vertical scale so the pair is comparable at a
  // glance (per-series scaling would make a small series look like a large
  // one). The second rides the second data hue.
  function sparklinePair(a, b, gapMs) {
    return sparkSvg([{ samples: a, b: false, fill: false }, { samples: b, b: true, fill: false }], gapMs);
  }

  // The shared sparkline geometry. The vertical scale fits the min/max of
  // EVERY series with a div-by-zero guard (a flat series draws a centered
  // line); the horizontal one spans the samples' own timestamps, so a point
  // sits where its bucket starts rather than on an assumed stride.
  function sparkSvg(series, gapMs) {
    var W = 120, H = 24;
    var svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('class', 'spark');
    svg.setAttribute('viewBox', '0 0 ' + W + ' ' + H);
    svg.setAttribute('preserveAspectRatio', 'none');
    svg.setAttribute('aria-hidden', 'true');
    var all = [];
    series.forEach(function (s) { if (s.samples) { all = all.concat(s.samples); } });
    if (all.length < 2) { return svg; }
    var min = all[0].v, max = all[0].v, t0 = all[0].t, t1 = all[0].t;
    all.forEach(function (s) {
      if (s.v < min) { min = s.v; }
      if (s.v > max) { max = s.v; }
      if (s.t < t0) { t0 = s.t; }
      if (s.t > t1) { t1 = s.t; }
    });
    var vspan = max - min, tspan = t1 - t0;
    function px(s) {
      var x = tspan > 0 ? (s.t - t0) / tspan * W : W;
      var y = vspan > 0 ? (H - 2) - ((s.v - min) / vspan) * (H - 4) : H / 2;
      return x.toFixed(1) + ',' + y.toFixed(1);
    }
    series.forEach(function (s) {
      if (s.samples && s.samples.length >= 2) { drawSegments(svg, s, gapMs, px, H); }
    });
    return svg;
  }

  // Append one series as one polyline per gap-free run of samples, each run
  // optionally backed by the area under it. The area is a polygon closed on the
  // baseline rather than a filled polyline, so a run with a hole beside it
  // fills only its own span.
  function drawSegments(svg, series, gapMs, px, H) {
    var samples = series.samples;
    var seg = [];
    function flush() {
      if (seg.length >= 2) {
        if (series.fill) { svg.appendChild(areaPolygon(seg, H)); }
        var pl = document.createElementNS(SVG_NS, 'polyline');
        pl.setAttribute('points', seg.join(' '));
        if (series.b) { pl.setAttribute('class', 'series-b'); }
        svg.appendChild(pl);
      }
      seg = [];
    }
    for (var i = 0; i < samples.length; i++) {
      if (i > 0 && gapMs > 0 && (samples[i].t - samples[i - 1].t) > gapMs) { flush(); }
      seg.push(px(samples[i]));
    }
    flush();
  }

  // The run's own points plus the two baseline corners under its first and last
  // sample. Every coordinate is numeric-by-construction, exactly as the
  // polyline points are.
  function areaPolygon(seg, H) {
    var firstX = seg[0].split(',')[0];
    var lastX = seg[seg.length - 1].split(',')[0];
    var poly = document.createElementNS(SVG_NS, 'polygon');
    poly.setAttribute('points',
      firstX + ',' + H + ' ' + seg.join(' ') + ' ' + lastX + ',' + H);
    return poly;
  }

  // A title + optional hint header row. Shared by `card` and by sections that
  // are deliberately NOT cards (a set of separate objects rather than one
  // reading), so both carry identical header typography.
  function sectionHead(title, hint) {
    var head = document.createElement('div');
    head.className = 'card-head';
    var h = document.createElement('h2');
    h.className = 'card-title';
    h.textContent = title;
    head.appendChild(h);
    if (hint) {
      var hp = document.createElement('span');
      hp.className = 'card-hint';
      hp.textContent = hint;
      head.appendChild(hp);
    }
    return head;
  }

  // A refresh unit is a section the in-place update holds back, whole, while
  // the operator has text selected inside it (morphPanel), so a selection is
  // never cut by a refresh. Card and section wrappers mark themselves with a
  // key naming the section, unique within its view: a unit is held only
  // against its own rebuild, never against another section the morph happens
  // to pair it with.
  var REFRESH_UNIT_ATTR = 'data-unit';

  function markRefreshUnit(node, key) {
    node.setAttribute(REFRESH_UNIT_ATTR, key);
    return node;
  }

  // A card shell with a title, an optional hint, and a body. The single
  // container every tab section builds into, so card chrome lives in one
  // place rather than once per tab.
  function card(title, hint, bodyNode) {
    var c = markRefreshUnit(document.createElement('div'), title);
    c.className = 'card';
    c.appendChild(sectionHead(title, hint));
    if (bodyNode) { c.appendChild(bodyNode); }
    return c;
  }

  // A vertical stack of sections, the shape a tab body returns so its sections
  // sit on the 8px rhythm.
  function tabStack() {
    var stack = document.createElement('div');
    stack.className = 'tabstack';
    return stack;
  }

  // A big figure: the numeral plus an optional faint unit slot on either side
  // (a leading one for a currency mark, a trailing one for a magnitude). A zero
  // or absent value de-emphasizes whole -- a zero is faint, never an error.
  function figure(text, unit, pre) {
    var span = document.createElement('span');
    if (text === '-' || text === '0' || text === '0.00') { span.classList.add('mag-zero'); }
    if (pre) { span.appendChild(unitSlot(pre)); }
    span.appendChild(document.createTextNode(text));
    if (unit) { span.appendChild(unitSlot(unit)); }
    return span;
  }

  function unitSlot(text) {
    var u = document.createElement('span');
    u.className = 'unit';
    u.textContent = text;
    return u;
  }

  // A figure that carries no measurement at all (an unpriced cost, a metric
  // the window never observed): the word itself, de-emphasized.
  function faintFigure(text) {
    var span = figure(text, null, null);
    span.classList.add('mag-zero');
    return span;
  }

  // A millisecond span split into numeral + unit, promoted to seconds past a
  // full second so a tile never shows five digits of milliseconds. A
  // non-positive input means the metric was never observed (the adapter
  // coerces an absent figure to zero and no real latency is 0ms), so it reads
  // as absent rather than as an instant response.
  function msParts(ms) {
    var v = num0(ms);
    if (v <= 0) { return { v: '-', u: '' }; }
    if (v < 1000) { return { v: String(Math.round(v)), u: 'ms' }; }
    return { v: (v / 1000).toFixed(1), u: 's' };
  }

  function msText(ms) {
    var p = msParts(ms);
    return p.v + p.u;
  }

  // A dollar amount at a precision that cannot hide a real cost: cents at a
  // dollar and above, tenths of a cent below it.
  function money(v) {
    var x = Number(v);
    if (!isFinite(x) || x === 0) { return '0.00'; }
    return Math.abs(x) >= 1 ? x.toFixed(2) : x.toFixed(3);
  }

  // A faint one-line note under a tile's figure, carrying the error color only
  // when it reports a negative signal.
  function subNote(text, neg) {
    var span = document.createElement('span');
    if (neg) { span.className = 'neg'; }
    span.textContent = text;
    return span;
  }

  // A thin proportion bar: a hairline track with a fill sized by --pct. Used
  // where the proportion rides UNDER a reading rather than behind a numeric
  // cell (which is barCell's job).
  function shareBar(pct) {
    var track = document.createElement('span');
    track.className = 'sharebar';
    var fill = document.createElement('span');
    fill.className = 'sharebar-fill';
    fill.style.setProperty('--pct', Math.max(0, Math.min(100, isFinite(pct) ? pct : 0)));
    track.appendChild(fill);
    return track;
  }

  // =====================================================================
  // Per-tab section builders.
  //
  // One contiguous marker-delimited block per tab, in VIEWS order, each
  // registered as a view's `build` in dash_80_views.js. Every builder:
  //   1. consults `stateCard(rec)` first and returns it when non-null, so
  //      loading / unavailable / incompatible / dead states are uniform;
  //   2. reads a query-backed tab's data through `queryView()` (adapter
  //      properties only, never raw JSON) and a GET-backed tab's through
  //      `rec.data` / `SOURCES[<name>].data` per the view's `sources`;
  //   3. wraps EVERY section that reads a `sources` entry other than the
  //      primary in `safeSection(SOURCES[<name>], build)`, so a secondary
  //      source's fault costs that section only -- renderActiveTab's
  //      try/catch replaces the whole tab and is the floor, not the plan;
  //   4. returns ONE node.
  // =====================================================================

