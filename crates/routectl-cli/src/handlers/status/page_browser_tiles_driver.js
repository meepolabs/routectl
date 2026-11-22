// ---- scenarios: a KPI tile sparkline draws what each bucket reported -----
//
// Test-only snippet injected after page_browser_driver.js. Each scenario
// replaces the Now series with a synthetic six-bucket grid, one bucket width
// apart, so a gap in a drawn line can only come from a bucket's own value.
// The n-th series answer takes the n-th entry of the scenario's `rounds`
// (the last repeats), and every bucket's metrics start as a copy of the
// fixture's first bucket with the round's per-bucket overrides applied.
(function (d) {
  'use strict';

  var BUCKETS = 6;

  // `rounds[r][field]` is a six-value array for that field, or absent to keep
  // the fixture's value.
  function rewriteSeries(rounds) {
    var served = 0;
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status/query') { return text; }
      var panel = JSON.parse(text);
      if (!panel.data || !panel.data.series) { return text; }
      var round = rounds[Math.min(served, rounds.length - 1)];
      served += 1;
      panel.data.series.buckets = syntheticBuckets(panel.data.series, round);
      return JSON.stringify(panel);
    });
  }

  function syntheticBuckets(series, round) {
    var first = series.buckets[0];
    var out = [];
    for (var i = 0; i < BUCKETS; i++) {
      var metrics = JSON.parse(JSON.stringify(first.metrics));
      Object.keys(round).forEach(function (field) { metrics[field] = round[field][i]; });
      out.push({ start_ms: first.start_ms + i * series.bucket_ms, metrics: metrics });
    }
    return out;
  }

  function tileByLabel(label) {
    var tiles = document.querySelectorAll('#body-now .kpi');
    for (var i = 0; i < tiles.length; i++) {
      var l = tiles[i].querySelector('.kpi-label');
      if (l && l.textContent === label) { return tiles[i]; }
    }
    return null;
  }

  function points(node) {
    return node.getAttribute('points').split(' ').map(function (pair) {
      return pair.split(',').map(Number);
    });
  }

  // Every polyline of the tile's sparkline as its points, split by series.
  function sparkOf(label) {
    var tile = tileByLabel(label);
    if (!tile) { throw new Error('no KPI tile labelled ' + label); }
    var svg = tile.querySelector('svg.spark');
    var out = { a: [], b: [], polygons: svg.querySelectorAll('polygon').length };
    Array.prototype.forEach.call(svg.querySelectorAll('polyline'), function (pl) {
      var key = pl.getAttribute('class') === 'series-b' ? 'b' : 'a';
      out[key].push(points(pl));
    });
    return out;
  }

  // Observe `label` after the first series answer, then after each further one.
  function acrossRounds(label, rounds) {
    rewriteSeries(rounds);
    return d.waitUntil('the ' + label + ' tile', function () { return tileByLabel(label); })
      .then(function () {
        var out = [sparkOf(label)];
        var step = Promise.resolve();
        for (var r = 1; r < rounds.length; r++) {
          step = step.then(function () { return d.waitFor('/status/query', 1); })
            .then(function () { out.push(sparkOf(label)); });
        }
        return step.then(function () { return out; });
      });
  }

  var TTFT = 'Time to first token (p50)';

  d.scenario('tiles-ttft-null-bucket', function () {
    return acrossRounds(TTFT, [
      { ttft_p50_ms: [100, 200, 300, 150, 120, 180] },
      { ttft_p50_ms: [100, 200, null, 150, 120, 180] }
    ]);
  });

  d.scenario('tiles-ttft-zero-bucket', function () {
    return acrossRounds(TTFT, [
      { ttft_p50_ms: [100, 200, 0, 150, 120, 180] }
    ]);
  });

  // Totals and buckets whose mean and median disagree, so a tile reading the
  // mean anywhere shows it: the headline, its note and the sparkline.
  d.scenario('tiles-ttft-p50', function () {
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status/query') { return text; }
      var panel = JSON.parse(text);
      if (!panel.data || !panel.data.series) { return text; }
      panel.data.totals.ttft_avg_ms = 9400;
      panel.data.totals.ttft_max_ms = 61000;
      panel.data.totals.ttft_p50_ms = 2500;
      return JSON.stringify(panel);
    });
    rewriteSeries([{
      ttft_avg_ms: [600, 500, 400, 300, 200, 100],
      ttft_p50_ms: [100, 200, 300, 400, 500, 600]
    }]);
    return d.waitUntil('the ' + TTFT + ' tile', function () { return tileByLabel(TTFT); })
      .then(function () {
        var tile = tileByLabel(TTFT);
        return {
          labels: Array.prototype.map.call(document.querySelectorAll('#body-now .kpi-label'),
            function (l) { return l.textContent; }),
          value: tile.querySelector('.kpi-value').textContent,
          sub: tile.querySelector('.kpi-sub').textContent,
          spark: sparkOf(TTFT)
        };
      });
  });

  d.scenario('tiles-cache-write-absent', function () {
    return acrossRounds('Cache traffic', [
      {
        cache_read_billed: [50, 60, 70, 80, 90, 100],
        cache_write_tokens: [10, 20, 0, 0, 30, 40],
        cache_write_present: [1, 1, 0, 0, 1, 1]
      },
      {
        cache_read_billed: [50, 60, 70, 80, 90, 100],
        cache_write_tokens: [0, 0, 0, 0, 0, 0],
        cache_write_present: [0, 0, 0, 0, 0, 0]
      }
    ]);
  });
})(window.routectlDriver);

// ---- scenarios: the cost tile, the provider card costs, the first run ----
//
// The n-th series answer has the n-th entry of the scenario's `rounds` (the
// last repeats) applied to its totals and groups, so each observation reads
// cost fields the scenario chose rather than whatever the fixture priced.
(function (d) {
  'use strict';

  var COST_TILE = 'Est. cost';

  function textOr(node, selector) {
    var found = node.querySelector(selector);
    return found ? found.textContent : null;
  }

  function titleOr(node, selector) {
    var found = node.querySelector(selector);
    return found ? found.title : null;
  }

  // The totals and groups of the last series answer handed to the page, as
  // rewritten.
  var lastTotals = {};
  var lastGroups = [];

  // `round.totals` is merged into the totals; `round.groups[i]` into the
  // i-th group's metrics (cycling, so every group gets one).
  function rewriteCosts(rounds) {
    var served = 0;
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status/query') { return text; }
      var panel = JSON.parse(text);
      if (!panel.data || !panel.data.series) { return text; }
      var round = rounds[Math.min(served, rounds.length - 1)];
      served += 1;
      merge(panel.data.totals, round.totals);
      panel.data.groups.forEach(function (g, i) {
        merge(g.metrics, round.groups[i % round.groups.length]);
      });
      lastTotals = panel.data.totals;
      lastGroups = panel.data.groups;
      return JSON.stringify(panel);
    });
  }

  function merge(into, from) {
    Object.keys(from).forEach(function (k) { into[k] = from[k]; });
  }

  function costTile() {
    var tiles = document.querySelectorAll('#body-now .kpi');
    for (var i = 0; i < tiles.length; i++) {
      if (textOr(tiles[i], '.kpi-label') === COST_TILE) { return tiles[i]; }
    }
    return null;
  }

  // The cost tile's reading, and every drawn provider card's cost with the
  // cost status the round gave it: its group's, or the totals' for the
  // all-providers card. A card whose name matches no group reports
  // `unmatched`, so a test cannot mistake it for an unqualified one.
  function costObservation() {
    var tile = costTile();
    if (!tile) { throw new Error('no ' + COST_TILE + ' tile'); }
    var statusOf = {};
    lastGroups.forEach(function (g) { statusOf[g.label] = g.metrics.cost_status; });
    var cards = Array.prototype.map.call(
      document.querySelectorAll('#body-now .provcard-scope[aria-pressed]'),
      function (btn) {
        var name = textOr(btn, '.provcard-name');
        return {
          id: btn.id,
          status: btn.id === 'overview-provider-all'
            ? lastTotals.cost_status
            : (statusOf[name] || 'unmatched'),
          cost: textOr(btn, '.provcard-cost'),
          qual: textOr(btn, '.provcard-qual')
        };
      });
    return {
      value: textOr(tile, '.kpi-value'),
      alt_label: textOr(tile, '.kpi-alt-label'),
      alt_value: d.shownText(tile.querySelector('.kpi-alt-value')),
      alt_approx: d.approxType(tile.querySelector('.kpi-alt-value')),
      alt_note: textOr(tile, '.kpi-alt-note'),
      alt_mark: d.shownText(tile.querySelector('.kpi-alt-value .cost-mark')),
      alt_mark_spoken: d.spokenMark(tile.querySelector('.kpi-alt-value .cost-mark')),
      alt_value_name: d.accessibleName(tile.querySelector('.kpi-alt-value')),
      alt_mark_title: titleOr(tile, '.kpi-alt-value .cost-mark'),
      alt_faint: !!tile.querySelector('.kpi-alt-value.mag-zero'),
      requests: lastTotals.requests,
      sub: textOr(tile, '.kpi-sub'),
      caption: textOr(tile, '.kpi-caption'),
      caption_before_spark: captionPrecedesSpark(tile),
      cards: cards
    };
  }

  function captionPrecedesSpark(tile) {
    var caption = tile.querySelector('.kpi-caption');
    var spark = tile.querySelector('svg.spark');
    return !!(caption && spark && caption.nextElementSibling === spark);
  }

  function costAcrossRounds(rounds) {
    rewriteCosts(rounds);
    return d.waitUntil('the cost tile', costTile).then(function () {
      var out = [costObservation()];
      var step = Promise.resolve();
      for (var r = 1; r < rounds.length; r++) {
        step = step.then(function () { return d.waitFor('/status/query', 1); })
          .then(function () { out.push(costObservation()); });
      }
      return step.then(function () { return out; });
    });
  }

  // Each equivalent covers every subscription row: no price is missing.
  d.scenario('tiles-cost-equivalent', function () {
    var group = { cost_status: 'priced', cost_usd: 1.25, equivalent_cost_usd: null };
    return costAcrossRounds([
      { totals: { cost_status: 'priced', cost_usd: 12.5, equivalent_cost_usd: 40.25,
        equivalent_status: 'complete', missing_prices: [], missing_price_dimensions: [] },
        groups: [group] },
      { totals: { cost_status: 'subscription', cost_usd: null, equivalent_cost_usd: 40.25,
        equivalent_status: 'complete', missing_prices: [], missing_price_dimensions: [] },
        groups: [group] },
      { totals: { cost_status: 'priced', cost_usd: 12.5, equivalent_cost_usd: null },
        groups: [group] }
    ]);
  });

  d.scenario('tiles-cost-partial', function () {
    return costAcrossRounds([
      {
        totals: { cost_status: 'partial', cost_usd: 30.85, equivalent_cost_usd: null },
        groups: [
          { cost_status: 'partial', cost_usd: 27.39 },
          { cost_status: 'priced', cost_usd: 3.46 }
        ]
      },
      {
        totals: { cost_status: 'priced', cost_usd: 30.85, equivalent_cost_usd: null },
        groups: [{ cost_status: 'priced', cost_usd: 10.0 }]
      }
    ]);
  });

  // The window's API-price equivalent: a subtotal the server calls partial,
  // one it calls complete, subscription traffic with no row priced at all,
  // and a complete equivalent in a window whose listed gap belongs to an
  // unpriced API-key row.
  d.scenario('tiles-equivalent-coverage', function () {
    var group = { cost_status: 'priced', cost_usd: 1.25, equivalent_cost_usd: null };
    var gap = [{ model: 'opus', dimension: 'cache write (unknown lifetime)', config_key: null }];
    return costAcrossRounds([
      { totals: { cost_status: 'subscription', cost_usd: null, equivalent_cost_usd: 617.54,
        equivalent_status: 'partial',
        missing_prices: gap, missing_price_dimensions: [gap[0].dimension] },
        groups: [group] },
      { totals: { cost_status: 'subscription', cost_usd: null, equivalent_cost_usd: 40.25,
        equivalent_status: 'complete',
        missing_prices: [], missing_price_dimensions: [] },
        groups: [group] },
      { totals: { cost_status: 'subscription', cost_usd: null, equivalent_cost_usd: null,
        equivalent_status: 'partial',
        missing_prices: gap, missing_price_dimensions: [gap[0].dimension] },
        groups: [group] },
      { totals: { cost_status: 'partial', cost_usd: null, equivalent_cost_usd: 40.25,
        equivalent_status: 'complete',
        missing_prices: gap, missing_price_dimensions: [gap[0].dimension] },
        groups: [group] }
    ]);
  });

  // A partial window and partial provider whose mix holds no priced row: the
  // server sends no dollar figure for either.
  d.scenario('tiles-cost-partial-unpriced', function () {
    return costAcrossRounds([
      {
        totals: { cost_status: 'partial', cost_usd: null, equivalent_cost_usd: null },
        groups: [
          { cost_status: 'partial', cost_usd: null },
          { cost_status: 'priced', cost_usd: 3.46 }
        ]
      }
    ]);
  });

  // The series answer reports no request in the window, and the ledger's
  // first request at `earliestMs` (null: the ledger holds none).
  function emptyWindow(earliestMs) {
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status/query') { return text; }
      var panel = JSON.parse(text);
      if (!panel.data || !panel.data.series) { return text; }
      panel.data.totals.requests = 0;
      panel.data.groups = [];
      panel.data.earliest_ms = earliestMs;
      return JSON.stringify(panel);
    });
  }

  function emptyObservation() {
    var card = document.querySelector('#body-now .card.empty');
    if (!card) { return null; }
    var code = card.querySelector('.empty-baseurl code');
    return {
      title: textOr(card, '.empty-title'),
      body: textOr(card, '.empty-body'),
      base_url: code ? code.textContent : null,
      base_url_focusable: code ? code.tabIndex === 0 : false
    };
  }

  function emptyAfterSeriesAnswer() {
    return d.waitFor('/status/query', 1)
      .then(function () { return d.waitUntil('the empty state', emptyObservation); });
  }

  d.scenario('tiles-first-run', function () {
    emptyWindow(null);
    return emptyAfterSeriesAnswer();
  });

  // A first request well before any window the page offers.
  var EARLIEST_MS = 86400000;

  d.scenario('tiles-empty-window', function () {
    emptyWindow(EARLIEST_MS);
    return emptyAfterSeriesAnswer();
  });
})(window.routectlDriver);

// ---- scenario: the cache-hit tile's text for each row of the shared table -
//
// The table arrives as `window.routectlCacheHitRows` ([{pct, text}], the
// test sets it from the fixture). Row i is served as the totals'
// `cache_hit_pct` of the series answers delivered after it is chosen, and the
// tile is read once such an answer has landed.
(function (d) {
  'use strict';

  var CACHE_HIT_TILE = 'Cache hit';

  function cacheHitValue() {
    var tiles = document.querySelectorAll('#body-now .kpi');
    for (var i = 0; i < tiles.length; i++) {
      var label = tiles[i].querySelector('.kpi-label');
      if (label && label.textContent === CACHE_HIT_TILE) {
        var value = tiles[i].querySelector('.kpi-value');
        return value ? value.textContent : null;
      }
    }
    return null;
  }

  d.scenario('tiles-cache-hit-rows', function () {
    var rows = window.routectlCacheHitRows;
    if (!rows || !rows.length) { throw new Error('no cache hit rows were set'); }
    var current = 0;
    d.rewrite(function (url, text) {
      if (url.pathname !== '/status/query') { return text; }
      var panel = JSON.parse(text);
      if (!panel.data || !panel.data.series) { return text; }
      panel.data.totals.cache_hit_pct = rows[current].pct;
      return JSON.stringify(panel);
    });
    var out = [];
    var step = d.waitUntil('the ' + CACHE_HIT_TILE + ' tile', cacheHitValue);
    rows.forEach(function (_, i) {
      step = step.then(function () {
        current = i;
        return d.waitFor('/status/query', 1);
      }).then(function () {
        out.push({ row: i, text: cacheHitValue() });
      });
    });
    return step.then(function () { return out; });
  });
})(window.routectlDriver);
