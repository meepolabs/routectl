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

  var TTFT = 'Time to first token (avg)';

  d.scenario('tiles-ttft-null-bucket', function () {
    return acrossRounds(TTFT, [
      { ttft_avg_ms: [100, 200, 300, 150, 120, 180] },
      { ttft_avg_ms: [100, 200, null, 150, 120, 180] }
    ]);
  });

  d.scenario('tiles-ttft-zero-bucket', function () {
    return acrossRounds(TTFT, [
      { ttft_avg_ms: [100, 200, 0, 150, 120, 180] }
    ]);
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
