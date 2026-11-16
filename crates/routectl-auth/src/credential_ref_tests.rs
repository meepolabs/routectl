use super::{CredentialRef, SecretRef};

const SENTINEL: &str = "SENTINELq7x2NOTREAL";

#[test]
fn parse_accepts_every_credential_form() {
    let cases = [
        (
            "env",
            "env://X",
            CredentialRef::Secret(SecretRef::Env("X".into())),
        ),
        (
            "file",
            "file:///abs",
            CredentialRef::Secret(SecretRef::File("/abs".into())),
        ),
        (
            "oauth",
            "oauth://anthropic",
            CredentialRef::Secret(SecretRef::OAuth {
                provider: "anthropic".into(),
                label: None,
            }),
        ),
        (
            "oauth with seat",
            "oauth://anthropic#seat",
            CredentialRef::Secret(SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("seat".into()),
            }),
        ),
        (
            "aws profile",
            "aws://profile/name",
            CredentialRef::AwsProfile("name".into()),
        ),
        ("aws chain", "aws://chain", CredentialRef::AwsChain),
        ("forwarded", "forwarded", CredentialRef::Forwarded),
    ];

    for (name, input, expected) in cases {
        let parsed = CredentialRef::parse(input);

        assert_eq!(parsed.ok(), Some(expected), "{name}: {input}");
    }
}

#[test]
fn parse_refuses_malformed_credentials_without_echoing_them() {
    let cases = [
        ("literal", format!("literal:{SENTINEL}")),
        ("aws profile without name", "aws://profile/".to_string()),
        (
            "aws profile with two segments",
            format!("aws://profile/a/{SENTINEL}"),
        ),
        ("aws chain with a path", format!("aws://chain/{SENTINEL}")),
        ("aws unknown form", format!("aws://{SENTINEL}")),
        ("forwarded wrong case", "Forwarded".to_string()),
        ("forwarded with suffix", format!("forwarded{SENTINEL}")),
        ("empty", String::new()),
        ("bare token", SENTINEL.to_string()),
        ("relative file path", format!("file://sk-ant-{SENTINEL}")),
        (
            "relative file path, non-ASCII",
            format!("file://cl\u{e9}-{SENTINEL}"),
        ),
        (
            "oauth with a blank seat label",
            "oauth://anthropic# ".to_string(),
        ),
    ];

    for (name, input) in cases {
        let err = CredentialRef::parse(&input).expect_err(name);

        let msg = err.to_string();
        assert!(!msg.contains(SENTINEL), "{name}: error echoed input: {msg}");
        assert!(
            !msg.contains("Forwarded"),
            "{name}: error echoed input: {msg}"
        );
    }
}

/// The relative-path refusal keeps naming the scheme rule once it stops
/// echoing the value: the message stays actionable.
#[test]
fn a_relative_file_path_is_refused_naming_the_absolute_path_rule() {
    let err = CredentialRef::parse(&format!("file://sk-ant-{SENTINEL}"))
        .expect_err("relative file path refused");

    let msg = err.to_string();
    assert!(msg.contains("file://"), "{msg}");
    assert!(msg.contains("absolute path"), "{msg}");
    assert!(!msg.contains(SENTINEL), "{msg}");
}

#[test]
fn parse_refuses_literal_with_the_secret_ref_steer() {
    let err = CredentialRef::parse("literal:x").expect_err("literal refused");

    assert!(
        err.to_string().contains("--api-key-stdin"),
        "literal refusal must keep the safe-path steer: {err}"
    );
}

#[test]
fn scheme_label_is_one_closed_token_per_variant() {
    let cases = [
        (CredentialRef::Secret(SecretRef::Env("X".into())), "env"),
        (
            CredentialRef::Secret(SecretRef::File("/abs".into())),
            "file",
        ),
        (
            CredentialRef::Secret(SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some("seat".into()),
            }),
            "oauth",
        ),
        (
            CredentialRef::Secret(SecretRef::Literal(SENTINEL.into())),
            "literal",
        ),
        (CredentialRef::AwsProfile(SENTINEL.into()), "aws-profile"),
        (CredentialRef::AwsChain, "aws-chain"),
        (CredentialRef::Forwarded, "forwarded"),
    ];
    assert_eq!(cases.len(), CredentialRef::SCHEME_LABELS.len());

    for (credential, expected) in &cases {
        let label = credential.scheme_label();

        assert_eq!(label, *expected, "{credential:?}");
        assert!(CredentialRef::SCHEME_LABELS.contains(&label), "{label}");
    }
}

#[test]
fn display_and_debug_render_no_more_than_secret_ref() {
    let literal = SecretRef::Literal(SENTINEL.into());
    let wrapped = CredentialRef::Secret(literal.clone());

    assert_eq!(wrapped.to_string(), literal.to_string());
    assert!(!wrapped.to_string().contains(SENTINEL));
    assert!(!format!("{wrapped:?}").contains(SENTINEL));
    assert!(format!("{wrapped:?}").contains("[REDACTED]"));
}

#[test]
fn display_round_trips_through_parse() {
    for input in [
        "env://X",
        "file:///abs",
        "oauth://anthropic#seat",
        "aws://profile/name",
        "aws://chain",
        "forwarded",
    ] {
        let parsed = CredentialRef::parse(input).expect(input);

        assert_eq!(parsed.to_string(), input);
    }
}

#[test]
fn unrecognized_credential_error_names_every_accepted_form() {
    let err = CredentialRef::parse("vault://x").expect_err("unknown scheme refused");

    let msg = err.to_string();
    for form in [
        "env://",
        "file://",
        "oauth://",
        "aws://profile/",
        "aws://chain",
        "forwarded",
    ] {
        assert!(msg.contains(form), "error must name {form}: {msg}");
    }
}
