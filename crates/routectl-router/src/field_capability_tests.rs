use std::fs;
use std::path::{Path, PathBuf};

use routectl_core::capability::normalize_capability_key;

use super::*;

/// The one realistic path Stage 1 is grounded on: the qualified dotted
/// field an anthropic-api rejection names when the thinking display mode
/// is refused. Kept as a single const so every byte-preservation
/// assertion below is about the same string the wire carries.
const GROUNDED_PATH: &str = "thinking.enabled.display";

/// The lane Stage 1 mints on. Named so a reader can see at a glance which
/// assertions are lane-scoped.
const ANTHROPIC: &str = "anthropic-api";

// --- namespace ownership ---

/// The prefix is permanent, so its exact bytes are a contract: changing them
/// re-partitions every key already written to the ledger. This pin is
/// INDEPENDENT of the const -- the expectation is an explicit ASCII byte
/// array, so an edit to the const cannot drag the assertion along with it.
/// Spelling bytes also keeps this from becoming the second prefix literal
/// the uniqueness scan below forbids.
#[test]
fn the_prefix_is_exactly_the_six_ascii_bytes_of_the_permanent_namespace() {
    // Arrange -- f, i, e, l, d, colon.
    let permanent: [u8; 6] = [0x66, 0x69, 0x65, 0x6c, 0x64, 0x3a];

    // Act -- the bytes the module actually mints with.
    let minted = FIELD_CAPABILITY_PREFIX.as_bytes();

    // Assert.
    assert_eq!(
        minted,
        permanent.as_slice(),
        "the permanent prefix must not change: {} would re-partition every persisted key",
        FIELD_CAPABILITY_PREFIX
    );
    let key = field_capability_key(GROUNDED_PATH).expect("accepted");
    assert_eq!(
        &key.as_bytes()[..permanent.len()],
        permanent.as_slice(),
        "a minted key must open with the permanent bytes; got {key}"
    );
}

/// A second COMPILED spelling of the prefix anywhere under `crates/` is free
/// to drift from this module's and silently re-partition already-persisted
/// keys. Only a Rust string literal reaches a running binary and can mint a
/// key, so the scan covers every `.rs` file and counts occurrences rather
/// than carrying files -- a duplicate const beside the original is the same
/// risk as one in another crate. The needle is derived from the const, so
/// this test cannot become the copy it forbids.
///
/// Fail-closed throughout: a directory it cannot read, an entry it cannot
/// read or type, a source it cannot read, and a source it cannot lex all
/// panic naming the path, since a scan that silently skips what it cannot
/// open reports "unique" for a namespace it never examined.
#[test]
fn the_field_prefix_literal_occurs_exactly_once_under_crates() {
    // Arrange -- every Rust source under the workspace crates dir.
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root must resolve from the crate manifest dir")
        .join("crates");
    let sources = rust_sources_under(&crates_dir);
    assert!(
        sources.len() > 100,
        "the scan must actually walk the workspace; found {} sources under {}",
        sources.len(),
        crates_dir.display()
    );

    // Act -- count compiled-literal occurrences per source.
    let carriers: Vec<(PathBuf, usize)> = sources
        .into_iter()
        .filter_map(|path| {
            let count = count_prefix_literals_in(&path);
            (count > 0).then_some((path, count))
        })
        .collect();

    // Assert -- one occurrence, in this module. A zero-length result
    // would mean the scan missed the owner, not that the rule holds.
    let owner_occurrences: usize = carriers
        .iter()
        .filter(|(path, _)| path.ends_with("routectl-router/src/field_capability.rs"))
        .map(|(_, count)| *count)
        .sum();
    assert_eq!(
        owner_occurrences, 1,
        "the owning module must carry the prefix literal exactly once; carriers: {carriers:?}"
    );
    let total: usize = carriers.iter().map(|(_, count)| *count).sum();
    assert_eq!(
        total, 1,
        "the prefix literal must occur exactly once under crates/; carriers: {carriers:?}"
    );
}

/// Every spelling the language offers for the same compiled value. The
/// lexer decodes each one, so a raw string and an escaped colon are
/// compared by VALUE -- a byte scan would see three different byte
/// sequences here and could only catch the first.
///
/// Every fixture is BUILT from the const rather than spelled out: a literal
/// fixture would itself be a carrier and the uniqueness test above would
/// count this file -- the invariant holding on its own test.
#[test]
fn the_scan_accepts_every_literal_spelling_of_a_key() {
    let p = FIELD_CAPABILITY_PREFIX;
    let escaped = p.replace(':', "\\u{3a}");
    for (label, source) in [
        ("a bare prefix literal", format!("const P: &str = \"{p}\";")),
        (
            "a raw string literal",
            format!("const P: &str = r\"{p}{GROUNDED_PATH}\";"),
        ),
        (
            "a hashed raw string literal",
            format!("const P: &str = r#\"{p}x\"#;"),
        ),
        (
            "a literal with an escaped colon",
            format!("const P: &str = \"{escaped}x\";"),
        ),
        (
            "a literal inside a macro invocation",
            format!("fn f() {{ println!(\"{{}}\", \"{p}x\"); }}"),
        ),
        (
            "a literal inside a non-doc attribute",
            format!("#[cfg_attr(test, path = \"{p}x\")]\nstruct S;"),
        ),
        (
            "a literal carrying a punctuation-led path",
            format!("const P: &str = \"{p}-private\";"),
        ),
    ] {
        assert_eq!(
            count_prefix_literals_in_source(&source),
            1,
            "a {label} must be detected: {source}"
        );
    }
}

/// Documentation prose is not a compiled key, and the lexer cannot tell it
/// from a value on its own: `///` and `//!` DESUGAR to `#[doc = "..."]`
/// before tokenization, so a doc line explaining the namespace -- as the
/// owning module's own docs do -- would otherwise count as a spelling of it.
///
/// Comment-marker fixtures carry NO space after the marker: `/// x` desugars
/// to `" x"`, which the full-suffix rule already rejects on the leading
/// space, so a spaced fixture would pass with or without the doc skip --
/// vacuous. The unspaced form desugars to exactly the key. Verified by
/// deletion.
#[test]
fn the_scan_rejects_doc_attributes_and_doc_comments() {
    let p = FIELD_CAPABILITY_PREFIX;
    for (label, source) in [
        (
            "an explicit outer doc attribute",
            format!("#[doc = \"{p}thinking\"]\nstruct S;"),
        ),
        (
            "an explicit inner doc attribute",
            format!("#![doc = \"{p}thinking\"]\nstruct S;"),
        ),
        ("an outer doc comment", format!("///{p}thinking\nstruct S;")),
        (
            "an inner module doc comment",
            format!("//!{p}{GROUNDED_PATH}\nstruct S;"),
        ),
        (
            "a doc attribute on a field",
            format!("struct S {{\n    #[doc = \"{p}x\"]\n    field: u8,\n}}"),
        ),
        (
            "a doc comment carrying prose after the key",
            format!("/// the namespace is {p}thinking\nstruct S;"),
        ),
        // Doc values are routinely BUILT rather than written literally, so
        // the whole value expression must be skipped -- its nested literals
        // are still documentation.
        (
            "a doc value built with concat!",
            format!("#[doc = concat!(\"{p}thinking\")]\nstruct S;"),
        ),
        (
            "a doc value built with a nested macro call",
            format!("#[doc = concat!(\"{p}\", stringify!(\"{p}thinking\"))]\nstruct S;"),
        ),
        // A doc value nested in cfg_attr is the same prose one level
        // deeper: the docs.rs idiom writes it exactly this way. Each of
        // these carries an otherwise VALID key, so the full-suffix rule
        // cannot be what rejects them -- only the doc rule can.
        (
            "a doc value nested in cfg_attr",
            format!("#[cfg_attr(docsrs, doc = \"{p}{GROUNDED_PATH}\")]\nstruct S;"),
        ),
        (
            "a concat! doc value nested in cfg_attr",
            format!("#[cfg_attr(docsrs, doc = concat!(\"{p}thinking\"))]\nstruct S;"),
        ),
        (
            "a concat! doc value beside a later cfg_attr item",
            format!("#[cfg_attr(a, doc = concat!(\"{p}x\"), inline)]\nstruct S;"),
        ),
        (
            "a doc value nested two cfg_attr levels deep",
            format!("#[cfg_attr(a, cfg_attr(b, doc = concat!(\"{p}thinking\")))]\nstruct S;"),
        ),
        (
            "a doc value beside a non-literal cfg_attr item",
            format!("#[cfg_attr(docsrs, no_inline, doc = \"{p}thinking\")]\nstruct S;"),
        ),
    ] {
        assert_eq!(
            count_prefix_literals_in_source(&source),
            0,
            "a {label} must not be counted as a carrier: {source}"
        );
    }
}

/// The doc rule drops values that are genuine documentation, never every
/// literal in an attribute and never every `doc =` anywhere. These paired
/// positives keep the rejections above honest: `doc` in a non-doc attribute
/// is a key that attribute defines for itself, and `doc` as a `cfg_attr`
/// PREDICATE is a config key -- suppressing either would let a real key hide
/// behind ordinary attribute syntax.
#[test]
fn non_documentation_literals_inside_attributes_still_count() {
    let p = FIELD_CAPABILITY_PREFIX;
    for (label, source) in [
        (
            "a doc-named argument of a non-doc attribute",
            format!("#[some_attribute(doc = \"{p}thinking\")]\nstruct S;"),
        ),
        (
            "a macro-built value of a non-doc attribute",
            format!("#[some_attribute(path = concat!(\"{p}thinking\"))]\nstruct S;"),
        ),
        // A doc value followed by a sibling item in ONE token stream: the
        // skip must stop at the comma or it swallows the sibling. Written as
        // a direct attribute because `cfg_attr` splits its arguments on
        // top-level commas before the skip ever sees them.
        (
            "a sibling item after a doc value in one attribute",
            format!("#[doc = \"prose\", path = concat!(\"{p}thinking\")]\nmod m;"),
        ),
        (
            "a doc-named macro-built value of a non-doc attribute",
            format!("#[some_attribute(doc = concat!(\"{p}thinking\"))]\nstruct S;"),
        ),
        (
            "a doc-named argument nested under a non-doc attribute",
            format!("#[some_attribute(inner(doc = \"{p}thinking\"))]\nstruct S;"),
        ),
        (
            "a doc-named argument of a non-doc attribute inside cfg_attr",
            format!("#[cfg_attr(test, some_attribute(doc = \"{p}thinking\"))]\nstruct S;"),
        ),
        (
            "a doc-named cfg_attr predicate",
            format!("#[cfg_attr(doc = \"{p}thinking\", path = \"other\")]\nmod m;"),
        ),
        (
            "a doc-named predicate of a nested cfg_attr",
            format!("#[cfg_attr(a, cfg_attr(doc = \"{p}thinking\", inline))]\nstruct S;"),
        ),
    ] {
        assert_eq!(
            count_prefix_literals_in_source(&source),
            1,
            "a {label} must still be detected: {source}"
        );
    }
}

/// A literal that merely OPENS with the prefix is not a key. The suffix is
/// validated whole against the production grammar, so prose that begins
/// with the namespace, and a dotted tail the constructor would refuse, are
/// both rejected -- a first-byte check admits every one of these.
#[test]
fn the_scan_rejects_literals_whose_full_suffix_is_not_a_key() {
    let p = FIELD_CAPABILITY_PREFIX;
    let over_cap = "a".repeat(MAX_FIELD_PATH_BYTES + 1);
    for (label, suffix) in [
        ("a prose tail with a space", "thinking display"),
        ("a prose sentence", "thinking is rejected upstream"),
        ("a tail with a tab", "thinking\\tdisplay"),
        ("a tail with a newline", "thinking\\ndisplay"),
        ("an empty interior segment", "thinking..display"),
        ("a trailing separator", "thinking.display."),
        ("a leading separator", ".thinking.display"),
        ("a non-ASCII tail", "affichag\\u{e9}"),
        ("an over-length tail", over_cap.as_str()),
    ] {
        let source = format!("const P: &str = \"{p}{suffix}\";");

        assert_eq!(
            count_prefix_literals_in_source(&source),
            0,
            "a {label} must not be counted as a carrier: {source}"
        );
    }
}

/// The reason this is a lexer rather than a boundary heuristic: the measured
/// workspace carries 124 occurrences of the prefix bytes across 55 files,
/// essentially all field labels, identifiers and prose the lexer cannot see.
/// COMPACT labels (`field:Type`, no space) are the case a byte heuristic gets
/// wrong -- the byte after the colon is a key byte, so a boundary rule counts
/// it and only real tokenization does not.
#[test]
fn the_scan_rejects_labels_identifiers_and_comments() {
    for (label, source) in [
        ("a compact struct field label", "struct S { field:String }"),
        (
            "a compact initializer label",
            "fn f() -> S { S { field:value } }",
        ),
        (
            "a compact generic field label",
            "struct S { field:Vec<u8>, other:u8 }",
        ),
        (
            "a spaced field label",
            "struct S { headers_field: HeaderMap }",
        ),
        (
            "a field reference in a format string",
            "fn f() { println!(\"got {headers_field:?}\"); }",
        ),
        (
            "a line comment naming the field",
            "// leaked into the headers field:see below\nstruct S;",
        ),
        (
            "a block comment naming the field",
            "/* the field:name is prose */\nstruct S;",
        ),
        (
            "an identifier ending in the bytes",
            "fn f() { let subfield:u8 = 0; }",
        ),
        (
            "a match arm on the bare name",
            "fn f(x: &str) { match x { \"field\" => (), _ => () } }",
        ),
    ] {
        assert_eq!(
            count_prefix_literals_in_source(source),
            0,
            "a {label} must not be counted as a carrier: {source}"
        );
    }
}

/// The scan must be measured against the real corpus, not only against
/// synthetic sources: on the committed tree it finds exactly the one owning
/// declaration, which is what makes the uniqueness assertion meaningful
/// rather than trivially satisfied by a scan that accepts nothing.
#[test]
fn the_scan_finds_the_owning_declaration_in_the_owning_module() {
    let owner = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/field_capability.rs");

    assert_eq!(count_prefix_literals_in(&owner), 1);
}

// --- byte preservation ---

/// The token is permanent, so the minted key must be the prefix plus the
/// upstream's own bytes: any normalization (case folding, segment reduction,
/// separator rewriting) would fuse two distinct fields onto one row.
#[test]
fn a_minted_key_is_the_prefix_plus_the_path_byte_for_byte() {
    let key = field_capability_key(GROUNDED_PATH).expect("a qualified dotted path is accepted");

    assert!(
        key.starts_with(FIELD_CAPABILITY_PREFIX),
        "minted key must sit inside the namespace; got {key}"
    );
    assert_eq!(key, format!("{FIELD_CAPABILITY_PREFIX}{GROUNDED_PATH}"));
}

/// The qualified path is kept whole rather than reduced to its leaf: two
/// fields sharing a leaf name must not collide on one permanent token.
#[test]
fn two_paths_sharing_a_leaf_segment_mint_distinct_keys() {
    let nested = field_capability_key("thinking.enabled.display").expect("accepted");
    let shallow = field_capability_key("output_config.display").expect("accepted");

    assert_ne!(nested, shallow);
    assert!(nested.ends_with("thinking.enabled.display"));
    assert!(shallow.ends_with("output_config.display"));
}

/// A key round-trips back to the exact path it was minted from, and the reader
/// refuses a key from any other namespace rather than guessing.
#[test]
fn a_key_round_trips_to_its_original_path() {
    let key = field_capability_key(GROUNDED_PATH).expect("accepted");

    assert_eq!(field_capability_path(&key), Some(GROUNDED_PATH));
    assert_eq!(field_capability_path("web_search"), None);
    assert_eq!(field_capability_path("reasoning_replay:codex"), None);
}

// --- bounded grammar ---

/// The accept control for the reject cases below: realistic envelope paths,
/// from one segment to a deeply qualified one, plus a path exactly at the cap.
/// Without this the rejects would pass on a constructor accepting nothing.
#[test]
fn realistic_envelope_paths_and_a_path_at_the_cap_are_accepted() {
    let at_cap = "a".repeat(MAX_FIELD_PATH_BYTES);
    for path in [
        "thinking",
        "thinking.display",
        GROUNDED_PATH,
        "tools.0.input_schema.properties",
        "output_config.format",
        "anthropic_beta",
        "metadata.user_id",
        at_cap.as_str(),
    ] {
        assert_eq!(
            field_capability_key(path),
            Some(format!("{FIELD_CAPABILITY_PREFIX}{path}")),
            "a realistic envelope path must be accepted: {path}"
        );
    }
    assert_eq!(at_cap.len(), MAX_FIELD_PATH_BYTES);
}

/// Two grammar rejects. An empty path names no field and an empty SEGMENT
/// means the string was not a path -- a leading dot, a trailing dot, a run of
/// dots, and only-separators would each collapse onto a real path's shape.
/// Non-printable-ASCII is rejected too: whitespace and control bytes would
/// forge an operator log line, and a multi-byte sequence would make a token
/// whose rendering depends on the reader.
#[test]
fn an_empty_segment_or_a_non_printable_ascii_path_is_rejected() {
    for path in [
        "",
        ".thinking.display",
        "thinking.display.",
        "thinking..display",
        ".",
        "...",
        "thinking display",
        "thinking.\ndisplay",
        "thinking.\tdisplay",
        "thinking.display\r\n",
        "thinking.display\0",
        "thinking.affichag\u{e9}",
        "thinking.\u{200b}display",
        "\u{feff}thinking.display",
    ] {
        assert_eq!(
            field_capability_key(path),
            None,
            "an empty-segment or non-printable-ASCII path must be rejected: {path:?}"
        );
    }
}

/// Two rejects the grammar's other rules would otherwise admit: one byte
/// over the cap (the boundary counterpart of the at-cap accept above), and a
/// path that is already a KEY -- minting that would double the prefix into a
/// token no reader can attribute.
#[test]
fn an_over_length_path_or_a_key_passed_back_as_a_path_is_rejected() {
    let over_cap = "a".repeat(MAX_FIELD_PATH_BYTES + 1);
    let key = field_capability_key(GROUNDED_PATH).expect("accepted");

    assert_eq!(field_capability_key(&over_cap), None);
    assert_eq!(field_capability_key(&key), None);
    assert_eq!(field_capability_key(FIELD_CAPABILITY_PREFIX), None);
}

// --- catalog scope ---

/// Wire-shape facts are catalog-independent, so the predicate is false for
/// the whole namespace -- not only for the one grounded path.
#[test]
fn every_field_key_is_not_catalog_scoped() {
    for path in ["thinking", GROUNDED_PATH, "tools.0.input_schema"] {
        let key = field_capability_key(path).expect("accepted");

        assert!(
            !capability_key_is_catalog_scoped(&key),
            "a field key must not be catalog scoped: {key}"
        );
    }
}

/// The default is catalog-scoped: every known catalog key, the existing
/// composite replay key, and an unknown key from a namespace this build does
/// not recognize all stay scoped. The unknown-key control is what makes the
/// default a default rather than a closed list.
#[test]
fn known_and_unknown_non_field_keys_are_catalog_scoped() {
    for key in routectl_core::capability::WELL_KNOWN_CAPABILITY_KEYS {
        assert!(
            capability_key_is_catalog_scoped(key),
            "a known catalog capability key must be catalog scoped: {key}"
        );
    }
    for key in [
        "reasoning_replay:codex",
        "some_future_namespace:thinking.display",
        "",
    ] {
        assert!(
            capability_key_is_catalog_scoped(key),
            "an unrecognized key must default to catalog scoped: {key:?}"
        );
    }
}

/// The predicate answers a namespace question, never a validity one: a key
/// persisted by an earlier build keeps its scope regardless of whether
/// today's grammar would still mint its path.
#[test]
fn scope_is_decided_by_namespace_not_by_path_validity() {
    let malformed_but_in_namespace = format!("{FIELD_CAPABILITY_PREFIX}.thinking..display.");

    assert!(field_capability_key(".thinking..display.").is_none());
    assert!(!capability_key_is_catalog_scoped(
        &malformed_but_in_namespace
    ));
}

// --- Stage 1 lane boundary ---

/// Stage 1 mints on anthropic-api, where the shared normalizer is a
/// pass-through, so the key reaching the registry and ledger is the minted
/// bytes exactly.
#[test]
fn the_anthropic_lane_normalizes_a_field_key_unchanged() {
    let key = field_capability_key(GROUNDED_PATH).expect("accepted");

    assert_eq!(normalize_capability_key(&key, ANTHROPIC), key);
}

/// Pinned Stage 1 exclusion, NOT a desired behavior: the Bedrock branch of
/// the shared normalizer reduces a dotted key to its head segment, so two
/// distinct fields collapse onto one token. Stage 1 does not mint or act on
/// field keys for that lane and core normalization is deliberately left
/// alone; this test fails the moment that changes, which is when the
/// exclusion needs revisiting. The plain-catalog-key assertions are the
/// control that keeps this from reading as a claim about all keys.
#[test]
fn the_bedrock_lane_currently_truncates_a_field_key_at_the_first_dot() {
    let nested = field_capability_key("thinking.display").expect("accepted");
    let sibling = field_capability_key("thinking.budget_tokens").expect("accepted");

    let normalized = normalize_capability_key(&nested, "bedrock");

    assert_eq!(normalized, format!("{FIELD_CAPABILITY_PREFIX}thinking"));
    assert_eq!(
        normalize_capability_key(&sibling, "bedrock"),
        normalized,
        "the truncation is lossy: two distinct fields collapse onto one token"
    );
    assert_eq!(
        normalize_capability_key("web_search", ANTHROPIC),
        "web_search"
    );
    assert_eq!(
        normalize_capability_key("web_search", "bedrock"),
        "web_search"
    );
}

// --- scan implementation ---

/// Count the compiled string literals in the Rust source at `path` whose
/// decoded value is a complete field capability key. An unreadable source
/// panics rather than counting zero: silently scoring it clean would make the
/// uniqueness claim vacuous over the file most likely to be mid-edit.
fn count_prefix_literals_in(path: &Path) -> usize {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("the scan must read {}: {err}", path.display()));
    count_prefix_literals_in_source(&source)
}

/// Count matching string literals in `source` by walking a real Rust token
/// tree. A source that does not lex panics, for the same fail-closed
/// reason an unreadable one does.
fn count_prefix_literals_in_source(source: &str) -> usize {
    let stream: proc_macro2::TokenStream = source
        .parse()
        .unwrap_or_else(|err| panic!("a Rust source must lex as a token stream: {err}"));
    count_prefix_literals_in_tokens(stream)
}

/// Recurse the token tree, testing every string literal's decoded value.
/// Non-literal tokens carry no value a key could hide in, so they are walked
/// past -- which is precisely why a field label cannot be counted.
///
/// Documentation VALUES are skipped, but only in the syntactic positions
/// documentation can occupy: `///` and `//!` desugar to `#[doc = "..."]`,
/// and `cfg_attr`'s later arguments carry the same prose one level deeper.
/// Every other literal in an attribute is still examined.
fn count_prefix_literals_in_tokens(stream: proc_macro2::TokenStream) -> usize {
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
                count += count_prefix_literals_in_attribute(group.stream(), true);
                trees.next();
            }
            proc_macro2::TokenTree::Group(group) => {
                count += count_prefix_literals_in_tokens(group.stream());
            }
            proc_macro2::TokenTree::Literal(literal) => {
                if let syn::Lit::Str(text) = syn::Lit::new(literal)
                    && is_field_capability_key(&text.value())
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
fn count_prefix_literals_in_attribute(
    body: proc_macro2::TokenStream,
    is_doc_position: bool,
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
                        count_prefix_literals_in_cfg_attr(body)
                    } else {
                        count_prefix_literals_in_attribute(body, false)
                    };
                    trees.next();
                }
            }
            proc_macro2::TokenTree::Group(group) => {
                count += count_prefix_literals_in_attribute(group.stream(), is_doc_position);
            }
            proc_macro2::TokenTree::Literal(literal) => {
                if let syn::Lit::Str(text) = syn::Lit::new(literal)
                    && is_field_capability_key(&text.value())
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
fn count_prefix_literals_in_cfg_attr(body: proc_macro2::TokenStream) -> usize {
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
    let mut count = count_prefix_literals_in_attribute(predicate, false);
    for attribute in items {
        count += count_prefix_literals_in_attribute(attribute, true);
    }
    count
}

/// True when `value` is a complete field capability key: exactly the prefix
/// the PRODUCTION grammar would accept.
///
/// Validating the whole suffix rather than its first byte is what separates
/// a key from prose that merely opens the same way. The production
/// predicate is the authority here on purpose: a carrier is defined as
/// something the constructor could really mint, so a second copy of the
/// grammar in this test could drift from the grammar it claims to mirror.
fn is_field_capability_key(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix(FIELD_CAPABILITY_PREFIX) else {
        return false;
    };
    suffix.is_empty() || is_qualified_field_path(suffix)
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
fn rust_sources_under(root: &Path) -> Vec<PathBuf> {
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
