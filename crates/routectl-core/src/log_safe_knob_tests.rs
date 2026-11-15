//! Resolution of the log knobs from the environment, with the layer that
//! supplied each value.

use super::{
    DEFAULT_REDACT_PROMPTS, DEFAULT_TRACE_HEADERS, LogKnob, LogKnobSource, MAX_TRACE_BODY_BYTES,
    REDACT_PROMPTS_ENV, TRACE_BODY_BYTES_ENV, TRACE_HEADERS_ENV, resolve_redact_prompts,
    resolve_trace_body_bytes, resolve_trace_headers,
};

/// An env reader holding at most one variable.
fn env_with(var: &'static str, value: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
    move |asked| value.filter(|_| asked == var).map(str::to_string)
}

type BoolResolver = fn(&dyn Fn(&str) -> Option<String>) -> LogKnob<bool>;

#[test]
fn the_log_knobs_keep_their_env_var_names_and_defaults() {
    assert_eq!(TRACE_HEADERS_ENV, "ROUTECTL_TRACE_HEADERS");
    assert_eq!(REDACT_PROMPTS_ENV, "ROUTECTL_LOG_REDACT_PROMPTS");
    assert_eq!(TRACE_BODY_BYTES_ENV, "ROUTECTL_TRACE_BODY_BYTES");
    assert_eq!([DEFAULT_TRACE_HEADERS, DEFAULT_REDACT_PROMPTS], [false; 2]);
    assert_eq!(MAX_TRACE_BODY_BYTES, 16 * 1024);
}

#[test]
fn a_bool_log_knob_resolves_env_when_set_else_its_default() {
    let knobs: [(&str, &'static str, BoolResolver); 2] = [
        ("trace_headers", TRACE_HEADERS_ENV, |env| {
            resolve_trace_headers(env)
        }),
        ("redact_prompts", REDACT_PROMPTS_ENV, |env| {
            resolve_redact_prompts(env)
        }),
    ];
    for (knob, var, resolve) in knobs {
        let env_rows: [(&str, Option<&'static str>, bool); 10] = [
            ("env 1", Some("1"), true),
            ("env true", Some("true"), true),
            ("env yes", Some("yes"), true),
            ("env on", Some("on"), true),
            ("env mixed case, padded", Some("  TrUe \n"), true),
            ("env 0", Some("0"), false),
            ("env off", Some("off"), false),
            ("env empty", Some(""), false),
            ("env unrecognized", Some("enabled"), false),
            ("env 2", Some("2"), false),
        ];
        for (row, env_value, value) in env_rows {
            let env = env_with(var, env_value);

            let resolved = resolve(&env);

            let source = LogKnobSource::Env(var);
            assert_eq!(resolved, LogKnob { value, source }, "{knob}: row `{row}`");
        }

        let unset = env_with(var, None);

        let resolved = resolve(&unset);

        let default = LogKnob {
            value: false,
            source: LogKnobSource::Default,
        };
        assert_eq!(resolved, default, "{knob}: row `env unset`");
    }
}

#[test]
fn a_bool_log_knob_ignores_the_other_knobs_env_var() {
    let rows: [(&str, &'static str, BoolResolver); 2] = [
        ("trace_headers", REDACT_PROMPTS_ENV, |env| {
            resolve_trace_headers(env)
        }),
        ("redact_prompts", TRACE_HEADERS_ENV, |env| {
            resolve_redact_prompts(env)
        }),
    ];
    for (knob, other_var, resolve) in rows {
        let env = env_with(other_var, Some("1"));

        let resolved = resolve(&env);

        assert_eq!(resolved.source, LogKnobSource::Default, "{knob}");
    }
}

#[test]
fn the_trace_body_cap_resolves_a_positive_env_value_else_16_kib() {
    let var = TRACE_BODY_BYTES_ENV;
    let env = LogKnobSource::Env(var);
    let default = LogKnobSource::Default;
    let rows = [
        ("env positive", Some("4096"), 4096, env),
        ("env padded", Some(" 1048576\n"), 1_048_576, env),
        ("env one", Some("1"), 1, env),
        ("env zero falls to default", Some("0"), 16_384, default),
        ("env garbage falls to default", Some("x"), 16_384, default),
        ("env negative falls to default", Some("-5"), 16_384, default),
        ("env empty falls to default", Some(""), 16_384, default),
        ("env unset", None, 16_384, default),
    ];
    for (row, env_value, value, source) in rows {
        let reader = env_with(var, env_value);

        let resolved = resolve_trace_body_bytes(&reader);

        assert_eq!(resolved, LogKnob { value, source }, "row `{row}`");
    }
}

#[test]
fn the_trace_body_cap_ignores_another_env_var() {
    let env = env_with(TRACE_HEADERS_ENV, Some("4096"));

    let resolved = resolve_trace_body_bytes(&env);

    assert_eq!(resolved.source, LogKnobSource::Default);
}
