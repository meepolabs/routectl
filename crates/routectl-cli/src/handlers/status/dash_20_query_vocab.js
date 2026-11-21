  // ---- query field vocabulary + adapter --------------------------------

  // The field vocabulary of a `/status/query` metrics object, split by how a
  // value is read rather than by what it means. These arrays are the ONLY
  // place a raw query field name appears in this file: the shape check and
  // adaptMetrics below are their sole consumers, and every render path reads
  // adapter properties instead of touching raw JSON or repeating a field name.
  // A page.rs drift test asserts every name in them is a field of the server's
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
    'cache_write_tokens',
    'cache_write_present',
    'cache_read_present',
    'server_tool_calls',
    'server_tool_present',
    'stream_count',
    'client_disconnect_total',
    'client_disconnect_pre_dispatch',
    'fallback_served',
    'cache_write_unknown_tokens',
    'ttft_avg_ms',
    'ttft_max_ms',
    'ttft_p50_ms',
    'ttft_p90_ms',
    'ttft_p99_ms',
    'latency_avg_ms',
    'latency_max_ms',
    'latency_p50_ms',
    'latency_p90_ms',
    'latency_p99_ms',
    'throughput_tok_s',
    'ctx_avg',
    'ctx_peak',
    'cache_hit_pct',
    'cost_usd',
    'equivalent_cost_usd'
  ];

  // The metrics the server sends as an explicit null when no row could
  // contribute to them (an average with no samples, a cost with no priced
  // row). Every other metric is a count that is always present; a payload
  // missing one, or carrying it as anything but a finite number, is malformed.
  // A test derives both halves from a serialized default `QueryMetrics`.
  var QUERY_NULLABLE_METRICS = [
    'ttft_avg_ms',
    'ttft_max_ms',
    'ttft_p50_ms',
    'ttft_p90_ms',
    'ttft_p99_ms',
    'latency_avg_ms',
    'latency_max_ms',
    'latency_p50_ms',
    'latency_p90_ms',
    'latency_p99_ms',
    'throughput_tok_s',
    'ctx_avg',
    'ctx_peak',
    'cache_hit_pct',
    'cost_usd',
    'equivalent_cost_usd'
  ];

  // Fields whose value is a TOKEN, not a figure: carried through verbatim,
  // never coerced (a num0 pass would turn `unpriced` into a zero cost).
  var QUERY_TOKENS = [
    'cost_status'
  ];

  // Fields whose value is a LIST of missing prices, always present (empty
  // when every used dimension was priced). Each entry names a model and a
  // dimension as text, plus the config key that would price it, null where
  // no key can; the two field lists below are that entry's whole shape.
  var QUERY_MISSING_PRICE_LISTS = [
    'missing_prices'
  ];

  var MISSING_PRICE_TEXT = [
    'model',
    'dimension'
  ];

  var MISSING_PRICE_NULLABLE_TEXT = [
    'config_key'
  ];

  // The dimensions a query can group by, as [token, picker label], in picker
  // order. The URL codec, the transport and the Traffic picker all read this
  // one list; a page.rs test pins the tokens to the server's groupings.
  var GROUP_BYS = [
    ['model', 'Model'],
    ['alias', 'Alias'],
    ['provider', 'Provider'],
    ['seat', 'Seat']
  ];

  // The fields this page reads from each GET panel, at any depth, envelope
  // included. Nothing reads these arrays at runtime: they are the page's half
  // of a contract page.rs checks in both directions. Each name must be a key
  // the server's own DTO serializes for THAT panel, and every lowercase member
  // read anywhere in the script must be declared here, be a key of an object
  // literal the script builds, or be a JS/DOM built-in. A field the page starts
  // reading goes into its panel's list here.
  var USAGE_FIELDS = [
    'schema_version', 'unavailable', 'as_of', 'data',
    'window', 'groups', 'totals',
    'alias', 'model', 'requests', 'errors'
  ];

  var HEALTH_FIELDS = [
    'schema_version', 'unavailable', 'as_of', 'data',
    'targets', 'pools', 'seat_driver', 'learned_negatives',
    'nickname', 'provider_name', 'upstream', 'seat_label', 'state_key', 'learned_lane',
    'circuit', 'open_since_ms', 'half_open_probe_in_flight', 'rpm_available',
    'last_outcome', 'last_outcome_at_ms',
    'eligibility', 'reason', 'cause', 'since_ms', 'fix',
    'pool', 'configured', 'eligible', 'by_reason', 'seats',
    'running', 'last_sweep_ms', 'last_good_sweep_ms', 'sweeps', 'transient', 'definitive',
    'capability_key', 'verdict', 'source', 'value', 'last_seen_ms'
  ];

  var CONFIG_FIELDS = [
    'schema_version', 'unavailable', 'as_of', 'data',
    'source', 'aliases', 'models', 'providers', 'capabilities', 'activation', 'classes',
    'config_path', 'loaded_age_ms', 'alias_count', 'provider_count', 'listen_addr', 'version',
    'alias', 'chain',
    'nickname', 'provider', 'upstream', 'provider_kind', 'context_window', 'fill', 'tokens',
    'prices', 'field', 'per_mtok', 'table_date',
    'provider_id', 'format', 'endpoint_origin', 'cloud', 'auth_token', 'credential_ref_scheme',
    'rpm_limit', 'status', 'reason', 'referenced_by_aliases',
    'target_spec', 'capability_key', 'verdict', 'provenance',
    'class', 'retry_cap', 'fallback', 'debits_breaker'
  ];

  var DOCTOR_FIELDS = [
    'schema_version', 'unavailable', 'as_of', 'data',
    'report', 'reachability', 'capability_writes',
    'findings', 'status', 'section', 'name', 'detail', 'remediation',
    'capability_events_dropped_full', 'write_errors', 'capability_writer_unavailable',
    'panels', 'capability_matrix', 'availability', 'state', 'code', 'source', 'columns', 'lanes',
    'warm', 'outcome', 'class', 'summary', 'replay',
    'loaded_rows', 'replayed', 'skipped_vocab', 'skipped_owner', 'skipped_revision',
    'skipped_lane', 'skipped_unknown',
    'lane', 'routed', 'provider_kind', 'nicknames', 'cells',
    'action', 'verdict', 'layer', 'stale', 'nickname_actions', 'nickname',
    'first_seen_ms', 'last_seen_ms', 'expires_at_ms', 'value'
  ];

  var ATTENTION_FIELDS = [
    'schema_version', 'unavailable', 'as_of', 'data',
    'verdict', 'counts', 'coverage', 'items',
    'severity', 'text', 'target', 'targets',
    'broken', 'degraded', 'recovered', 'doctor_fail',
    'doctor',
    'key', 'kind', 'state', 'subject', 'account', 'cause', 'raw_token',
    'since_ms', 'until_ms', 'recovered_at_ms', 'fix', 'remediation'
  ];

  var ACCOUNTS_FIELDS = [
    'schema_version', 'unavailable', 'as_of', 'data',
    'accounts', 'refresh_driver', 'poll_driver', 'running',
    'account', 'provider_entry', 'login_label', 'report', 'windows', 'last_accepted', 'poll',
    'pools', 'subscription', 'credential',
    'class', 'length_secs', 'scope', 'used', 'remaining', 'reset_at_ms', 'started', 'source',
    'observed_at_ms', 'age_ms', 'stale', 'kept',
    'placement', 'pool', 'model', 'seats', 'rank', 'reason',
    'applicability', 'vendor', 'last_attempt_ms', 'last_result', 'next_due_ms', 'failure_since_ms',
    'state', 'cause', 'since_ms', 'until_ms', 'fix', 'remediation'
  ];

  // The COMPLETE set of request bodies this page can issue: every selectable
  // window crossed with every group-by the views use, in both the series read
  // path (Overview) and the non-series one (Usage and the per-seat read), each
  // series body carrying the bucket that window resolves to. A calendar window
  // wider than a day needs daily; the trailing ranges stay hourly, since seven
  // days of hours sits well under the server's series bucket cap. An emitted
  // body is one of these entries verbatim plus the optional provider scope, so
  // nothing is overwritten at runtime and the page.rs drift test validates the
  // whole request vocabulary through the server's own parser. Written as
  // strict JSON for that test.
  var QUERY_SHAPES = [
    {"window":"today","group_by":"provider","bucket":"hour"},
    {"window":"week","group_by":"provider","bucket":"day"},
    {"window":"month","group_by":"provider","bucket":"day"},
    {"window":"all","group_by":"provider","bucket":"day"},
    {"window":"last24h","group_by":"provider","bucket":"hour"},
    {"window":"last7d","group_by":"provider","bucket":"hour"},
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
    {"window":"all","group_by":"provider"},
    {"window":"last24h","group_by":"model"},
    {"window":"last24h","group_by":"alias"},
    {"window":"last24h","group_by":"provider"},
    {"window":"last7d","group_by":"model"},
    {"window":"last7d","group_by":"alias"},
    {"window":"last7d","group_by":"provider"},
    {"window":"today","group_by":"seat"},
    {"window":"week","group_by":"seat"},
    {"window":"month","group_by":"seat"},
    {"window":"all","group_by":"seat"},
    {"window":"last24h","group_by":"seat"},
    {"window":"last7d","group_by":"seat"}
  ];

  // Flatten ONE validated metrics object (see isMetricsShape): every field
  // carried through as it arrived, a nullable metric's null included, so an
  // absent measurement reaches the renderer as null rather than as a zero. No
  // rename, no derived field.
  function adaptMetrics(raw) {
    var out = {};
    QUERY_METRICS.forEach(function (key) { out[key] = raw[key]; });
    QUERY_TOKENS.forEach(function (key) { out[key] = raw[key]; });
    QUERY_MISSING_PRICE_LISTS.forEach(function (key) { out[key] = raw[key]; });
    return out;
  }

  // The thin flat-extraction layer over a live QUERY payload that passed
  // isQueryShape: it walks `{groups, totals, series, earliest_ms}` ONCE and hands back the
  // same shape. Deliberately NOT a render model -- it renames nothing and
  // computes nothing, so if it ever grows either, inline it back into the
  // callers.
  function QueryAdapter(raw) {
    var series = raw.series
      ? {
        bucket_ms: raw.series.bucket_ms,
        buckets: raw.series.buckets.map(function (b) {
          return { start_ms: b.start_ms, metrics: adaptMetrics(b.metrics) };
        })
      }
      : null;
    return {
      groups: raw.groups.map(function (g) {
        return { label: g.label, metrics: adaptMetrics(g.metrics) };
      }),
      totals: adaptMetrics(raw.totals),
      series: series,
      earliest_ms: raw.earliest_ms
    };
  }

  // Whether a RAW query payload matches its declared shape, all the way down:
  // a `groups` array of labelled metrics, a `totals` metrics object, and a
  // `series` member present as an explicit null (the server always emits it,
  // never skips it) or as a bucket width beside an array of time-stamped
  // metrics buckets, and an `earliest_ms` present as a number or an explicit
  // null (an empty ledger). Checked on the raw payload, so a missing or wrongly typed
  // member fails the source as invalid_payload rather than reaching the page
  // as a zero or an empty ledger.
  //
  // An EMPTY ledger satisfies this shape: `groups: []` beside zero totals is a
  // measurement, and it must keep reaching the welcoming empty state.
  function isQueryShape(raw) {
    if (!isPlainObject(raw)) { return false; }
    if (!Array.isArray(raw.groups) || !raw.groups.every(isGroupShape)) { return false; }
    if (!isMetricsShape(raw.totals)) { return false; }
    if (!isNullableNumberMember(raw, 'earliest_ms')) { return false; }
    if (!Object.prototype.hasOwnProperty.call(raw, 'series')) { return false; }
    return raw.series === null || isSeriesShape(raw.series);
  }

  function isNullableNumberMember(obj, key) {
    if (!Object.prototype.hasOwnProperty.call(obj, key)) { return false; }
    return obj[key] === null || isFiniteNumber(obj[key]);
  }

  function isGroupShape(g) {
    return isPlainObject(g) && typeof g.label === 'string' && isMetricsShape(g.metrics);
  }

  function isSeriesShape(series) {
    return isPlainObject(series) && isFiniteNumber(series.bucket_ms) &&
      Array.isArray(series.buckets) && series.buckets.every(isBucketShape);
  }

  function isBucketShape(b) {
    return isPlainObject(b) && isFiniteNumber(b.start_ms) && isMetricsShape(b.metrics);
  }

  function isMetricsShape(m) {
    if (!isPlainObject(m)) { return false; }
    var metricsOk = QUERY_METRICS.every(function (key) {
      if (m[key] === null) { return QUERY_NULLABLE_METRICS.indexOf(key) >= 0; }
      return isFiniteNumber(m[key]);
    });
    var tokensOk = QUERY_TOKENS.every(function (key) { return typeof m[key] === 'string'; });
    return metricsOk && tokensOk && QUERY_MISSING_PRICE_LISTS.every(function (key) {
      return Array.isArray(m[key]) && m[key].every(isMissingPriceShape);
    });
  }

  function isMissingPriceShape(p) {
    if (!isPlainObject(p)) { return false; }
    var textOk = MISSING_PRICE_TEXT.every(function (key) { return typeof p[key] === 'string'; });
    return textOk && MISSING_PRICE_NULLABLE_TEXT.every(function (key) {
      return p[key] === null || typeof p[key] === 'string';
    });
  }

  function isPlainObject(v) {
    return !!v && typeof v === 'object' && !Array.isArray(v);
  }

  function isFiniteNumber(v) {
    return typeof v === 'number' && isFinite(v);
  }

  // The adapted QUERY payload of ONE query source, or null when that source
  // carries no usable payload. `stale` counts as usable: a failed refresh
  // RETAINS the last-good data by design, and the degradation is the stale
  // mark's job to report -- discarding it here would turn a recoverable
  // failure into an invalid-payload card and lose the stale-values reading.
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

