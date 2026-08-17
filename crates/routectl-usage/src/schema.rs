//! SQLite DDL for the usage-accounting store.
//!
//! One column per `UsageRecord` field (see `record.rs`) for every WRITTEN
//! column, with matching null-ability: `Option<T>` fields are NULLable;
//! non-`Option` fields are NOT NULL. The DDL additionally retains three
//! intentionally write-stopped legacy columns (`strategy`,
//! `reduction_strategy`, `selection_decision`) that have no `UsageRecord`
//! field, so old databases keep opening and historical values stay
//! readable. Timestamps
//! are epoch-millis `INTEGER`; JSON columns are `TEXT` (JSON1-queryable);
//! the two quota-utilization ratios are `REAL`.

/// Current on-disk schema version. The migrate-on-open ladder advances a
/// freshly-created or older DB to this version. Bump alongside a new
/// migration step in `migrate.rs`.
pub const SCHEMA_VERSION: i64 = 15;

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

    -- AUTO-CACHE DECISION (v2): the per-request auto-cache strategy token.
    -- DEPRECATED (0.9.x): write-stopped; NULL for rows written at or after
    -- this version. Retained so the fresh and migrated shapes stay
    -- identical (a v1 DB reaches this shape via
    -- `ALTER TABLE ... ADD COLUMN strategy`, which always appends).
    strategy        TEXT,

    -- CONTEXT-REDUCTION DECISION (v3): the per-request reduction strategy
    -- token. DEPRECATED (0.9.x): write-stopped; NULL for rows written at or
    -- after this version. Retained so the fresh and migrated shapes stay
    -- identical (a v2 DB reaches this shape via
    -- `ALTER TABLE ... ADD COLUMN reduction_strategy`, which always appends).
    reduction_strategy TEXT,

    -- SEAT-SELECTION DECISION (v4): the per-request seat-selection decision
    -- token for the served target's home seat. DEPRECATED (0.9.x):
    -- write-stopped; NULL for rows written at or after this version.
    -- Retained so the fresh and migrated shapes stay identical (a v3 DB
    -- reaches this shape via `ALTER TABLE ... ADD COLUMN
    -- selection_decision`, which always appends).
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
    -- SUM/SUM. `would_trim_recorder_version` is NULL on rows written before
    -- the recorder existed and stamped by the near-lossless recorder, so
    -- reporting can filter to non-NULL rows and never mix unstamped baseline
    -- against recorded semantics in an aggregate.
    -- `would_trim_raw_marks` is a capped JSON blob (see
    -- `writer::capped_raw_marks_text`) capturing per-mark ordering for the
    -- future path-extraction sweep. `would_trim_context_fraction` is NULL
    -- when the pricing row's context window is unknown. Appended last so
    -- these columns land in the same ordinal position whether the DB was
    -- created fresh at v8 or migrated from v7 via `ALTER TABLE ... ADD
    -- COLUMN` (which always appends). The live request is NEVER mutated --
    -- this is recording only.
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
    resolved_class  TEXT,

    -- TOKEN-ESTIMATE CALIBRATION EVIDENCE (v14): the raw (estimate, actual)
    -- pair for one served request, so a per-lane correction factor can be
    -- learned offline or in memory from raw evidence rather than a
    -- pre-averaged ratio. `calib_estimated_tokens` is routectl's own
    -- byte-heuristic estimate of the dispatched payload;
    -- `calib_prompt_tokens` is the upstream's own cache-INCLUSIVE prompt
    -- total, recorded only on a success and only when nonzero (an unreported
    -- total arrives as a real 0 and would train the factor on a data bug).
    -- Both NULL on any row that is not a success with a reported total; a
    -- NULL in either column simply makes the row inadmissible as evidence,
    -- which is why there is no CHECK tying them together.
    --
    -- `calib_prompt_tokens` is NOT derivable from `input_tokens + cache_read
    -- + cache_write_5m + cache_write_1h`: `input_tokens` is cache-EXCLUSIVE
    -- and its subtraction uses the AGGREGATE cache-creation total, which is
    -- not itself persisted (only the per-TTL split is, and that split is
    -- frequently absent). Such a derivation is short by the whole
    -- cache-creation total on the majority of cache-reusing rows, which
    -- biases a learned factor LOW -- the direction that makes a corrected
    -- estimate too small. Hence the direct column.
    --
    -- Appended last so these columns land in the same ordinal position
    -- whether the DB was created fresh at v14 or migrated from v13 via
    -- `ALTER TABLE ... ADD COLUMN` (which always appends). No backfill --
    -- older rows stay NULL.
    calib_estimated_tokens INTEGER,
    calib_prompt_tokens INTEGER,

    -- CONTEXT-REDUCTION OUTCOME (v15): the per-request lossless-minifier
    -- outcome and its four effect counters, persisted for every dispatched
    -- request. `reduction_decision` carries the SAME token vocabulary the
    -- dispatch log emits (`applied`, `skipped:disabled`, `skipped:no-tail`,
    -- `skipped:nothing-to-strip`, `skipped:unknown`) and records the TERMINAL
    -- target's outcome; the four counters aggregate across fallback-entry
    -- preparations (a same-target network retry reuses the prepared request
    -- and never re-counts). `reduction_strings_skipped` counts targets left
    -- untouched (non-JSON or already compact); `reduction_strings_rejected`
    -- counts targets that parsed as JSON but whose re-parse equality guard
    -- declined -- the two are deliberately separate because a skip is a
    -- permanent ceiling while a rejection is a fail-closed invariant alarm
    -- (structurally unreachable with the current minifier, so nonzero means a
    -- minifier defect, not traffic headroom).
    -- `reduction_bytes_saved` is exact bytes removed from prepared outbound
    -- payloads, NOT billed tokens; the token estimate is derived on read
    -- (bytes / 4) and is deliberately never persisted, so there is one source
    -- of truth.
    --
    -- This is a NEW column, not the write-stopped v3 `reduction_strategy`
    -- above: NULL there means write-stopped to existing readers, and reusing
    -- the name would make historical and current rows indistinguishable.
    --
    -- Appended last so these columns land in the same ordinal position
    -- whether the DB was created fresh at v15 or migrated from v14 via
    -- `ALTER TABLE ... ADD COLUMN` (which always appends). No backfill --
    -- older rows stay NULL.
    reduction_decision TEXT,
    reduction_strings_compressed INTEGER,
    reduction_strings_skipped INTEGER,
    reduction_strings_rejected INTEGER,
    reduction_bytes_saved INTEGER
)";

/// Index over `ts_start` for time-range scans (the dominant query
/// shape for reporting and pruning).
pub const CREATE_TS_START_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_requests_ts_start ON requests (ts_start)";

/// DDL for the `capability_learn_events` table (v9).
///
/// DEPRECATED: superseded by the unified `capability_events` ledger
/// (`CREATE_CAPABILITY_EVENTS_TABLE`). The request path no longer writes
/// here -- learned negatives now land as `broken` rows in `capability_events`.
/// The table and its DDL remain (no DROP migration) so existing rows survive
/// and the writer path stays compilable; removal is a later change once no
/// deployed DB carries rows only in this table.
///
/// One append-only row per confirmed learned-capability observation. This
/// is NOT a request row: learn events are their own closed shape and must
/// never share the `requests` table (whose rows are treated as requests by
/// every reporting query). Nothing reads this table.
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
///
/// `id` is an explicit `INTEGER PRIMARY KEY` -- an alias for the rowid that
/// SQLite preserves across `VACUUM`. It is the ledger's insertion-order
/// boundary key: the tombstone marks a point on it, the replayer reads only
/// rows after it, and the hygiene prune stays below it. A bare implicit
/// rowid would be renumbered by `VACUUM` and silently break that boundary,
/// so the alias is load-bearing, not cosmetic. The read/prune queries
/// address it through its rowid alias, which resolves to this stable column.
pub const CREATE_CAPABILITY_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS capability_events (
    id               INTEGER PRIMARY KEY,
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
