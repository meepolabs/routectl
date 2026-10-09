//! Plain `routectl doctor` report data types. Orchestration (which checks
//! run, in what order) and rendering stay CLI-side; this module owns only
//! the serialize-safe shapes the CLI collects into and prints. Mirrors the
//! config `CheckReport` split: derivable data here, side-effecting checks
//! and rendering in the command layer.

use serde::Serialize;

pub use routectl_core::ProbeOutcome;

/// Severity of a single doctor finding. Fixed triad; not a growth enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Status {
    /// The check passed.
    Pass,
    /// The check passed with a caveat worth surfacing.
    Warn,
    /// The check failed.
    Fail,
}

/// One line of the doctor report. `section` is a stable, display-safe
/// category token; `detail` and `remediation` are operator-facing messages
/// that never carry a token, path, or env value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Stable, display-safe category token.
    pub section: &'static str,
    /// The check's name.
    pub name: String,
    /// The check's severity.
    pub status: Status,
    /// Operator-facing detail message.
    pub detail: String,
    /// Optional operator-facing remediation hint.
    pub remediation: Option<String>,
}

/// Steady-state would-trim opportunity panel. Router-local mirror of the
/// usage crate's `WouldTrimSummary` (router does not depend on usage): the
/// CLI copies the fields across when it assembles the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WouldTrimPanel {
    /// Requests eligible for trimming.
    pub candidate_requests: i64,
    /// Tokens that would be trimmed.
    pub would_trim_tokens: i64,
    /// Candidates whose break-even verdict was met.
    pub verdict_met: i64,
    /// Candidates whose break-even verdict was unmet.
    pub verdict_unmet: i64,
    /// Candidates with a cold cache.
    pub verdict_cold: i64,
    /// Candidates that could not be priced.
    pub verdict_unpriced: i64,
}

/// Optional structured panels attached to a doctor report. Extensible: new
/// panels land here additively as `Option` fields.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct DoctorPanels {
    /// The steady-state would-trim panel, when computed.
    pub would_trim: Option<WouldTrimPanel>,
    /// The learned-capability truth matrix panel, when computed.
    pub capability_matrix: Option<CapabilityMatrixPanel>,
}

/// Availability of the learned-capability matrix's learned source, whichever
/// [`MatrixSource`] produced it, as a first-class tri-state: `Available` (at
/// least one learned entry present -- replayed for a `ledger_replay` panel,
/// resident for a `resident` one), `Empty` (the source was readable and held
/// zero entries -- an honest, non-degraded empty), or `Unavailable` with a
/// path-free class code (the source could not be read). A diagnostic never
/// silently collapses "could not read" into "nothing learned".
///
/// A `ledger_replay` panel's codes describe this report's replay (e.g.
/// `no_data`, `revision_mismatch`). A `resident` panel is `Unavailable` only
/// when its registry is empty after a boot warm that failed, and its code is
/// that warm's outcome token: `unreadable` or `restate_failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MatrixAvailability {
    /// At least one learned entry is present.
    Available,
    /// The source was readable and held zero learned entries.
    Empty,
    /// The source could not be read; `code` is a path-free class token.
    Unavailable {
        /// Path-free class token (e.g. `no_data`, `revision_mismatch`, or a
        /// resident panel's `unreadable` / `restate_failed`).
        code: &'static str,
    },
}

/// One resolved cell of the capability matrix: the display verdict for a
/// `(lane, capability)` pair. `verdict` is a stable token (the core
/// `Verdict::as_str` vocabulary plus the panel-only `forced_supported` /
/// `forced_unsupported` override tokens, or `mixed` when the nicknames on the
/// lane resolve different verdicts); `supported` carries the polarity
/// the token alone omits for a prior `assumed` cell (`None` for an
/// `unknown` or `mixed` cell); `source` is the winning layer's evidence tag
/// (`override` / `live` / `probe` / `seed` / `prior`) and `layer` the layer
/// itself (`override` / `learned` / `seed` / `prior`), both `None` for
/// `unknown` or `mixed`. A `seed` cell is the shipped beta seed: `broken`
/// while it withholds the flag, `cleared` once a seed-clear marker lifts it.
/// `action` is what the dispatch filter does with the cell (`drop` /
/// `route_away` / `strip` / `withhold` / `reprobe` / `allow` / `none`), or
/// `mixed` when the nicknames on the lane resolve it to different actions.
/// `nickname_actions` carries each nickname's own verdict, layer and action
/// whenever the verdict or the action is `mixed`, and is empty otherwise.
///
/// The timestamps are epoch milliseconds and describe the resident learned
/// entry, so they are present only when one exists -- including when an
/// override or prior wins the cell, since the learned entry still exists
/// underneath. `expires_at_ms` is a negative's decay deadline and is `None`
/// for a positive, which never decays. `age_ms` is the winning learned
/// cell's age since last seen; `stale` flags a verified cell older than the
/// operator staleness hint or a prior stamp past the same threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatrixCell {
    /// The display verdict token.
    pub verdict: &'static str,
    /// Support polarity; `None` only for an `unknown` cell.
    pub supported: Option<bool>,
    /// The winning layer's source tag; `None` only for an `unknown` cell.
    pub source: Option<&'static str>,
    /// The winning layer; `None` only for an `unknown` cell.
    pub layer: Option<&'static str>,
    /// The routing action the dispatch filter takes for this cell.
    pub action: &'static str,
    /// Age since last seen in ms for a learned/verified cell; else `None`.
    pub age_ms: Option<i64>,
    /// Whether the cell is stale past the operator staleness hint.
    pub stale: bool,
    /// When the resident learned entry was first observed (epoch ms).
    pub first_seen_ms: Option<i64>,
    /// When the resident learned entry was last observed (epoch ms).
    pub last_seen_ms: Option<i64>,
    /// When a resident learned negative's decay window lapses (epoch ms).
    pub expires_at_ms: Option<i64>,
    /// Each nickname's own resolution, sorted by nickname, when `verdict` or
    /// `action` is `mixed`; empty when every nickname on the lane agrees.
    pub nickname_actions: Vec<MatrixNicknameAction>,
}

/// One nickname's resolution of a `mixed` matrix cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatrixNicknameAction {
    /// The model nickname.
    pub nickname: String,
    /// The display verdict token for this nickname's target.
    pub verdict: &'static str,
    /// The layer that decided this nickname's verdict; `None` for `unknown`.
    pub layer: Option<&'static str>,
    /// The action the dispatch filter takes for this nickname's target.
    pub action: &'static str,
}

/// One matrix row: a learned lane and its cells aligned 1:1 with the panel's
/// `columns`. `lane` is the serialized `provider_entry#upstream` form the
/// learned store keys on and `capability purge` accepts; a legacy ledger key
/// that is not a lane is shown verbatim. `nicknames` are the config models
/// that dispatch to the lane (two nicknames for one upstream on one provider
/// entry share it). `routed` is false for a lane the loaded config no longer
/// maps -- a stale ledger row for a removed model or provider entry, surfaced
/// honestly rather than silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatrixLane {
    /// The lane key (`provider_entry#upstream`, or a legacy key verbatim).
    pub lane: String,
    /// The config model nicknames that map to this lane, sorted.
    pub nicknames: Vec<String>,
    /// The lane's provider kind (`kind_str`), empty when its provider entry
    /// is not configured.
    pub provider_kind: &'static str,
    /// Whether the loaded config still maps this lane to a provider.
    pub routed: bool,
    /// Cells aligned 1:1 with the panel's `columns`.
    pub cells: Vec<MatrixCell>,
}

/// What the read-only ledger replay behind the matrix did with the rows it
/// read: how many replayed, and how many it skipped by reason. A skip is not
/// an error; it is a row this build cannot attribute to a current lane or
/// vocabulary, which the operator should be able to see rather than infer
/// from a thin matrix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MatrixReplaySummary {
    /// Ledger rows read past the replay boundary.
    pub loaded_rows: usize,
    /// Rows replayed into the registry (positives, negatives, and clears).
    pub replayed: usize,
    /// Rows skipped for a vocabulary version or token this build retires.
    pub skipped_vocab: usize,
    /// Rows skipped because their provider entry no longer owns the lane.
    pub skipped_owner: usize,
    /// Catalog-scoped rows skipped for a catalog / overlay revision change.
    pub skipped_revision: usize,
    /// Field rows skipped because their key is not a learned lane.
    pub skipped_lane: usize,
    /// Rows skipped for a token the current vocabulary does not recognize.
    pub skipped_unknown: usize,
}

/// Where the matrix's learned layer came from: `ledger_replay` is a
/// read-only replay of the usage ledger performed for this report;
/// `resident` is the daemon's in-memory learned registry, warmed once at
/// boot and updated live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixSource {
    /// The daemon's resident learned registry.
    Resident,
    /// A read-only ledger replay run for this report.
    LedgerReplay,
}

/// How the resident learned registry was warmed at daemon boot: the boot
/// outcome token, the failure class of an `unreadable` warm, and, when the
/// warm replay ran, its tally. It describes the boot-time warm, not the
/// current state of the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatrixWarm {
    /// The boot warm outcome token (e.g. `replayed`, or a fail-closed class).
    pub outcome: String,
    /// The path-free failure class of an `unreadable` warm (e.g.
    /// `open_failed`); `None` for every other outcome.
    pub class: Option<String>,
    /// The boot warm replay tally, when the warm replay ran.
    pub summary: Option<MatrixReplaySummary>,
}

/// The learned-capability truth matrix panel: lanes (rows) by capability
/// keys (columns). `columns` is the well-known capability keys followed by
/// any observed keys outside that set, capped at a fixed render width;
/// `other_overflow` is the count of observed keys beyond the cap (rendered
/// as `(+N more)`). `lanes` is empty when `availability` is not `Available`
/// and no config-derived cell exists.
///
/// `source` names the learned layer's origin and fixes which tally is
/// carried: a `ledger_replay` panel carries `replay` (present whenever the
/// replay ran -- `Available` or `Empty` -- and `None` when the source was
/// unavailable) and `warm: None`; a `resident` panel carries `replay: None`
/// and `warm: Some`. A resident view can legitimately differ from a CLI
/// ledger replay of the same daemon: live positives the daemon holds but
/// does not persist appear only in the resident view, and each replay reads
/// at most `REBUILD_ROW_LIMIT` ledger rows past the boundary (the replay
/// row-limit constant in the CLI's server ledger reader), so on a ledger
/// beyond that cap the boot warm and a later replay can cover different row
/// windows. The two are not reconciled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapabilityMatrixPanel {
    /// The learned source availability tri-state.
    pub availability: MatrixAvailability,
    /// The origin of the learned layer.
    pub source: MatrixSource,
    /// Column keys: the well-known keys, then capped observed others.
    pub columns: Vec<String>,
    /// Count of observed other-column keys beyond the render cap.
    pub other_overflow: u32,
    /// Matrix rows.
    pub lanes: Vec<MatrixLane>,
    /// The ledger replay tally, when this report's replay ran.
    pub replay: Option<MatrixReplaySummary>,
    /// The resident registry's boot warm, for a `resident` panel.
    pub warm: Option<MatrixWarm>,
}

/// The full doctor report: a flat findings list plus the structured panels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    /// Report schema version.
    pub schema_version: u32,
    /// The flat list of findings.
    pub findings: Vec<Finding>,
    /// The structured panels.
    pub panels: DoctorPanels,
}

/// Process exit code for a collected findings slice: nonzero iff any finding
/// is [`Status::Fail`]. `Pass` and `Warn` both map to 0. Pure in the slice;
/// callers compute it after collecting and sorting their findings.
pub fn overall_exit(findings: &[Finding]) -> i32 {
    i32::from(findings.iter().any(|f| f.status == Status::Fail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};

    fn finding(status: Status) -> Finding {
        Finding {
            section: "config",
            name: "sample".into(),
            status,
            detail: "detail".into(),
            remediation: None,
        }
    }

    #[test]
    fn overall_exit_is_zero_for_all_pass() {
        let findings = vec![finding(Status::Pass), finding(Status::Pass)];
        assert_eq!(overall_exit(&findings), 0);
    }

    #[test]
    fn overall_exit_is_zero_for_pass_and_warn_only() {
        let findings = vec![finding(Status::Pass), finding(Status::Warn)];
        assert_eq!(overall_exit(&findings), 0);
    }

    #[test]
    fn overall_exit_is_nonzero_when_any_fail() {
        let findings = vec![finding(Status::Pass), finding(Status::Fail)];
        assert_ne!(overall_exit(&findings), 0);
    }

    #[test]
    fn overall_exit_is_order_independent() {
        let fail_first = vec![
            finding(Status::Fail),
            finding(Status::Warn),
            finding(Status::Pass),
        ];
        let fail_last = vec![
            finding(Status::Pass),
            finding(Status::Warn),
            finding(Status::Fail),
        ];
        assert_eq!(overall_exit(&fail_first), overall_exit(&fail_last));
        assert_ne!(overall_exit(&fail_first), 0);
    }

    #[test]
    fn overall_exit_is_zero_for_empty_slice() {
        assert_eq!(overall_exit(&[]), 0);
    }

    #[test]
    fn doctor_report_serializes_to_stable_json_object() {
        let report = DoctorReport {
            schema_version: 1,
            findings: vec![Finding {
                section: "auth",
                name: "anthropic".into(),
                status: Status::Warn,
                detail: "no credentials configured".into(),
                remediation: Some("run routectl init".into()),
            }],
            panels: DoctorPanels {
                would_trim: Some(WouldTrimPanel {
                    candidate_requests: 3,
                    would_trim_tokens: 60_000,
                    verdict_met: 1,
                    verdict_unmet: 1,
                    verdict_cold: 1,
                    verdict_unpriced: 0,
                }),
                capability_matrix: None,
            },
        };

        let text = serde_json::to_string(&report).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&text).expect("parse");
        let obj = value.as_object().expect("top-level object");

        assert_eq!(obj.len(), 3);
        assert_eq!(obj["schema_version"], serde_json::json!(1));
        assert!(obj["findings"].is_array());
        let finding = &obj["findings"][0];
        assert_eq!(finding["section"], serde_json::json!("auth"));
        assert_eq!(finding["status"], serde_json::json!("Warn"));
        assert_eq!(
            finding["remediation"],
            serde_json::json!("run routectl init")
        );
        assert_eq!(
            obj["panels"]["would_trim"]["would_trim_tokens"],
            serde_json::json!(60_000)
        );
    }

    fn matrix_panel(
        source: MatrixSource,
        replay: Option<MatrixReplaySummary>,
        warm: Option<MatrixWarm>,
    ) -> serde_json::Value {
        let panel = CapabilityMatrixPanel {
            availability: MatrixAvailability::Empty,
            source,
            columns: Vec::new(),
            other_overflow: 0,
            lanes: Vec::new(),
            replay,
            warm,
        };
        serde_json::to_value(&panel).expect("serialize")
    }

    #[test]
    fn ledger_replay_matrix_serializes_source_and_replay_without_warm() {
        let replay = MatrixReplaySummary {
            loaded_rows: 4,
            replayed: 3,
            ..MatrixReplaySummary::default()
        };

        let json = matrix_panel(MatrixSource::LedgerReplay, Some(replay), None);

        assert_eq!(json["source"], serde_json::json!("ledger_replay"));
        assert_eq!(json["replay"]["loaded_rows"], serde_json::json!(4));
        assert_eq!(json["replay"]["replayed"], serde_json::json!(3));
        assert!(
            json["warm"].is_null(),
            "a replay panel carries no warm: {json}"
        );
    }

    #[test]
    fn resident_matrix_serializes_source_and_warm_without_replay() {
        let warm = MatrixWarm {
            outcome: "replayed".into(),
            class: None,
            summary: Some(MatrixReplaySummary {
                loaded_rows: 7,
                skipped_owner: 2,
                ..MatrixReplaySummary::default()
            }),
        };

        let json = matrix_panel(MatrixSource::Resident, None, Some(warm));

        assert_eq!(json["source"], serde_json::json!("resident"));
        assert!(
            json["replay"].is_null(),
            "a resident panel carries no replay: {json}"
        );
        assert_eq!(json["warm"]["outcome"], serde_json::json!("replayed"));
        assert_eq!(json["warm"]["summary"]["loaded_rows"], serde_json::json!(7));
        assert_eq!(
            json["warm"]["summary"]["skipped_owner"],
            serde_json::json!(2)
        );
    }

    #[test]
    fn resident_matrix_warm_without_summary_serializes_null_summary() {
        let warm = MatrixWarm {
            outcome: "unreadable".into(),
            class: Some("open_failed".into()),
            summary: None,
        };

        let json = matrix_panel(MatrixSource::Resident, None, Some(warm));

        assert_eq!(json["warm"]["outcome"], serde_json::json!("unreadable"));
        assert_eq!(json["warm"]["class"], serde_json::json!("open_failed"));
        assert!(json["warm"]["summary"].is_null(), "{json}");
    }

    #[test]
    fn resident_matrix_warm_without_class_serializes_null_class() {
        let warm = MatrixWarm {
            outcome: "replayed".into(),
            class: None,
            summary: None,
        };

        let json = matrix_panel(MatrixSource::Resident, None, Some(warm));

        let warm = json["warm"].as_object().expect("warm object");
        assert!(warm.contains_key("class"), "{json}");
        assert!(warm["class"].is_null(), "{json}");
    }

    struct StubProvider {
        id: String,
    }

    #[async_trait]
    impl Provider for StubProvider {
        fn id(&self) -> &str {
            &self.id
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("stub", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            unreachable!()
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn default_probe_reports_unsupported_free_probe() {
        let provider = StubProvider { id: "stub".into() };
        assert_eq!(provider.probe().await, ProbeOutcome::UnsupportedFreeProbe);
    }
}
