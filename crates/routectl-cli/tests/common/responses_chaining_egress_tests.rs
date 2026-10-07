//! Chaining preserves the actual upstream input[] order, not merely the
//! canonical messages. Both rendering paths must save the passthrough carrier.
use super::*;

async fn chained_turn(base: &str, stream: bool, previous: Option<&str>, input: Value) -> String {
    let mut body = json!({"model":"tool-chain", "stream":stream, "input":input});
    if previous.is_none() {
        body["instructions"] = json!("top-level-only");
    }
    if let Some(previous) = previous {
        body["previous_response_id"] = json!(previous);
    }
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
        .post(format!("{base}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(
        status, 200,
        "stream={stream}, previous={previous:?}: {text}"
    );
    let wire: Value = if stream {
        text.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find(|value| value["type"] == "response.completed")
            .expect("stream must complete")["response"]
            .clone()
    } else {
        serde_json::from_str(&text).unwrap()
    };
    assert_eq!(wire["status"], "completed", "{wire}");
    wire["id"].as_str().expect("stored response id").to_string()
}

fn message(text: &str) -> Value {
    json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]})
}

async fn healthy_chaining_upstream(streaming: bool) -> (MockServer, String) {
    let (upstream, base) = spawn_responses_upstream().await;
    if streaming {
        // The completion double has terminal output only; stream() also needs
        // actual delta events to establish visible content before completion.
        upstream.reset().await;
        let prefix = [
            json!({"type":"response.created", "response":{"id":"resp_01", "model":"mock-model"}}),
            json!({"type":"response.output_item.added", "output_index":0, "item":{"type":"message", "id":"msg_1", "role":"assistant", "content":[]}}),
            json!({"type":"response.output_text.delta", "output_index":0, "content_index":0, "delta":"ok"}),
            json!({"type":"response.output_item.done", "output_index":0, "item":{"type":"message", "id":"msg_1", "role":"assistant", "content":[{"type":"output_text", "text":"ok"}]}}),
        ].into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(prefix + &responses_completed_sse()),
            )
            .mount(&upstream)
            .await;
    }
    (upstream, base)
}

#[tokio::test]
async fn native_upstream_output_is_a_top_level_item_and_replays_on_both_render_paths() {
    for streaming in [false, true] {
        let (upstream, base) = spawn_responses_upstream().await;
        upstream.reset().await;
        let native = json!({"type":"local_shell_call", "id":"native_1",
            "action":{"type":"exec", "command":["true"]}});
        let native_message = json!({"type":"message", "id":"msg_native", "role":"assistant",
            "content":[{"type":"output_text", "text":"ok", "annotations":[]}]});
        let wire = [
            json!({"type":"response.created", "response":{"id":"resp_native","model":"mock-model"}}),
            json!({"type":"response.output_item.added", "output_index":0, "item":native}),
            json!({"type":"response.output_item.done", "output_index":0, "item":native}),
            json!({"type":"response.output_item.added", "output_index":1,
                "item":{"type":"message","id":"msg_native","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta", "output_index":1, "content_index":0,"delta":"ok"}),
            json!({"type":"response.output_item.done", "output_index":1, "item":native_message}),
            json!({"type":"response.completed", "response":{"id":"resp_native", "model":"mock-model",
                "status":"completed", "output":[native, native_message], "usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}),
        ].into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(wire),
            )
            .mount(&upstream)
            .await;
        let first = chained_turn(&base, streaming, None, json!([message("one")])).await;
        let stored: Value = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{base}/v1/responses/{first}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stored["output"][0], native, "stream={streaming}");
        assert_eq!(stored["output"][1]["type"], "message");
        chained_turn(&base, streaming, Some(&first), json!([message("two")])).await;
        let received = upstream.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[1].body).unwrap();
        assert_eq!(
            body["input"][1], native,
            "native output must replay verbatim at the historical boundary"
        );
        assert_eq!(body["input"][2]["content"][0]["text"], "ok");
        assert_eq!(body["input"][3]["content"][0]["text"], "two");
    }
}

#[tokio::test]
async fn native_only_output_is_visible_stored_and_replayable_on_both_render_paths() {
    for streaming in [false, true] {
        let (upstream, base) = spawn_responses_upstream().await;
        upstream.reset().await;
        let native = json!({"type":"local_shell_call", "id":"native_only",
            "action":{"type":"exec", "command":["true"]}});
        let wire = [
            json!({"type":"response.created", "response":{"id":"resp_native_only", "model":"mock-model"}}),
            json!({"type":"response.output_item.added", "output_index":0, "item":native}),
            json!({"type":"response.output_item.done", "output_index":0, "item":native}),
            json!({"type":"response.completed", "response":{"id":"resp_native_only", "model":"mock-model",
                "status":"completed", "output":[native], "usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}),
        ].into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(wire),
            )
            .mount(&upstream)
            .await;
        let first = chained_turn(&base, streaming, None, json!([message("one")])).await;
        let stored: Value = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{base}/v1/responses/{first}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(stored["output"], json!([native]), "stream={streaming}");
        chained_turn(&base, streaming, Some(&first), json!([message("two")])).await;
        let received = upstream.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[1].body).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3, "{body}");
        assert_eq!(input[1], native);
        assert_eq!(input[2]["content"][0]["text"], "two");
    }
}

#[tokio::test]
async fn chaining_keeps_system_input_history_but_not_prior_top_level_instructions() {
    for streaming in [false, true] {
        let (upstream, base) = healthy_chaining_upstream(streaming).await;
        let first = chained_turn(
            &base,
            streaming,
            None,
            json!([
                {"role":"system", "content":"persistent system"},
                {"role":"developer", "content":"persistent developer"},
                message("one")
            ]),
        )
        .await;
        chained_turn(&base, streaming, Some(&first), json!([message("two")])).await;
        let received = upstream.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[1].body).unwrap();
        assert_eq!(
            body["instructions"],
            "persistent system\npersistent developer"
        );
        assert!(
            !body["instructions"]
                .as_str()
                .unwrap()
                .contains("top-level-only")
        );
    }
}

#[tokio::test]
async fn passthrough_barriers_survive_modeled_content_drops_and_history_rebasing() {
    for streaming in [false, true] {
        let (upstream, base) = healthy_chaining_upstream(streaming).await;
        let barrier = json!({"type":"future_native_item", "id":"barrier"});
        let first = chained_turn(
            &base,
            streaming,
            None,
            json!([
                {"type":"reasoning", "id":"rs_empty", "summary":[]},
                {"role":"assistant", "content":[{"type":"unsupported_content"}]},
                barrier,
                message("one")
            ]),
        )
        .await;
        let next = json!({"type":"future_native_item", "id":"next"});
        chained_turn(
            &base,
            streaming,
            Some(&first),
            json!([
                {"role":"assistant", "content":[]}, next, message("two")
            ]),
        )
        .await;
        let received = upstream.received_requests().await.unwrap();
        for request in &received {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let input = body["input"].as_array().unwrap();
            assert_eq!(input[0], barrier);
            assert_eq!(input[1]["content"][0]["text"], "one");
        }
        let body: Value = serde_json::from_slice(&received[1].body).unwrap();
        let input = body["input"].as_array().unwrap();
        let index = input.iter().position(|item| item == &next).unwrap();
        assert_eq!(input[index + 1]["content"][0]["text"], "two");
    }
}

#[tokio::test]
async fn chaining_replays_passthrough_and_rebases_fresh_positions_on_both_render_paths() {
    for streaming in [false, true] {
        let (upstream, base) = healthy_chaining_upstream(streaming).await;
        let shell = json!({"type":"local_shell_call", "id":"shell_1", "action":{"type":"exec", "command":["true"]}});
        let future =
            json!({"type":"future_native_item", "id":"future_1", "payload":{"opaque":[1,2]}});
        let output =
            json!({"type":"custom_tool_call_output", "call_id":"shell_1", "output":"done"});
        let search = json!({"type":"tool_search_call", "id":"search_1", "arguments":"{}"});
        let boundary = json!({"type":"future_native_item", "id":"future_2"});
        let call_boundary = json!({"type":"future_native_item", "id":"future_3"});
        let first = chained_turn(
            &base,
            streaming,
            None,
            json!([message("one"), shell, message("two"), future]),
        )
        .await;
        let second = chained_turn(
            &base,
            streaming,
            Some(&first),
            json!([output, message("three"), search]),
        )
        .await;
        let _third = chained_turn(&base, streaming, Some(&second), json!([
            {"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque-replay", "summary":[]},
            boundary,
            {"type":"function_call", "call_id":"call_1", "name":"shell", "arguments":"{}"},
            call_boundary,
            {"type":"function_call_output", "call_id":"call_1", "output":"done"},
            message("four")
        ])).await;
        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 3);
        let bodies: Vec<Value> = received
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect();
        let types = |body: &Value| -> Vec<String> {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["type"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            types(&bodies[0]),
            [
                "message",
                "local_shell_call",
                "message",
                "future_native_item"
            ]
        );
        assert_eq!(
            types(&bodies[1]),
            [
                "message",
                "local_shell_call",
                "message",
                "future_native_item",
                "message",
                "custom_tool_call_output",
                "message",
                "tool_search_call"
            ]
        );
        assert_eq!(
            types(&bodies[2]),
            [
                "message",
                "local_shell_call",
                "message",
                "future_native_item",
                "message",
                "custom_tool_call_output",
                "message",
                "tool_search_call",
                "message",
                "reasoning",
                "future_native_item",
                "function_call",
                "future_native_item",
                "function_call_output",
                "message"
            ]
        );
        let input = bodies[2]["input"].as_array().unwrap();
        for (index, expected) in [
            (1, shell),
            (3, future),
            (5, output),
            (7, search),
            (10, boundary),
            (12, call_boundary),
        ] {
            assert_eq!(input[index], expected, "stream={streaming}, index={index}");
        }
        assert_eq!(input[9]["encrypted_content"], "opaque-replay");
        assert_eq!(input[11]["call_id"], "call_1");
        let text: Vec<&str> = input
            .iter()
            .filter(|item| item["type"] == "message")
            .map(|item| item["content"][0]["text"].as_str().unwrap())
            .collect();
        assert_eq!(text, ["one", "two", "ok", "three", "ok", "four"]);
    }
}
