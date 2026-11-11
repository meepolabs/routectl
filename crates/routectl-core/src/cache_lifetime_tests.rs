use serde_json::json;

use super::*;

#[test]
fn member_names_with_one_duration_parse_to_seconds() {
    let rows = [
        ("anthropic 5m", "ephemeral_5m_input_tokens", 300),
        ("anthropic 1h", "ephemeral_1h_input_tokens", 3600),
        ("ttl minutes", "ttl_30m_tokens", 1800),
        ("cache write hours", "cache_write_12h_tokens", 43_200),
        ("seconds unit", "ttl_45s_tokens", 45),
        ("days unit", "ttl_1d_tokens", 86_400),
        ("zero padded", "ttl_05m_tokens", 300),
    ];
    for (row, name, seconds) in rows {
        assert_eq!(parse_cache_lifetime_member(name), Some(seconds), "{row}");
    }
}

#[test]
fn bare_durations_parse_to_seconds() {
    let rows = [
        ("5m", 300),
        ("24h", 86_400),
        ("05m", 300),
        ("0000000000000000000005m", 300),
    ];
    for (duration, seconds) in rows {
        assert_eq!(parse_cache_lifetime(duration), Some(seconds), "{duration}");
    }
}

#[test]
fn member_names_outside_the_grammar_are_refused() {
    let rows = [
        ("no duration", "input_tokens"),
        ("read side cached", "cached_tokens"),
        ("read side cache_read", "cache_read_5m_tokens"),
        ("read side cache_reads", "cache_reads_5m_tokens"),
        ("read side cached with duration", "cached_5m_tokens"),
        ("read side trailing read", "ttl_5m_read_tokens"),
        ("read side joined cacheread", "cacheread_5m_tokens"),
        ("read side uppercase", "Cache_Read_5m_tokens"),
        ("write total", "cache_creation_input_tokens"),
        ("two durations", "ttl_5m_1h_tokens"),
        ("zero", "ttl_0m_tokens"),
        ("padded zero", "ttl_00m_tokens"),
        ("signed", "ttl_-5m_tokens"),
        ("fraction", "ttl_1.5h_tokens"),
        ("uppercase unit", "ttl_5M_tokens"),
        ("overflows u32 seconds", "ttl_50000d_tokens"),
        ("no tokens suffix", "ephemeral_5m_input"),
    ];
    for (row, name) in rows {
        assert_eq!(parse_cache_lifetime_member(name), None, "{row}");
    }
}

#[test]
fn bare_durations_outside_the_grammar_are_refused() {
    let rows = [
        ("zero", "0m"),
        ("padded zero", "00m"),
        ("no unit", "5"),
        ("no digits", "m"),
        ("empty", ""),
        ("overflows u32 seconds", "4294967296s"),
        ("days overflow", "50000d"),
    ];
    for (row, duration) in rows {
        assert_eq!(parse_cache_lifetime(duration), None, "{row}");
    }
}

#[test]
fn refused_members_and_non_integer_values_stay_out_of_the_split() {
    let rows = [
        ("refused name", json!({"ttl_5m_1h_tokens": 9})),
        ("fractional value", json!({"ttl_30m_tokens": 2.5})),
        ("string value", json!({"ttl_30m_tokens": "12"})),
        ("negative value", json!({"ttl_30m_tokens": -1})),
    ];
    for (row, mut value) in rows {
        value["ephemeral_5m_input_tokens"] = json!(7);
        let creation: CacheCreation = serde_json::from_value(value).expect(row);
        assert_eq!(creation.iter().collect::<Vec<_>>(), vec![(300, 7)], "{row}");
    }
}

#[test]
fn refused_member_emits_one_debug_line() {
    let mut creation = None;
    let events = routectl_testkit::capture_events(|| {
        creation = Some(
            serde_json::from_value::<CacheCreation>(
                json!({"ephemeral_5m_input_tokens": 1, "ttl_5m_1h_tokens": 9}),
            )
            .expect("parses"),
        );
    });

    assert_eq!(creation.expect("ran").get(300), Some(1));
    let refused: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains("cache write member refused"))
        .collect();
    assert_eq!(refused.len(), 1, "{events:?}");
    assert_eq!(refused[0].level, tracing::Level::DEBUG);
    assert_eq!(refused[0].field("member"), Some("ttl_5m_1h_tokens"));
}

#[test]
fn seventeenth_distinct_lifetime_is_dropped_with_one_debug_line() {
    // Built as text: a serde_json::Map would reorder the members.
    let members: Vec<String> = (1..=17)
        .map(|minutes| format!("\"ttl_{minutes}m_tokens\":{minutes}"))
        .collect();
    let wire = format!("{{{}}}", members.join(","));
    let mut creation = None;
    let events = routectl_testkit::capture_events(|| {
        creation = Some(serde_json::from_str::<CacheCreation>(&wire).expect("parses"));
    });

    let creation = creation.expect("ran");
    assert_eq!(creation.len(), MAX_CACHE_LIFETIME_ENTRIES);
    assert_eq!(creation.get(16 * 60), Some(16));
    assert_eq!(creation.get(17 * 60), None);
    assert_eq!(creation.total(), (1..=16).sum::<u64>());
    let refused: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains("cache write member refused"))
        .collect();
    assert_eq!(refused.len(), 1, "{events:?}");
    assert_eq!(refused[0].field("reason"), Some("lifetime_cap"));
}

#[test]
fn anthropic_wire_round_trips_with_zero_entry_kept() {
    let wire = json!({"ephemeral_5m_input_tokens": 2540, "ephemeral_1h_input_tokens": 0});

    let creation: CacheCreation = serde_json::from_value(wire.clone()).expect("parses");

    assert_eq!(
        creation,
        CacheCreation::from_lifetimes([(300, 2540), (3600, 0)])
    );
    assert_eq!(serde_json::to_value(&creation).expect("renders"), wire);
}

#[test]
fn zero_second_lifetime_is_dropped_so_construction_round_trips() {
    let creation = CacheCreation::from_lifetimes([(0, 9), (300, 0)]);

    let wire = serde_json::to_value(&creation).expect("renders");

    assert_eq!(wire, json!({"ephemeral_5m_input_tokens": 0}));
    assert_eq!(
        serde_json::from_value::<CacheCreation>(wire).expect("parses"),
        creation
    );
}

#[test]
fn lifetimes_render_in_their_largest_exact_unit() {
    let rows = [
        (1800, "ephemeral_30m_input_tokens"),
        (5400, "ephemeral_90m_input_tokens"),
        (7200, "ephemeral_2h_input_tokens"),
        (86_400, "ephemeral_1d_input_tokens"),
        (45, "ephemeral_45s_input_tokens"),
    ];
    for (seconds, member) in rows {
        let rendered =
            serde_json::to_value(CacheCreation::from_lifetimes([(seconds, 1)])).expect("renders");
        assert_eq!(rendered, json!({ member: 1 }), "{seconds}");
    }
}

#[test]
fn repeated_lifetime_sums_with_saturation() {
    let creation = CacheCreation::from_lifetimes([(300, u32::MAX), (300, 5), (3600, 2)]);

    assert_eq!(creation.get(300), Some(u32::MAX));
    assert_eq!(creation.total(), u64::from(u32::MAX) + 2);
}
