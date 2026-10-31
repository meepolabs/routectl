//! Source guards for the scripted refresh seam: every seam hook in the
//! production files is compiled only for tests and `test-utils`, the
//! provider token URLs stay fixed, and the seam reads no environment.

const SEAM_GATE: &str = "#[cfg(any(test, feature = \"test-utils\"))]";

const SEAM_TOKENS: [&str; 4] = ["refresh_flow", "now_override", "pinned_now", "scripted"];

/// Each production file that hosts a seam hook, with the minimum number of
/// gated hook lines it must contain (so an empty scan cannot pass).
const HOOK_FILES: [(&str, &str, usize); 3] = [
    ("store/mod.rs", include_str!("mod.rs"), 7),
    ("store/refresh.rs", include_str!("refresh.rs"), 2),
    ("oauth/mod.rs", include_str!("../mod.rs"), 1),
];

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// Non-comment lines naming a seam token, each paired with the nearest
/// preceding non-comment line.
fn seam_hook_lines(src: &str) -> Vec<(String, String)> {
    let code: Vec<&str> = src.lines().filter(|l| !is_comment(l)).collect();
    code.iter()
        .enumerate()
        .filter(|(_, line)| SEAM_TOKENS.iter().any(|t| line.contains(t)))
        .map(|(i, line)| {
            let prev = if i == 0 { "" } else { code[i - 1] };
            ((*line).to_string(), prev.trim().to_string())
        })
        .collect()
}

#[test]
fn every_seam_hook_is_gated_to_tests_and_test_utils() {
    for (name, src, min_hooks) in HOOK_FILES {
        // Act
        let hooks = seam_hook_lines(src);

        // Assert
        assert!(
            hooks.len() >= min_hooks,
            "{name}: expected at least {min_hooks} seam hook lines, found {}",
            hooks.len()
        );
        for (line, prev) in hooks {
            assert_eq!(
                prev,
                SEAM_GATE,
                "{name}: seam hook `{}` is not directly gated",
                line.trim()
            );
        }
    }
}

#[test]
fn provider_token_urls_are_fixed_literals() {
    let rows = [
        (
            "anthropic",
            include_str!("../providers/anthropic.rs"),
            "pub const TOKEN_URL: &str = \"https://platform.claude.com/v1/oauth/token\";",
        ),
        (
            "codex",
            include_str!("../providers/codex.rs"),
            "const TOKEN_URL: &str = \"https://auth.openai.com/oauth/token\";",
        ),
        (
            "xai",
            include_str!("../providers/xai.rs"),
            "const TOKEN_URL: &str = \"https://auth.x.ai/oauth2/token\";",
        ),
        (
            "antigravity",
            include_str!("../providers/antigravity.rs"),
            "const TOKEN_URL: &str = \"https://oauth2.googleapis.com/token\";",
        ),
    ];
    for (name, src, expected) in rows {
        // Act
        let decls: Vec<&str> = src
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("TOKEN_URL") && l.contains("const "))
            .collect();

        // Assert
        assert_eq!(
            decls,
            [expected],
            "{name}: TOKEN_URL must stay one fixed const"
        );
    }
}

#[test]
fn seam_and_its_hooks_read_no_environment() {
    let needles = [
        concat!("std", "::env"),
        concat!("env", "::"),
        concat!("env", "!("),
        concat!("option_env", "!("),
    ];
    let files = [
        ("store/scripted.rs", include_str!("scripted.rs")),
        ("store/mod.rs", include_str!("mod.rs")),
        ("store/refresh.rs", include_str!("refresh.rs")),
    ];
    for (name, src) in files {
        for needle in needles {
            assert!(!src.contains(needle), "{name} contains `{needle}`");
        }
    }
}

#[test]
fn hook_scan_flags_an_ungated_seam_line() {
    // Positive control for the scan itself: a hook preceded by a plain
    // `cfg(test)` is reported with that predecessor.
    let src = "struct S {\n    #[cfg(test)]\n    /// doc\n    refresh_flow: u8,\n}\n";

    let hooks = seam_hook_lines(src);

    assert_eq!(
        hooks,
        [(
            "    refresh_flow: u8,".to_string(),
            "#[cfg(test)]".to_string()
        )]
    );
}
