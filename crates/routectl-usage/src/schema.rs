//! SQLite DDL for the usage-accounting store.
//!
//! One column per `UsageRecord` field (see `record.rs`, the single
//! source of truth for the column set and null-ability). `Option<T>`
//! fields are NULLable; non-`Option` fields are NOT NULL. Timestamps
//! are epoch-millis `INTEGER`; JSON columns are `TEXT` (JSON1-queryable);
//! the two quota-utilization ratios are `REAL`.

/// Current on-disk schema version. The migrate-on-open ladder advances a
/// freshly-created or older DB to this version. Bump alongside a new
/// migration step in `migrate.rs`.
pub const SCHEMA_VERSION: i64 = 13;

/// `meta` key holding the DB creation timestamp (epoch ms).
pub const META_CREATED_AT_MS: &str = "created_at_ms";

/// `meta` key mirroring the schema version at creation time. The
/// authoritative version is `PRAGMA user_version`; this row is a
/// human-readable convenience for inspection / debugging.
pub const META_SCHEMA_VERSION: &str = "schema_version";

/// DDL for the `requests` table.
///
/// The `outcome` CHECK tokens MUST match `Outcome::as_str()` in
/// `record.rs` -- that enum is the source of truth for this closed set.
/// `request_id` is UNIQUE: it is the idempotency key, and the future
/// writer uses `INSERT OR IGNORE` to drop duplicate captures.
pub const CREATE_REQUESTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS requests (
    -- IDENTITY
    ts_start        INTEGER NOT NULL,
    ts_end          INTEGER NOT NULL,
    request_id      TEXT    NOT NULL UNIQUE,
    ingress_dialect TEXT    NOT NULL,
    requested_model TEXT    NOT NULL,
    alias           TEXT    NOT NULL,
    model           TEXT,
    upstream        TEXT,
    provider        TEXT,
    provider_kind   TEXT,
    seat            TEXT,
    session_id      TEXT,

    -- SHAPE
    stream          INTEGER NOT NULL,
    max_tokens_req  INTEGER,
    tool_count      INTEGER NOT NULL,
    thinking_req    INTEGER,
    thinking_req_kind TEXT,
    msg_count       INTEGER NOT NULL,
    service_tier    TEXT,

    -- OUTCOME (CHECK tokens mirror Outcome::as_str in record.rs)
    outcome         TEXT    NOT NULL CHECK (outcome IN (
                        'ok',
                        'upstream_error',
                        'client_disconnect',
                        'timeout',
                        'cancelled',
                        'gate_blocked'
                    )),
    -- http_status is the client-transport status: 200 for a delivered
    -- non-streaming body and for a committed SSE head; a mid-stream
    -- provider failure keeps 200 and is carried by outcome / error_class /
    -- stream_stage, never by this column. Streaming rows written before the
    -- commit-point fix are NULL and are not back-migrated.
    http_status     INTEGER,
    error_class     TEXT,
    finish_reason   TEXT,
    attempt_count   INTEGER NOT NULL,
    fallback_count  INTEGER NOT NULL,

    -- TIMING
    latency_ms      INTEGER NOT NULL,
    ttfb_ms         INTEGER,

    -- TOKENS
    input_tokens    INTEGER,
    output_tokens   INTEGER,
    reasoning_tokens INTEGER,
    cache_read      INTEGER,
    cache_write_5m  INTEGER,
    cache_write_1h  INTEGER,
    server_tool_use TEXT,

    -- QUOTA snapshot
    quota_claim     TEXT,
    quota_status    TEXT,
    quota_overage_status TEXT,
    quota_utilization    REAL,
    quota_overage_utilization REAL,
    quota_reset     INTEGER,
    quota_extras    TEXT,

    -- EXTENSIBILITY
    extra           TEXT,

    -- AUTO-CACHE DECISION (v2): the per-request strategy token recorded by
    -- the router. Appended last so this column lands in the same ordinal
    -- position whether the DB was created fresh at v2 or migrated from v1
    -- via `ALTER TABLE ... ADD COLUMN strategy` (which always appends).
    strategy        TEXT,

    -- CONTEXT-REDUCTION DECISION (v3): the per-request reduction strategy
    -- token recorded by the router. Appended last so this column lands in
    -- the same ordinal position whether the DB was created fresh at v3 or
    -- migrated from v2 via `ALTER TABLE ... ADD COLUMN reduction_strategy`
    -- (which always appends).
    reduction_strategy TEXT,

    -- SEAT-SELECTION DECISION (v4): the per-request seat-selection decision
    -- token recorded by the router for the served target's home seat.
    -- Appended last so this column lands in the same ordinal position
    -- whether the DB was created fresh at v4 or migrated from v3 via
    -- `ALTER TABLE ... ADD COLUMN selection_decision` (which always appends).
    selection_decision TEXT,

    -- STEADY-STATE WOULD-TRIM ADVISORY (v5): the non-mutating record of the
    -- steady-state trimmer's would-cut candidate for this request. NULL when
    -- the trimmer proposed no cut. `would_trim_tokens` is the candidate's
    -- freed-token count `d`; `would_trim_break_even_k` is the break-even reuse
    -- count K* the cost gate priced for it. Appended last so these columns
    -- land in the same ordinal position whether the DB was created fresh at v5
    -- or migrated from v4 via `ALTER TABLE ... ADD COLUMN` (which always
    -- appends). The live request is NEVER mutated -- this is recording only.
    would_trim_tokens INTEGER,
    would_trim_break_even_k REAL,

    -- STEADY-STATE WOULD-TRIM K FLOOR (v6): the per-session K estimator's
    -- lower confidence bound `k_floor`, recorded only when the estimate was
    -- `Calibrated` for the request's (session, provider_kind, model) triple.
    -- NULL for a cold / thin estimate, for an unverified pricing cell, and
    -- when the trimmer proposed no cut. Appended last so it lands in the same
    -- ordinal position whether the DB was created fresh at v6 or migrated from
    -- v5 via `ALTER TABLE ... ADD COLUMN` (which always appends). The live
    -- request is NEVER mutated -- this is recording only.
    would_trim_k_floor REAL,

    -- SHADOW MISFIRE MONITOR (v7): 0 = Stable (prefix byte-identical),
    -- 1 = Misfire (prefix shifted turn-to-turn), NULL = FirstSeen or no session
    -- key. Appended last so it lands in the same ordinal position whether the
    -- DB was created fresh at v7 or migrated from v6 via `ALTER TABLE ... ADD
    -- COLUMN` (which always appends). The live request is NEVER mutated.
    would_trim_shadow_misfire INTEGER,

    -- NEAR-LOSSLESS ATTRIBUTION (v8): plumbing only -- this wires the
    -- columns end-to-end (DispatchMeta -> observe_meta -> UsageRecord ->
    -- SQLite); the near-lossless recorder pass computes the values.
    -- `would_trim_dedup_tokens`
    -- / `would_trim_supersession_tokens` are per-heuristic freed-token counts
    -- (plain columns, not a bitmask). `would_trim_path_units` /
    -- `would_trim_path_extractable` are a count-pair (NOT a pre-averaged
    -- rate) so the extractability rate is reconstructable offline via
    -- SUM/SUM. `would_trim_recorder_version` is NULL on pre-M1 rows and
    -- stamped by the M1 recorder, so reporting can filter to non-NULL rows
    -- and never mix baseline vs M1 semantics in an aggregate.
    -- `would_trim_raw_marks` is a capped JSON blob (see
    -- `writer::capped_raw_marks_text`) capturing per-mark ordering for the
    -- future M3 sweep. `would_trim_context_fraction` is NULL when the
    -- pricing row's context window is unknown. Appended last so these
    -- columns land in the same ordinal position whether the DB was created
    -- fresh at v8 or migrated from v7 via `ALTER TABLE ... ADD COLUMN`
    -- (which always appends). The live request is NEVER mutated -- this is
    -- recording only.
    would_trim_dedup_tokens INTEGER,
    would_trim_supersession_tokens INTEGER,
    would_trim_path_units INTEGER,
    would_trim_path_extractable INTEGER,
    would_trim_recorder_version INTEGER,
    would_trim_raw_marks TEXT,
    would_trim_context_fraction REAL,

    -- RESOLVED FAILURE CLASS (v12): the canonical kebab failure-class token
    -- (FailureClass::class_token) for a request that reached a dispatch
    -- attempt and failed, stamped by the CLI capture. NULL for a success and
    -- for any pre-dispatch / validation / local-gate failure that never
    -- reached an upstream (those read back as unclassified), and NULL when the
    -- class has no token (Unknown). Appended last so this column lands in the
    -- same ordinal position whether the DB was created fresh at v12 or migrated
    -- from v11 via ALTER TABLE ... ADD COLUMN resolved_class (which always
    -- appends). No backfill -- older rows stay NULL.
    resolved_class  TEXT
)";

/// Index over `ts_start` for time-range scans (the dominant query
/// shape for reporting and pruning).
pub const CREATE_TS_START_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_requests_ts_start ON requests (ts_start)";

/// DDL for the `capability_learn_events` table (v9).
///
/// One append-only row per confirmed learned-capability observation. This
/// is NOT a request row: learn events are their own closed shape and must
/// never share the `requests` table (whose rows are treated as requests by
/// every reporting query). Nothing reads this table yet -- it is the
/// forever-contract landing pad for the warm-rebuild replayer.
///
/// Columns mirror the row struct in `learn_event.rs` (the source of truth
/// for the set). `capability_key` is the NORMALIZED capability key. `signal_tier`
/// is a closed set whose CHECK tokens mirror the two producer tiers.
/// `remapped` is always 0 by construction but persisted so a replayer can
/// filter defensively. `request_features` is a JSON array TEXT (the in-flight
/// feature set the replayer verifies against). NEVER a body / message / prompt
/// column (log hygiene).
pub const CREATE_CAPABILITY_LEARN_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS capability_learn_events (
    ts               INTEGER NOT NULL,
    state_key        TEXT    NOT NULL,
    capability_key   TEXT    NOT NULL,
    provider_kind    TEXT    NOT NULL,
    signal_tier      TEXT    NOT NULL CHECK (signal_tier IN (
                         'self-identifying',
                         'inferred'
                     )),
    observations     INTEGER NOT NULL,
    upstream_status  INTEGER NOT NULL,
    remapped         INTEGER NOT NULL,
    request_features TEXT    NOT NULL
)";

/// DDL for the `capability_events` table (v13).
///
/// One append-only row per capability admission event -- the forever
/// contract the warm-rebuild replayer reads on boot. Distinct from the
/// legacy `capability_learn_events` landing pad: this table is the
/// unified ledger across learned negatives, verified/suspect observations,
/// probe-settled clears, and reload/boot tombstones.
///
/// `ts` is epoch-millis. `lane_key` / `capability` are the NORMALIZED keys
/// (a tombstone row carries them empty). `verdict` / `phase` / `source` /
/// `tier` are open-set tokens the replayer parses tolerantly. `tier` is
/// persisted so live-vs-rebuild equivalence can distinguish
/// self-identifying from inferred negatives. `evidence_class` is nullable:
/// it carries the pinned observation tokens and is NULL for rows that have
/// none. `upstream_token` is nullable, forensic / display only -- never
/// consulted by admission or replay (the negative ride-along carries the
/// normalized capability, not the raw wire token). `catalog_version` /
/// `overlay_revision` stamp the boundary revision a row was written under,
/// so replay can filter defensively and a tombstone can mark the boundary.
/// NEVER a body / message / prompt column (log hygiene).
pub const CREATE_CAPABILITY_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS capability_events (
    ts               INTEGER NOT NULL,
    lane_key         TEXT,
    capability       TEXT,
    verdict          TEXT,
    phase            TEXT,
    source           TEXT,
    tier             TEXT,
    evidence_class   TEXT,
    upstream_token   TEXT,
    catalog_version  INTEGER,
    overlay_revision INTEGER
)";

/// Index over `capability_events.ts` for time-range scans (the dominant
/// query shape for the hygiene prune and the ts-ordered rebuild replay).
pub const CREATE_CAPABILITY_EVENTS_TS_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_capability_events_ts ON capability_events (ts)";

/// DDL for the `meta` key/value table. Holds the DB creation timestamp
/// and a human-readable copy of the schema version. Survives migrations.
pub const CREATE_META_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
)";
