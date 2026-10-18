//! Every [`RefusalReason`] variant has at least one production producer: a
//! `RefusalReason::<Variant>` path in code a non-test build compiles, outside
//! the file that defines the enum.
//!
//! The production side is derived the way the compiler derives it, not from
//! file names. The walk starts at each crate's roots (`src/lib.rs`,
//! `src/main.rs`, `src/bin/*.rs`) and follows every `mod` declaration a
//! non-test build keeps, so a file reached only through a `#[cfg(test)]`
//! declaration, and a file no declaration reaches, never count. Inside a
//! kept file, every item, statement, arm, field or expression whose `cfg`
//! cannot hold without `test` is pruned before the paths are read: inline
//! test modules live inside production files. A `cfg` predicate is read with
//! `test` false and every other atom unknown, so `cfg(any(test, feature =
//! "x"))` stays production. Macro bodies are scanned as tokens, so a variant
//! named inside `vec![...]` counts.
//!
//! An item-position `include!` in kept code is followed into the included
//! file. Every shape the derivation cannot classify (an unreadable or
//! unparseable source, a `cfg_attr` that could add a `path` or `cfg`, a `mod`
//! declaration inside an inline module, an `include!` in expression position
//! or with a non-literal argument) is an error, never a skip: fewer files on
//! the production side would make the census pass by checking less.
//!
//! Out of scope: a variant reached only through a glob import or a `use`
//! alias. No production code names one that way.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::{Attribute, Expr, ImplItem, Item, Meta, Token, TraitItem};

use super::RefusalReason;

/// The file that defines the enum: its `ALL`, token and class tables name
/// every variant, so it is never evidence of a producer.
const DEFINING_FILE: &str = "routectl-core/src/refusal_reason.rs";

/// Crates linked only as dev-dependencies; nothing in them ships.
const DEV_ONLY_CRATES: &[&str] = &["routectl-testkit"];

const ENUM_NAME: &str = "RefusalReason";

/// The include macro's name as written in source, split so this file carries
/// no literal call site for the fragment formatter's scan to parse.
const INCLUDE_MACRO: &str = concat!("inc", "lude!");

/// The workspace `crates/` directory.
fn workspace_crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("the crates directory resolves from this crate's manifest")
}

/// Every reason no production source under `crates_dir` names, in
/// declaration order.
fn reasons_without_a_producer(crates_dir: &Path) -> Result<Vec<RefusalReason>, String> {
    let named = named_variants(crates_dir)?;
    Ok(RefusalReason::ALL
        .iter()
        .copied()
        .filter(|reason| !named.contains(&format!("{reason:?}")))
        .collect())
}

/// The variant names every production source under `crates_dir` references.
fn named_variants(crates_dir: &Path) -> Result<BTreeSet<String>, String> {
    let mut named = BTreeSet::new();
    for file in production_files(crates_dir)? {
        if file.relative == DEFINING_FILE {
            continue;
        }
        let mut census = VariantCensus::new(&file.relative, &file.dir);
        census.visit_file(&file.parsed);
        census.result?;
        named.extend(census.named);
    }
    Ok(named)
}

/// One production source: its path relative to `crates/`, its directory,
/// and its parse.
struct ProductionFile {
    relative: String,
    dir: PathBuf,
    parsed: syn::File,
}

/// Every source file a non-test build compiles, across every shipped crate
/// under `crates_dir`, sorted by path.
fn production_files(crates_dir: &Path) -> Result<Vec<ProductionFile>, String> {
    let mut crates = read_dir_sorted(crates_dir)?;
    crates.retain(|dir| dir.is_dir() && dir.join("Cargo.toml").is_file());
    let mut files = Vec::new();
    for crate_dir in crates {
        let name = file_name(&crate_dir);
        if DEV_ONLY_CRATES.contains(&name.as_str()) {
            continue;
        }
        for root in crate_roots(&crate_dir)? {
            let child_dir = root
                .parent()
                .expect("a crate root has a parent")
                .to_path_buf();
            walk_module(crates_dir, &root, &child_dir, &mut files)?;
        }
    }
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

/// A crate's compilation roots: `src/lib.rs`, `src/main.rs`, and every
/// `src/bin/*.rs`.
fn crate_roots(crate_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let src = crate_dir.join("src");
    let mut roots: Vec<PathBuf> = ["lib.rs", "main.rs"]
        .iter()
        .map(|name| src.join(name))
        .filter(|path| path.is_file())
        .collect();
    let bin = src.join("bin");
    if bin.is_dir() {
        roots.extend(
            read_dir_sorted(&bin)?
                .into_iter()
                .filter(|path| path.extension().is_some_and(|ext| ext == "rs")),
        );
    }
    Ok(roots)
}

/// Parse `file`, keep it, and recurse into every `mod` declaration it keeps
/// in a non-test build. `child_dir` is where its undecorated declarations
/// resolve.
fn walk_module(
    crates_dir: &Path,
    file: &Path,
    child_dir: &Path,
    out: &mut Vec<ProductionFile>,
) -> Result<(), String> {
    let relative = file
        .strip_prefix(crates_dir)
        .map_err(|_| format!("{} lies outside the crates directory", file.display()))?
        .to_string_lossy()
        .into_owned();
    let source =
        std::fs::read_to_string(file).map_err(|err| format!("cannot read {relative}: {err}"))?;
    let parsed =
        syn::parse_file(&source).map_err(|err| format!("cannot parse {relative}: {err}"))?;
    let declared = declared_modules(&relative, &parsed.items)?;
    let own_dir = file.parent().expect("a source file has a parent");
    out.push(ProductionFile {
        relative,
        dir: own_dir.to_path_buf(),
        parsed,
    });
    for declaration in declared {
        let (path, nested_dir) = match declaration.path {
            // A file loaded through `#[path]` owns its directory, as `mod.rs`
            // does.
            Some(path) => {
                let path = own_dir.join(path);
                let dir = path
                    .parent()
                    .expect("a module path has a parent")
                    .to_path_buf();
                (path, dir)
            }
            None => resolve_module(child_dir, &declaration.name)?,
        };
        walk_module(crates_dir, &path, &nested_dir, out)?;
    }
    Ok(())
}

/// Where an undecorated `mod name;` in a file whose children live in
/// `child_dir` resolves, and where that module's own children live.
fn resolve_module(child_dir: &Path, name: &str) -> Result<(PathBuf, PathBuf), String> {
    let flat = child_dir.join(format!("{name}.rs"));
    let nested = child_dir.join(name).join("mod.rs");
    match (flat.is_file(), nested.is_file()) {
        (true, false) => Ok((flat, child_dir.join(name))),
        (false, true) => Ok((nested, child_dir.join(name))),
        (true, true) => Err(format!(
            "mod {name} is ambiguous: both {} and {} exist",
            flat.display(),
            nested.display()
        )),
        (false, false) => Err(format!(
            "mod {name} resolves to no file under {}",
            child_dir.display()
        )),
    }
}

/// A `mod name;` declaration a non-test build keeps.
struct ModDeclaration {
    name: String,
    path: Option<String>,
}

/// The out-of-line module declarations at the top level of a file that a
/// non-test build keeps.
fn declared_modules(file: &str, items: &[Item]) -> Result<Vec<ModDeclaration>, String> {
    let mut declared = Vec::new();
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        if is_test_only(file, &module.attrs)? {
            continue;
        }
        match &module.content {
            None => declared.push(ModDeclaration {
                name: module.ident.to_string(),
                path: path_attribute(file, &module.attrs)?,
            }),
            Some((_, inner)) => refuse_nested_declarations(file, inner)?,
        }
    }
    Ok(declared)
}

/// An inline module that declares an out-of-line one resolves it relative to
/// a directory this walk does not model, so it is refused.
fn refuse_nested_declarations(file: &str, items: &[Item]) -> Result<(), String> {
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        if is_test_only(file, &module.attrs)? {
            continue;
        }
        match &module.content {
            None => {
                return Err(format!(
                    "{file} declares `mod {};` inside an inline module; this walk does not \
                     resolve that path",
                    module.ident
                ));
            }
            Some((_, inner)) => refuse_nested_declarations(file, inner)?,
        }
    }
    Ok(())
}

fn path_attribute(file: &str, attrs: &[Attribute]) -> Result<Option<String>, String> {
    for attr in attrs {
        if !attr.path().is_ident("path") {
            continue;
        }
        let Meta::NameValue(pair) = &attr.meta else {
            return Err(format!(
                "{file} carries a `path` attribute that is not `path = \"..\"`"
            ));
        };
        let Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(path),
            ..
        }) = &pair.value
        else {
            return Err(format!(
                "{file} carries a `path` attribute whose value is not a string"
            ));
        };
        return Ok(Some(path.value()));
    }
    Ok(None)
}

/// Collects the variant names referenced as `RefusalReason::<Variant>` in
/// the parts of one file a non-test build compiles. The first shape it cannot
/// classify is kept in `result` and stops nothing else; the caller reads it.
struct VariantCensus<'f> {
    file: &'f str,
    dir: &'f Path,
    named: BTreeSet<String>,
    result: Result<(), String>,
}

impl<'f> VariantCensus<'f> {
    const fn new(file: &'f str, dir: &'f Path) -> Self {
        Self {
            file,
            dir,
            named: BTreeSet::new(),
            result: Ok(()),
        }
    }

    fn fail(&mut self, err: String) {
        if self.result.is_ok() {
            self.result = Err(err);
        }
    }

    /// Visit the items of the file an item-position `include!` names, read
    /// relative to the including file.
    fn visit_included(&mut self, mac: &syn::Macro) {
        let included = match mac.parse_body::<syn::LitStr>() {
            Ok(path) => self.dir.join(path.value()),
            Err(err) => {
                self.fail(format!(
                    "{} carries an `{INCLUDE_MACRO}` whose argument is not one string literal \
                     ({err})",
                    self.file
                ));
                return;
            }
        };
        let parsed = std::fs::read_to_string(&included)
            .map_err(|err| format!("cannot read {}: {err}", included.display()))
            .and_then(|source| {
                syn::parse_file(&source)
                    .map_err(|err| format!("cannot parse {}: {err}", included.display()))
            });
        match parsed {
            Ok(file) => {
                for item in &file.items {
                    self.visit_item(item);
                }
            }
            Err(err) => self.fail(err),
        }
    }

    /// Whether the node carrying `attrs` is compiled without `test`. A
    /// classification error is recorded and the node is skipped.
    fn keeps(&mut self, attrs: &[Attribute]) -> bool {
        match is_test_only(self.file, attrs) {
            Ok(test_only) => !test_only,
            Err(err) => {
                self.fail(err);
                false
            }
        }
    }

    fn scan_tokens(&mut self, tokens: proc_macro2::TokenStream) {
        let trees: Vec<proc_macro2::TokenTree> = tokens.into_iter().collect();
        for (at, tree) in trees.iter().enumerate() {
            match tree {
                proc_macro2::TokenTree::Group(group) => self.scan_tokens(group.stream()),
                proc_macro2::TokenTree::Ident(ident) if ident == ENUM_NAME => {
                    if let [
                        proc_macro2::TokenTree::Punct(first),
                        proc_macro2::TokenTree::Punct(second),
                        proc_macro2::TokenTree::Ident(variant),
                        ..,
                    ] = &trees[at + 1..]
                        && first.as_char() == ':'
                        && second.as_char() == ':'
                    {
                        self.named.insert(variant.to_string());
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for VariantCensus<'_> {
    fn visit_item(&mut self, item: &'ast Item) {
        if !self.keeps(item_attrs(item)) {
            return;
        }
        if let Item::Macro(mac) = item
            && mac.mac.path.is_ident("include")
        {
            self.visit_included(&mac.mac);
            return;
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        if self.keeps(impl_item_attrs(item)) {
            syn::visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        if self.keeps(trait_item_attrs(item)) {
            syn::visit::visit_trait_item(self, item);
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if self.keeps(expr_attrs(expr)) {
            syn::visit::visit_expr(self, expr);
        }
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        if self.keeps(&local.attrs) {
            syn::visit::visit_local(self, local);
        }
    }

    fn visit_stmt_macro(&mut self, stmt: &'ast syn::StmtMacro) {
        if self.keeps(&stmt.attrs) {
            syn::visit::visit_stmt_macro(self, stmt);
        }
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if self.keeps(&arm.attrs) {
            syn::visit::visit_arm(self, arm);
        }
    }

    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        if self.keeps(&field.attrs) {
            syn::visit::visit_field_value(self, field);
        }
    }

    fn visit_field(&mut self, field: &'ast syn::Field) {
        if self.keeps(&field.attrs) {
            syn::visit::visit_field(self, field);
        }
    }

    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        if self.keeps(&variant.attrs) {
            syn::visit::visit_variant(self, variant);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<&syn::PathSegment> = path.segments.iter().collect();
        for pair in segments.windows(2) {
            if pair[0].ident == ENUM_NAME {
                self.named.insert(pair[1].ident.to_string());
            }
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.is_ident("include") {
            self.fail(format!(
                "{} carries an `{INCLUDE_MACRO}` outside item position; this walk does not read \
                 it",
                self.file
            ));
            return;
        }
        self.scan_tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }
}

const fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => i.attrs.as_slice(),
        Item::Enum(i) => i.attrs.as_slice(),
        Item::ExternCrate(i) => i.attrs.as_slice(),
        Item::Fn(i) => i.attrs.as_slice(),
        Item::ForeignMod(i) => i.attrs.as_slice(),
        Item::Impl(i) => i.attrs.as_slice(),
        Item::Macro(i) => i.attrs.as_slice(),
        Item::Mod(i) => i.attrs.as_slice(),
        Item::Static(i) => i.attrs.as_slice(),
        Item::Struct(i) => i.attrs.as_slice(),
        Item::Trait(i) => i.attrs.as_slice(),
        Item::TraitAlias(i) => i.attrs.as_slice(),
        Item::Type(i) => i.attrs.as_slice(),
        Item::Union(i) => i.attrs.as_slice(),
        Item::Use(i) => i.attrs.as_slice(),
        _ => &[],
    }
}

const fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(i) => i.attrs.as_slice(),
        ImplItem::Fn(i) => i.attrs.as_slice(),
        ImplItem::Type(i) => i.attrs.as_slice(),
        ImplItem::Macro(i) => i.attrs.as_slice(),
        _ => &[],
    }
}

const fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(i) => i.attrs.as_slice(),
        TraitItem::Fn(i) => i.attrs.as_slice(),
        TraitItem::Type(i) => i.attrs.as_slice(),
        TraitItem::Macro(i) => i.attrs.as_slice(),
        _ => &[],
    }
}

const fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
        Expr::Array(e) => e.attrs.as_slice(),
        Expr::Assign(e) => e.attrs.as_slice(),
        Expr::Async(e) => e.attrs.as_slice(),
        Expr::Await(e) => e.attrs.as_slice(),
        Expr::Binary(e) => e.attrs.as_slice(),
        Expr::Block(e) => e.attrs.as_slice(),
        Expr::Break(e) => e.attrs.as_slice(),
        Expr::Call(e) => e.attrs.as_slice(),
        Expr::Cast(e) => e.attrs.as_slice(),
        Expr::Closure(e) => e.attrs.as_slice(),
        Expr::Const(e) => e.attrs.as_slice(),
        Expr::Continue(e) => e.attrs.as_slice(),
        Expr::Field(e) => e.attrs.as_slice(),
        Expr::ForLoop(e) => e.attrs.as_slice(),
        Expr::Group(e) => e.attrs.as_slice(),
        Expr::If(e) => e.attrs.as_slice(),
        Expr::Index(e) => e.attrs.as_slice(),
        Expr::Infer(e) => e.attrs.as_slice(),
        Expr::Let(e) => e.attrs.as_slice(),
        Expr::Lit(e) => e.attrs.as_slice(),
        Expr::Loop(e) => e.attrs.as_slice(),
        Expr::Macro(e) => e.attrs.as_slice(),
        Expr::Match(e) => e.attrs.as_slice(),
        Expr::MethodCall(e) => e.attrs.as_slice(),
        Expr::Paren(e) => e.attrs.as_slice(),
        Expr::Path(e) => e.attrs.as_slice(),
        Expr::Range(e) => e.attrs.as_slice(),
        Expr::RawAddr(e) => e.attrs.as_slice(),
        Expr::Reference(e) => e.attrs.as_slice(),
        Expr::Repeat(e) => e.attrs.as_slice(),
        Expr::Return(e) => e.attrs.as_slice(),
        Expr::Struct(e) => e.attrs.as_slice(),
        Expr::Try(e) => e.attrs.as_slice(),
        Expr::TryBlock(e) => e.attrs.as_slice(),
        Expr::Tuple(e) => e.attrs.as_slice(),
        Expr::Unary(e) => e.attrs.as_slice(),
        Expr::Unsafe(e) => e.attrs.as_slice(),
        Expr::While(e) => e.attrs.as_slice(),
        Expr::Yield(e) => e.attrs.as_slice(),
        _ => &[],
    }
}

/// Three-valued truth of a `cfg` predicate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Truth {
    False,
    Unknown,
    True,
}

/// Whether no non-test configuration compiles a node carrying `attrs`.
/// Several `cfg` attributes are a conjunction, so one that cannot hold
/// without `test` is enough.
fn is_test_only(file: &str, attrs: &[Attribute]) -> Result<bool, String> {
    let mut test_only = false;
    for attr in attrs {
        if attr.path().is_ident("cfg_attr") {
            refuse_gating_cfg_attr(file, attr)?;
            continue;
        }
        if !attr.path().is_ident("cfg") {
            continue;
        }
        let predicate = attr
            .parse_args::<Meta>()
            .map_err(|err| format!("{file} carries a `cfg` this walk cannot parse ({err})"))?;
        if evaluate_without_test(file, &predicate)? == Truth::False {
            test_only = true;
        }
    }
    Ok(test_only)
}

/// A `cfg_attr` that can add a `path` or a `cfg` changes the module tree or
/// the gating under a configuration this walk does not evaluate.
fn refuse_gating_cfg_attr(file: &str, attr: &Attribute) -> Result<(), String> {
    let nested = attr
        .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        .map_err(|err| format!("{file} carries a `cfg_attr` this walk cannot parse ({err})"))?;
    for injected in nested.iter().skip(1) {
        let path = injected.path();
        if path.is_ident("path") || path.is_ident("cfg") || path.is_ident("cfg_attr") {
            return Err(format!(
                "{file} carries a `cfg_attr` that can add a `path` or `cfg`; this walk cannot \
                 classify it"
            ));
        }
    }
    Ok(())
}

/// Evaluate a `cfg` predicate with `test` false and every other atom
/// unknown.
fn evaluate_without_test(file: &str, meta: &Meta) -> Result<Truth, String> {
    match meta {
        Meta::Path(path) => Ok(if path.is_ident("test") {
            Truth::False
        } else {
            Truth::Unknown
        }),
        Meta::NameValue(_) => Ok(Truth::Unknown),
        Meta::List(list) => {
            let operands: Vec<Meta> = list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .map_err(|err| format!("{file} carries a `cfg` this walk cannot parse ({err})"))?
                .into_iter()
                .collect();
            let truths = operands
                .iter()
                .map(|operand| evaluate_without_test(file, operand))
                .collect::<Result<Vec<Truth>, String>>()?;
            if list.path.is_ident("not") {
                let [only] = truths[..] else {
                    return Err(format!(
                        "{file} carries a `cfg(not(..))` without one operand"
                    ));
                };
                return Ok(match only {
                    Truth::False => Truth::True,
                    Truth::Unknown => Truth::Unknown,
                    Truth::True => Truth::False,
                });
            }
            if list.path.is_ident("all") {
                return Ok(if truths.contains(&Truth::False) {
                    Truth::False
                } else if truths.contains(&Truth::Unknown) {
                    Truth::Unknown
                } else {
                    Truth::True
                });
            }
            if list.path.is_ident("any") {
                return Ok(if truths.contains(&Truth::True) {
                    Truth::True
                } else if truths.contains(&Truth::Unknown) {
                    Truth::Unknown
                } else {
                    Truth::False
                });
            }
            Ok(Truth::Unknown)
        }
    }
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
    let mut paths = entries
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<PathBuf>>>()
        .map_err(|err| format!("cannot read an entry of {}: {err}", dir.display()))?;
    paths.sort();
    Ok(paths)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[test]
fn every_refusal_reason_has_a_production_producer() {
    // Arrange
    let crates_dir = workspace_crates_dir();

    // Act
    let files = production_files(&crates_dir).expect("the production walk succeeds");
    let missing = reasons_without_a_producer(&crates_dir).expect("the census succeeds");

    // Assert: the walk reached this workspace's production tree, not an empty
    // or wrong directory, and kept test code out of it.
    let relative: BTreeSet<&str> = files.iter().map(|f| f.relative.as_str()).collect();
    assert!(
        files.len() > 300,
        "the walk found only {} production sources -- wrong directory?",
        files.len()
    );
    for kept in [
        "routectl-providers/src/anthropic_api/messages.rs",
        "routectl-router/src/router/dispatch.rs",
        "routectl-cli/src/main.rs",
    ] {
        assert!(relative.contains(kept), "the walk misses {kept}");
    }
    for pruned in [
        "routectl-core/src/refusal_reason_census_tests.rs",
        "routectl-providers/src/bedrock/converse/request_tests_output_schema.rs",
        "routectl-cli/src/test_secret.rs",
    ] {
        assert!(
            !relative.contains(pruned),
            "the walk keeps test-only {pruned}"
        );
    }
    assert_eq!(missing, Vec::<RefusalReason>::new());
}

/// Where a fixture names the reason under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant {
    /// A production function body.
    ProductionBody,
    /// Inside a macro invocation in a production function.
    ProductionMacro,
    /// An item gated `cfg(any(test, feature = ..))`, which ships.
    TestOrFeatureItem,
    /// An item gated `cfg(not(test))`, which ships.
    NotTestItem,
    /// An inline `#[cfg(test)] mod` inside a production file.
    InlineTestModule,
    /// A `#[cfg(all(test, unix))]` function in a production file.
    TestAndUnixItem,
    /// A `#[cfg(test)]` statement inside a production function.
    TestOnlyStatement,
    /// A `#[cfg(test)]` match arm inside a production function.
    TestOnlyArm,
    /// A file reached only through a `#[cfg(test)] mod` declaration.
    TestGatedModuleFile,
    /// A sidecar reached through `#[cfg(test)] #[path = ..] mod`.
    TestPathSidecar,
    /// A file no `mod` declaration reaches.
    OrphanFile,
    /// A file a production item-position `include!` reads.
    IncludedFile,
    /// A file an `include!` inside an inline `#[cfg(test)] mod` reads.
    TestIncludedFile,
    /// The enum's defining file.
    DefiningFile,
    /// A dev-only crate.
    DevOnlyCrate,
}

const MISSING: RefusalReason = RefusalReason::Unparseable;

fn named(reason: RefusalReason) -> String {
    format!("{ENUM_NAME}::{reason:?}")
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("a fixture file has a parent"))
        .expect("create fixture directory");
    std::fs::write(path, text).expect("write fixture file");
}

/// A `crates/` tree of three crates: a producer crate naming every reason
/// but [`MISSING`] in production, a core crate holding the defining file, and
/// a dev-only crate, with [`MISSING`] planted where `plant` says.
fn planted_tree(root: &Path, plant: Plant) {
    let missing = named(MISSING);
    let others: Vec<String> = RefusalReason::ALL
        .iter()
        .filter(|&&r| r != MISSING)
        .map(|&r| format!("    let _ = {};\n", named(r)))
        .collect();
    let in_body = match plant {
        Plant::ProductionBody => format!("    let _ = {missing};\n"),
        Plant::ProductionMacro => format!("    let _ = vec![{missing}];\n"),
        Plant::TestOnlyStatement => format!("    #[cfg(test)]\n    let _ = {missing};\n"),
        Plant::TestOnlyArm => format!(
            "    match 0 {{\n        #[cfg(test)]\n        1 => {{ let _ = {missing}; }}\n        _ => {{}}\n    }}\n"
        ),
        _ => String::new(),
    };
    let extra_item = match plant {
        Plant::TestOrFeatureItem => {
            format!("#[cfg(any(test, feature = \"x\"))]\npub fn extra() {{ let _ = {missing}; }}\n")
        }
        Plant::NotTestItem => {
            format!("#[cfg(not(test))]\npub fn extra() {{ let _ = {missing}; }}\n")
        }
        Plant::InlineTestModule => {
            format!("#[cfg(test)]\nmod tests {{\n    fn t() {{ let _ = {missing}; }}\n}}\n")
        }
        Plant::TestAndUnixItem => {
            format!("#[cfg(all(test, unix))]\nfn helper() {{ let _ = {missing}; }}\n")
        }
        Plant::IncludedFile => format!("{INCLUDE_MACRO}(\"included.rs\");\n"),
        Plant::TestIncludedFile => {
            format!("#[cfg(test)]\nmod tests {{\n    {INCLUDE_MACRO}(\"included.rs\");\n}}\n")
        }
        _ => String::new(),
    };
    let producer = root.join("producer/src");
    write(&root.join("producer/Cargo.toml"), "");
    write(
        &producer.join("lib.rs"),
        "mod refusals;\n#[cfg(test)]\nmod gated;\n#[cfg(test)]\n#[path = \"refusals_tests.rs\"]\nmod refusals_tests;\n",
    );
    write(
        &producer.join("refusals.rs"),
        &format!(
            "pub fn produce() {{\n{}{in_body}}}\n{extra_item}",
            others.concat()
        ),
    );
    let in_file = format!("fn f() {{ let _ = {missing}; }}\n");
    let empty = String::new();
    let pick = |at: Plant| if at == plant { &in_file } else { &empty };
    write(&producer.join("gated.rs"), pick(Plant::TestGatedModuleFile));
    write(
        &producer.join("refusals_tests.rs"),
        pick(Plant::TestPathSidecar),
    );
    write(&producer.join("orphan.rs"), pick(Plant::OrphanFile));
    let included = if matches!(plant, Plant::IncludedFile | Plant::TestIncludedFile) {
        &in_file
    } else {
        &empty
    };
    write(&producer.join("included.rs"), included);
    write(&root.join("routectl-core/Cargo.toml"), "");
    write(
        &root.join("routectl-core/src/lib.rs"),
        "pub mod refusal_reason;\n",
    );
    write(
        &root.join("routectl-core/src/refusal_reason.rs"),
        pick(Plant::DefiningFile),
    );
    write(&root.join("routectl-testkit/Cargo.toml"), "");
    write(
        &root.join("routectl-testkit/src/lib.rs"),
        pick(Plant::DevOnlyCrate),
    );
}

#[test]
fn a_reason_named_only_outside_production_code_is_reported_without_a_producer() {
    let rows = [
        (Plant::ProductionBody, Vec::new()),
        (Plant::ProductionMacro, Vec::new()),
        (Plant::TestOrFeatureItem, Vec::new()),
        (Plant::NotTestItem, Vec::new()),
        (Plant::InlineTestModule, vec![MISSING]),
        (Plant::TestAndUnixItem, vec![MISSING]),
        (Plant::TestOnlyStatement, vec![MISSING]),
        (Plant::TestOnlyArm, vec![MISSING]),
        (Plant::TestGatedModuleFile, vec![MISSING]),
        (Plant::TestPathSidecar, vec![MISSING]),
        (Plant::OrphanFile, vec![MISSING]),
        (Plant::IncludedFile, Vec::new()),
        (Plant::TestIncludedFile, vec![MISSING]),
        (Plant::DefiningFile, vec![MISSING]),
        (Plant::DevOnlyCrate, vec![MISSING]),
    ];
    let mut observed = Vec::new();
    for (plant, _) in &rows {
        let root = tempfile::TempDir::new().expect("tempdir");
        planted_tree(root.path(), *plant);

        let missing = reasons_without_a_producer(root.path()).expect("the census succeeds");

        observed.push((*plant, missing));
    }

    let rendered = |rows: &[(Plant, Vec<RefusalReason>)]| -> Vec<String> {
        rows.iter().map(|(p, m)| format!("{p:?}: {m:?}")).collect()
    };
    assert_eq!(rendered(&observed), rendered(&rows));
}

#[test]
fn a_cfg_attr_that_can_add_a_path_fails_the_walk() {
    let root = tempfile::TempDir::new().expect("tempdir");
    planted_tree(root.path(), Plant::ProductionBody);
    write(
        &root.path().join("producer/src/lib.rs"),
        "#[cfg_attr(feature = \"x\", path = \"elsewhere.rs\")]\nmod refusals;\n",
    );

    let err = reasons_without_a_producer(root.path()).expect_err("an unclassifiable cfg_attr");

    assert!(err.contains("cfg_attr"), "{err}");
}
