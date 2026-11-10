//! Routing and the providers never read a price: no source file under the
//! router's dispatch module or the providers crate names the price module,
//! the shipped table, the price book, the effective-price resolvers, the
//! `[registry]` price lookup, or a model's pricing block.

use std::path::{Path, PathBuf};

/// The tokens a file reaching price state would have to name. Each is
/// assembled from parts so this file does not name it whole.
fn forbidden_tokens() -> [String; 9] {
    [
        format!("{}{}", "prices", "::"),
        format!("{}{}", "crate::", "prices"),
        format!("{}{}", "shipped", "_table"),
        format!("{}{}", "Price", "Book"),
        format!("{}{}", "pricing", "_for"),
        format!("{}{}", "Pricing", "Config"),
        format!("{}{}", "effective", "_pricing"),
        format!("{}{}", "derive_effective", "_view"),
        format!("{}{}", ".", "pricing"),
    ]
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).expect("source directory is readable") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out
}

/// `(file, token)` for every forbidden token a `.rs` file under `dir` names.
fn hits(dir: &Path) -> Vec<(PathBuf, String)> {
    let tokens = forbidden_tokens();
    rust_files(dir)
        .into_iter()
        .flat_map(|path| {
            let text = std::fs::read_to_string(&path).expect("source file is readable");
            tokens
                .iter()
                .filter(|token| text.contains(token.as_str()))
                .map(|token| (path.clone(), token.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn routing_and_providers_name_no_price_state() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scanned = [
        manifest.join("src/router"),
        manifest.join("../routectl-providers/src"),
    ];
    for dir in &scanned {
        assert!(
            rust_files(dir).len() > 10,
            "premise: {} holds the sources",
            dir.display()
        );
        assert_eq!(hits(dir), Vec::new(), "{}", dir.display());
    }
}

/// Positive control: each name a price reference would spell, planted alone
/// in a nested file, is found. The names are written out here, apart from
/// the scan's own list, so a weakened list fails this test.
#[test]
fn the_scan_finds_each_forbidden_token() {
    for token in [
        "prices::",
        "crate::prices",
        "shipped_table",
        "PriceBook",
        "pricing_for",
        "PricingConfig",
        "effective_pricing",
        "derive_effective_view",
        ".pricing",
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).expect("nested dir");
        std::fs::write(nested.join("planted.rs"), format!("let _ = {token};\n"))
            .expect("plant file");
        std::fs::write(dir.path().join("clean.rs"), "fn f() {}\n").expect("clean file");

        let found = hits(dir.path());

        assert_eq!(found.len(), 1, "{token}: {found:?}");
        assert!(
            found[0].0.ends_with("nested/planted.rs"),
            "{token}: {found:?}"
        );
    }
}
