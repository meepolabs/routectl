//! Provider names `config check` prints in its own lines go through the
//! operator-name renderer, and its pool readout shows seat labels while a
//! credential reference shows only its scheme.

use routectl_auth::MemoryStore;
use routectl_core::MAX_PROVIDER_NAME_BYTES;
use routectl_router::{CURRENT_CONFIG_VERSION, Config};

use super::{
    check_findings, safe_line, secret_ref_parse_errors, secret_ref_resolution_warnings,
    validation_report,
};
use crate::commands::seat_report::{pool_rows, seat_pool_lines, stored_seat_pool_rows};

const HOSTILE: &str = "bad\u{1b}[31m\nname";
const HOSTILE_KEY: &str = "bad\\u001b[31m\\nname";
const HOSTILE_RENDERED: &str = "badname";

fn provider(key: &str, credential: &str) -> String {
    format!(
        "[providers.\"{key}\"]\nformat = \"anthropic-messages\"\n\
         base_url = \"https://api.anthropic.com\"\ncredential = \"{credential}\"\n\
         auth = \"bearer\"\n"
    )
}

fn parse(body: &str) -> Config {
    let text = format!("version = {CURRENT_CONFIG_VERSION}\n{body}");
    toml::from_str(&text).unwrap_or_else(|e| panic!("fixture parses: {e}\n{text}"))
}

/// One `config check` line that interpolates a provider name, and the config
/// shape that makes it fire for the provider written under `key`.
struct Row {
    label: &'static str,
    marker: &'static str,
}

const ROWS: &[Row] = &[
    Row {
        label: "secret-ref parse error",
        marker: "secret-ref parse failed",
    },
    Row {
        label: "secret-ref resolution warning",
        marker: "cannot resolve secret-ref",
    },
    Row {
        label: "oauth presence warning",
        marker: "cannot verify secret-ref",
    },
    Row {
        label: "unknown model provider",
        marker: "references unknown provider or pool",
    },
];

/// The rendered lines `row` produces for a provider written under the TOML
/// key `key`, filtered to the row's own marker.
async fn row_lines(row: &Row, key: &str) -> Vec<String> {
    let lines = match row.label {
        "secret-ref parse error" => {
            secret_ref_parse_errors(&parse(&provider(key, "bogus://x")), None)
        }
        "secret-ref resolution warning" => {
            let config = parse(&provider(key, "env://ROUTECTL_NAME_RENDER_TEST_UNSET"));
            secret_ref_resolution_warnings(&config, &MemoryStore::new(), &None).await
        }
        "oauth presence warning" => {
            let config = parse(&provider(key, "oauth://anthropic"));
            secret_ref_resolution_warnings(&config, &MemoryStore::new(), &None).await
        }
        "unknown model provider" => {
            let config = parse(&format!(
                "[models.m]\nprovider = \"{key}\"\nupstream = \"claude-sonnet-4-5\"\n"
            ));
            validation_report(&config, None).errors
        }
        other => panic!("unknown row {other}"),
    };
    let matching: Vec<String> = lines
        .iter()
        .filter(|line| line.contains(row.marker))
        .map(|line| safe_line(line))
        .collect();
    assert_eq!(matching.len(), 1, "{}: {lines:?}", row.label);
    matching
}

#[tokio::test]
async fn a_name_over_the_byte_ceiling_is_withheld_from_every_check_line() {
    let over = "p".repeat(MAX_PROVIDER_NAME_BYTES + 1);
    for row in ROWS {
        let lines = row_lines(row, &over).await;

        assert!(!lines[0].contains(&over), "{}: {}", row.label, lines[0]);
        assert!(
            lines[0].contains(&format!("`<{}-byte name withheld>`", over.len())),
            "{}: {}",
            row.label,
            lines[0]
        );
    }
}

#[tokio::test]
async fn a_name_at_the_byte_ceiling_is_shown_on_every_check_line() {
    let at = "p".repeat(MAX_PROVIDER_NAME_BYTES);
    for row in ROWS {
        let lines = row_lines(row, &at).await;

        assert!(
            lines[0].contains(&format!("`{at}`")),
            "{}: {}",
            row.label,
            lines[0]
        );
    }
}

#[tokio::test]
async fn a_name_with_esc_and_newline_renders_stripped_on_one_check_line() {
    assert!(HOSTILE.contains('\u{1b}') && HOSTILE.contains('\n'));
    for row in ROWS {
        let lines = row_lines(row, HOSTILE_KEY).await;

        assert!(!lines[0].chars().any(char::is_control), "{}", row.label);
        assert!(
            lines[0].contains(&format!("`{HOSTILE_RENDERED}`")),
            "{}: {}",
            row.label,
            lines[0]
        );
    }
}

/// A pool member pinned to a labelled seat shows the label in the readout,
/// while the env var name and file path behind static refs never reach any
/// `config check` line: those refs are named by scheme only.
#[tokio::test]
async fn the_check_output_shows_seat_labels_and_names_refs_by_scheme_only() {
    const VAR: &str = "ROUTECTL_NAME_RENDER_SENTINEL_VAR";
    const PATH: &str = "/nonexistent/routectl-name-render-sentinel";
    let config = parse(&format!(
        "{}{}{}[pools.team]\nmembers = [\"seat-work\"]\n",
        provider("seat-work", "oauth://anthropic#work"),
        provider("by-env", &format!("env://{VAR}")),
        provider("by-file", &format!("file://{PATH}")),
    ));
    let stored = vec!["anthropic#work".to_string()];

    let found = check_findings(&config, None, &MemoryStore::new(), &Some(stored.clone())).await;
    let mut lines = seat_pool_lines(
        &pool_rows(&config, Some(&stored)),
        &stored_seat_pool_rows(&config, Some(&stored)),
    );
    lines.extend(found.errors.iter().map(|e| safe_line(e)));
    lines.extend(found.warnings.iter().map(|w| safe_line(w)));

    assert!(
        lines
            .iter()
            .any(|l| l.contains("pool `team`") && l.contains("seat-work=work")),
        "{lines:?}"
    );
    for scheme in ["(scheme `env://`)", "(scheme `file://`)"] {
        assert!(
            lines.iter().any(|l| l.contains(scheme)),
            "positive control: {scheme} is reported: {lines:?}"
        );
    }
    for sentinel in [VAR, PATH] {
        assert!(
            !lines.iter().any(|l| l.contains(sentinel)),
            "`{sentinel}` leaked: {lines:?}"
        );
    }
}
