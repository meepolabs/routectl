//! No production source outside `config_dir.rs` builds the config folder on
//! its own: the literals `".config"` and `"XDG_CONFIG_HOME"` and the `dirs::`
//! crate path appear nowhere else under `crates/*/src`. `*_tests.rs` files are
//! skipped, and a file is read only up to its first inline test module (a
//! `#[cfg(test)]` line immediately followed by `mod <ident> {`); a
//! `#[cfg(test)] use` line does not end the production part.

use std::path::{Path, PathBuf};

const NEEDLES: &[&str] = &["\".config\"", "\"XDG_CONFIG_HOME\"", "dirs::"];
const OWNER: &str = "routectl-core/src/config_dir.rs";

fn opens_inline_mod(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    let words = words.strip_prefix(&["pub(crate)"]).unwrap_or(&words);
    matches!(words, ["mod", ident, "{"] if ident.chars().all(|c| c.is_alphanumeric() || c == '_'))
}

fn offending(rel: &str, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let production = (0..lines.len())
        .find(|&i| {
            lines[i].trim() == "#[cfg(test)]"
                && lines.get(i + 1).is_some_and(|l| opens_inline_mod(l))
        })
        .map_or(&lines[..], |cut| &lines[..cut]);
    (1..)
        .zip(production)
        .filter(|(_, line)| NEEDLES.iter().any(|needle| line.contains(needle)))
        .map(|(n, line)| format!("{rel}:{n}: {}", line.trim()))
        .collect()
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
fn only_the_shared_resolver_builds_the_config_folder() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    for krate in std::fs::read_dir(crates).unwrap() {
        let src = krate.unwrap().path().join("src");
        if src.is_dir() {
            production_sources(&src, &mut files);
        }
    }
    let mut hits = Vec::new();
    for path in files.iter().filter(|path| !path.ends_with(OWNER)) {
        hits.extend(offending(
            &path.display().to_string(),
            &std::fs::read_to_string(path).unwrap(),
        ));
    }
    assert!(files.len() > 300, "walk reached only {} files", files.len());
    assert!(
        hits.is_empty(),
        "config folder built outside {OWNER}:\n{}",
        hits.join("\n")
    );
}

#[test]
fn census_scanner_rows() {
    let rows = [
        ("dot config in production", "home.join(\".config\");", true),
        (
            "xdg after a cfg(test) use",
            "#[cfg(test)]\nuse a::b;\nvar(\"XDG_CONFIG_HOME\");",
            true,
        ),
        (
            "inside an inline test mod",
            "#[cfg(test)]\nmod tests {\n    \".config\";\n}",
            false,
        ),
        ("temp-file prefix", "let tmp = \".config.tmp.\";", false),
    ];
    for (name, text, fires) in rows {
        assert_eq!(!offending("x.rs", text).is_empty(), fires, "row {name}");
    }
}
