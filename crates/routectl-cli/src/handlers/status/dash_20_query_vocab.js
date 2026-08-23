  // ---- query field vocabulary + adapter --------------------------------

  // The field vocabulary of a `/status/query` metrics object, split by how a
  // value is read rather than by what it means. These two arrays are the ONLY
  // place a raw query field name appears in this file: adaptMetrics below is
  // their sole consumer, and every render path reads adapter properties
  // instead of touching raw JSON or repeating a field name. A page.rs drift
  // test asserts every name in BOTH arrays is a field of the server's
  // `QueryMetrics`, derived from serde rather than a second hardcoded list, so
  // a server-side rename cannot silently turn a column into zeroes.
  var QUERY_METRICS = [
    'requests',
    'ok',
    'errors',
    'input_tokens',
    'output_tokens',
    'reasoning_tokens',
    'cache_read_billed',
    'cache_write_5m',
    'cache_write_1h',
    'server_tool_calls',
    'stream_count',
    'client_disconnect_total',
    'fallback_served',
    'ttft_p50_ms',
    'ttft_p95_ms',
    'latency_p50_ms',
    'latency_p95_ms',
    'throughput_tok_s',
    'ctx_avg',
    'ctx_peak',
    'cache_hit_pct',
    'cost_usd'
  ];

  // Fields whose value is a TOKEN, not a figure: carried through verbatim,
  // never coerced (a num0 pass would turn `unpriced` into a zero cost).
  var QUERY_TOKENS = [
    'cost_status'
  ];

  // The COMPLETE set of request bodies this page can issue: every selectable
  // window crossed with every group-by the tabs use, in both the series read
  // path (Overview) and the non-series one (Usage), each carrying the bucket
  // that window resolves to -- hourly reads well over a day, anything wider
  // needs daily. An emitted body is one of these entries verbatim plus the
  // optional provider scope, so nothing is overwritten at runtime and the
  // page.rs drift test validates the whole request vocabulary through the
  // server's own parser. Written as strict JSON for that test.
  var QUERY_SHAPES = [
    {"window":"today","group_by":"provider","bucket":"hour"},
    {"window":"week","group_by":"provider","bucket":"day"},
    {"window":"month","group_by":"provider","bucket":"day"},
    {"window":"all","group_by":"provider","bucket":"day"},
    {"window":"today","group_by":"model"},
    {"window":"today","group_by":"alias"},
    {"window":"today","group_by":"provider"},
    {"window":"week","group_by":"model"},
    {"window":"week","group_by":"alias"},
    {"window":"week","group_by":"provider"},
    {"window":"month","group_by":"model"},
    {"window":"month","group_by":"alias"},
    {"window":"month","group_by":"provider"},
    {"window":"all","group_by":"model"},
    {"window":"all","group_by":"alias"},
    {"window":"all","group_by":"provider"}
  ];

  // Flatten ONE metrics object: every numeric field coerced through num0,
  // every token field carried through as it arrived. No rename, no derived
  // field.
  function adaptMetrics(raw) {
    var from = raw || {};
    var out = {};
    QUERY_METRICS.forEach(function (key) { out[key] = num0(from[key]); });
    QUERY_TOKENS.forEach(function (key) { out[key] = from[key]; });
    return out;
  }

  // The thin flat-extraction layer over a live QUERY payload: it walks
  // `{groups, totals, series}` ONCE and hands back the same shape with every
  // metric coerced. Deliberately NOT a render model -- it renames nothing
  // and computes nothing, so if it ever grows either, inline it back into
  // the callers.
  function QueryAdapter(raw) {
    var from = raw || {};
    var series = from.series
      ? {
        bucket_ms: num0(from.series.bucket_ms),
        buckets: (from.series.buckets || []).map(function (b) {
          return { start_ms: num0(b.start_ms), metrics: adaptMetrics(b.metrics) };
        })
      }
      : null;
    return {
      groups: (from.groups || []).map(function (g) {
        return { label: g.label, metrics: adaptMetrics(g.metrics) };
      }),
      totals: adaptMetrics(from.totals),
      series: series
    };
  }

  // Whether a RAW query payload matches its declared shape: an object carrying
  // a `groups` ARRAY, an object `totals`, and a `series` member present as
  // either an object or an explicit null (the server always emits it, never
  // skips it). Checked on the raw payload rather than after adaptation, because
  // the adapter substitutes empty arrays and zero metrics for absent members --
  // which would render a corrupt payload as an empty ledger.
  //
  // An EMPTY ledger satisfies this shape: `groups: []` beside zero totals is a
  // measurement, and it must keep reaching the welcoming empty state.
  function isQueryShape(raw) {
    if (!raw || typeof raw !== 'object' || Array.isArray(raw)) { return false; }
    if (!Array.isArray(raw.groups)) { return false; }
    if (!raw.totals || typeof raw.totals !== 'object' || Array.isArray(raw.totals)) {
      return false;
    }
    if (!Object.prototype.hasOwnProperty.call(raw, 'series')) { return false; }
    var series = raw.series;
    return series === null ||
      (!!series && typeof series === 'object' && !Array.isArray(series));
  }

  // The adapted QUERY payload of ONE query source, or null when that source
  // carries no usable payload. `stale` counts as usable: a 503 RETAINS the
  // last-good data by design, and the degradation is the section status line's
  // job to report -- discarding it here would turn a recoverable overload into
  // an invalid-payload card and lose the stale-values reading.
  //
  // A payload that does not match its declared shape returns null, so both
  // query-backed builders throw inside `safeSection` and the source records
  // `invalid_payload` -- corruption is never adapted into an empty ledger.
  function queryViewOf(name) {
    var rec = SOURCES[name];
    var usable = rec.state === 'live' || rec.state === 'stale';
    return (usable && isQueryShape(rec.data)) ? QueryAdapter(rec.data) : null;
  }

  // Usage's series-less read.
  function queryView() {
    return queryViewOf(QUERY_SOURCE);
  }

  // Overview's bucketed read. A separate source rather than the same one at
  // another shape, so a series-less Usage payload can never become what Overview
  // renders from (see the QUERY source split in the state part).
  function querySeriesView() {
    return queryViewOf(QUERY_SERIES_SOURCE);
  }

