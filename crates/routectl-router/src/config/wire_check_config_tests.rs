use super::{Config, WireCheckConfig};

#[test]
fn an_empty_section_loads_the_defaults() {
    let config: Config = toml::from_str("version = 5\n[wire_check]\n").expect("parse");

    assert_eq!(config.wire_check, WireCheckConfig::default());
    assert_eq!(config.wire_check.first_n, 8);
    assert_eq!(config.wire_check.sample_one_in.get(), 100);
    assert_eq!(config.wire_check.window_secs.get(), 86_400);
}

#[test]
fn an_absent_section_loads_the_defaults() {
    let config: Config = toml::from_str("version = 5\n").expect("parse");

    assert_eq!(config.wire_check, WireCheckConfig::default());
}

#[test]
fn explicit_values_reach_the_fields() {
    let config: Config = toml::from_str(
        "version = 5\n[wire_check]\nfirst_n = 0\nsample_one_in = 1\nwindow_secs = 60\n",
    )
    .expect("parse");

    assert_eq!(config.wire_check.first_n, 0);
    assert_eq!(config.wire_check.sample_one_in.get(), 1);
    assert_eq!(config.wire_check.window_secs.get(), 60);
}

/// Each row is a section body the loader must refuse, with the key the
/// error has to name so the operator can find the mistake.
#[test]
fn invalid_sections_are_refused_naming_the_key() {
    let rows = [
        ("unknown key", "sample_rate = 10", "sample_rate"),
        ("no enabled switch", "enabled = false", "enabled"),
        ("zero sample_one_in", "sample_one_in = 0", "sample_one_in"),
        ("zero window_secs", "window_secs = 0", "window_secs"),
    ];
    for (name, body, key) in rows {
        let err = toml::from_str::<Config>(&format!("version = 5\n[wire_check]\n{body}\n"))
            .expect_err(name);

        assert!(err.to_string().contains(key), "{name}: {err}");
    }
}

#[test]
fn a_router_samples_and_reports_its_window_per_the_section() {
    let config: Config = toml::from_str(
        "version = 5\n[wire_check]\nfirst_n = 3\nsample_one_in = 7\nwindow_secs = 60\n",
    )
    .expect("parse");

    let router = crate::router::Router::new(std::sync::Arc::new(config));

    assert_eq!(
        router.wire_check_params_for_tests(),
        crate::wire_check_state::WireCheckParams {
            first_n: 3,
            sample_one_in: std::num::NonZeroU32::new(7).expect("non-zero"),
            window_secs: std::num::NonZeroU64::new(60).expect("non-zero"),
        }
    );
    assert_eq!(router.wire_check_snapshot().window_secs, 60);
}

#[test]
fn the_section_defaults_are_the_sampler_defaults() {
    let config: Config = toml::from_str("version = 5\n").expect("parse");

    let router = crate::router::Router::new(std::sync::Arc::new(config));

    assert_eq!(
        router.wire_check_params_for_tests(),
        crate::wire_check_state::WireCheckParams::default()
    );
}
