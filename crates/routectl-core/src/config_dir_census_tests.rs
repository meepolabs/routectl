//! No production source outside the two owners builds a config folder on its
//! own: the literals `"XDG_CONFIG_HOME"`, `"APPDATA"` and `"USERPROFILE"`, a
//! `".config` literal followed by `"`, `/` or an escaped backslash, and the
//! `dirs::` crate path appear nowhere else under `crates/*/src`. `*_tests.rs`
//! files are skipped, and so is every inline test module: a `#[cfg(test)]`
//! line immediately followed by `mod <ident> {` (optionally `pub` or
//! `pub(...)`), through the line whose `}` balances that brace. Scanning
//! resumes after it, and a module that never closes panics. A
//! `#[cfg(test)] use` line skips nothing. The balance ignores braces inside
//! `//` comments, `/* */` comments (nested too), string literals (an escaped
//! quote does not end one), `r#"` and `br#"` raw strings, and char literals,
//! escaped or multi-byte.

use std::path::{Path, PathBuf};

const NEEDLES: &[&str] = &[
    "\"XDG_CONFIG_HOME\"",
    "\"APPDATA\"",
    "\"USERPROFILE\"",
    "dirs::",
];
const DOT_CONFIG: &str = "\".config";
const DOT_CONFIG_FOLLOWERS: &[&[u8]] = &[b"\"", b"/", b"\\\\"];
const OWNERS: &[&str] = &[
    "routectl-core/src/config_dir.rs",
    "routectl-cli/src/config_folder.rs",
];
/// The walk reached 544 files on 2026-10-24
/// (`rg --files crates/*/src -g '*.rs' -g '!*_tests.rs' | wc -l`); the floor
/// only catches a walk that silently stopped early.
const MIN_FILES: usize = 300;

fn opens_inline_mod(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    let words = match words.first() {
        Some(vis) if *vis == "pub" || vis.starts_with("pub(") => &words[1..],
        _ => &words[..],
    };
    matches!(words, ["mod", ident, "{"] if ident.chars().all(|c| c.is_alphanumeric() || c == '_'))
}

fn has_needle(line: &str) -> bool {
    NEEDLES.iter().any(|needle| line.contains(needle))
        || line.match_indices(DOT_CONFIG).any(|(at, _)| {
            let rest = &line.as_bytes()[at + DOT_CONFIG.len()..];
            DOT_CONFIG_FOLLOWERS.iter().any(|f| rest.starts_with(f))
        })
}

/// The index of the line that closes the block opened on `lines[open]`.
/// Braces inside comments and string, raw-string and char literals do not
/// count; test modules hold many of them (JSON bodies, format strings).
/// Panics, naming `rel` and the opening line, when the block never closes.
fn block_end(rel: &str, lines: &[&str], open: usize) -> usize {
    let text = lines[open..].join("\n");
    let b = text.as_bytes();
    let mut depth = 0i64;
    let mut i = 0;
    while i < b.len() {
        i = match (b[i], b.get(i + 1)) {
            (b'{', _) => {
                depth += 1;
                i + 1
            }
            (b'}', _) => {
                depth -= 1;
                if depth == 0 {
                    return open + text[..i].matches('\n').count();
                }
                i + 1
            }
            (b'/', Some(b'/')) => find(b, i, b"\n"),
            (b'/', Some(b'*')) => block_comment_end(b, i + 2),
            (b'"', _) => string_end(b, i + 1),
            (b'r', _) if raw_string_opens(b, i) => raw_string_end(b, i + 1),
            (b'\'', _) => char_end(&text, i),
            _ => i + 1,
        };
    }
    panic!("{rel}:{}: this block never closes", open + 1);
}

/// The index just past the first `needle` at or after `from`.
fn find(b: &[u8], from: usize, needle: &[u8]) -> usize {
    b.get(from..)
        .and_then(|rest| rest.windows(needle.len()).position(|w| w == needle))
        .map_or(b.len(), |n| from + n + needle.len())
}

/// The index just past the `*/` closing a block comment whose body starts at
/// `i`. Block comments nest.
fn block_comment_end(b: &[u8], mut i: usize) -> usize {
    let mut depth = 1;
    while i < b.len() {
        match (b[i], b.get(i + 1)) {
            (b'*', Some(b'/')) => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return i;
                }
            }
            (b'/', Some(b'*')) => {
                depth += 1;
                i += 2;
            }
            _ => i += 1,
        }
    }
    b.len()
}

/// The index just past the `"` closing a string whose body starts at `i`.
fn string_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// Whether `b[i]` is the `r` of a raw string: hashes, then a quote.
fn raw_string_opens(b: &[u8], i: usize) -> bool {
    let quote = b[i + 1..].iter().position(|&c| c != b'#');
    quote.is_some_and(|q| b[i + 1 + q] == b'"')
}

/// The index just past a raw string whose hashes start at `i`.
fn raw_string_end(b: &[u8], i: usize) -> usize {
    let hashes = b[i..].iter().take_while(|&&c| c == b'#').count();
    let mut close = vec![b'"'];
    close.extend(std::iter::repeat_n(b'#', hashes));
    find(b, i + hashes + 1, &close)
}

/// The index just past a char literal whose `'` is at `i`, or `i + 1` when
/// the `'` starts a lifetime or a label.
fn char_end(text: &str, i: usize) -> usize {
    let b = text.as_bytes();
    if b.get(i + 1) == Some(&b'\\') {
        return find(b, i + 3, b"'");
    }
    match text[i + 1..].chars().next() {
        Some(c) if b.get(i + 1 + c.len_utf8()) == Some(&b'\'') => i + 2 + c.len_utf8(),
        _ => i + 1,
    }
}

/// The 1-based lines of `text` that name a needle outside test modules.
fn offending(rel: &str, text: &str) -> Vec<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "#[cfg(test)]"
            && lines.get(i + 1).is_some_and(|l| opens_inline_mod(l))
        {
            i = block_end(rel, &lines, i + 1) + 1;
            continue;
        }
        if has_needle(lines[i]) {
            hits.push(i + 1);
        }
        i += 1;
    }
    hits
}

fn production_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for path in std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
    {
        if path.is_dir() {
            production_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !path.to_string_lossy().ends_with("_tests.rs")
        {
            out.push(path);
        }
    }
}

#[test]
fn only_the_owners_build_a_config_folder() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    for krate in std::fs::read_dir(crates).unwrap() {
        let src = krate.unwrap().path().join("src");
        if src.is_dir() {
            production_sources(&src, &mut files);
        }
    }
    let mut hits = Vec::new();
    for path in files
        .iter()
        .filter(|path| !OWNERS.iter().any(|owner| path.ends_with(owner)))
    {
        let rel = path.display().to_string();
        let text = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        hits.extend(
            offending(&rel, &text)
                .into_iter()
                .map(|n| format!("{rel}:{n}: {}", lines[n - 1].trim())),
        );
    }
    assert!(
        files.len() > MIN_FILES,
        "walk reached only {} files",
        files.len()
    );
    assert!(
        hits.is_empty(),
        "config folder built outside {OWNERS:?}:\n{}",
        hits.join("\n")
    );
}

/// A test module whose first line hides a `}` inside `hidden`, then holds a
/// needle on line 4, then closes; a production needle follows on line 6.
/// Only line 6 may fire: a lexer that misreads `hidden` closes the module on
/// line 3 and also reports line 4.
fn module_hiding(hidden: &str) -> String {
    format!(
        "#[cfg(test)]\nmod tests {{\n    {hidden}\n    \".config\";\n}}\nhome.join(\".config\");"
    )
}

#[test]
fn scanner_flags_production_needles() {
    let rows: [(&str, &str, &[usize]); 9] = [
        ("dot config in production", "home.join(\".config\");", &[1]),
        (
            "dot config with a subpath",
            "home.join(\".config/routectl\");",
            &[1],
        ),
        (
            "dot config with a windows separator",
            "home.join(\".config\\\\routectl\");",
            &[1],
        ),
        (
            "dot config before another escape",
            "let s = \".config\\tx\";",
            &[],
        ),
        ("temp-file prefix", "let tmp = \".config.tmp.\";", &[]),
        ("dirs crate", "let d = dirs::config_dir();", &[1]),
        ("appdata", "var_os(\"APPDATA\");", &[1]),
        ("userprofile", "var_os(\"USERPROFILE\");", &[1]),
        (
            "xdg after a cfg(test) use",
            "#[cfg(test)]\nuse a::b;\nvar(\"XDG_CONFIG_HOME\");",
            &[3],
        ),
    ];
    for (name, text, expected) in rows {
        assert_eq!(offending("x.rs", text), expected, "row {name}");
    }
}

#[test]
fn scanner_skips_inline_test_modules_through_their_closing_line() {
    let rows: [(&str, &str, &[usize]); 4] = [
        (
            "production after a closed test mod",
            "#[cfg(test)]\nmod tests {\n    fn f() {\n    }\n}\nhome.join(\".config\");",
            &[6],
        ),
        (
            "production after a closed pub(crate) test mod",
            "#[cfg(test)]\npub(crate) mod hooks {\n    \".config\";\n}\nvar(\"XDG_CONFIG_HOME\");",
            &[5],
        ),
        (
            "inside an inline test mod",
            "#[cfg(test)]\nmod tests {\n    \".config\";\n}",
            &[],
        ),
        (
            "inside a pub test mod",
            "#[cfg(test)]\npub mod tests {\n    dirs::home_dir();\n}",
            &[],
        ),
    ];
    for (name, text, expected) in rows {
        assert_eq!(offending("x.rs", text), expected, "row {name}");
    }
}

#[test]
fn braces_inside_comments_and_literals_do_not_close_a_test_module() {
    let rows = [
        ("string", "let a = \"}\";"),
        ("string with an escaped quote", "let a = \"\\\"}\";"),
        ("raw string", "let b = r#\"say \"}\"#;"),
        ("raw string closed by its hashes", "let b = r#\"}\"#;"),
        ("byte raw string", "let b = br#\"say \"}\"#;"),
        ("char", "let c = '}';"),
        ("escaped quote char", "let c = ['\\'','}'];"),
        ("non-ascii char", "let c = ['\u{e9}','}'];"),
        ("line comment", "// }"),
        ("block comment", "/* } */"),
        ("nested block comment", "/* a /* b */ } */"),
    ];
    for (name, hidden) in rows {
        assert_eq!(offending("x.rs", &module_hiding(hidden)), [6], "row {name}");
    }
}

#[test]
fn open_braces_inside_literals_do_not_hold_a_test_module_open() {
    let rows = [("string", "let a = \"{\";"), ("char", "let c = '{';")];
    for (name, hidden) in rows {
        assert_eq!(offending("x.rs", &module_hiding(hidden)), [6], "row {name}");
    }
}

#[test]
#[should_panic(expected = "x.rs:2: this block never closes")]
fn a_test_module_that_never_closes_panics() {
    offending("x.rs", "#[cfg(test)]\nmod tests {\n    fn f() {\n}");
}
