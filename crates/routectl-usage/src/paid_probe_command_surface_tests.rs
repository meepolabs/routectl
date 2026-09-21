// Surface guards: the vocabulary this boundary publishes, and the operations it
// must never publish. Lexical, because an assertion about an ABSENT function
// cannot be made by calling it -- each guard asserts its own premise first, so a
// zero match is an absence rather than an unread file.

/// The reservation module's own source, read whole.
///
/// Whole, not cut at a test marker: the tests are SIDECARS declared mid-file, so
/// there is nothing of them in this text to cut, and cutting at the declaration
/// would truncate the scan before the code it exists to cover.
const COMMAND_SOURCE: &str = include_str!("paid_probe_command.rs");

/// A committed unit is spent, so no release, refund, decrement, or cancel
/// operation may exist anywhere on this boundary.
#[test]
fn the_boundary_exposes_no_way_to_give_a_committed_unit_back() {
    // Assert the premise: the scan does see this module's own functions, so a
    // zero match below is an absence and not an unread file.
    assert!(
        COMMAND_SOURCE.contains("fn admit_paid_probe_reservation("),
        "the guard must be reading the reservation module",
    );

    // Assert
    for forbidden in [
        "fn release",
        "fn refund",
        "fn decrement",
        "fn cancel",
        "fn unreserve",
        "fn give_back",
    ] {
        assert!(
            !COMMAND_SOURCE.contains(forbidden),
            "a committed unit is never given back, so `{forbidden}` must not exist",
        );
    }
}

/// There is no timed-out or unknown answer: a caller that cannot tell whether a
/// unit was spent has no safe move, because nothing is ever given back.
#[test]
fn no_outcome_names_a_timeout_or_an_unknown_state() {
    // Assert the premise: the enum's own variants are in scan range, so a zero
    // match below is an absence.
    for present in ["Committed", "CapExhausted", "MalformedState", "WriteFailed"] {
        assert!(
            COMMAND_SOURCE.contains(present),
            "the guard must be reading the outcome vocabulary ({present})",
        );
    }

    // Assert: no variant-shaped token for an unsettled answer. `Unavailable` is
    // NOT in this class: it is a settled refusal (the caller may not call),
    // whereas a timeout or unknown would mean the unit's fate is still open --
    // and with no refund path, a caller has no safe move on that.
    for forbidden in [
        "Timeout,",
        "TimedOut",
        "Unknown,",
        "Unknown {",
        "Maybe",
        "Pending,",
    ] {
        assert!(
            !COMMAND_SOURCE.contains(forbidden),
            "an unsettled answer has no safe caller response, so `{forbidden}` must not exist",
        );
    }
}

/// The authorizing value cannot be built outside this crate.
///
/// Lexical, because the property is a compile-time one a downstream crate would
/// discover and this crate cannot: a same-crate test can always construct the
/// variant, so no in-crate behavioural test can witness the restriction. The
/// out-of-crate half is a compile probe run against the built rlib.
#[test]
fn the_authorizing_variant_cannot_be_constructed_downstream() {
    // Arrange
    let commit_enum = top_level(COMMAND_SOURCE, "pub enum PaidProbeCommit");

    // Assert the premise: the variant exists and is the one that authorizes.
    assert!(
        commit_enum.contains("Committed {"),
        "the guard must be reading the authorizing variant",
    );

    // Assert: it carries the attribute that makes a downstream struct expression
    // a compile error, and the attribute sits on THIS variant rather than merely
    // somewhere in the file.
    let before_committed = commit_enum
        .split_once("Committed {")
        .expect("the authorizing variant")
        .0;
    assert!(
        before_committed
            .rsplit('\n')
            .take(3)
            .any(|line| line.trim() == "#[non_exhaustive]"),
        "the authorizing variant must be non_exhaustive so no other crate can forge it: \
         {before_committed}",
    );
}

/// The reservation path must not touch the shared recovery edge.
///
/// Paired with the behavioural test: this one pins that the CALL is absent (so it
/// cannot be reintroduced silently), while the behavioural test pins what an
/// operator would observe. Neither alone is enough -- the call could move to a
/// helper, and the observable state could be reached another way.
#[test]
fn the_reservation_path_never_marks_the_writer_healthy() {
    // Arrange: the reservation path now lives in its own module, so the guard
    // reads THAT file whole plus the writer's narrow host impl.
    let lifecycle_source = include_str!("paid_probe_lifecycle.rs");
    let writer_source = include_str!("writer.rs");
    let host_impl = text_between(
        writer_source,
        "impl crate::paid_probe_lifecycle::ReservationHost for WriterState {",
        "\n}\n",
    );

    // Assert the premise: the edge exists and other paths DO use it, so its
    // absence here is a deliberate asymmetry rather than a missing feature.
    assert!(
        writer_source.contains("fn mark_healthy(&mut self)"),
        "the guard must be reading a writer that HAS a recovery edge",
    );
    assert!(
        writer_source.matches("self.mark_healthy()").count() >= 2,
        "other write paths must still claim recovery, or this proves nothing",
    );

    // Assert: neither the reservation module nor the writer's host impl for it
    // can reach the edge.
    for (name, body) in [
        ("reservation module", lifecycle_source.to_string()),
        ("writer host impl", host_impl),
    ] {
        assert!(
            !body.contains("mark_healthy"),
            "a control write must not claim the request path recovered ({name}): {body}",
        );
    }
}

/// A reservation must be refused once shutdown has begun, and the check must
/// happen BEFORE anything is queued.
#[test]
fn admission_refuses_after_shutdown_has_begun() {
    // Arrange
    let body = admission_body();

    // Assert: the shutdown check exists ...
    assert!(
        body.contains("is_shutting_down()"),
        "admission must refuse once shutdown has begun: {body}",
    );
    // ... and precedes the send, or a reservation could be queued into a
    // draining writer before the check ran.
    let check_at = body
        .find("is_shutting_down()")
        .expect("the shutdown check is present");
    let send_at = body.find("try_send(").expect("the send is present");
    assert!(
        check_at < send_at,
        "the shutdown check must precede the send: {body}",
    );
}

/// Admission is non-blocking BY CONSTRUCTION: a bounded `try_send` and nothing
/// else. A blocking or unbounded send would let a wedged writer stall a request
/// path before anything was even queued.
#[test]
fn admission_uses_only_the_bounded_nonblocking_send() {
    // Arrange: read the admission function's own body, so the guard cannot be
    // satisfied by a sibling.
    let body = admission_body();

    // Assert
    assert!(
        body.contains("try_send("),
        "admission must use the bounded nonblocking send: {body}",
    );
    for forbidden in [
        ".send(",
        "blocking_send",
        "await",
        "reserve_owned",
        "unbounded",
    ] {
        assert!(
            !body.contains(forbidden),
            "admission must never block or grow unbounded ({forbidden}): {body}",
        );
    }
}

/// A saturated or closed channel maps to its OWN refusal, never to anything a
/// paid call may follow. The permissive mapping is the one defect a review of
/// the enum alone would not catch.
#[test]
fn a_refused_admission_never_maps_to_an_admitted_receipt() {
    // Arrange
    let body = admission_body();
    let full_arm = arm_after(&body, "TrySendError::Full");
    let closed_arm = arm_after(&body, "TrySendError::Closed");

    // Assert
    assert!(
        full_arm.contains("Saturated"),
        "a full channel must be refused as saturated: {full_arm}",
    );
    assert!(
        closed_arm.contains("Unavailable"),
        "a closed channel must be refused as unavailable: {closed_arm}",
    );
    for arm in [&full_arm, &closed_arm] {
        assert!(
            !arm.contains("Admitted"),
            "a refused admission must not yield a receipt: {arm}",
        );
        assert!(
            !arm.contains("Committed"),
            "a refused admission must not claim a commit: {arm}",
        );
    }
}

/// The enabled gate is deliberately NOT consulted at admission: honoring a
/// telemetry preference here would disable the spend ceiling.
#[test]
fn admission_does_not_consult_the_usage_enabled_gate() {
    // Arrange
    let body = admission_body();

    // Assert the premise: the gate exists and is reachable from a handle, so
    // its absence here is a choice rather than a missing API.
    assert!(
        include_str!("handle.rs").contains("pub fn is_enabled(&self)"),
        "the guard must be reading a handle that HAS an enabled gate",
    );

    // Assert
    for forbidden in ["is_enabled", "enabled", "dropped_disabled"] {
        assert!(
            !body.contains(forbidden),
            "a spend control must not be gated on a telemetry preference ({forbidden}): {body}",
        );
    }
}

/// The accounting day and the storage key stay on this side of the boundary:
/// which day a unit landed in is an accounting concern, and a day carried
/// outward becomes state a spender logs and reasons about without owning.
#[test]
fn no_published_type_carries_the_accounting_day_or_the_storage_key() {
    // Arrange: the published vocabulary is the outcome enum, the admission enum,
    // and the receipt. Scan their declarations, which is where a field would
    // have to appear.
    let published = [
        top_level(COMMAND_SOURCE, "pub enum PaidProbeCommit"),
        top_level(COMMAND_SOURCE, "pub enum PaidProbeAdmission"),
        top_level(COMMAND_SOURCE, "pub struct PaidProbeReceipt"),
    ];

    // Assert the premise: the day is a real concept in this module, so its
    // absence from the published types is a boundary and not a missing feature.
    assert!(
        COMMAND_SOURCE.contains("utc_day"),
        "the guard must be reading a module that HAS a day concept",
    );

    // Assert
    for decl in &published {
        for forbidden in ["utc_day", "day", "epoch_ms", "reservation_key", "key"] {
            assert!(
                !decl.contains(forbidden),
                "no published type may carry `{forbidden}`: {decl}",
            );
        }
    }
}

/// The body of `admit_paid_probe_reservation`, comments stripped.
fn admission_body() -> String {
    member_declaration(COMMAND_SOURCE, "pub fn admit_paid_probe_reservation(")
}

/// A top-level item's text, from its opener to the first column-zero closing
/// brace, with comment lines removed.
///
/// Comments are stripped because these guards assert the absence of CODE. Every
/// concept they forbid in a declaration (the accounting day, the enabled gate) is
/// legitimately EXPLAINED in the prose right beside it, so a scan that read the
/// prose would fail on correct code -- and the reader's fix would be to delete
/// the explanation.
fn top_level(source: &str, opener: &str) -> String {
    text_between(source, opener, "\n}\n")
}

/// An `impl` member's text, from its opener to the first four-space-indented
/// closing brace, with comment lines removed.
fn member_declaration(source: &str, opener: &str) -> String {
    text_between(source, opener, "\n    }\n")
}

fn text_between(source: &str, opener: &str, terminator: &str) -> String {
    let start = source
        .find(opener)
        .unwrap_or_else(|| panic!("declaration not found: {opener}"));
    let rest = &source[start..];
    let end = rest
        .find(terminator)
        .unwrap_or_else(|| panic!("declaration terminator not found: {opener}"));
    strip_comments(&rest[..end])
}

/// Drop whole-line comments, keeping the code text the guards scan.
fn strip_comments(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of a match arm, from `marker` to the start of the next arm.
fn arm_after(body: &str, marker: &str) -> String {
    let start = body
        .find(marker)
        .unwrap_or_else(|| panic!("match arm not found: {marker}"));
    let rest = &body[start..];
    let end = rest.find("\n            Err").unwrap_or(rest.len());
    rest[..end].to_string()
}
