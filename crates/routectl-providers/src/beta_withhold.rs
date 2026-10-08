//! The withheld-beta decision shared by the two anthropic-vocabulary egresses.
//!
//! The router hands each attempt the client `anthropic-beta` flags its lane
//! must not send (`routectl_internal.withheld_betas`). Both egresses drop
//! those flags from the client set before the wire, except a flag the
//! operator-asserted floor also carries: the floor always ships. What counts
//! as the floor is each egress's own call; the decision itself lives here so
//! the two lanes cannot drift on it.

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
    fn entry_without_a_readable_flag_is_kept() {
        let items = vec![Some("x"), None, Some("a")];

        let got = split_withheld(&items, |i| *i, &owned(&["x"]), &[]);

        assert_eq!(got.kept, vec![None, Some("a")]);
        assert_eq!(got.dropped, vec![Some("x")]);
    }
}
