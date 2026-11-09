//! Shared fixture builders for the price-table tests.

use serde_json::{Value, json};

/// One priced model row, so a table built around it is never empty.
pub(super) fn priced_model(id: &str) -> Value {
    json!({ "id": id, "input": 1.0, "output": 2.0 })
}

/// A canonical table with `providers`, dated 2026-11-01.
pub(super) fn table(providers: Value) -> Value {
    json!({
        "schema_version": 1,
        "generated_at": "2026-11-01",
        "providers": providers,
    })
}

/// A canonical table with one provider holding `models`.
pub(super) fn one_provider(models: Value) -> Value {
    table(json!([{ "id": "acme", "models": models }]))
}

pub(super) fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("fixture serializes")
}
