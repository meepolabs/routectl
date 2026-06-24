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
pub const SCHEMA_VERSION: i64 = 4;

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
    selection_decision TEXT
)";

/// Index over `ts_start` for time-range scans (the dominant query
/// shape for reporting and pruning).
pub const CREATE_TS_START_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_requests_ts_start ON requests (ts_start)";

/// DDL for the `meta` key/value table. Holds the DB creation timestamp
/// and a human-readable copy of the schema version. Survives migrations.
pub const CREATE_META_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
)";
