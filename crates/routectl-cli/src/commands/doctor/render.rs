//! Human-text rendering of the doctor report.

use routectl_router::{DoctorReport, Finding, Status};

use crate::commands::doctor_panels::{render_capability_matrix_panel, render_would_trim_panel};

use super::SECTIONS;

pub(super) fn render_human(report: &DoctorReport) -> Vec<String> {
    let mut out = vec!["routectl doctor".to_string(), String::new()];
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
        "pools" => "OAuth seat pools",
        "seats" => "OAuth seats",
        "secrets" => "Managed secrets",
        "probe" => "Provider reachability",
        "capability" => "Capability",
        "freshness" => "Catalog freshness",
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
