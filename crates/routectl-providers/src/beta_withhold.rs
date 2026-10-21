//! The withheld-beta decision shared by the two anthropic-vocabulary egresses.
//!
//! The router hands each attempt the client `anthropic-beta` flags its lane
//! must not send (`routectl_internal.withheld_betas`). Both egresses drop
//! those flags from the client set before the wire, except a flag the
//! operator-asserted floor also carries: the floor always ships. What counts
//! as the floor is each egress's own call; the decision itself lives here so
//! the two lanes cannot drift on it.
//!
//! Each carrier also merges its beta lists into one before the wire; the
//! order-preserving merge they share lives here too.

/// The client flags split by the withhold decision, each side in its
/// original order.
#[derive(Debug, PartialEq, Eq)]
pub struct WithholdSplit<T> {
    /// Entries that ship.
    pub kept: Vec<T>,
    /// Entries withheld from this lane.
    pub dropped: Vec<T>,
}

/// Split `flags` into the entries that ship and the ones withheld. An entry
/// is withheld when `flag_of` reads a flag from it that `withheld` names and
/// `floor` does not; every other entry (including one `flag_of` cannot read)
/// is kept in place.
pub fn split_withheld<T: Clone>(
    flags: &[T],
    flag_of: impl Fn(&T) -> Option<&str>,
    withheld: &[String],
    floor: &[String],
) -> WithholdSplit<T> {
    let mut split = WithholdSplit {
        kept: Vec::with_capacity(flags.len()),
        dropped: Vec::new(),
    };
    for item in flags {
        let is_withheld = flag_of(item).is_some_and(|flag| {
            withheld.iter().any(|w| w == flag) && !floor.iter().any(|f| f == flag)
        });
        if is_withheld {
            split.dropped.push(item.clone());
        } else {
            split.kept.push(item.clone());
        }
    }
    split
}

/// `entries` in order, minus every entry whose flag an earlier entry already
/// carries: the first occurrence of a flag keeps its position. An entry
/// `flag_of` cannot read a flag from is never a repeat, so it is kept in
/// place.
pub fn merge_unique<T>(
    entries: impl IntoIterator<Item = T>,
    flag_of: impl Fn(&T) -> Option<&str>,
) -> Vec<T> {
    let mut merged: Vec<T> = Vec::new();
    for entry in entries {
        let is_repeat = flag_of(&entry)
            .is_some_and(|flag| merged.iter().any(|kept| flag_of(kept) == Some(flag)));
        if !is_repeat {
            merged.push(entry);
        }
    }
    merged
}

/// Record `floor` and `body_implied` on the attempt's beta assembly slot, when
/// the router installed one. `floor` keeps only the flags `ships` says the
/// final carrier holds, so a floor flag a later strip removed is not claimed
/// as shipped. Records no flag anywhere else.
pub fn deposit_beta_assembly(
    req: &routectl_core::ChatRequest,
    floor: Vec<String>,
    body_implied: Vec<String>,
    ships: impl Fn(&str) -> bool,
) {
    let Some(slot) = req.routectl_internal.beta_assembly.as_ref() else {
        return;
    };
    let floor = floor.into_iter().filter(|flag| ships(flag)).collect();
    slot.record(routectl_core::BetaAssembly::new(floor, body_implied));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(flags: &[&str]) -> Vec<String> {
        flags.iter().map(|f| (*f).to_string()).collect()
    }

    fn split(flags: &[&str], withheld: &[&str], floor: &[&str]) -> WithholdSplit<String> {
        split_withheld(
            &owned(flags),
            |f| Some(f.as_str()),
            &owned(withheld),
            &owned(floor),
        )
    }

    #[test]
    fn withheld_flag_outside_the_floor_is_dropped_and_order_is_kept() {
        let got = split(&["a", "x", "b", "x"], &["x"], &[]);

        assert_eq!(got.kept, owned(&["a", "b"]));
        assert_eq!(got.dropped, owned(&["x", "x"]));
    }

    #[test]
    fn withheld_flag_the_floor_asserts_is_kept_in_place() {
        let got = split(&["x", "a"], &["x"], &["x"]);

        assert_eq!(got.kept, owned(&["x", "a"]));
        assert!(got.dropped.is_empty());
    }

    #[test]
    fn empty_withheld_set_keeps_everything() {
        let got = split(&["a", "b"], &[], &[]);

        assert_eq!(got.kept, owned(&["a", "b"]));
        assert!(got.dropped.is_empty());
    }

    #[test]
    fn merge_unique_keeps_the_first_occurrence_in_order() {
        let rows: [(&str, &[&str], &[&str]); 5] = [
            ("empty in, empty out", &[], &[]),
            (
                "distinct flags keep their order",
                &["b", "a", "c"],
                &["b", "a", "c"],
            ),
            (
                "a repeat keeps the first position",
                &["a", "b", "a"],
                &["a", "b"],
            ),
            (
                "every repeat collapses",
                &["c", "c", "b", "c", "b"],
                &["c", "b"],
            ),
            (
                "flags compare exactly",
                &["a", " a", "A"],
                &["a", " a", "A"],
            ),
        ];

        for (name, input, want) in rows {
            let got = merge_unique(owned(input), |f| Some(f.as_str()));

            assert_eq!(got, owned(want), "{name}");
        }
    }

    #[test]
    fn merge_unique_keeps_every_entry_without_a_readable_flag() {
        let items = vec![None, Some("x"), None, Some("x"), Some("a")];

        let got = merge_unique(items, |i| *i);

        assert_eq!(got, vec![None, Some("x"), None, Some("a")]);
    }

    #[test]
    fn entry_without_a_readable_flag_is_kept() {
        let items = vec![Some("x"), None, Some("a")];

        let got = split_withheld(&items, |i| *i, &owned(&["x"]), &[]);

        assert_eq!(got.kept, vec![None, Some("a")]);
        assert_eq!(got.dropped, vec![Some("x")]);
    }
}
