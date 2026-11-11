//! The conditions under which a localized rejection names a carried member
//! the generic arm may remove: each condition alone refuses.

use routectl_core::capability::SignalTier;

use super::{LocalizedPath, RejectionKind, member_refused_by};

/// One row's edit of the grounded localization.
type Flip = fn(&mut LocalizedPath<'static>);

/// A localization meeting every condition: a grounded member refusal,
/// self-identifying, of the field itself, at a member rather than an element.
fn grounded() -> LocalizedPath<'static> {
    LocalizedPath {
        raw: "messages.1.content.0.caller",
        segments: vec!["messages", "content", "caller"],
        ends_in_index: false,
        tier: SignalTier::SelfIdentifying,
        kind: RejectionKind::UnsupportedField,
        presupposes_presence: true,
        replacements: Vec::new(),
        ceiling: None,
        echoed: None,
        levels: 0,
        extra_member: true,
    }
}

/// Mutation checks, one per conjunct of `member_refused_by`: drop the
/// `extra_member` check -> red on "template not grounded"; drop the tier
/// check -> red on "inferred tier"; drop the kind check -> red on "refused
/// value"; drop the `ends_in_index` check -> red on "array element".
#[test]
fn each_parser_fact_alone_refuses_a_member_repair() {
    let control = member_refused_by(&grounded()).expect("control: the grounded row names a member");
    assert_eq!(
        control.segments().collect::<Vec<_>>(),
        ["messages", "content", "caller"],
        "control: the index-free path",
    );
    let rows: [(&str, Flip); 4] = [
        ("template not grounded", |l| l.extra_member = false),
        ("inferred tier", |l| l.tier = SignalTier::Inferred),
        ("refused value", |l| l.kind = RejectionKind::InvalidValue),
        ("array element", |l| {
            l.raw = "messages.1.content.0";
            l.segments = vec!["messages", "content"];
            l.ends_in_index = true;
        }),
    ];
    for (name, flip) in rows {
        let mut localized = grounded();
        flip(&mut localized);

        let named = member_refused_by(&localized);

        assert!(named.is_none(), "{name}");
    }
}
