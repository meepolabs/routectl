//! `routectl test <target> [--prompt ...]` -- one-shot completion against
//! a configured alias or `[models.X]` nickname.

use std::sync::Arc;

use routectl_core::{schema::MessageContent, ChatRequest, Error, Message, Result, Role};
use routectl_router::{
    build_resolved_models, validate_alias_chain_targets, validate_bedrock_global_config,
    validate_reasoning_defaults, validate_registry_patterns, validate_retry_policy, BuildOptions,
    Config, Router,
};

use crate::server::CompositeStore;

pub async fn run(config: Config, target: &str, prompt: &str) -> Result<()> {
    let config = Arc::new(config);
    // Same composite resolver as `serve`: oauth:// goes to OAuthStore,
    // env/file/literal to MemoryStore. Lets `routectl test` exercise
    // the same secret-resolution path the server uses.
    let secrets: Arc<dyn routectl_auth::SecretStore> =
        Arc::new(CompositeStore::open_default().await?);
    let mut router = Router::new(config.clone());

    // Surface incoherent `[bedrock]` config (e.g. populated
    // `allowed_body_fields` missing routectl-mandatory keys) here
    // instead of at first-request 400. Empty lists are pass-through
    // and accepted; see `validate_bedrock_global_config`.
    validate_bedrock_global_config(&config)?;

    // Reject empty-string `thinking = ""` on any provider before
    // dispatch, so the operator gets a clean error rather than silently
    // emitting `effort: ""` on the routed request.
    validate_reasoning_defaults(&config)?;

    // Reject `[aliases]` chains pointing at unknown/disabled models.
    // Mirrors the serve-side guard so `routectl test` against a
    // misconfigured alias produces the same precise startup error
    // instead of an UnknownAlias at dispatch time.
    validate_alias_chain_targets(&config)?;

    // Reject `[retry]` blocks that set both `retry_allowlist` and
    // `retry_denylist`. Mirrors the serve-side guard so
    // `routectl test` against a misconfigured retry block surfaces
    // the conflict immediately.
    validate_retry_policy(&config)?;

    // Reject malformed `[registry]` glob keys (embedded/bare asterisks)
    // before query-time cost resolution silently skips them. Mirrors the
    // serve-side guard.
    validate_registry_patterns(&config)?;

    // Same BuildOptions path as `serve` so a `routectl test` run
    // exercises exactly the production translation contract. Without
    // this, `[server] strict_translation = true` was honored by
    // serve but silently ignored by test, masking real ingress
    // misconfigurations from operators using the test command for
    // pre-production validation.
    let opts = BuildOptions::new()
        .with_strict_translation(config.server.strict_translation)
        .with_bedrock_allowed_betas(config.bedrock.allowed_betas.clone())
        .with_bedrock_allowed_body_fields(config.bedrock.allowed_body_fields.clone());

    // v0.6.0: build per-model resolved providers from the `[models]`
    // table once. Failures only become fatal when the requested
    // `target` references a model whose provider couldn't build.
    let (resolved_models, failed) = build_resolved_models(&config, secrets, opts).await?;
    router.install_resolved_models(resolved_models);

    // Mirror serve's "fail loudly when a referenced model can't
    // build" guard so `routectl test heavy` against a broken
    // `[aliases.heavy]` chain produces a precise startup error
    // instead of an `UnknownAlias` at dispatch.
    if !failed.is_empty() {
        let failed_models: std::collections::HashSet<&str> =
            failed.iter().map(|(n, _)| n.as_str()).collect();
        let referenced = if let Some(alias) = config.aliases.get(target) {
            alias.nicknames().any(|nick| failed_models.contains(nick))
        } else {
            failed_models.contains(target)
        };
        if referenced {
            let detail = failed
                .iter()
                .map(|(n, e)| format!("  - {n}: {e}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(Error::Config(format!(
                "target `{target}` references model(s) that failed to build:\n{detail}"
            )));
        }
    }

    let req = ChatRequest {
        model: target.to_string(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(prompt.to_string()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        max_tokens: Some(512),
        stream: Some(false),
        ..Default::default()
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
                println!(
                    "[{}] (encrypted/redacted)",
                    detail.format.as_deref().unwrap_or("unknown")
                );
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
                // Display the JSON wire form. Typed ContentPart variants
                // (text, image, tool_use, ...) serialize to the original
                // Anthropic-shape body verbatim.
                match serde_json::to_string(part) {
                    Ok(s) => println!("{s}"),
                    Err(_) => println!("{part:?}"),
                }
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
