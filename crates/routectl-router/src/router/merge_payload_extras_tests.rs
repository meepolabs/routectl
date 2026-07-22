use super::*;
use serde_json::json;

fn req() -> ChatRequest {
    ChatRequest {
        model: "any".into(),
        ..Default::default()
    }
}

#[test]
fn empty_both_is_noop() {
    let mut r = req();
    merge_payload_extras("p", None, None, &mut r);
    assert!(r.provider_extras.is_none());
}

#[test]
fn provider_only_lands_on_req() {
    let mut r = req();
    let p = json!({"top_k": 5, "metadata": {"x": 1}});
    merge_payload_extras("p", Some(&p), None, &mut r);
    let v = r.provider_extras.expect("set");
    assert_eq!(v["top_k"], json!(5));
    assert_eq!(v["metadata"]["x"], json!(1));
}

#[test]
fn deep_merge_objects_recursively() {
    let mut r = req();
    let p = json!({"a": {"shared": 1, "p_only": "p"}});
    let m = json!({"a": {"shared": 2, "m_only": "m"}});
    merge_payload_extras("p", Some(&p), Some(&m), &mut r);
    let v = r.provider_extras.expect("set");
    // Nested objects merge recursively.
    assert_eq!(v["a"]["shared"], json!(2), "model wins on leaf collision");
    assert_eq!(v["a"]["p_only"], json!("p"));
    assert_eq!(v["a"]["m_only"], json!("m"));
}

#[test]
fn scalar_collision_model_wins() {
    let mut r = req();
    let p = json!({"k": "provider"});
    let m = json!({"k": "model"});
    merge_payload_extras("p", Some(&p), Some(&m), &mut r);
    let v = r.provider_extras.expect("set");
    assert_eq!(v["k"], json!("model"));
}

#[test]
fn array_collision_model_wins() {
    let mut r = req();
    let p = json!({"k": [1, 2]});
    let m = json!({"k": [3]});
    merge_payload_extras("p", Some(&p), Some(&m), &mut r);
    let v = r.provider_extras.expect("set");
    assert_eq!(v["k"], json!([3]));
}

#[test]
fn ingress_sweep_preserved_underneath_provider_and_model() {
    // ingress's forward-compat sweep populates req.provider_extras;
    // the merge layers provider + model on top.
    let mut r = req();
    r.provider_extras = Some(json!({"mcp_servers": ["s1"], "k": "ingress"}));
    let p = json!({"k": "provider"});
    let m = json!({"other": true});
    merge_payload_extras("p", Some(&p), Some(&m), &mut r);
    let v = r.provider_extras.expect("set");
    assert_eq!(v["mcp_servers"], json!(["s1"]));
    // Provider overrode the ingress sweep value on `k`.
    assert_eq!(v["k"], json!("provider"));
    assert_eq!(v["other"], json!(true));
}
