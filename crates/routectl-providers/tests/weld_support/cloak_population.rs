//! The `syn`-derived ITEM population of the OAuth-egress cloak: every function
//! item in `anthropic_api/cloak.rs` plus the flat `anthropic_api/cloak/*.rs`
//! whose visibility REACHES the orchestrator's own module.
//!
//! # Why a parser and not a declaration scan
//!
//! Eligibility is a question about module depth, not about spelling. A
//! `pub(super) fn` at the top level of a leaf file reaches the cloak module and
//! can be wired into the orchestrator; the same spelling inside an inline
//! submodule of that file reaches only the submodule, while a `pub(crate) fn`
//! beside it reaches the orchestrator. Telling those apart requires knowing
//! which module the item sits in, which requires tracking braces -- and a
//! hand-rolled Rust grammar is the one thing this repo's scan-based checks are
//! not allowed to grow. So the population comes out of a real parse.
//!
//! # The single eligibility rule
//!
//! A visibility qualifier names the MODULE it opens the item up to; call it
//! `M`. The item is visible inside `M` and every descendant of `M`. So the item
//! is reachable from the orchestrator exactly when the cloak module is `M` or a
//! descendant of it -- that is, when `M` is an ancestor-or-equal of
//! `crate::anthropic_api::cloak`. That one predicate covers `pub`,
//! `pub(crate)`, `pub(super)`, a classifiable `pub(in ...)`, and the inherited
//! (private) case without a special arm for any of them, which is why it is
//! stated once here rather than as a table of spellings.
//!
//! Two consequences worth stating because they surprise:
//!
//! - a PRIVATE function at the top level of `cloak.rs` IS eligible: its module
//!   is the cloak module, so the orchestrator can call it. That is the point --
//!   an inline transform added next to the orchestrator must not be able to
//!   evade a population drawn from the leaf directory alone.
//! - a private function in a leaf file is NOT eligible: its module is the leaf,
//!   which the orchestrator cannot reach into. Those are file-local helpers.
//!
//! # Three item kinds carry a body, and each gets its scope from a different place
//!
//! - a FREE function's scope is its own visibility.
//! - an INHERENT impl member's scope is its own visibility, because an inherent
//!   member carries one.
//! - a TRAIT impl member and a TRAIT default body carry NO visibility of their
//!   own, so the scope comes from the declaration they hang off: the self type's
//!   declared visibility for a trait impl, the trait's own for a default body.
//!   Neither is assumed crate-wide -- a file-private type's trait impl is
//!   callable only where that type is nameable.
//!
//! # `cfg` classification is a satisfiability question, not a token search
//!
//! An item is TEST-ONLY exactly when no production configuration can enable it:
//! evaluate the predicate with `test` bound false and every other atom left
//! UNKNOWN, and the item is test-only iff the result is definitely false. So
//! `cfg(test)` and `cfg(all(test, unix))` are test-only, while
//! `cfg(any(test, feature = "x"))` is enabled in a production configuration and
//! STAYS in the population. A token search for `test` cannot tell those apart,
//! and reading the union case as test-only silently drops a live item.
//!
//! # Nothing is ever skipped
//!
//! A subdirectory, a symlink, a test sidecar under the leaf directory, a
//! production `#[path]` redirect, ANY item-position macro invocation, a
//! `cfg_attr` that could inject a path or a gate, a negated-`test` predicate, a
//! parse error, an unclassifiable visibility path, an unresolvable trait-impl
//! self type, an eligible function-pointer const, or two eligible items that
//! render to one identity are all ERRORS. Each of them would otherwise remove
//! items from this side of the weld, and fewer items to compare is green by
//! having less to check.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use syn::punctuated::Punctuated;
use syn::{Attribute, ImplItem, Item, Meta, Token, TraitItem, Type, Visibility};

/// The orchestrator's own file, relative to the crate's `src`.
pub const CLOAK_ROOT: &str = "anthropic_api/cloak.rs";

/// The leaf-transform directory, relative to the crate's `src`.
pub const CLOAK_DIR: &str = "anthropic_api/cloak";

/// The module the population is measured against, as an absolute module path.
/// An item is eligible exactly when its visibility scope is an ancestor-or-equal
/// of this.
const TARGET_MODULE: &[&str] = &["crate", "anthropic_api", "cloak"];

/// One eligible function item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EligibleItem {
    /// The item's identity within its file: a bare name for a free function, a
    /// `Type::method` pair for an associated one, prefixed by any inline module
    /// path it sits under.
    pub item: String,
    /// The file it was parsed out of, relative to the crate's `src`.
    pub file: String,
}

pub fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The population's FILES: the orchestrator plus every flat `.rs` under the
/// leaf directory, sorted.
///
/// Three refusals rather than skips. A SUBDIRECTORY would hide items from a
/// flat walk. A SYMLINK resolves to content outside the directory this
/// population is defined as, so its module path would be a guess. A TEST
/// SIDECAR (`*_tests.rs`) is pulled in by a `#[path]` from some other module,
/// so treating it as `cloak::<stem>` assigns it a module path it does not have
/// -- and every item in it would then be measured against the wrong scope.
pub fn population_files() -> Result<Vec<String>, String> {
    let dir = src_root().join(CLOAK_DIR);
    let entries = std::fs::read_dir(&dir)
        .map_err(|err| format!("{CLOAK_DIR} must be a readable directory ({err})"))?;
    let mut files = vec![CLOAK_ROOT.to_string()];
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read an entry of {CLOAK_DIR}: {err}"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        // `file_type` on a DirEntry does not follow the link, so a symlink
        // reports as one here rather than as whatever it points at.
        let kind = entry
            .file_type()
            .map_err(|err| format!("cannot type {CLOAK_DIR}/{name}: {err}"))?;
        if kind.is_dir() {
            return Err(format!(
                "{CLOAK_DIR} gained the subdirectory {name}; this walk is flat and would not see \
                 the items inside it. Widen the walk, then pin the new files."
            ));
        }
        if kind.is_symlink() {
            return Err(format!(
                "{CLOAK_DIR}/{name} is a symlink; its content lives outside the directory this \
                 population is defined as, so the module path its items would be measured \
                 against is a guess. Refusing rather than guessing."
            ));
        }
        if Path::new(&name).extension().is_some_and(|ext| ext == "rs") {
            if is_test_sidecar(&name) {
                return Err(format!(
                    "{CLOAK_DIR}/{name} is a test sidecar; it is pulled in by a `#[path]` from \
                     whichever module owns it, so calling it `cloak::<stem>` assigns it a module \
                     path it does not have and measures its items against the wrong scope"
                ));
            }
            files.push(format!("{CLOAK_DIR}/{name}"));
        }
    }
    files.sort();
    if files.len() < 2 {
        return Err(format!(
            "{CLOAK_DIR} holds no .rs file; the population is being read from the wrong place"
        ));
    }
    Ok(files)
}

/// A sidecar test file by NAME, matching the crate's explicit suffix
/// convention (`*_tests.rs` / `*_test_support.rs`). A production name that
/// merely contains either token is not a sidecar.
pub fn is_test_sidecar(name: &str) -> bool {
    let file_name = name.rsplit('/').next().unwrap_or(name);
    let stem = file_name.strip_suffix(".rs").unwrap_or(file_name);
    stem.ends_with("_tests") || stem.ends_with("_test_support")
}

/// The population as `(file, source)` pairs, read off the real tree.
pub fn read_population() -> Result<Vec<(String, String)>, String> {
    let mut population = Vec::new();
    for file in population_files()? {
        let source = std::fs::read_to_string(src_root().join(&file))
            .map_err(|err| format!("{file} must be readable ({err})"))?;
        population.push((file, source));
    }
    Ok(population)
}

/// Every eligible item of a SUPPLIED population, so a control can drive the
/// derivation over injected source rather than over a mutated tree.
pub fn eligible_items(population: &[(String, String)]) -> Result<Vec<EligibleItem>, String> {
    let mut found = Vec::new();
    for (file, source) in population {
        found.extend(eligible_in_source(file, source)?);
    }
    found.sort();
    Ok(found)
}

/// Every eligible item of one source text.
pub fn eligible_in_source(file: &str, source: &str) -> Result<Vec<EligibleItem>, String> {
    let parsed = syn::parse_file(source)
        .map_err(|err| format!("{file} does not parse as Rust ({err}); refusing to guess"))?;
    let module = module_path_of(file)?;
    let mut found = Vec::new();
    walk(file, &parsed.items, &module, "", &mut found)?;
    let mut identities: Vec<&String> = found.iter().map(|item| &item.item).collect();
    identities.sort();
    for pair in identities.windows(2) {
        if pair[0] == pair[1] {
            return Err(format!(
                "{file} yields two eligible items that both render as {:?}; the derivation cannot \
                 tell them apart, so it refuses rather than counting one of them twice",
                pair[0]
            ));
        }
    }
    Ok(found)
}

/// The absolute module path of a file in the population.
fn module_path_of(file: &str) -> Result<Vec<String>, String> {
    let mut path: Vec<String> = TARGET_MODULE.iter().map(|s| (*s).to_string()).collect();
    if file == CLOAK_ROOT {
        return Ok(path);
    }
    let leaf = file
        .strip_prefix(&format!("{CLOAK_DIR}/"))
        .and_then(|name| name.strip_suffix(".rs"))
        .ok_or_else(|| {
            format!(
                "{file} is neither {CLOAK_ROOT} nor a flat file under {CLOAK_DIR}, so its module \
                 depth cannot be derived"
            )
        })?;
    if leaf.contains('/') {
        return Err(format!(
            "{file} sits below the flat leaf directory; the walk does not descend, so its items \
             would leave this side of the weld"
        ));
    }
    if is_test_sidecar(leaf) {
        return Err(format!(
            "{file} is a test sidecar; its module path is owned by whichever module `#[path]`s it \
             in, so it cannot be measured as a leaf of the cloak module"
        ));
    }
    path.push(leaf.to_string());
    Ok(path)
}

// ---------------------------------------------------------------------------
// The walk.
// ---------------------------------------------------------------------------

/// Recurse the items of one module, carrying its absolute module path and the
/// inline-module prefix an identity is qualified by.
fn walk(
    file: &str,
    items: &[Item],
    module: &[String],
    prefix: &str,
    found: &mut Vec<EligibleItem>,
) -> Result<(), String> {
    // Declared-type visibilities of THIS item list, needed before the walk
    // because a trait impl can precede the type it is for.
    let declared = declared_type_scopes(file, items, module)?;
    for item in items {
        let attrs = attrs_of(item);
        // Classified ONCE and threaded through, rather than re-derived at the
        // refusal and again at the walk: two derivations of the same predicate
        // can disagree, and the one a reader checks would not be the one that
        // decided the item's fate.
        let gating = classify_gating(file, attrs)?;
        refuse_unknown_shapes(file, item, attrs, gating)?;
        if gating == Gating::TestOnly {
            continue;
        }
        match item {
            Item::Fn(f) => {
                let scope = visibility_scope(file, &f.vis, module)?;
                if reaches_target(&scope) {
                    found.push(EligibleItem {
                        item: format!("{prefix}{}", f.sig.ident),
                        file: file.to_string(),
                    });
                }
            }
            Item::Impl(block) => {
                walk_impl(file, block, module, prefix, &declared, found)?;
            }
            Item::Trait(t) => {
                walk_trait(file, t, module, prefix, found)?;
            }
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    let mut nested: Vec<String> = module.to_vec();
                    nested.push(m.ident.to_string());
                    let nested_prefix = format!("{prefix}{}::", m.ident);
                    walk(file, inner, &nested, &nested_prefix, found)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn walk_impl(
    file: &str,
    block: &syn::ItemImpl,
    module: &[String],
    prefix: &str,
    declared: &BTreeMap<String, Vec<String>>,
    found: &mut Vec<EligibleItem>,
) -> Result<(), String> {
    let type_name = impl_type_name(file, block)?;
    // A TRAIT impl's members carry no visibility of their own: they are
    // callable wherever the trait and the self type both are. The trait side is
    // at worst public (an external trait always is, and a local one is checked
    // where it is declared), so the SELF TYPE's declared visibility is the
    // binding constraint -- not `crate`, which would over-include the members
    // of a file-private type's impl.
    let trait_impl_scope = if block.trait_.is_some() {
        Some(declared.get(&type_name).cloned().ok_or_else(|| {
            format!(
                "{file} carries a trait impl for `{type_name}`, which is not declared in this \
                 file, so the visibility bounding its members cannot be resolved. Refusing rather \
                 than assuming crate-wide reach."
            )
        })?)
    } else {
        None
    };
    for member in &block.items {
        let attrs = impl_member_attrs(member);
        let gating = classify_gating(file, attrs)?;
        refuse_member_macro(file, matches!(member, ImplItem::Macro(_)), &type_name)?;
        if gating == Gating::TestOnly {
            continue;
        }
        let ImplItem::Fn(f) = member else {
            continue;
        };
        let scope = match &trait_impl_scope {
            Some(scope) => scope.clone(),
            None => visibility_scope(file, &f.vis, module)?,
        };
        if reaches_target(&scope) {
            found.push(EligibleItem {
                item: format!("{prefix}{type_name}::{}", f.sig.ident),
                file: file.to_string(),
            });
        }
    }
    Ok(())
}

/// A trait's DEFAULT method bodies are real function bodies that ship, and they
/// are callable wherever the trait is nameable -- so their scope is the TRAIT's
/// visibility. A method with no default body declares a signature and ships no
/// code, so it is not an item this population is about.
fn walk_trait(
    file: &str,
    decl: &syn::ItemTrait,
    module: &[String],
    prefix: &str,
    found: &mut Vec<EligibleItem>,
) -> Result<(), String> {
    let trait_name = decl.ident.to_string();
    let scope = visibility_scope(file, &decl.vis, module)?;
    for member in &decl.items {
        match member {
            TraitItem::Macro(_) => {
                return Err(format!(
                    "{file} carries a macro invocation inside trait `{trait_name}`; it can expand \
                     to method bodies this derivation cannot see"
                ));
            }
            TraitItem::Verbatim(_) => {
                return Err(format!(
                    "{file} carries a trait member inside `{trait_name}` that this derivation \
                     cannot classify; refusing rather than skipping it"
                ));
            }
            TraitItem::Fn(f) => {
                let gating = classify_gating(file, &f.attrs)?;
                if gating == Gating::TestOnly || f.default.is_none() {
                    continue;
                }
                if reaches_target(&scope) {
                    found.push(EligibleItem {
                        item: format!("{prefix}{trait_name}::{}", f.sig.ident),
                        file: file.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The declared visibility SCOPE of every named type in one item list, so a
/// trait impl's members can be bounded by the type they hang off.
fn declared_type_scopes(
    file: &str,
    items: &[Item],
    module: &[String],
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut scopes = BTreeMap::new();
    for item in items {
        let (name, vis) = match item {
            Item::Struct(s) => (s.ident.to_string(), &s.vis),
            Item::Enum(e) => (e.ident.to_string(), &e.vis),
            Item::Union(u) => (u.ident.to_string(), &u.vis),
            Item::Type(t) => (t.ident.to_string(), &t.vis),
            _ => continue,
        };
        scopes.insert(name, visibility_scope(file, vis, module)?);
    }
    Ok(scopes)
}

/// Whether a visibility scope reaches the orchestrator's module.
fn reaches_target(scope: &[String]) -> bool {
    scope.len() <= TARGET_MODULE.len() && TARGET_MODULE[..scope.len()] == *scope
}

/// The absolute module path a visibility qualifier opens the item up to.
fn visibility_scope(
    file: &str,
    vis: &Visibility,
    module: &[String],
) -> Result<Vec<String>, String> {
    match vis {
        // Visible outside the crate, so visible at the crate root and below.
        Visibility::Public(_) => Ok(vec!["crate".to_string()]),
        Visibility::Inherited => Ok(module.to_vec()),
        Visibility::Restricted(restricted) => {
            let segments: Vec<String> = restricted
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            // The spelling as WRITTEN, so a diagnostic quotes the source rather
            // than inventing a `pub(in ...)` the author never typed.
            let spelling = if restricted.in_token.is_some() {
                format!("pub(in {})", segments.join("::"))
            } else {
                format!("pub({})", segments.join("::"))
            };
            let mut cursor = segments.iter();
            let first = cursor.next().ok_or_else(|| {
                format!("{file} carries a `pub(...)` with an empty path; refusing to guess")
            })?;
            match first.as_str() {
                "crate" => {
                    let mut scope = vec!["crate".to_string()];
                    scope.extend(cursor.cloned());
                    Ok(scope)
                }
                "self" => {
                    if cursor.next().is_some() {
                        return Err(unclassifiable(file, &spelling));
                    }
                    Ok(module.to_vec())
                }
                "super" => {
                    let ups = segments.iter().take_while(|s| *s == "super").count();
                    if segments.len() > ups {
                        return Err(unclassifiable(file, &spelling));
                    }
                    if ups >= module.len() {
                        return Err(format!(
                            "{file} carries `{spelling}`, which climbs past the crate root; \
                             refusing to guess what it means"
                        ));
                    }
                    Ok(module[..module.len() - ups].to_vec())
                }
                _ => Err(unclassifiable(file, &spelling)),
            }
        }
    }
}

fn unclassifiable(file: &str, spelling: &str) -> String {
    format!(
        "{file} carries `{spelling}`, a visibility this derivation cannot resolve to a module. \
         Teach it the shape or respell the visibility; it will not guess whether the item reaches \
         the orchestrator."
    )
}

/// The type an `impl` block is for, as a bare name.
fn impl_type_name(file: &str, block: &syn::ItemImpl) -> Result<String, String> {
    let Type::Path(path) = &*block.self_ty else {
        return Err(format!(
            "{file} carries an impl for a type this derivation cannot name, so its members cannot \
             be given a stable identity"
        ));
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .ok_or_else(|| format!("{file} carries an impl for an empty type path"))
}

fn attrs_of(item: &Item) -> &[Attribute] {
    match item {
        Item::Fn(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Const(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_member_attrs(member: &ImplItem) -> &[Attribute] {
    match member {
        ImplItem::Fn(f) => &f.attrs,
        ImplItem::Const(c) => &c.attrs,
        ImplItem::Type(t) => &t.attrs,
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------------

/// The shapes this derivation refuses rather than reads.
fn refuse_unknown_shapes(
    file: &str,
    item: &Item,
    attrs: &[Attribute],
    gating: Gating,
) -> Result<(), String> {
    if gating == Gating::Production && attrs.iter().any(|attr| attr.path().is_ident("path")) {
        return Err(format!(
            "{file} carries a production `#[path]` redirect; the file walk would then be reading \
             one file while the compiler reads another"
        ));
    }
    if let Item::Macro(mac) = item {
        let name = mac
            .mac
            .path
            .segments
            .last()
            .map_or_else(String::new, |segment| segment.ident.to_string());
        // Specific messages first, because these two are the shapes an author
        // is most likely to write and the reason each is refused differs.
        if mac.ident.is_some() || name == "macro_rules" {
            return Err(format!(
                "{file} defines a macro; a macro can expand to function items this derivation \
                 cannot see, so it refuses rather than under-counting"
            ));
        }
        if name == "include" {
            return Err(format!(
                "{file} pulls in a source fragment at item position; the fragment's items are not \
                 in this walk's population and would leave this side of the weld"
            ));
        }
        // CATCH-ALL. Any macro at item position can expand to function items,
        // whether it is defined here, imported from elsewhere in the crate, or
        // a procedural macro from a dependency -- and this derivation reads the
        // pre-expansion tokens, so it cannot see what any of them produce.
        // Enumerating the known ones and skipping the rest is how a population
        // silently shrinks.
        return Err(format!(
            "{file} invokes the macro `{name}` at item position; this derivation reads source \
             before expansion, so it cannot see the items the expansion adds. Refusing rather \
             than under-counting the population."
        ));
    }
    refuse_fn_pointer_value(file, item)?;
    Ok(())
}

/// An eligible `const` or `static` whose type is a function pointer is a
/// transform reachable by call, held in a value slot this derivation does not
/// treat as a function item. Supporting it would mean following the value's
/// initializer through arbitrary expressions; refusing it keeps the ceiling
/// honest and visible instead of leaving a silent hole.
fn refuse_fn_pointer_value(file: &str, item: &Item) -> Result<(), String> {
    let (kind, name, ty) = match item {
        Item::Const(c) => ("const", c.ident.to_string(), &*c.ty),
        Item::Static(s) => ("static", s.ident.to_string(), &*s.ty),
        _ => return Ok(()),
    };
    if is_fn_pointer_type(ty) {
        return Err(format!(
            "{file} declares the {kind} `{name}` with a function-pointer type; a transform held \
             in a value slot is callable without being a function item, and this derivation does \
             not follow value initializers. Give it a named `fn` instead."
        ));
    }
    Ok(())
}

fn is_fn_pointer_type(ty: &Type) -> bool {
    match ty {
        Type::BareFn(_) => true,
        Type::Reference(r) => is_fn_pointer_type(&r.elem),
        Type::Paren(p) => is_fn_pointer_type(&p.elem),
        Type::Group(g) => is_fn_pointer_type(&g.elem),
        _ => false,
    }
}

fn refuse_member_macro(file: &str, is_macro: bool, type_name: &str) -> Result<(), String> {
    if is_macro {
        return Err(format!(
            "{file} carries a macro invocation inside the impl for `{type_name}`; it can expand to \
             associated functions this derivation cannot see"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// cfg classification.
// ---------------------------------------------------------------------------

/// Whether an item can be enabled in any PRODUCTION configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gating {
    /// Some configuration without `test` enables it, so it is in the union
    /// population.
    Production,
    /// No configuration without `test` enables it.
    TestOnly,
}

/// A three-valued truth, for evaluating a `cfg` predicate with `test` bound
/// false and every other atom left unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truth {
    False,
    Unknown,
    True,
}

impl Truth {
    const fn negate(self) -> Self {
        match self {
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
            Self::True => Self::False,
        }
    }
}

/// Classify an item's gating from its attributes, refusing the shapes where the
/// classification would be a guess.
///
/// Several `cfg` attributes on one item are a CONJUNCTION, so any one of them
/// being unsatisfiable without `test` makes the item test-only.
fn classify_gating(file: &str, attrs: &[Attribute]) -> Result<Gating, String> {
    let mut gating = Gating::Production;
    for attr in attrs {
        if attr.path().is_ident("cfg_attr") {
            refuse_injecting_cfg_attr(file, attr)?;
            continue;
        }
        if !attr.path().is_ident("cfg") {
            continue;
        }
        let Meta::List(list) = &attr.meta else {
            return Err(format!(
                "{file} carries a `cfg` that is not a predicate list; refusing to classify it"
            ));
        };
        let predicate = list
            .parse_args_with(<Meta as syn::parse::Parse>::parse)
            .map_err(|err| {
                format!("{file} carries a `cfg` this derivation cannot parse ({err})")
            })?;
        if mentions_negated_test(file, &predicate, false)? {
            return Err(format!(
                "{file} carries a `cfg` whose predicate NEGATES `test`; that enables the item in \
                 production while wearing a test-shaped attribute, and reading it either way \
                 silently moves the item across the population boundary"
            ));
        }
        if evaluate_without_test(file, &predicate)? == Truth::False {
            gating = Gating::TestOnly;
        }
    }
    Ok(gating)
}

/// A `cfg_attr` expands to the attributes in its tail, so one whose tail can
/// introduce a `#[path]` redirect or a `cfg` gate changes the two facts this
/// derivation depends on -- and it does so only under a configuration the
/// derivation cannot evaluate. Refused rather than read under one arbitrary
/// configuration.
fn refuse_injecting_cfg_attr(file: &str, attr: &Attribute) -> Result<(), String> {
    let Meta::List(list) = &attr.meta else {
        return Err(format!(
            "{file} carries a `cfg_attr` that is not a list; refusing to classify it"
        ));
    };
    let nested = list
        .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .map_err(|err| {
            format!("{file} carries a `cfg_attr` this derivation cannot parse ({err})")
        })?;
    // The first element is the condition; the rest are the attributes it would
    // add.
    for injected in nested.iter().skip(1) {
        let name = injected
            .path()
            .segments
            .last()
            .map_or_else(String::new, |segment| segment.ident.to_string());
        if name == "path" || name == "cfg" || name == "cfg_attr" {
            return Err(format!(
                "{file} carries a `cfg_attr` that can add `{name}`; that would redirect the file \
                 walk or re-gate the item under a configuration this derivation cannot evaluate"
            ));
        }
    }
    Ok(())
}

/// Whether `test` appears under an ODD number of `not`s. That is the shape
/// whose classification is genuinely ambiguous -- `not(test)` is a production
/// item behind a test-shaped attribute. `all(test, not(unix))` is NOT this
/// shape: the `not` does not wrap `test`, so the item is plainly test-only.
fn mentions_negated_test(file: &str, meta: &Meta, negated: bool) -> Result<bool, String> {
    match meta {
        Meta::Path(path) => Ok(negated && path.is_ident("test")),
        Meta::NameValue(_) => Ok(false),
        Meta::List(list) => {
            let name = list
                .path
                .segments
                .last()
                .map_or_else(String::new, |segment| segment.ident.to_string());
            let inner_negated = if name == "not" { !negated } else { negated };
            for inner in &nested_metas(file, list)? {
                if mentions_negated_test(file, inner, inner_negated)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// Evaluate a `cfg` predicate with `test` bound FALSE and every other atom
/// UNKNOWN. `False` means no production configuration enables the item.
fn evaluate_without_test(file: &str, meta: &Meta) -> Result<Truth, String> {
    match meta {
        Meta::Path(path) => Ok(if path.is_ident("test") {
            Truth::False
        } else {
            Truth::Unknown
        }),
        // `feature = "x"`, `target_os = "y"`: configuration this derivation
        // does not resolve, so either value is possible.
        Meta::NameValue(_) => Ok(Truth::Unknown),
        Meta::List(list) => {
            let name = list
                .path
                .segments
                .last()
                .map_or_else(String::new, |segment| segment.ident.to_string());
            let inner = nested_metas(file, list)?;
            match name.as_str() {
                "not" => {
                    let [only] = &inner[..] else {
                        return Err(format!(
                            "{file} carries a `cfg` `not` with {} operands rather than one; \
                             refusing to classify it",
                            inner.len()
                        ));
                    };
                    Ok(evaluate_without_test(file, only)?.negate())
                }
                "all" => {
                    let mut result = Truth::True;
                    for operand in &inner {
                        match evaluate_without_test(file, operand)? {
                            Truth::False => return Ok(Truth::False),
                            Truth::Unknown => result = Truth::Unknown,
                            Truth::True => {}
                        }
                    }
                    Ok(result)
                }
                "any" => {
                    let mut result = Truth::False;
                    for operand in &inner {
                        match evaluate_without_test(file, operand)? {
                            Truth::True => return Ok(Truth::True),
                            Truth::Unknown => result = Truth::Unknown,
                            Truth::False => {}
                        }
                    }
                    Ok(result)
                }
                // A predicate with operands that is none of the three
                // combinators (`target_has_atomic(...)` and friends).
                _ => Ok(Truth::Unknown),
            }
        }
    }
}

fn nested_metas(file: &str, list: &syn::MetaList) -> Result<Vec<Meta>, String> {
    list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .map(|parsed| parsed.into_iter().collect())
        .map_err(|err| format!("{file} carries a `cfg` this derivation cannot parse ({err})"))
}
