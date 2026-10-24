//! No production source outside the two owners builds a config folder on its
//! own: the literals `".config"` (alone or followed by `/`),
//! `"XDG_CONFIG_HOME"`, `"APPDATA"` and `"USERPROFILE"` and the `dirs::`
//! crate path appear nowhere else under `crates/*/src`. `*_tests.rs` files
//! are skipped, and so is every inline test module (a `#[cfg(test)]` line
//! immediately followed by `mod <ident> {`, optionally `pub` or `pub(...)`),
//! brace-balanced through its closing line; scanning resumes after it. A
//! `#[cfg(test)] use` line skips nothing. Braces inside comments and
//! string, raw-string and char literals do not count toward the balance.

use std::path::{Path, PathBuf};

const NEEDLES: &[&str] = &[
    "\"XDG_CONFIG_HOME\"",
    "\"APPDATA\"",
    "\"USERPROFILE\"",
    "dirs::",
];
const DOT_CONFIG: &str = "\".config";
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
            matches!(
                line.as_bytes().get(at + DOT_CONFIG.len()),
                Some(b'"' | b'/')
            )
        })
}

/// The index of the line that closes the block opened on `lines[open]`.
/// Braces inside comments and string, raw-string and char literals do not
/// count; test modules hold many of them (JSON bodies, format strings).
fn block_end(lines: &[&str], open: usize) -> usize {
    let text = lines[open..].join("\n");
    let b = text.as_bytes();
    let mut depth = 0i64;
    let mut i = 0;
    while i < b.len() {
        i = match (b[i], b.get(i + 1), b.get(i + 2)) {
            (b'{', ..) => {
                depth += 1;
                i + 1
            }
            (b'}', ..) => {
                depth -= 1;
                if depth == 0 {
                    return open + text[..i].matches('\n').count();
                }
                i + 1
            }
            (b'/', Some(b'/'), _) => find(b, i, b"\n"),
            (b'/', Some(b'*'), _) => find(b, i + 2, b"*/"),
            (b'"', ..) => string_end(b, i + 1),
            (b'r', ..) if raw_string_opens(b, i) => raw_string_end(b, i + 1),
            (b'\'', Some(b'\\'), _) => find(b, i + 3, b"'"),
            (b'\'', _, Some(b'\'')) => i + 3,
            _ => i + 1,
        };
    }
    lines.len() - 1
}

/// The index just past the first `needle` at or after `from`.
fn find(b: &[u8], from: usize, needle: &[u8]) -> usize {
    b.get(from..)
        .and_then(|rest| rest.windows(needle.len()).position(|w| w == needle))
        .map_or(b.len(), |n| from + n + needle.len())
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

/// Whether `b[i]` is the `r` of a raw string (`r"`, `br#"`, `cr"`, ...)
/// rather than part of an identifier.
fn raw_string_opens(b: &[u8], i: usize) -> bool {
    let ident =
        |at: Option<usize>| at.is_some_and(|at| b[at].is_ascii_alphanumeric() || b[at] == b'_');
    let prefixed = i > 0 && matches!(b[i - 1], b'b' | b'c');
    let in_ident = if prefixed {
        ident(i.checked_sub(2))
    } else {
        ident(i.checked_sub(1))
    };
    let quote = b[i + 1..].iter().position(|&c| c != b'#');
    !in_ident && quote.is_some_and(|q| b[i + 1 + q] == b'"')
}

/// The index just past a raw string whose hashes start at `i`.
fn raw_string_end(b: &[u8], i: usize) -> usize {
    let hashes = b[i..].iter().take_while(|&&c| c == b'#').count();
    let mut close = vec![b'"'];
    close.extend(std::iter::repeat_n(b'#', hashes));
    find(b, i + hashes + 1, &close)
}

fn offending(rel: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "#[cfg(test)]"
            && lines.get(i + 1).is_some_and(|l| opens_inline_mod(l))
        {
            i = block_end(&lines, i + 1) + 1;
            continue;
        }
        if has_needle(lines[i]) {
            hits.push(format!("{rel}:{}: {}", i + 1, lines[i].trim()));
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
        hits.extend(offending(
            &path.display().to_string(),
            &std::fs::read_to_string(path).unwrap(),
        ));
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

#[test]
fn scanner_flags_production_needles_and_skips_inline_test_modules() {
    let rows = [
        ("dot config in production", "home.join(\".config\");", true),
        (
            "dot config with a subpath",
            "home.join(\".config/routectl\");",
            true,
        ),
        ("dirs crate", "let d = dirs::config_dir();", true),
        ("appdata", "var_os(\"APPDATA\");", true),
        ("userprofile", "var_os(\"USERPROFILE\");", true),
        (
            "xdg after a cfg(test) use",
            "#[cfg(test)]\nuse a::b;\nvar(\"XDG_CONFIG_HOME\");",
            true,
        ),
        (
            "production after a closed test mod",
            "#[cfg(test)]\nmod tests {\n    fn f() {\n    }\n}\nhome.join(\".config\");",
            true,
        ),
        (
            "production after a closed pub(crate) test mod",
            "#[cfg(test)]\npub(crate) mod hooks {\n}\nvar(\"XDG_CONFIG_HOME\");",
            true,
        ),
        (
            "inside an inline test mod",
            "#[cfg(test)]\nmod tests {\n    \".config\";\n}",
            false,
        ),
        (
            "inside a pub test mod",
            "#[cfg(test)]\npub mod tests {\n    dirs::home_dir();\n}",
            false,
        ),
        (
            "close brace in a string inside a test mod",
            "#[cfg(test)]\nmod tests {\n    let a = \"}\";\n    \".config\";\n}",
            false,
        ),
        (
            "close brace in a raw string inside a test mod",
            "#[cfg(test)]\nmod tests {\n    let b = r#\"say \"}\"#;\n    \".config\";\n}",
            false,
        ),
        (
            "close brace in a byte raw string inside a test mod",
            "#[cfg(test)]\nmod tests {\n    let b = br#\"say \"}\"#;\n    \".config\";\n}",
            false,
        ),
        (
            "close brace in a char inside a test mod",
            "#[cfg(test)]\nmod tests {\n    let c = '}';\n    \".config\";\n}",
            false,
        ),
        (
            "close brace in a comment inside a test mod",
            "#[cfg(test)]\nmod tests {\n    // }\n    \".config\";\n}",
            false,
        ),
        (
            "production after a test mod with an open brace in a string",
            "#[cfg(test)]\nmod tests {\n    let a = \"{\";\n}\nhome.join(\".config\");",
            true,
        ),
        (
            "production after a test mod with an open brace in a char",
            "#[cfg(test)]\nmod tests {\n    let c = '{';\n}\nhome.join(\".config\");",
            true,
        ),
        ("temp-file prefix", "let tmp = \".config.tmp.\";", false),
    ];
    for (name, text, fires) in rows {
        assert_eq!(!offending("x.rs", text).is_empty(), fires, "row {name}");
    }
}
