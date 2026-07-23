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
