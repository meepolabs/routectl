//! `routectl test <target> [--prompt ...]` -- one-shot completion against
//! a configured alias or `provider:model` direct target.

use std::sync::Arc;

use routectl_auth::MemoryStore;
use routectl_core::{
    schema::MessageContent, ChatRequest, Message, Result, Role,
};
use routectl_router::{build_provider, Config, Router};

pub async fn run(config: Config, target: &str, prompt: &str) -> Result<()> {
    let config = Arc::new(config);
    let secrets = MemoryStore::new();
    let mut router = Router::new(config.clone());

    for (name, entry) in &config.providers {
        match build_provider(name, entry, &secrets).await {
            Ok(p) => router.register(name, p),
            Err(e) => {
                tracing::warn!(provider = name, error = ?e, "skipping provider that failed to build");
            }
        }
    }

    let req = ChatRequest {
        model: target.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(prompt.to_string()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(512),
        stop: None,
        stream: Some(false),
        n: None,
        seed: None,
        logprobs: None,
        top_logprobs: None,
        logit_bias: None,
        presence_penalty: None,
        frequency_penalty: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        reasoning: None,
        chat_template_kwargs: None,
        provider_extras: None,
    };

    let resp = router.complete(req).await?;
    print_response(&resp);
    Ok(())
}

fn print_response(resp: &routectl_core::ChatResponse) {
    if let Some(provider) = resp.routectl_provider.as_deref() {
        println!("[provider: {provider}, model: {}]", resp.model);
    }
    let Some(choice) = resp.choices.first() else {
        println!("(no choices in response)");
        return;
    };

    if !choice.message.reasoning_details.is_empty() {
        println!("--- reasoning ---");
        for detail in &choice.message.reasoning_details {
            if let Some(text) = detail.payload.get("text").and_then(|v| v.as_str()) {
                println!("{text}");
            } else if let Some(summary) = detail.payload.get("summary").and_then(|v| v.as_str()) {
                println!("[summary] {summary}");
            } else {
                println!("[{}] (encrypted/redacted)", detail.format.as_deref().unwrap_or("unknown"));
            }
        }
        println!("--- response ---");
    } else if let Some(reasoning) = choice.message.reasoning.as_deref() {
        println!("--- reasoning ---\n{reasoning}\n--- response ---");
    }

    match &choice.message.content {
        MessageContent::Text(t) => println!("{t}"),
        MessageContent::Null => println!("(no content)"),
        MessageContent::Parts(p) => {
            for part in p {
                println!("{part}");
            }
        }
    }

    if let Some(usage) = &resp.usage {
        let reasoning_part = usage
            .reasoning_tokens
            .map(|n| format!(", reasoning: {n}"))
            .unwrap_or_default();
        println!(
            "\n[tokens: prompt {} + completion {} = total {}{reasoning_part}]",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        );
    }
}
