//! Human-text rendering of the doctor report.

use routectl_router::{DoctorReport, Finding, Status};

use crate::commands::doctor_panels::{render_capability_matrix_panel, render_would_trim_panel};

use super::daemon_attention;
use super::sections::POOLS_SECTION;
use super::{AttentionRead, SECTIONS};

/// The report alone, with no attention block.
#[cfg(test)]
pub(super) fn render_human(report: &DoctorReport) -> Vec<String> {
    render_with_block(report, Vec::new())
}

/// The report with the daemon's needs-attention block above the checks: the
/// served list, or the one line naming why none could be read. The
/// `summary:` line stays last.
pub(super) fn render_human_with_attention(
    report: &DoctorReport,
    attention: &AttentionRead,
) -> Vec<String> {
    let block = match attention {
        Ok(served) => daemon_attention::render(served),
        Err(failure) => vec![daemon_attention::render_unavailable(failure)],
    };
    render_with_block(report, block)
}

fn render_with_block(report: &DoctorReport, block: Vec<String>) -> Vec<String> {
    let mut out = vec!["routectl doctor".to_string(), String::new()];
    if !block.is_empty() {
        out.extend(block);
        out.push(String::new());
    }
    for (key, _) in SECTIONS {
        let section_findings: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.section == *key)
            .collect();
        render_section(key, &section_findings, &mut out);
        out.push(String::new());
    }
    if let Some(panel) = &report.panels.capability_matrix {
        for line in render_capability_matrix_panel(panel).lines() {
            out.push(line.to_string());
        }
        out.push(String::new());
    }
    if let Some(panel) = &report.panels.would_trim {
        for line in render_would_trim_panel(panel).lines() {
            out.push(line.to_string());
        }
        out.push(String::new());
    }
    out.push(render_summary(&report.findings));
    out
}

fn render_section(key: &str, findings: &[&Finding], out: &mut Vec<String>) {
    out.push(format!("[{}]", section_title(key)));
    if findings.is_empty() {
        return;
    }
    for f in findings {
        out.push(format!(
            "  {} {}: {}",
            status_label(f.status),
            f.name,
            f.detail
        ));
        if let Some(rem) = &f.remediation {
            out.push(format!("       fix: {rem}"));
        }
    }
}

fn render_summary(findings: &[Finding]) -> String {
    let mut pass = 0;
    let mut warn = 0;
    let mut fail = 0;
    for f in findings {
        match f.status {
            Status::Pass => pass += 1,
            Status::Warn => warn += 1,
            Status::Fail => fail += 1,
        }
    }
    format!("summary: PASS {pass}  WARN {warn}  FAIL {fail}")
}

fn section_title(key: &str) -> &'static str {
    match key {
        "inventory" => "Provider activation",
        "version" => "Config schema version",
        "config" => "Config validation",
        "auth" => "OAuth credentials",
        POOLS_SECTION => "OAuth seat pools",
        "seats" => "OAuth seats",
        "secrets" => "Managed secrets",
        "probe" => "Provider reachability",
        "capability" => "Capability",
        "pricing" => "Cost pricing",
        "knobs" => "Model knobs",
        "tuning" => "Resolved tuning",
        "cache" => "Cache markers",
        _ => "Other",
    }
}

const fn status_label(status: Status) -> &'static str {
    match status {
        Status::Pass => "PASS",
        Status::Warn => "WARN",
        Status::Fail => "FAIL",
    }
}
