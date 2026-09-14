//! The ONE bounded resolver every register in this crate's welds uses to turn
//! a NAME it claims into evidence that the name exists in the tree.
//!
//! Two resolvers, deliberately different in kind:
//!
//! - [`holds_fn`] answers "does some source define `fn <name>`", bounded so a
//!   longer name starting with `name` is not a match. It is a text matcher
//!   because a pinning TEST can live anywhere in the crate -- in a sidecar
//!   `_tests.rs`, in an inline `#[cfg(test)] mod`, in an integration binary --
//!   and a whole-tree parse to answer one membership question buys nothing the
//!   bound does not already give.
//! - [`holds_named_item`] answers "does THIS file define an item named
//!   `<name>`", through a real parse, because an ANCHOR is a claim about one
//!   named file and the claim covers consts and associated functions as well
//!   as free ones. A text match would accept the name inside a comment or a
//!   string literal, which is exactly the ghost an anchor exists to refuse.
//!
//! One module rather than a copy per weld: two resolvers that disagree about
//! what "resolves" means let a register pass one weld and fail another, and the
//! reader has no way to tell which answer is the contract.
//!
//! [`crate_rust_sources`] is here for the same reason. Both registers resolve a
//! pinning-test name against "the crate's Rust source", and that phrase has to
//! mean ONE thing: the SCOPE is `src/` plus `tests/`, because a pinning test
//! legitimately lives in either (inline `#[cfg(test)]` modules and sidecar
//! `_tests.rs` files under `src/`, integration binaries under `tests/`). A
//! src-only corpus reports a test that exists as deleted, which reds a correct
//! register -- and a check that reds correct code gets loosened.

// Each weld uses the helpers it needs; the rest are dead in that compilation
// unit, which is expected for a shared module.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use syn::{ImplItem, Item, TraitItem};

/// Whether `source` defines `fn <name>`, bounded so a longer name that starts
/// with `name` is not a match.
pub fn holds_fn(source: &str, name: &str) -> bool {
    let needle = format!("fn {name}");
    source.match_indices(&needle).any(|(at, _)| {
        source[at + needle.len()..].starts_with(|c: char| !c.is_alphanumeric() && c != '_')
    })
}

/// Whether the parsed `source` defines an item NAMED `name`: a free or
/// associated function, a trait method, or a named `const` / `static`.
///
/// A parse error is an error, never a `false`: "the file does not parse" and
/// "the file does not carry this name" are different facts, and collapsing them
/// reports a broken checkout as a broken register.
pub fn holds_named_item(source: &str, name: &str) -> Result<bool, String> {
    let parsed =
        syn::parse_file(source).map_err(|err| format!("cannot parse the source: {err}"))?;
    Ok(items_hold_name(&parsed.items, name))
}

fn items_hold_name(items: &[Item], name: &str) -> bool {
    items.iter().any(|item| match item {
        Item::Fn(f) => f.sig.ident == name,
        Item::Const(c) => c.ident == name,
        Item::Static(s) => s.ident == name,
        Item::Mod(m) => m
            .content
            .as_ref()
            .is_some_and(|(_, inner)| items_hold_name(inner, name)),
        Item::Impl(i) => i.items.iter().any(|member| match member {
            ImplItem::Fn(f) => f.sig.ident == name,
            ImplItem::Const(c) => c.ident == name,
            _ => false,
        }),
        Item::Trait(t) => t.items.iter().any(|member| match member {
            TraitItem::Fn(f) => f.sig.ident == name,
            TraitItem::Const(c) => c.ident == name,
            _ => false,
        }),
        _ => false,
    })
}

/// The crate's own Rust source, as `(display path, source)` pairs: everything
/// under `src/` plus everything under `tests/`.
///
/// ONE definition of the corpus, shared by every register that resolves a
/// pinning-test name, because the two halves of the scope are both load-bearing
/// and dropping either one reds a correct register. An EMPTY corpus is an error:
/// a name resolves against nothing when the walk broke, and reporting that as
/// "the test does not exist" sends the reader to the wrong place.
pub fn crate_rust_sources() -> Result<Vec<(String, String)>, String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    for root in [manifest.join("src"), manifest.join("tests")] {
        collect_rust_sources(&root, &mut sources)?;
    }
    if sources.is_empty() {
        return Err(
            "the crate's src and tests trees together hold no Rust source, which cannot be right; \
             the walk is reading the wrong place"
                .to_string(),
        );
    }
    Ok(sources)
}

fn collect_rust_sources(dir: &Path, sources: &mut Vec<(String, String)>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|err| format!("{} must be a readable directory ({err})", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("cannot read an entry of {}: {err}", dir.display()))?;
        let path: PathBuf = entry.path();
        let kind = entry
            .file_type()
            .map_err(|err| format!("cannot type {}: {err}", path.display()))?;
        if kind.is_dir() {
            collect_rust_sources(&path, sources)?;
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            let source = std::fs::read_to_string(&path)
                .map_err(|err| format!("{} must be readable ({err})", path.display()))?;
            sources.push((path.display().to_string(), source));
        }
    }
    Ok(())
}

/// A `<...>f<digits>.<digits>` run, the board task-id shape. Detected by SCAN
/// rather than by spelling an id out: a literal example of the shape would
/// itself be the leak this refuses.
///
/// Shared by every register that refuses a planning id in a reason, so the two
/// cannot drift into refusing different shapes and leaving the reader unsure
/// which rule is the contract.
///
/// A digit run that spells a Rust FLOAT WIDTH is prose, not an id: `f32.0` /
/// `f64.5` read as a task id to a naive scan, and these are numeric
/// wire-translation surfaces where that prose is likely. Excluded by SPELLING
/// rather than by requiring two digits after the dot, because a real id can
/// carry only one there.
pub fn holds_task_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b'f' {
            continue;
        }
        if idx > 0 && (bytes[idx - 1].is_ascii_alphanumeric() || bytes[idx - 1] == b'_') {
            continue;
        }
        let digits_start = idx + 1;
        let mut cursor = digits_start;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == digits_start || cursor >= bytes.len() || bytes[cursor] != b'.' {
            continue;
        }
        if matches!(&text[digits_start..cursor], "16" | "32" | "64" | "128") {
            continue;
        }
        if bytes.get(cursor + 1).is_some_and(u8::is_ascii_digit) {
            return true;
        }
    }
    false
}
