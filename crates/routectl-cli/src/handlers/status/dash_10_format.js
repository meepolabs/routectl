  // ---- number + time humanizers (match the CLI's usage formatters) -----

  // Compact a count exactly as the CLI's `human_count`: below 10000 the
  // plain integer; else one decimal with a K/M/B suffix, trimming ".0".
  // Null/undefined/non-finite render as "-".
  function humanCount(n) {
    if (n === null || n === undefined) { return '-'; }
    var x = Number(n);
    if (!isFinite(x)) { return '-'; }
    var neg = x < 0, a = Math.abs(x);
    if (a < 10000) { return (neg ? '-' : '') + String(Math.trunc(a)); }
    var v, s;
    if (a >= 1e9) { v = a / 1e9; s = 'B'; }
    else if (a >= 1e6) { v = a / 1e6; s = 'M'; }
    else { v = a / 1e3; s = 'K'; }
    var body = v.toFixed(1);
    if (body.slice(-2) === '.0') { body = body.slice(0, -2); }
    return (neg ? '-' : '') + body + s;
  }

  function num0(x) { var n = Number(x); return isFinite(n) ? n : 0; }

  // A percentage that ARRIVED as a percentage (the ledger's own weighted
  // cache-hit share), to whole precision. A non-positive share renders "0%"
  // faintly at its call site rather than as an absence: zero warm reads is a
  // measurement, not a missing one.
  function pctText(v) {
    var x = Number(v);
    return (isFinite(x) ? Math.round(x) : 0) + '%';
  }

  // Compact absolute local timestamp for an epoch-ms instant.
  function fmtTs(ms) {
    if (ms === null || ms === undefined) { return '-'; }
    var d = new Date(ms);
    return isNaN(d.getTime()) ? String(ms) : d.toLocaleString();
  }

  // Timestamp cell that treats a non-positive epoch as absent ("-").
  // Guards the quota-reset field (a real reset is always well in the future).
  function fmtTsPos(ms) {
    if (ms === null || ms === undefined || Number(ms) <= 0) { return '-'; }
    return fmtTs(ms);
  }

  // Relative age of an as_of instant in seconds -> humane phrase. Seconds stay
  // exact below a minute: on a 5s cadence the figures are only ever a handful of
  // seconds old, and coarsening that range to a word would make the one label
  // that reports freshness look frozen.
  function relAge(sec) {
    if (sec < 60) { return sec + 's ago'; }
    if (sec < 3600) { return Math.floor(sec / 60) + 'm ago'; }
    return Math.floor(sec / 3600) + 'h ago';
  }

  // Percentage from a 0..1 fraction (quota utilization), rounded to whole.
  function pctFrac(f) {
    if (f === null || f === undefined) { return '-'; }
    var x = Number(f);
    return isFinite(x) ? Math.round(x * 100) + '%' : '-';
  }

  // Humane duration for a millisecond span (circuit "open for"). Coarse by
  // design: whole seconds under a minute, whole minutes under an hour, else
  // whole hours. Negative spans are clamped by the caller.
  function humanDuration(ms) {
    var sec = Math.floor(ms / 1000);
    if (sec < 60) { return sec + 's'; }
    if (sec < 3600) { return Math.floor(sec / 60) + 'm'; }
    return Math.floor(sec / 3600) + 'h';
  }

  // The window's plain-language span, for the verdict strip's req/span pair.
  var WINDOW_SPAN = {
    today: 'today',
    week: 'this week',
    month: 'this month',
    all: 'all time'
  };

  // ---- label maps ------------------------------------------------------

  // Humanized labels + hover tooltips for the value-domain wire tokens,
  // keyed by token FAMILY. The families here MIRROR the `.tok` CSS families
  // in dash_components.css (search "Value-domain tokens"): a new token is
  // added in BOTH adjacent, mutually-pointing places -- a color rule there, a
  // label entry here.
  //
  // Each entry is `{ label, title }`. `label` is the humanized VISIBLE text
  // (falls back to the raw token when omitted, so cramped wire tokens like
  // `five_hour` stay verbatim); `title` is the hover tooltip, which always
  // preserves the raw token or its meaning so nothing is lost. An unknown
  // token (or unknown family) passes through as its raw string with no
  // tooltip -- parity with the neutral `.tok` CSS fallback.
  var LABELS = {
    circuit: {
      closed: { label: 'ok', title: 'closed - healthy, requests flow' },
      open: { label: 'open', title: 'open - failing fast, requests shed' },
      half_open_ready: { label: 'half-open', title: 'half_open_ready - probing recovery' }
    },
    reach: {
      reachable: { label: 'reachable', title: 'last dispatch outcome was ok' },
      degraded: { label: 'degraded', title: 'last dispatch outcome was a failure family or gate refusal' },
      unknown: { label: 'unknown', title: 'no settled outcome yet (fresh state or post-restart)' }
    },
    status: {
      Pass: { label: 'pass', title: 'Pass' },
      Warn: { label: 'warn', title: 'Warn' },
      Fail: { label: 'fail', title: 'Fail' }
    },
    verdict: {
      'route-away': { label: 'route away', title: 'route-away - capability filtered out for this target' },
      'force-supported': { label: 'force supported', title: 'force-supported - capability forced on for this target' }
    },
    learned: {
      verified: { label: 'verified', title: 'verified - capability confirmed supported for this target' },
      broken: { label: 'broken', title: 'broken - capability confirmed unsupported for this target' }
    },
    prov: {
      provider: { label: 'provider', title: 'provider - legacy per-provider unsupported_features' },
      model: { label: 'model', title: 'model - legacy per-model unsupported_features' },
      override: { label: 'override', title: 'override - a [capability.overrides.<spec>] entry' },
      learned: { label: 'learned', title: 'learned - a non-expired acting negative in the learned registry' }
    },
    src: {
      user: { label: 'user', title: 'user - operator catalog layer' },
      import: { label: 'import', title: 'import - imported catalog layer' },
      baked: { label: 'baked', title: 'baked - built-in catalog layer' },
      disabled: { label: 'disabled', title: 'disabled - row present but disabled' },
      missing: { label: 'missing', title: 'missing - no catalog row resolved' }
    },
    tier: {
      'self-identifying': { label: 'self-identifying', title: 'the upstream declared the capability itself' },
      inferred: { label: 'inferred', title: 'inferred from observed behavior' }
    },
    // Quota representative-claim. The raw token stays visible (no label
    // override for five_hour); the window meaning goes in the tooltip.
    claim: {
      five_hour: { title: 'five_hour - 5-hour rolling subscription window' },
      overage: { label: 'overage', title: 'overage - billing past the included subscription quota' }
    },
    // Quota STATUS / OVERAGE pass-through values (color families in CSS as
    // `.qstatus-*`). Tooltips are light; unknown values pass through raw.
    qstatus: {
      allowed: { title: 'within quota' },
      allowed_warning: { title: 'approaching the quota limit' },
      rejected: { title: 'quota exhausted, requests rejected' },
      queued: { title: 'requests queued behind the quota' },
      triggered: { title: 'overage billing engaged' }
    },
    // Provider auto-activation STATUS. `unknown` is an activation state this
    // build does not recognize, not an absence of one.
    actstatus: {
      activated: { title: 'a usable credential resolved for this provider' },
      unresolved: { title: 'no usable credential resolved - the reason is beside it' },
      unknown: { title: 'an activation state this build does not recognize' }
    },
    // Provider auto-activation reason codes. Raw token stays visible where
    // space is tight; the meaning goes in the tooltip.
    activation: {
      oauth_missing: { title: 'oauth_missing - no OAuth credential on file' },
      oauth_expired: { title: 'oauth_expired - OAuth token expired, no refresh available' },
      oauth_store_unavailable: { title: 'oauth_store_unavailable - no OAuth store to probe (HOME or XDG absent)' },
      not_cataloged: { title: 'not_cataloged - own-credential provider has no baked catalog rows yet' },
      unknown: { title: 'unknown - an activation state this build does not recognize' }
    },
    // Health last-settled-outcome tokens.
    outcome: {
      ok: { label: 'ok' },
      rate_limited: { label: 'rate limited', title: 'rate_limited' },
      timeout: { label: 'timeout' },
      transport_error: { label: 'transport error', title: 'transport_error' },
      http_4xx: { label: 'http 4xx', title: 'http_4xx - a 4xx client-side rejection' },
      http_5xx: { label: 'http 5xx', title: 'http_5xx - a 5xx server-side failure' },
      circuit_open: { label: 'circuit open', title: 'circuit_open - derived from an open breaker' }
    },
    // Resolved failure-class names for the usage error breakdown.
    class: {
      'rate-limited': { label: 'rate limited' },
      auth: { label: 'auth' },
      'bad-request': { label: 'bad request' },
      'content-policy': { label: 'content policy' },
      'context-window': { label: 'context window' },
      'server-error': { label: 'server error' },
      timeout: { label: 'timeout' },
      'network-error': { label: 'network error' },
      overloaded: { label: 'overloaded' },
      'feature-unsupported': { label: 'feature unsupported' },
      unclassified: { label: 'unclassified', title: 'errors with no resolved failure class' }
    },
    // Query cost-resolution status (the `cost_status` wire token). Drives the
    // honest "unpriced" read -- an unpriced or subscription group never
    // renders a "$0".
    cost: {
      priced: { label: 'priced', title: 'every row in this group was priced' },
      unpriced: { label: 'unpriced', title: 'no row in this group had a usable price' },
      subscription: { label: 'subscription', title: 'managed-subscription usage: real usage, no per-token cost' },
      partial: { label: 'partial', title: 'mixed cost kinds: the figure is the priced subtotal only' }
    }
  };

  // Resolve a wire token to { label, title } for a family. Unknown token or
  // unknown family -> raw passthrough (label = raw, no tooltip), mirroring
  // the neutral `.tok` CSS fallback so nothing crashes or hides.
  function labelFor(family, raw) {
    var key = (raw === null || raw === undefined) ? '' : String(raw);
    var fam = LABELS[family];
    var entry = fam ? fam[key] : undefined;
    if (!entry) { return { label: key || '-', title: null }; }
    return { label: (entry.label === undefined) ? key : entry.label, title: entry.title || null };
  }

  // A humanized text cell (a plain value, or a titled span when the family
  // carries a tooltip) for tokens that are NOT `.tok` color pills: quota
  // claim, activation reasons, health last-outcomes, error-class names.
  // Returns a string when there is no tooltip (rendered via textContent) or
  // a titled span Node otherwise.
  function labelCell(family, raw) {
    var lab = labelFor(family, raw);
    if (!lab.title) { return lab.label; }
    var span = document.createElement('span');
    span.textContent = lab.label;
    span.title = lab.title;
    return span;
  }

