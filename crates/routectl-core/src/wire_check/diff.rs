//! Structural diff of the body a client sent against the body routectl sent
//! on, reduced to content-free findings.
//!
//! The engine is total: it recurses at most [`MAX_DIFF_DEPTH`] levels and
//! visits at most [`MAX_DIFF_NODES`] nodes, then stops and reports the
//! findings gathered so far with `truncated` set. It never panics on any
//! input shape.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::iter::once;
use std::ops::Range;

use serde_json::{Map, Number, Value};

use super::edits::{DeclaredEdit, EditKind, PathClass, is_concrete_pattern, pattern_matches};
use super::lane::LaneClass;
use super::path::{MAX_WIRE_PATH_DEPTH, WirePath};

/// Deepest nesting level the engine descends to; the root is level 0.
pub const MAX_DIFF_DEPTH: usize = 64;

/// Most nodes one diff visits. Every value a comparison, fingerprint or
/// finding touches counts once per touch, every element of an array pair is
/// charged before the pair is aligned, every declared edit is charged once
/// when the register is indexed, and every wildcard pattern tried against a
/// path counts once per try.
pub const MAX_DIFF_NODES: usize = 200_000;

/// One difference between the two bodies.
///
/// The path names the input location, except for an `added` value, which
/// has no input location and is named where it sits in the output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Finding {
    /// Sanitized pointer to the difference.
    pub path: WirePath,
    /// How the path differs.
    pub class: PathClass,
}

/// Result of one diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffOutcome {
    /// Undeclared differences, in traversal order.
    pub findings: Vec<Finding>,
    /// True when a depth or node budget stopped the walk; `findings` then
    /// holds only what was found before it stopped.
    pub truncated: bool,
}

/// Compare `input` with `output` on a lane of class `lane`, suppressing
/// differences that `edits` declare.
///
/// FIDELITY: object key order is ignored and numbers compare numerically;
/// nothing else is normalized. Within an array, a value found at another
/// index counts as `moved` only when its order relative to the other
/// matched values changed; an element a declared Remove (input side) or Add
/// (output side) names is taken out of the alignment, so later elements
/// shift without findings. A Remove explains `missing`, an Add `added`, a
/// Rewrite `changed`; a Relocate explains `moved`, and the `missing` or
/// `added` at its path together with the equal value at the other end of a
/// move across parents.
///
/// TRANSLATION: `input` is the canonical top-level `provider_extras`
/// object; each of its keys absent from the top level of `output` is
/// `translation_unconsumed`. Nothing nested is compared.
pub fn diff(input: &Value, output: &Value, lane: LaneClass, edits: &[DeclaredEdit]) -> DiffOutcome {
    let mut walker = Walker::new(edits);
    if walker.truncated {
        return walker.finish();
    }
    match lane {
        LaneClass::Fidelity => {
            walker.compare(input, output, &WirePath::root(), 0);
            walker.pair_relocations();
        }
        LaneClass::Translation => walker.unconsumed_extras(input, output),
    }
    walker.finish()
}

/// An array element with its original index.
type Element<'v> = (usize, &'v Value);

/// The two arrays being compared, minus the elements declared edits name.
struct Sides<'p, 'v> {
    input: Vec<Element<'v>>,
    output: Vec<Element<'v>>,
    path: &'p WirePath,
    depth: usize,
}

struct Recorded<'v> {
    path: WirePath,
    class: PathClass,
    value: Option<&'v Value>,
    relocation_end: bool,
}

enum Explanation {
    Undeclared,
    Declared,
    RelocationEnd,
}

/// Which input elements matched an equal output element, and which of those
/// matches keep their relative order.
struct Alignment {
    matched: Vec<bool>,
    used: Vec<bool>,
    anchors: Vec<(usize, usize)>,
}

impl Alignment {
    fn new(input_len: usize, output_len: usize) -> Self {
        Self {
            matched: vec![false; input_len],
            used: vec![false; output_len],
            anchors: Vec::new(),
        }
    }

    fn pair(&mut self, input: usize, output: usize, in_order: bool) {
        if let Some(slot) = self.matched.get_mut(input) {
            *slot = true;
        }
        if let Some(slot) = self.used.get_mut(output) {
            *slot = true;
        }
        if in_order {
            self.anchors.push((input, output));
        }
    }

    fn is_matched(&self, input: usize) -> bool {
        self.matched.get(input).copied().unwrap_or(false)
    }

    fn is_used(&self, output: usize) -> bool {
        self.used.get(output).copied().unwrap_or(true)
    }
}

/// Set of [`EditKind`]s, one bit per kind.
#[derive(Clone, Copy, Default)]
struct Kinds(u8);

impl Kinds {
    const fn bit(kind: EditKind) -> u8 {
        match kind {
            EditKind::Remove => 1,
            EditKind::Add => 2,
            EditKind::Rewrite => 4,
            EditKind::Relocate => 8,
        }
    }

    const fn with(self, kind: EditKind) -> Self {
        Self(self.0 | Self::bit(kind))
    }

    const fn contains(self, kind: EditKind) -> bool {
        self.0 & Self::bit(kind) != 0
    }

    /// True when a kind other than Relocate explains `class`.
    fn shifts(self, class: PathClass) -> bool {
        [EditKind::Remove, EditKind::Add, EditKind::Rewrite]
            .into_iter()
            .any(|kind| self.contains(kind) && explains(kind, class))
    }
}

/// The declared edits, indexed so that a concrete pattern costs one lookup
/// whatever the number of declarations; only wildcard patterns of the
/// path's depth are tried one by one.
#[derive(Default)]
struct Register<'e> {
    concrete: HashMap<&'e str, Kinds>,
    wildcards: [Vec<&'e DeclaredEdit>; MAX_WIRE_PATH_DEPTH + 1],
}

impl<'e> Register<'e> {
    fn new(edits: &'e [DeclaredEdit]) -> Self {
        let mut register = Self::default();
        for edit in edits {
            let pattern = edit.path();
            if is_concrete_pattern(pattern) {
                let kinds = register.concrete.entry(pattern).or_default();
                *kinds = kinds.with(edit.kind());
            } else if let Some(bucket) = register.wildcards.get_mut(pattern_depth(pattern)) {
                bucket.push(edit);
            }
        }
        register
    }

    fn is_empty(&self) -> bool {
        self.concrete.is_empty() && self.wildcards.iter().all(Vec::is_empty)
    }
}

fn pattern_depth(pattern: &str) -> usize {
    pattern.bytes().filter(|b| *b == b'/').count()
}

struct Walker<'v, 'e> {
    register: Register<'e>,
    visited: usize,
    truncated: bool,
    findings: Vec<Recorded<'v>>,
    /// Array elements materialized for alignment.
    #[cfg(test)]
    collected: usize,
    /// Register probes: concrete lookups plus wildcard patterns tried.
    #[cfg(test)]
    probes: usize,
}

impl<'v, 'e> Walker<'v, 'e> {
    /// A walker over `edits`, each charged once to the node budget; one
    /// that cannot afford its register starts truncated.
    fn new(edits: &'e [DeclaredEdit]) -> Self {
        let mut walker = Self {
            register: Register::default(),
            visited: 0,
            truncated: false,
            findings: Vec::new(),
            #[cfg(test)]
            collected: 0,
            #[cfg(test)]
            probes: 0,
        };
        if walker.charge_elements(edits.len()) {
            walker.register = Register::new(edits);
        }
        walker
    }

    fn finish(self) -> DiffOutcome {
        DiffOutcome {
            findings: self
                .findings
                .into_iter()
                .map(|r| Finding {
                    path: r.path,
                    class: r.class,
                })
                .collect(),
            truncated: self.truncated,
        }
    }

    const fn charge(&mut self) -> bool {
        if self.truncated {
            return false;
        }
        if self.visited >= MAX_DIFF_NODES {
            self.truncated = true;
            return false;
        }
        self.visited += 1;
        true
    }

    /// Charges `count` array elements up front, before anything sized by
    /// the arrays is allocated or scanned; truncates instead when they do
    /// not fit in the remaining budget.
    const fn charge_elements(&mut self, count: usize) -> bool {
        if self.truncated {
            return false;
        }
        if count > MAX_DIFF_NODES - self.visited {
            self.truncated = true;
            return false;
        }
        self.visited += count;
        true
    }

    const fn enter(&mut self, depth: usize) -> bool {
        if depth > MAX_DIFF_DEPTH {
            self.truncated = true;
            return false;
        }
        self.charge()
    }

    /// Every kind declared at a pattern matching `path`. Each wildcard
    /// pattern tried is charged; `None` once the budget is exhausted.
    fn declared_kinds(&mut self, path: &WirePath) -> Option<Kinds> {
        if path.is_coarsened() {
            return Some(Kinds::default());
        }
        #[cfg(test)]
        {
            self.probes += 1;
        }
        let mut kinds = self
            .register
            .concrete
            .get(path.as_str())
            .copied()
            .unwrap_or_default();
        let Some(bucket) = self.register.wildcards.get(path.depth()) else {
            return Some(kinds);
        };
        for edit in bucket {
            if self.truncated || self.visited >= MAX_DIFF_NODES {
                self.truncated = true;
                return None;
            }
            self.visited += 1;
            #[cfg(test)]
            {
                self.probes += 1;
            }
            if pattern_matches(edit.path(), path) {
                kinds = kinds.with(edit.kind());
            }
        }
        Some(kinds)
    }

    /// How the edits declared at `path` account for a `class` difference
    /// there; `None` once the budget is exhausted.
    fn explanation(&mut self, path: &WirePath, class: PathClass) -> Option<Explanation> {
        let kinds = self.declared_kinds(path)?;
        let relocated = kinds.contains(EditKind::Relocate);
        Some(match class {
            _ if kinds.shifts(class) => Explanation::Declared,
            PathClass::Moved if relocated => Explanation::Declared,
            PathClass::Missing | PathClass::Added if relocated => Explanation::RelocationEnd,
            _ => Explanation::Undeclared,
        })
    }

    fn record(&mut self, path: WirePath, class: PathClass, value: Option<&'v Value>) {
        if !self.charge() {
            return;
        }
        let relocation_end = match self.explanation(&path, class) {
            None | Some(Explanation::Declared) => return,
            Some(Explanation::Undeclared) => false,
            Some(Explanation::RelocationEnd) => true,
        };
        self.findings.push(Recorded {
            path,
            class,
            value,
            relocation_end,
        });
    }

    fn compare(&mut self, input: &'v Value, output: &'v Value, path: &WirePath, depth: usize) {
        if !self.enter(depth) {
            return;
        }
        match (input, output) {
            (Value::Object(a), Value::Object(b)) => self.compare_objects(a, b, path, depth),
            (Value::Array(a), Value::Array(b)) => self.compare_arrays(a, b, path, depth),
            _ => {
                if !scalars_equal(input, output) {
                    self.record(path.clone(), PathClass::Changed, None);
                }
            }
        }
    }

    fn compare_objects(
        &mut self,
        input: &'v Map<String, Value>,
        output: &'v Map<String, Value>,
        path: &WirePath,
        depth: usize,
    ) {
        for (key, value) in input {
            if self.truncated {
                return;
            }
            let child = path.child(key);
            match output.get(key) {
                Some(other) => self.compare(value, other, &child, depth + 1),
                None => self.record(child, PathClass::Missing, Some(value)),
            }
        }
        for (key, value) in output {
            if self.truncated {
                return;
            }
            if !input.contains_key(key) {
                self.record(path.child(key), PathClass::Added, Some(value));
            }
        }
    }

    fn compare_arrays(
        &mut self,
        input: &'v [Value],
        output: &'v [Value],
        path: &WirePath,
        depth: usize,
    ) {
        if !self.charge_elements(input.len().saturating_add(output.len())) {
            return;
        }
        let sides = Sides {
            input: self.undeclared_elements(input, path, PathClass::Missing),
            output: self.undeclared_elements(output, path, PathClass::Added),
            path,
            depth,
        };
        #[cfg(test)]
        {
            self.collected += sides.input.len() + sides.output.len();
        }
        if self.truncated {
            return;
        }
        let alignment = self.align(&sides);
        if self.truncated {
            return;
        }
        let end = (sides.input.len(), sides.output.len());
        let mut start = (0, 0);
        for &(i, j) in alignment.anchors.iter().chain(once(&end)) {
            self.walk_gap(&sides, &alignment, start.0..i, start.1..j);
            start = (i + 1, j + 1);
        }
    }

    /// The elements no declared edit of `class` names. Each element an edit
    /// does name is charged as a suppressed finding and dropped from the
    /// alignment.
    fn undeclared_elements(
        &mut self,
        elements: &'v [Value],
        path: &WirePath,
        class: PathClass,
    ) -> Vec<Element<'v>> {
        if self.register.is_empty() {
            return elements.iter().enumerate().collect();
        }
        let mut kept = Vec::with_capacity(elements.len());
        for (index, value) in elements.iter().enumerate() {
            let child = path.index(index);
            let Some(kinds) = self.declared_kinds(&child) else {
                return kept;
            };
            if kinds.shifts(class) {
                if !self.charge() {
                    return kept;
                }
            } else {
                kept.push((index, value));
            }
        }
        kept
    }

    fn align(&mut self, sides: &Sides<'_, 'v>) -> Alignment {
        let (input, output) = (&sides.input, &sides.output);
        let depth = sides.depth + 1;
        let mut alignment = Alignment::new(input.len(), output.len());
        let prefix = input
            .iter()
            .zip(output)
            .take_while(|(a, b)| self.same(a.1, b.1, depth))
            .count();
        let room = input.len().min(output.len()) - prefix;
        let suffix = input
            .iter()
            .rev()
            .zip(output.iter().rev())
            .take(room)
            .take_while(|(a, b)| self.same(a.1, b.1, depth))
            .count();
        for k in 0..prefix {
            alignment.pair(k, k, true);
        }
        let middle_in = prefix..input.len() - suffix;
        let middle_out = prefix..output.len() - suffix;
        self.match_middle(sides, middle_in, middle_out, &mut alignment);
        for k in 0..suffix {
            alignment.pair(input.len() - suffix + k, output.len() - suffix + k, true);
        }
        alignment
    }

    /// Pairs each middle input element with the first unused equal output
    /// element; the pairs on one longest order-preserving chain become
    /// anchors, the rest are moves.
    fn match_middle(
        &mut self,
        sides: &Sides<'_, 'v>,
        middle_in: Range<usize>,
        middle_out: Range<usize>,
        alignment: &mut Alignment,
    ) {
        let depth = sides.depth + 1;
        let mut buckets: HashMap<u64, VecDeque<usize>> = HashMap::new();
        for j in middle_out {
            let Some(&(_, value)) = sides.output.get(j) else {
                continue;
            };
            let Some(print) = self.fingerprint(value, depth) else {
                return;
            };
            buckets.entry(print).or_default().push_back(j);
        }
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        for i in middle_in {
            let Some(&(_, value)) = sides.input.get(i) else {
                continue;
            };
            let Some(print) = self.fingerprint(value, depth) else {
                return;
            };
            let Some(bucket) = buckets.get_mut(&print) else {
                continue;
            };
            let position = bucket.iter().position(|&j| {
                sides
                    .output
                    .get(j)
                    .is_some_and(|&(_, other)| self.same(value, other, depth))
            });
            if let Some(j) = position.and_then(|p| bucket.remove(p)) {
                pairs.push((i, j));
            }
        }
        let order: Vec<usize> = pairs.iter().map(|&(_, j)| j).collect();
        for ((i, j), in_order) in pairs.into_iter().zip(longest_increasing(&order)) {
            alignment.pair(i, j, in_order);
        }
    }

    /// Between two anchors: a matched input element moved; an unmatched one
    /// is compared with the next unused output element, or is missing when
    /// none is left; unused output elements left over are added.
    fn walk_gap(
        &mut self,
        sides: &Sides<'_, 'v>,
        alignment: &Alignment,
        gap_in: Range<usize>,
        gap_out: Range<usize>,
    ) {
        let mut unmatched_out = gap_out
            .filter(|&j| !alignment.is_used(j))
            .filter_map(|j| sides.output.get(j).copied());
        for i in gap_in {
            if self.truncated {
                return;
            }
            let Some(&(index, value)) = sides.input.get(i) else {
                continue;
            };
            let child = sides.path.index(index);
            if alignment.is_matched(i) {
                self.record(child, PathClass::Moved, None);
                continue;
            }
            match unmatched_out.next() {
                Some((_, other)) => self.compare(value, other, &child, sides.depth + 1),
                None => self.record(child, PathClass::Missing, Some(value)),
            }
        }
        for (index, value) in unmatched_out {
            if self.truncated {
                return;
            }
            self.record(sides.path.index(index), PathClass::Added, Some(value));
        }
    }

    /// Equality with numbers compared numerically and key order ignored.
    /// Returns false once a budget is exhausted.
    fn same(&mut self, a: &Value, b: &Value, depth: usize) -> bool {
        if !self.enter(depth) {
            return false;
        }
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .all(|(k, v)| y.get(k).is_some_and(|w| self.same(v, w, depth + 1)))
            }
            (Value::Array(x), Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(v, w)| self.same(v, w, depth + 1))
            }
            _ => scalars_equal(a, b),
        }
    }

    /// Hash consistent with [`Self::same`]: values it calls equal hash
    /// equal. `None` once a budget is exhausted.
    fn fingerprint(&mut self, value: &Value, depth: usize) -> Option<u64> {
        if !self.enter(depth) {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        match value {
            Value::Null => 0u8.hash(&mut hasher),
            Value::Bool(b) => (1u8, b).hash(&mut hasher),
            Value::Number(n) => (2u8, number_bits(n)).hash(&mut hasher),
            Value::String(s) => (3u8, s).hash(&mut hasher),
            Value::Array(items) => {
                (4u8, items.len()).hash(&mut hasher);
                for item in items {
                    self.fingerprint(item, depth + 1)?.hash(&mut hasher);
                }
            }
            Value::Object(map) => {
                let mut entries = 0u64;
                for (key, item) in map {
                    let mut entry = DefaultHasher::new();
                    (key, self.fingerprint(item, depth + 1)?).hash(&mut entry);
                    entries = entries.wrapping_add(entry.finish());
                }
                (5u8, map.len(), entries).hash(&mut hasher);
            }
        }
        Some(hasher.finish())
    }

    /// A `missing` or `added` at a declared Relocate is suppressed together
    /// with the first opposite finding that carries an equal value: the two
    /// ends of one move. An end with no counterpart stays a finding.
    fn pair_relocations(&mut self) {
        let mut paired = vec![false; self.findings.len()];
        for end in 0..self.findings.len() {
            if self.truncated {
                break;
            }
            let Some((class, Some(moved))) = self
                .findings
                .get(end)
                .filter(|f| f.relocation_end)
                .map(|f| (f.class, f.value))
            else {
                continue;
            };
            if paired.get(end).copied().unwrap_or(true) {
                continue;
            }
            if let Some(other) = self.find_counterpart(end, class, moved, &paired) {
                for slot in [end, other] {
                    if let Some(flag) = paired.get_mut(slot) {
                        *flag = true;
                    }
                }
            }
        }
        let mut flags = paired.into_iter();
        self.findings.retain(|_| !flags.next().unwrap_or(false));
    }

    fn find_counterpart(
        &mut self,
        end: usize,
        class: PathClass,
        moved: &Value,
        paired: &[bool],
    ) -> Option<usize> {
        let opposite = match class {
            PathClass::Missing => PathClass::Added,
            PathClass::Added => PathClass::Missing,
            _ => return None,
        };
        for position in (0..self.findings.len()).filter(|&p| p != end) {
            if !self.charge() {
                return None;
            }
            if paired.get(position).copied().unwrap_or(true) {
                continue;
            }
            let candidate = self
                .findings
                .get(position)
                .filter(|f| f.class == opposite && !f.path.is_coarsened())
                .and_then(|f| f.value);
            if candidate.is_some_and(|value| self.same(moved, value, 0)) {
                return Some(position);
            }
        }
        None
    }

    fn unconsumed_extras(&mut self, extras: &Value, egress: &Value) {
        let Value::Object(extras) = extras else {
            return;
        };
        let carried = egress.as_object();
        for key in extras.keys() {
            if !self.charge() {
                return;
            }
            if !carried.is_some_and(|top| top.contains_key(key)) {
                self.record(
                    WirePath::root().child(key),
                    PathClass::TranslationUnconsumed,
                    None,
                );
            }
        }
    }
}

const fn explains(kind: EditKind, class: PathClass) -> bool {
    matches!(
        (kind, class),
        (EditKind::Remove, PathClass::Missing)
            | (EditKind::Add, PathClass::Added)
            | (EditKind::Rewrite, PathClass::Changed)
            | (
                EditKind::Relocate,
                PathClass::Moved | PathClass::Missing | PathClass::Added
            )
    )
}

fn scalars_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => numbers_equal(x, y),
        (Value::String(x), Value::String(y)) => x == y,
        _ => false,
    }
}

fn numbers_equal(a: &Number, b: &Number) -> bool {
    match (integer(a), integer(b)) {
        (Some(x), Some(y)) => x == y,
        (Some(x), None) => b.as_f64().is_some_and(|f| integer_equals_float(x, f)),
        (None, Some(y)) => a.as_f64().is_some_and(|f| integer_equals_float(y, f)),
        (None, None) => a.as_f64() == b.as_f64(),
    }
}

fn integer(n: &Number) -> Option<i128> {
    n.as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from))
}

/// Exclusive upper bound of `u64`, exactly representable as an `f64`.
const U64_END: f64 = 18_446_744_073_709_551_616.0;

/// Inclusive lower bound of `i64`, exactly representable as an `f64`.
const I64_START: f64 = -9_223_372_036_854_775_808.0;

/// True only when `float` is finite, integral, inside the 64-bit integer
/// range and converts to exactly `int`. Comparing through `f64` instead
/// would call integers above 2^53 equal to a neighboring float.
fn integer_equals_float(int: i128, float: f64) -> bool {
    float.is_finite()
        && float.fract() == 0.0
        && (I64_START..U64_END).contains(&float)
        && float as i128 == int
}

/// Bits of the number as an `f64`, with `-0.0` folded onto `0.0` so that
/// numerically equal numbers hash equal.
fn number_bits(n: &Number) -> u64 {
    let float = n.as_f64().unwrap_or(0.0);
    if float == 0.0 { 0 } else { float.to_bits() }
}

/// Marks the members of one longest strictly increasing subsequence of
/// `seq`; ties resolve deterministically.
fn longest_increasing(seq: &[usize]) -> Vec<bool> {
    let mut tails: Vec<usize> = Vec::new();
    let mut previous: Vec<Option<usize>> = Vec::with_capacity(seq.len());
    for (i, &value) in seq.iter().enumerate() {
        let slot = tails.partition_point(|&t| seq.get(t).is_some_and(|&v| v < value));
        previous.push(slot.checked_sub(1).and_then(|s| tails.get(s).copied()));
        match tails.get_mut(slot) {
            Some(tail) => *tail = i,
            None => tails.push(i),
        }
    }
    let mut members = vec![false; seq.len()];
    let mut cursor = tails.last().copied();
    while let Some(i) = cursor {
        if let Some(member) = members.get_mut(i) {
            *member = true;
        }
        cursor = previous.get(i).copied().flatten();
    }
    members
}

#[cfg(test)]
#[allow(clippy::panic)]
#[path = "diff_tests.rs"]
mod tests;
