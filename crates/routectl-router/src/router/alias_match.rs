//! Alias precedence and chain expansion over the `[aliases]` table: the
//! one owner of both. Pure over the table; the caller says what a model
//! nickname resolves to (an installed model at dispatch, a `[models]` key
//! in a config-only report), so dispatch and every reader of the routing
//! config walk the same rules.

use std::collections::BTreeMap;

use routectl_core::{Error, Result};

use crate::config::AliasValue;
use crate::glob::{AliasPattern, PrefixIndex};

use super::ALIAS_MAX_RECURSION_DEPTH;

/// The `[aliases]` key consulted when nothing else matches a wire name.
const DEFAULT_ALIAS_KEY: &str = "default";

/// One step of wire-name resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteStep {
    /// An `[aliases]` key matching the wire name: exact, else the longest
    /// prefix glob ([`match_alias_key`]).
    AliasKey,
    /// The wire name itself as a model nickname.
    Nickname,
    /// The `default` catch-all key.
    Default,
}

/// Resolution order for a wire name. An alias key shadows a nickname of
/// the same name (glob keys included), and a nickname wins over the
/// catch-all.
pub(super) const ROUTE_PRECEDENCE: [RouteStep; 3] =
    [RouteStep::AliasKey, RouteStep::Nickname, RouteStep::Default];

/// Whether a chain entry names a model nickname or another alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasStepKind {
    /// The entry is a `[models]` nickname.
    Model,
    /// The entry is an `[aliases]` key, expanded in place.
    Alias,
}

/// The prefix-glob keys of `aliases`, longest prefix first. A glob key
/// that fails to parse never matches; `on_invalid` gets it with the parse
/// error.
pub(super) fn glob_index(
    aliases: &BTreeMap<String, AliasValue>,
    mut on_invalid: impl FnMut(&str, &str),
) -> PrefixIndex<AliasValue> {
    let mut index = PrefixIndex::new();
    for (key, value) in aliases {
        if !key.contains('*') {
            continue;
        }
        match AliasPattern::parse(key) {
            Ok(pattern @ AliasPattern::Prefix(_)) => index.insert(pattern, value.clone()),
            Ok(AliasPattern::Exact(_)) => {}
            Err(err) => on_invalid(key, &err),
        }
    }
    index
}

/// The alias value whose key matches `wire`: the exact key, else the
/// longest matching prefix glob of `globs` (built by [`glob_index`] from
/// the same table).
fn match_alias_key(
    aliases: &BTreeMap<String, AliasValue>,
    globs: &PrefixIndex<AliasValue>,
    wire: &str,
) -> Option<AliasValue> {
    aliases
        .get(wire)
        .cloned()
        .or_else(|| globs.longest_match(wire))
}

/// What one resolution step offers for a wire name.
pub(super) enum RouteMatch<T> {
    /// An alias value to expand.
    Alias(AliasValue),
    /// The wire name itself, resolved as a model nickname.
    Nickname(T),
}

/// What `step` offers for `wire`, or `None` when the step does not apply.
/// `globs` is [`glob_index`] of `aliases`; `lookup` resolves a model
/// nickname.
pub(super) fn route_match<T, L>(
    aliases: &BTreeMap<String, AliasValue>,
    globs: &PrefixIndex<AliasValue>,
    wire: &str,
    step: RouteStep,
    lookup: &L,
) -> Option<RouteMatch<T>>
where
    L: Fn(&str) -> Option<T>,
{
    match step {
        RouteStep::AliasKey => match_alias_key(aliases, globs, wire).map(RouteMatch::Alias),
        RouteStep::Nickname => lookup(wire).map(RouteMatch::Nickname),
        RouteStep::Default => aliases
            .get(DEFAULT_ALIAS_KEY)
            .cloned()
            .map(RouteMatch::Alias),
    }
}

/// Expands every entry of `value` into `out` by [`expand_entry`], at
/// nesting `depth` (0 for a top-level chain).
///
/// # Errors
/// `Error::Config` once `depth` exceeds `ALIAS_MAX_RECURSION_DEPTH`, which
/// is how a cycle startup validation missed surfaces.
pub(super) fn expand_value<T, L>(
    aliases: &BTreeMap<String, AliasValue>,
    value: &AliasValue,
    depth: usize,
    lookup: &L,
    out: &mut Vec<T>,
) -> Result<()>
where
    L: Fn(&str) -> Option<T>,
{
    if depth > ALIAS_MAX_RECURSION_DEPTH {
        return Err(Error::Config(format!(
            "alias chain recursion exceeded depth {ALIAS_MAX_RECURSION_DEPTH}; \
             possible cycle that startup validation missed -- \
             run `routectl config check` to surface the offending alias"
        )));
    }
    for entry in value.nicknames() {
        expand_entry(aliases, entry, depth, lookup, out)?;
    }
    Ok(())
}

/// Expands one chain entry at nesting `depth` into `out`, in dispatch
/// order. An `[aliases]` key wins over a nickname of the same name and is
/// expanded depth-first; otherwise `lookup` resolves the entry as a model
/// nickname. Returns what the entry named, `None` when it is neither (the
/// entry is dropped; startup validation reports it).
///
/// Glob keys are expanded here by their literal key string: this relies on
/// glob keys living in `[aliases]` under that literal pattern as well as in
/// the glob index.
///
/// # Errors
/// As [`expand_value`].
pub(super) fn expand_entry<T, L>(
    aliases: &BTreeMap<String, AliasValue>,
    entry: &str,
    depth: usize,
    lookup: &L,
    out: &mut Vec<T>,
) -> Result<Option<AliasStepKind>>
where
    L: Fn(&str) -> Option<T>,
{
    if let Some(nested) = aliases.get(entry) {
        expand_value(aliases, nested, depth + 1, lookup, out)?;
        return Ok(Some(AliasStepKind::Alias));
    }
    Ok(lookup(entry).map(|model| {
        out.push(model);
        AliasStepKind::Model
    }))
}
