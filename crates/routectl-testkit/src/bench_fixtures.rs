//! Deterministic synthetic request fixtures for the perf benches.
//!
//! Each [`SpectrumProfile`] materializes into a [`BenchFixture`] carrying
//! the canonical [`ChatRequest`] (fed to the clone / egress benches) plus
//! raw wire-request bytes in both ingress dialects (fed to the parse
//! benches). The Anthropic wire bytes are shaped for the Anthropic
//! Messages ingress and the OpenAI wire bytes for the Chat Completions
//! ingress; each dialect's bytes are parseable by its matching adapter.
//!
//! Content is seeded pseudo-random (a hand-rolled `splitmix64`, never a
//! `rand`-family RNG and never any wall-clock or entropy source) so two
//! invocations of the same profile produce byte-identical output. Byte
//! stability is what makes a recorded baseline comparable against a later
//! run; the determinism unit test pins it.

use routectl_core::{
    ChatRequest, Message, MessageContent, Role,
    cache_control::CacheControl,
    content_part::{ContentPart, KnownContentPart},
    system_content::{SystemBlock, SystemContent},
    tool_def::{CustomTool, ToolDef},
};
use serde_json::{Map, Value, json};

/// The catalog of synthetic request shapes the perf benches replay.
///
/// # Stability contract
///
/// These variant names are part of the benchmark baseline history: every
/// recorded baseline and every feature perf report cites a bench name
/// built from the snake-cased variant (see [`SpectrumProfile::snake_name`]).
///
/// - ADDING a variant is backward-compatible: existing baselines keep
///   comparing unchanged and the new profile simply gains its own bench
///   series.
/// - RENAMING or REMOVING a variant breaks baseline continuity: the old
///   bench name vanishes and its historical numbers can no longer be
///   compared against. Do not rename or remove -- deprecate in place
///   (leave the variant, stop recording fresh baselines for it) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpectrumProfile {
    /// Many large tool schemas plus a tool-call round trip; cache markers
    /// on the system prefix and the last tool.
    ToolHeavy,
    /// A single user turn carrying a multi-megabyte base64 image block
    /// with a cache marker.
    LargeImage,
    /// A short plain-text chat with no tools, images, or cache markers.
    PlainRoundTrip,
    /// A tool-and-system-bearing request deliberately free of any
    /// `cache_control` marker (the "scanner finds nothing" shape).
    NoMarker,
    /// A request shaped for a provider that cannot cache: system and
    /// tools present, but no cache markers to place.
    CacheLess,
    /// A long multi-turn session of tool-call round trips with a cache
    /// marker on the system prefix.
    LongSession,
}

impl SpectrumProfile {
    /// Every profile, in a stable order, for benches that fan out over
    /// the whole catalog.
    pub const ALL: [Self; 6] = [
        Self::ToolHeavy,
        Self::LargeImage,
        Self::PlainRoundTrip,
        Self::NoMarker,
        Self::CacheLess,
        Self::LongSession,
    ];

    /// The snake-cased profile token used to build a stable bench name
    /// (`<stage>__<profile>__<dialect>`). Part of the baseline-name
    /// contract documented on the enum.
    pub const fn snake_name(self) -> &'static str {
        match self {
            Self::ToolHeavy => "tool_heavy",
            Self::LargeImage => "large_image",
            Self::PlainRoundTrip => "plain_round_trip",
            Self::NoMarker => "no_marker",
            Self::CacheLess => "cache_less",
            Self::LongSession => "long_session",
        }
    }

    /// Per-profile seed salt so distinct profiles draw distinct content
    /// while each stays individually deterministic.
    const fn seed_salt(self) -> u64 {
        match self {
            Self::ToolHeavy => 0x1111_1111_1111_1111,
            Self::LargeImage => 0x2222_2222_2222_2222,
            Self::PlainRoundTrip => 0x3333_3333_3333_3333,
            Self::NoMarker => 0x4444_4444_4444_4444,
            Self::CacheLess => 0x5555_5555_5555_5555,
            Self::LongSession => 0x6666_6666_6666_6666,
        }
    }

    /// Materialize this profile into its canonical request and both
    /// dialect wire encodings. Byte-stable across calls.
    ///
    /// Generate once per profile BEFORE entering a timed bench closure --
    /// calling this inside `b.iter` measures fixture construction (for
    /// `LargeImage`, multi-MB base64 assembly), not the benched stage.
    pub fn generate(self) -> BenchFixture {
        let mut rng = SplitMix64::new(FIXTURE_SEED ^ self.seed_salt());
        match self {
            Self::ToolHeavy => build_tool_heavy(&mut rng),
            Self::LargeImage => build_large_image(&mut rng),
            Self::PlainRoundTrip => build_plain_round_trip(&mut rng),
            Self::NoMarker => build_no_marker(&mut rng),
            Self::CacheLess => build_cache_less(&mut rng),
            Self::LongSession => build_long_session(&mut rng),
        }
    }
}

/// One profile's generated artifacts: the canonical request plus the raw
/// wire bytes for each ingress dialect.
#[derive(Debug, Clone)]
pub struct BenchFixture {
    /// The canonical (OpenRouter-normalized) request for this profile.
    pub canonical: ChatRequest,
    /// Anthropic Messages wire body, parseable by the Anthropic ingress.
    pub anthropic_wire: Vec<u8>,
    /// OpenAI Chat Completions wire body, parseable by the OpenAI ingress.
    pub openai_wire: Vec<u8>,
}

/// Fixed generator seed. Combined with each profile's salt; never derived
/// from the clock or any entropy source, so output is reproducible.
const FIXTURE_SEED: u64 = 0x0D15_EA5E_B1D5_EED0;

/// Base64 character count of the `LargeImage` payload (2 MiB).
const LARGE_IMAGE_B64_LEN: usize = 2 * 1024 * 1024;

/// Tool-schema property count for the `ToolHeavy` profile.
const TOOL_HEAVY_SCHEMA_PROPS: usize = 24;
/// Number of tools the `ToolHeavy` profile carries.
const TOOL_HEAVY_TOOL_COUNT: usize = 4;

/// Tool-call round trips in the `LongSession` profile.
const LONG_SESSION_ROUNDS: usize = 12;

/// SplitMix64: a tiny, well-distributed PRNG. Deterministic given a seed;
/// no external state, so it cannot pull in clock or entropy noise.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

const WORD_BANK: &[&str] = &[
    "model",
    "request",
    "token",
    "cache",
    "system",
    "prompt",
    "tool",
    "result",
    "content",
    "message",
    "assistant",
    "context",
    "window",
    "stream",
    "reason",
    "effort",
    "budget",
    "schema",
    "input",
    "output",
];

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A deterministic space-separated pseudo-sentence of `words` words.
fn pseudo_sentence(rng: &mut SplitMix64, words: usize) -> String {
    let mut out = String::new();
    for i in 0..words {
        if i > 0 {
            out.push(' ');
        }
        let idx = (rng.next_u64() as usize) % WORD_BANK.len();
        out.push_str(WORD_BANK[idx]);
    }
    out
}

/// A deterministic base64-alphabet string of exactly `len` characters.
/// Synthetic by construction -- it need not decode to a real image, only
/// look like base64 of the requested size.
fn pseudo_base64(rng: &mut SplitMix64, len: usize) -> String {
    let mut out = String::with_capacity(len);
    while out.len() < len {
        let mut bits = rng.next_u64();
        for _ in 0..10 {
            if out.len() >= len {
                break;
            }
            out.push(B64_ALPHABET[(bits & 0x3F) as usize] as char);
            bits >>= 6;
        }
    }
    out
}

/// A JSON-Schema `object` with `prop_count` string properties, a third of
/// them required. Deterministic property descriptions.
fn big_tool_schema(rng: &mut SplitMix64, prop_count: usize) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for i in 0..prop_count {
        let name = format!("field_{i:03}");
        props.insert(
            name.clone(),
            json!({"type": "string", "description": pseudo_sentence(rng, 8)}),
        );
        if i % 3 == 0 {
            required.push(Value::String(name));
        }
    }
    json!({"type": "object", "properties": Value::Object(props), "required": required})
}

/// Intermediate tool description shared across the three representations.
struct ToolSpec {
    name: String,
    description: String,
    schema: Value,
    cache: bool,
}

fn tool_to_custom(spec: &ToolSpec) -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: spec.name.clone(),
        description: Some(spec.description.clone()),
        input_schema: spec.schema.clone(),
        cache_control: spec.cache.then(CacheControl::ephemeral_5m),
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

fn tool_to_anthropic_wire(spec: &ToolSpec) -> Value {
    let mut obj = json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.schema,
    });
    if spec.cache {
        obj["cache_control"] = json!({"type": "ephemeral", "ttl": "5m"});
    }
    obj
}

fn tool_to_openai_wire(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.schema,
        },
    })
}

const fn msg(role: Role, content: MessageContent) -> Message {
    Message {
        role,
        content,
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

const fn user_text(text: String) -> Message {
    msg(Role::User, MessageContent::Text(text))
}

const fn assistant_text(text: String) -> Message {
    msg(Role::Assistant, MessageContent::Text(text))
}

fn assistant_tool_use(id: String, name: String, input: Value) -> Message {
    msg(
        Role::Assistant,
        MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
            id,
            name,
            input,
            cache_control: None,
        })]),
    )
}

const fn tool_result_msg(id: String, text: String) -> Message {
    Message {
        role: Role::Tool,
        content: MessageContent::Text(text),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: Some(id),
        tool_calls: None,
        refusal: None,
    }
}

fn cached_system_blocks(text: String) -> SystemContent {
    SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text,
        cache_control: Some(CacheControl::ephemeral_5m()),
        citations: None,
    }])
}

fn fixture(canonical: ChatRequest, anthropic: &Value, openai: &Value) -> BenchFixture {
    BenchFixture {
        canonical,
        anthropic_wire: serde_json::to_vec(anthropic)
            .expect("bench fixture: anthropic wire serializes"),
        openai_wire: serde_json::to_vec(openai).expect("bench fixture: openai wire serializes"),
    }
}

/// JSON-encoded OpenAI tool-call `arguments` string (the wire form stores
/// the args object as a stringified JSON blob).
fn openai_arguments(input: &Value) -> String {
    serde_json::to_string(input).expect("bench fixture: tool input serializes")
}

fn build_plain_round_trip(rng: &mut SplitMix64) -> BenchFixture {
    let system = pseudo_sentence(rng, 10);
    let u1 = pseudo_sentence(rng, 12);
    let a1 = pseudo_sentence(rng, 14);
    let u2 = pseudo_sentence(rng, 8);

    let canonical = ChatRequest {
        model: "claude-3-5-sonnet".into(),
        messages: vec![
            user_text(u1.clone()),
            assistant_text(a1.clone()),
            user_text(u2.clone()),
        ]
        .into(),
        system: Some(SystemContent::Text(system.clone())),
        max_tokens: Some(1024),
        ..Default::default()
    };

    let anthropic = json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 1024,
        "system": system,
        "messages": [
            {"role": "user", "content": u1},
            {"role": "assistant", "content": a1},
            {"role": "user", "content": u2},
        ],
    });

    let openai = json!({
        "model": "gpt-4o",
        "max_tokens": 1024,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": u1},
            {"role": "assistant", "content": a1},
            {"role": "user", "content": u2},
        ],
    });

    fixture(canonical, &anthropic, &openai)
}

fn build_tool_heavy(rng: &mut SplitMix64) -> BenchFixture {
    let tools: Vec<ToolSpec> = (0..TOOL_HEAVY_TOOL_COUNT)
        .map(|i| ToolSpec {
            name: format!("tool_{i:02}"),
            description: pseudo_sentence(rng, 10),
            schema: big_tool_schema(rng, TOOL_HEAVY_SCHEMA_PROPS),
            cache: i + 1 == TOOL_HEAVY_TOOL_COUNT,
        })
        .collect();

    let system = pseudo_sentence(rng, 40);
    let question = pseudo_sentence(rng, 12);
    let tool_input = json!({"field_000": pseudo_sentence(rng, 4)});
    let tool_output = pseudo_sentence(rng, 20);
    let answer = pseudo_sentence(rng, 16);
    let call_id = "toolu_th_01";
    let tool_name = tools[0].name.clone();

    let canonical = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![
            user_text(question.clone()),
            assistant_tool_use(call_id.into(), tool_name.clone(), tool_input.clone()),
            tool_result_msg(call_id.into(), tool_output.clone()),
            assistant_text(answer.clone()),
        ]
        .into(),
        system: Some(cached_system_blocks(system.clone())),
        tools: Some(tools.iter().map(tool_to_custom).collect()),
        max_tokens: Some(2048),
        ..Default::default()
    };

    let anthropic = json!({
        "model": "claude-opus-4-7",
        "max_tokens": 2048,
        "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral", "ttl": "5m"}}],
        "tools": tools.iter().map(tool_to_anthropic_wire).collect::<Vec<_>>(),
        "messages": [
            {"role": "user", "content": question},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": call_id, "name": tool_name, "input": tool_input}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": call_id, "content": tool_output}
            ]},
            {"role": "assistant", "content": answer},
        ],
    });

    let openai = json!({
        "model": "gpt-4o",
        "max_tokens": 2048,
        "tools": tools.iter().map(tool_to_openai_wire).collect::<Vec<_>>(),
        "tool_choice": "auto",
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": question},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": call_id, "type": "function", "function": {"name": tool_name, "arguments": openai_arguments(&tool_input)}}
            ]},
            {"role": "tool", "tool_call_id": call_id, "content": tool_output},
            {"role": "assistant", "content": answer},
        ],
    });

    fixture(canonical, &anthropic, &openai)
}

fn build_large_image(rng: &mut SplitMix64) -> BenchFixture {
    let caption = pseudo_sentence(rng, 12);
    let data = pseudo_base64(rng, LARGE_IMAGE_B64_LEN);
    let data_url = format!("data:image/png;base64,{data}");
    let source = json!({"type": "base64", "media_type": "image/png", "data": data});

    let canonical = ChatRequest {
        model: "claude-3-5-sonnet".into(),
        messages: vec![msg(
            Role::User,
            MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: caption.clone(),
                    citations: None,
                    cache_control: None,
                }),
                ContentPart::Known(KnownContentPart::Image {
                    source: source.clone(),
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
            ]),
        )]
        .into(),
        max_tokens: Some(1024),
        ..Default::default()
    };

    let anthropic = json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": caption},
            {"type": "image", "source": source, "cache_control": {"type": "ephemeral", "ttl": "5m"}},
        ]}],
    });

    let openai = json!({
        "model": "gpt-4o",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": caption},
            {"type": "image_url", "image_url": {"url": data_url}},
        ]}],
    });

    fixture(canonical, &anthropic, &openai)
}

fn build_no_marker(rng: &mut SplitMix64) -> BenchFixture {
    let tool = ToolSpec {
        name: "lookup".into(),
        description: pseudo_sentence(rng, 8),
        schema: big_tool_schema(rng, 6),
        cache: false,
    };
    let system = pseudo_sentence(rng, 20);
    let u1 = pseudo_sentence(rng, 14);
    let a1 = pseudo_sentence(rng, 10);
    let u2 = pseudo_sentence(rng, 6);

    let canonical = ChatRequest {
        model: "claude-3-5-sonnet".into(),
        messages: vec![
            user_text(u1.clone()),
            assistant_text(a1.clone()),
            user_text(u2.clone()),
        ]
        .into(),
        system: Some(SystemContent::Text(system.clone())),
        tools: Some(vec![tool_to_custom(&tool)]),
        max_tokens: Some(1024),
        ..Default::default()
    };

    let anthropic = json!({
        "model": "claude-3-5-sonnet",
        "max_tokens": 1024,
        "system": system,
        "tools": [tool_to_anthropic_wire(&tool)],
        "messages": [
            {"role": "user", "content": u1},
            {"role": "assistant", "content": a1},
            {"role": "user", "content": u2},
        ],
    });

    let openai = json!({
        "model": "gpt-4o",
        "max_tokens": 1024,
        "tools": [tool_to_openai_wire(&tool)],
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": u1},
            {"role": "assistant", "content": a1},
            {"role": "user", "content": u2},
        ],
    });

    fixture(canonical, &anthropic, &openai)
}

fn build_cache_less(rng: &mut SplitMix64) -> BenchFixture {
    let tools: Vec<ToolSpec> = (0..2)
        .map(|i| ToolSpec {
            name: format!("fn_{i}"),
            description: pseudo_sentence(rng, 6),
            schema: big_tool_schema(rng, 10),
            cache: false,
        })
        .collect();
    let system = pseudo_sentence(rng, 60);
    let u1 = pseudo_sentence(rng, 18);
    let a1 = pseudo_sentence(rng, 22);

    let canonical = ChatRequest {
        model: "gpt-4o-mini".into(),
        messages: vec![user_text(u1.clone()), assistant_text(a1.clone())].into(),
        system: Some(SystemContent::Text(system.clone())),
        tools: Some(tools.iter().map(tool_to_custom).collect()),
        max_tokens: Some(1024),
        ..Default::default()
    };

    let anthropic = json!({
        "model": canonical.model.as_str(),
        "max_tokens": 1024,
        "system": system,
        "tools": tools.iter().map(tool_to_anthropic_wire).collect::<Vec<_>>(),
        "messages": [
            {"role": "user", "content": u1},
            {"role": "assistant", "content": a1},
        ],
    });

    let openai = json!({
        "model": "gpt-4o-mini",
        "max_tokens": 1024,
        "tools": tools.iter().map(tool_to_openai_wire).collect::<Vec<_>>(),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": u1},
            {"role": "assistant", "content": a1},
        ],
    });

    fixture(canonical, &anthropic, &openai)
}

fn build_long_session(rng: &mut SplitMix64) -> BenchFixture {
    let system = pseudo_sentence(rng, 30);
    let opener = pseudo_sentence(rng, 10);
    let tool_name = "step_tool";

    let mut canonical_messages = vec![user_text(opener.clone())];
    let mut anthropic_messages = vec![json!({"role": "user", "content": opener.clone()})];
    let mut openai_messages = vec![
        json!({"role": "system", "content": system.clone()}),
        json!({"role": "user", "content": opener}),
    ];

    for round in 0..LONG_SESSION_ROUNDS {
        let call_id = format!("toolu_ls_{round:02}");
        let input = json!({"round": round, "note": pseudo_sentence(rng, 3)});
        let output = pseudo_sentence(rng, 8);
        let reply = pseudo_sentence(rng, 6);
        let followup = pseudo_sentence(rng, 5);

        canonical_messages.push(assistant_tool_use(
            call_id.clone(),
            tool_name.into(),
            input.clone(),
        ));
        canonical_messages.push(tool_result_msg(call_id.clone(), output.clone()));
        canonical_messages.push(assistant_text(reply.clone()));
        canonical_messages.push(user_text(followup.clone()));

        anthropic_messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": call_id, "name": tool_name, "input": input}
        ]}));
        anthropic_messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": call_id, "content": output}
        ]}));
        anthropic_messages.push(json!({"role": "assistant", "content": reply}));
        anthropic_messages.push(json!({"role": "user", "content": followup}));

        openai_messages.push(json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": call_id, "type": "function", "function": {"name": tool_name, "arguments": openai_arguments(&input)}}
        ]}));
        openai_messages.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
        openai_messages.push(json!({"role": "assistant", "content": reply}));
        openai_messages.push(json!({"role": "user", "content": followup}));
    }

    let canonical = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: canonical_messages.into(),
        system: Some(cached_system_blocks(system.clone())),
        max_tokens: Some(4096),
        ..Default::default()
    };

    let anthropic = json!({
        "model": "claude-opus-4-7",
        "max_tokens": 4096,
        "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral", "ttl": "5m"}}],
        "messages": anthropic_messages,
    });

    let openai = json!({
        "model": "gpt-4o",
        "max_tokens": 4096,
        "messages": openai_messages,
    });

    fixture(canonical, &anthropic, &openai)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lower bound the `LargeImage` wire encodings must clear (1 MiB).
    const LARGE_IMAGE_SIZE_FLOOR: usize = 1024 * 1024;
    /// Lower bound on the `LongSession` canonical turn count.
    const LONG_SESSION_MIN_TURNS: usize = 40;

    fn wire_str(bytes: &[u8]) -> &str {
        std::str::from_utf8(bytes).expect("wire bytes are valid utf-8")
    }

    #[test]
    fn all_covers_every_variant() {
        // Wildcard-free match: adding a variant without updating
        // `SpectrumProfile::ALL` fails here at compile time via the
        // exhaustive match, and at runtime via the count assertion.
        let counted = SpectrumProfile::ALL
            .iter()
            .map(|p| match p {
                SpectrumProfile::ToolHeavy
                | SpectrumProfile::LargeImage
                | SpectrumProfile::PlainRoundTrip
                | SpectrumProfile::NoMarker
                | SpectrumProfile::CacheLess
                | SpectrumProfile::LongSession => 1,
            })
            .sum::<usize>();
        assert_eq!(counted, SpectrumProfile::ALL.len());
        let mut sorted: Vec<&str> = SpectrumProfile::ALL
            .iter()
            .map(|p| p.snake_name())
            .collect();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            SpectrumProfile::ALL.len(),
            "duplicate entries in ALL"
        );
    }

    #[test]
    fn two_calls_produce_byte_equal_output() {
        for profile in SpectrumProfile::ALL {
            let a = profile.generate();
            let b = profile.generate();
            assert_eq!(
                a.anthropic_wire, b.anthropic_wire,
                "{profile:?} anthropic wire is not deterministic"
            );
            assert_eq!(
                a.openai_wire, b.openai_wire,
                "{profile:?} openai wire is not deterministic"
            );
            assert_eq!(
                serde_json::to_vec(&a.canonical).unwrap(),
                serde_json::to_vec(&b.canonical).unwrap(),
                "{profile:?} canonical is not deterministic"
            );
        }
    }

    #[test]
    fn no_marker_profile_has_no_cache_control() {
        let f = SpectrumProfile::NoMarker.generate();
        let canonical = serde_json::to_string(&f.canonical).unwrap();
        assert!(!canonical.contains("cache_control"));
        assert!(!wire_str(&f.anthropic_wire).contains("cache_control"));
        assert!(!wire_str(&f.openai_wire).contains("cache_control"));
    }

    #[test]
    fn large_image_profile_exceeds_size_floor() {
        let f = SpectrumProfile::LargeImage.generate();
        assert!(
            f.anthropic_wire.len() > LARGE_IMAGE_SIZE_FLOOR,
            "anthropic wire {} bytes below floor",
            f.anthropic_wire.len()
        );
        assert!(
            f.openai_wire.len() > LARGE_IMAGE_SIZE_FLOOR,
            "openai wire {} bytes below floor",
            f.openai_wire.len()
        );
    }

    #[test]
    fn long_session_profile_meets_turn_floor() {
        let f = SpectrumProfile::LongSession.generate();
        assert!(
            f.canonical.messages.len() >= LONG_SESSION_MIN_TURNS,
            "long session had only {} turns",
            f.canonical.messages.len()
        );
    }

    #[test]
    fn every_profile_wire_is_a_json_object_with_model_and_messages() {
        for profile in SpectrumProfile::ALL {
            let f = profile.generate();
            for (dialect, bytes) in [("anthropic", &f.anthropic_wire), ("openai", &f.openai_wire)] {
                let value: Value = serde_json::from_slice(bytes)
                    .unwrap_or_else(|e| panic!("{profile:?} {dialect} wire is not JSON: {e}"));
                let obj = value
                    .as_object()
                    .unwrap_or_else(|| panic!("{profile:?} {dialect} wire is not an object"));
                assert!(
                    obj.contains_key("model"),
                    "{profile:?} {dialect} lacks model"
                );
                assert!(
                    obj.contains_key("messages"),
                    "{profile:?} {dialect} lacks messages"
                );
            }
        }
    }
}
