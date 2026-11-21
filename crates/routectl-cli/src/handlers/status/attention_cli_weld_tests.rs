//! The CLI attention reader welded to the daemon's own serialization: the
//! populated attention panel, serialized as the route serializes it, reads
//! back through the CLI's lenient shape with nothing lost.

use serde_json::Value;

use super::super::attention::fixture::{
    populated_attention, populated_attention_doctor_unavailable,
};
use crate::commands::doctor::daemon_attention::{SCHEMA_VERSION, parse};

fn served_keys(envelope: &Value) -> Vec<String> {
    envelope["data"]["items"]
        .as_array()
        .expect("served items")
        .iter()
        .map(|item| item["key"].as_str().expect("an item key").to_string())
        .collect()
}

#[test]
fn the_populated_panel_reads_back_through_the_cli_reader() {
    for (name, envelope) in [
        ("doctor current", populated_attention()),
        (
            "doctor unavailable",
            populated_attention_doctor_unavailable(),
        ),
    ] {
        // Arrange
        let served = served_keys(&envelope);
        assert!(served.len() >= 10, "{name}: the fixture lists {served:?}");
        let body = serde_json::to_vec(&envelope).expect("serializes");

        // Act
        let parsed = parse(&body).unwrap_or_else(|e| panic!("{name}: {e:?}"));

        // Assert
        let read: Vec<&str> = parsed.list.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(read, served, "{name}");
        let counts = &envelope["data"]["counts"];
        let read_counts = parsed.list.counts;
        for (field, value) in [
            ("broken", read_counts.broken),
            ("degraded", read_counts.degraded),
            ("recovered", read_counts.recovered),
            ("doctor_fail", read_counts.doctor_fail),
        ] {
            assert_eq!(counts[field].as_u64(), Some(value), "{name}: {field}");
        }
        assert_eq!(
            Some(parsed.list.verdict.text.as_str()),
            envelope["data"]["verdict"]["text"].as_str(),
            "{name}"
        );
        assert_eq!(
            Some(parsed.list.coverage.doctor.as_str()),
            envelope["data"]["coverage"]["doctor"].as_str(),
            "{name}"
        );
        assert_eq!(parsed.schema_version, SCHEMA_VERSION, "{name}");
        assert_eq!(parsed.data, envelope["data"], "{name}");
    }
}
