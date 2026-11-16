use super::{
    ResolvedWireCheckParams, SAMPLE_ONE_IN_ENV, SampleRateEnvError, SampleRateSource,
    resolve_wire_check_params,
};
use crate::wire_check_state::WireCheckParams;

/// An env reader holding at most one variable.
fn env_with(var: &'static str, value: Option<&'static str>) -> impl Fn(&str) -> Option<String> {
    move |asked| value.filter(|_| asked == var).map(str::to_string)
}

#[test]
fn the_variable_keeps_its_name() {
    assert_eq!(SAMPLE_ONE_IN_ENV, "ROUTECTL_WIRE_CHECK_SAMPLE_ONE_IN");
}

#[test]
fn the_default_params_check_the_first_8_then_1_in_100_over_a_24_hour_window() {
    let params = WireCheckParams::default();

    assert_eq!(
        (
            params.first_n,
            params.sample_one_in.get(),
            params.window_secs.get()
        ),
        (8, 100, 86_400)
    );
}

#[test]
fn a_positive_integer_sets_the_rate_and_unset_keeps_the_default() {
    let rows: [(&str, Option<&'static str>, u32, SampleRateSource); 6] = [
        ("unset", None, 100, SampleRateSource::Default),
        ("one", Some("1"), 1, SampleRateSource::Env),
        (
            "the default written out",
            Some("100"),
            100,
            SampleRateSource::Env,
        ),
        ("padded", Some(" 5 "), 5, SampleRateSource::Env),
        (
            "padded with a newline",
            Some("7\n"),
            7,
            SampleRateSource::Env,
        ),
        (
            "u32 max",
            Some("4294967295"),
            u32::MAX,
            SampleRateSource::Env,
        ),
    ];
    for (row, value, sample_one_in, source) in rows {
        let env = env_with(SAMPLE_ONE_IN_ENV, value);

        let resolved = resolve_wire_check_params(&env);

        let expected = ResolvedWireCheckParams {
            sample_one_in: std::num::NonZeroU32::new(sample_one_in).expect("non-zero row"),
            source,
        };
        assert_eq!(resolved, Ok(expected), "row `{row}`");
    }
}

#[test]
fn a_set_value_that_is_not_a_positive_u32_is_refused() {
    let rows: [(&str, &'static str); 7] = [
        ("zero", "0"),
        ("negative", "-1"),
        ("not a number", "abc"),
        ("empty", ""),
        ("blank", "   "),
        ("past u32", "4294967296"),
        ("fractional", "1.5"),
    ];
    for (row, value) in rows {
        let env = env_with(SAMPLE_ONE_IN_ENV, Some(value));

        let resolved = resolve_wire_check_params(&env);

        assert_eq!(resolved, Err(SampleRateEnvError), "row `{row}`");
    }
}

#[test]
fn the_refusal_names_the_variable_and_never_the_value() {
    let value = "8675309-SENTINEL";
    let env = env_with(SAMPLE_ONE_IN_ENV, Some(value));
    assert_eq!(
        env(SAMPLE_ONE_IN_ENV).as_deref(),
        Some(value),
        "control: the resolver is handed the sentinel under the name it reads"
    );

    let message = resolve_wire_check_params(&env)
        .expect_err("a non-numeric value is refused")
        .to_string();

    assert!(
        message.contains(SAMPLE_ONE_IN_ENV),
        "names the variable: {message}"
    );
    assert!(
        !message.contains(value),
        "never echoes the value: {message}"
    );
}

#[test]
fn another_variable_leaves_the_default() {
    let env = env_with("ROUTECTL_TRACE_BODY_BYTES", Some("1"));

    let resolved = resolve_wire_check_params(&env);

    assert_eq!(
        resolved.map(|r| r.source),
        Ok(SampleRateSource::Default),
        "only the named variable sets the rate"
    );
}
