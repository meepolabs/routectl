//! Resolution of the `[log]` knobs with the layer that supplied each value.

use super::{
    DEFAULT_REDACT_PROMPTS, DEFAULT_TRACE_HEADERS, LogKnob, LogKnobSource, MAX_TRACE_BODY_BYTES,
    REDACT_PROMPTS_ENV, TRACE_BODY_BYTES_ENV, TRACE_HEADERS_ENV, resolve_redact_prompts,
    resolve_trace_body_bytes, resolve_trace_headers,
};

/// An env reader holding at most one variable.
fn env_with(var: &'static str, value: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
    move |asked| value.filter(|_| asked == var).map(str::to_string)
}

type BoolResolver = fn(&dyn Fn(&str) -> Option<String>, Option<bool>) -> LogKnob<bool>;

#[test]
fn a_bool_log_knob_resolves_env_then_config_then_default() {
    let knobs: [(&str, &'static str, bool, BoolResolver); 2] = [
        (
            "trace_headers",
            TRACE_HEADERS_ENV,
            DEFAULT_TRACE_HEADERS,
            |env, config| resolve_trace_headers(env, config),
        ),
        (
            "redact_prompts",
            REDACT_PROMPTS_ENV,
            DEFAULT_REDACT_PROMPTS,
            |env, config| resolve_redact_prompts(env, config),
        ),
    ];
    for (knob, var, default, resolve) in knobs {
        let rows = [
            (
                "env truthy beats config",
                Some("1"),
                Some(false),
                true,
                LogKnobSource::Env(var),
            ),
            (
                "env falsy beats config",
                Some("0"),
                Some(true),
                false,
                LogKnobSource::Env(var),
            ),
            (
                "env empty reads false",
                Some(""),
                Some(true),
                false,
                LogKnobSource::Env(var),
            ),
            (
                "config when env unset",
                None,
                Some(!default),
                !default,
                LogKnobSource::Config,
            ),
            (
                "default when neither",
                None,
                None,
                default,
                LogKnobSource::Default,
            ),
        ];
        for (row, env_value, config, value, source) in rows {
            let env = env_with(var, env_value);

            let resolved = resolve(&env, config);

            assert_eq!(resolved, LogKnob { value, source }, "{knob}: row `{row}`");
        }
    }
}

#[test]
fn a_bool_log_knob_ignores_the_other_knobs_env_var() {
    let env = env_with(REDACT_PROMPTS_ENV, Some("1"));

    let resolved = resolve_trace_headers(&env, None);

    assert_eq!(resolved.source, LogKnobSource::Default);
}

#[test]
fn the_trace_body_cap_takes_the_first_positive_layer() {
    let var = TRACE_BODY_BYTES_ENV;
    let rows = [
        (
            "env positive",
            Some("4096"),
            Some(8192),
            4096,
            LogKnobSource::Env(var),
        ),
        (
            "env zero falls to config",
            Some("0"),
            Some(8192),
            8192,
            LogKnobSource::Config,
        ),
        (
            "env garbage falls to config",
            Some("x"),
            Some(8192),
            8192,
            LogKnobSource::Config,
        ),
        (
            "config zero falls to default",
            None,
            Some(0),
            MAX_TRACE_BODY_BYTES,
            LogKnobSource::Default,
        ),
        (
            "neither",
            None,
            None,
            MAX_TRACE_BODY_BYTES,
            LogKnobSource::Default,
        ),
    ];
    for (row, env_value, config, value, source) in rows {
        let env = env_with(var, env_value);

        let resolved = resolve_trace_body_bytes(&env, config);

        assert_eq!(resolved, LogKnob { value, source }, "row `{row}`");
    }
}
