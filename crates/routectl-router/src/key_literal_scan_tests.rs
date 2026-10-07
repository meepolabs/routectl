//! Test-only lexer-backed scan for compiled string literals that spell a
//! capability key.
//!
//! A permanent key namespace is owned by exactly one module holding its prefix
//! literal; this scan is how each owner's tests prove no second compiled
//! spelling exists under `crates/`. The caller supplies the key predicate, so
//! every namespace reuses one lexer walk and one fail-closed directory walk.

use std::fs;
use std::path::{Path, PathBuf};

/// Decides whether a decoded string literal is a complete key of the
/// namespace under scan.
pub type KeyPredicate = fn(&str) -> bool;

/// The workspace `crates/` directory, resolved from this crate's manifest.
pub fn workspace_crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root must resolve from the crate manifest dir")
        .join("crates")
}

/// Count the compiled string literals in the Rust source at `path` whose
/// decoded value `is_key` accepts. An unreadable source
/// panics rather than counting zero: silently scoring it clean would make the
/// uniqueness claim vacuous over the file most likely to be mid-edit.
pub fn count_key_literals_in(path: &Path, is_key: KeyPredicate) -> usize {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("the scan must read {}: {err}", path.display()));
    count_key_literals_in_source(&source, is_key)
}

/// Count matching string literals in `source` by walking a real Rust token
/// tree. A source that does not lex panics, for the same fail-closed
/// reason an unreadable one does.
pub fn count_key_literals_in_source(source: &str, is_key: KeyPredicate) -> usize {
    let stream: proc_macro2::TokenStream = source
        .parse()
        .unwrap_or_else(|err| panic!("a Rust source must lex as a token stream: {err}"));
    count_key_literals_in_tokens(stream, is_key)
}

/// Recurse the token tree, testing every string literal's decoded value.
/// Non-literal tokens carry no value a key could hide in, so they are walked
/// past -- which is precisely why a field label cannot be counted.
///
/// Documentation VALUES are skipped, but only in the syntactic positions
/// documentation can occupy: `///` and `//!` desugar to `#[doc = "..."]`,
/// and `cfg_attr`'s later arguments carry the same prose one level deeper.
/// Every other literal in an attribute is still examined.
fn count_key_literals_in_tokens(stream: proc_macro2::TokenStream, is_key: KeyPredicate) -> usize {
    let mut count = 0;
    let mut trees = stream.into_iter().peekable();
    while let Some(tree) = trees.next() {
        match tree {
            // An attribute is `#` or `#!` followed by a bracket group. Its
            // own arguments are the outermost place documentation appears.
            proc_macro2::TokenTree::Punct(punct) if punct.as_char() == '#' => {
                if let Some(proc_macro2::TokenTree::Punct(bang)) = trees.peek()
                    && bang.as_char() == '!'
                {
                    trees.next();
                }
                let Some(proc_macro2::TokenTree::Group(group)) = trees.peek() else {
                    continue;
                };
                if group.delimiter() != proc_macro2::Delimiter::Bracket {
                    continue;
                }
                count += count_key_literals_in_attribute(group.stream(), true, is_key);
                trees.next();
            }
            proc_macro2::TokenTree::Group(group) => {
                count += count_key_literals_in_tokens(group.stream(), is_key);
            }
            proc_macro2::TokenTree::Literal(literal) => {
                if let syn::Lit::Str(text) = syn::Lit::new(literal)
                    && is_key(&text.value())
                {
                    count += 1;
                }
            }
            proc_macro2::TokenTree::Ident(_) | proc_macro2::TokenTree::Punct(_) => {}
        }
    }
    count
}

/// Count matching literals inside one attribute body, dropping only values
/// that are genuine Rust DOCUMENTATION -- `doc = ...` as the attribute's own
/// argument (what `///` and `//!` desugar to), or as a later argument of
/// `cfg_attr`, recursively.
///
/// `doc` inside any OTHER attribute or macro is not documentation: it is a
/// key that attribute defines for itself, and its value is a compiled literal
/// like any other. Suppressing it would let a real key hide behind any
/// attribute that happens to accept a `doc` argument.
fn count_key_literals_in_attribute(
    body: proc_macro2::TokenStream,
    is_doc_position: bool,
    is_key: KeyPredicate,
) -> usize {
    let mut count = 0;
    let mut trees = body.into_iter().peekable();
    while let Some(tree) = trees.next() {
        match tree {
            // `doc = <expression>`, but only where documentation can appear.
            // The whole value is consumed to the next top-level comma, not
            // just a direct literal: doc values are routinely built with
            // `concat!(...)` or `include_str!(...)`, whose nested literals
            // are still documentation and must not be scanned.
            proc_macro2::TokenTree::Ident(ident) if is_doc_position && ident == "doc" => {
                if let Some(proc_macro2::TokenTree::Punct(eq)) = trees.peek()
                    && eq.as_char() == '='
                {
                    trees.next();
                    while let Some(next) = trees.peek() {
                        if let proc_macro2::TokenTree::Punct(punct) = next
                            && punct.as_char() == ','
                        {
                            break;
                        }
                        trees.next();
                    }
                }
            }
            // `cfg_attr(<predicate>, <attr>, ...)`: only the items AFTER
            // the first comma are attributes, so only they can carry
            // documentation. The predicate is an ordinary meta item and is
            // scanned normally -- a `doc = "..."` there is a config key
            // named `doc`, not a doc comment. Any other identifier followed
            // by a group is a different attribute or macro whose arguments
            // are never documentation.
            proc_macro2::TokenTree::Ident(ident) => {
                if let Some(proc_macro2::TokenTree::Group(group)) = trees.peek()
                    && group.delimiter() == proc_macro2::Delimiter::Parenthesis
                {
                    let body = group.stream();
                    count += if is_doc_position && ident == "cfg_attr" {
                        count_key_literals_in_cfg_attr(body, is_key)
                    } else {
                        count_key_literals_in_attribute(body, false, is_key)
                    };
                    trees.next();
                }
            }
            proc_macro2::TokenTree::Group(group) => {
                count += count_key_literals_in_attribute(group.stream(), is_doc_position, is_key);
            }
            proc_macro2::TokenTree::Literal(literal) => {
                if let syn::Lit::Str(text) = syn::Lit::new(literal)
                    && is_key(&text.value())
                {
                    count += 1;
                }
            }
            proc_macro2::TokenTree::Punct(_) => {}
        }
    }
    count
}

/// Count matching literals inside a `cfg_attr` argument list, splitting it on
/// top-level commas per Rust syntax: the FIRST item is the configuration
/// predicate and carries no documentation, every later item is a nested
/// attribute that can. Nested `cfg_attr` recurses through the same rule.
fn count_key_literals_in_cfg_attr(body: proc_macro2::TokenStream, is_key: KeyPredicate) -> usize {
    let mut items: Vec<proc_macro2::TokenStream> = vec![proc_macro2::TokenStream::new()];
    for tree in body {
        if let proc_macro2::TokenTree::Punct(ref punct) = tree
            && punct.as_char() == ','
        {
            items.push(proc_macro2::TokenStream::new());
            continue;
        }
        items
            .last_mut()
            .expect("the item list always holds the current item")
            .extend(std::iter::once(tree));
    }
    let mut items = items.into_iter();
    // The predicate is an ordinary meta item: scanned, never suppressed.
    let predicate = items.next().unwrap_or_default();
    let mut count = count_key_literals_in_attribute(predicate, false, is_key);
    for attribute in items {
        count += count_key_literals_in_attribute(attribute, true, is_key);
    }
    count
}

/// Recursively collect every Rust source under `root`. Hand-rolled because the
/// crate carries no directory-walk dependency and adding one for a single test
/// would be a heavier change than the test.
///
/// Only directories are recursed and only regular files are collected, so a
/// symlink or other special entry is never opened -- following one can leave
/// the tree entirely, and a symlinked directory can make the walk loop.
/// Fail-closed at every fallible step: an unreadable directory, an unreadable
/// entry, and an untypeable entry each panic naming the path, since flattening
/// any of them would make the uniqueness claim vacuous over what it skipped.
pub fn rust_sources_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("the scan must read directory {}: {err}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|err| {
                panic!("the scan must read every entry of {}: {err}", dir.display())
            });
            let path = entry.path();
            let kind = entry.file_type().unwrap_or_else(|err| {
                panic!("the scan must type every entry {}: {err}", path.display())
            });
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found
}
