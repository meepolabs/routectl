//! Static selector tables for [`crate::catalog_codegen`]: WHICH vendored
//! snapshot entries become baked cells, and the per-family facts neither
//! vendored source publishes (`ttl_seconds`, `min_prefix_tokens`,
//! `auto_cacher`, and the two escape hatches `economics_unconfirmed` /
//! `context_ambiguous`). The derivation logic that turns these into
//! [`crate::catalog_codegen::GeneratedCell`]s lives in the parent module;
//! this module is data only.
//!
//! Read unconditionally by `crate::catalog_codegen::derive_cells`: the
//! `gen-catalog`-gated render pipeline calls it on the vendored snapshots,
//! and `crate::catalog_import::build_import_candidate` (never
//! feature-gated) calls it on freshly fetched sources.

use crate::catalog::SENTINEL_MIN_PREFIX_TOKENS;

/// One Anthropic-shaped selector: a provider-direct or Bedrock Claude
/// model that carries BOTH a 5-minute and (when the source publishes one)
/// a 1-hour cache-write price. `min_prefix_tokens` is curated (see the
/// parent module's doc); every other economics field is derived.
pub struct TieredSelector {
    pub model_glob: &'static str,
    pub litellm_key: &'static str,
    pub models_dev_model: &'static str,
    pub min_prefix_tokens: u32,
}

/// Anthropic direct-API selectors. `models_dev` provider key is
/// `"anthropic"` for all of these (see
/// [`crate::catalog_codegen::anthropic_like_cells`]).
pub const ANTHROPIC_SELECTORS: &[TieredSelector] = &[
    TieredSelector {
        model_glob: "claude-opus-4-8*",
        litellm_key: "claude-opus-4-8",
        models_dev_model: "claude-opus-4-8",
        min_prefix_tokens: 1024,
    },
    TieredSelector {
        model_glob: "claude-sonnet-4-6*",
        litellm_key: "claude-sonnet-4-6",
        models_dev_model: "claude-sonnet-4-6",
        min_prefix_tokens: 1024,
    },
    TieredSelector {
        model_glob: "claude-sonnet-4-5*",
        litellm_key: "claude-sonnet-4-5",
        models_dev_model: "claude-sonnet-4-5",
        min_prefix_tokens: 1024,
    },
    TieredSelector {
        model_glob: "claude-opus-4-7*",
        litellm_key: "claude-opus-4-7",
        models_dev_model: "claude-opus-4-7",
        min_prefix_tokens: 2048,
    },
    TieredSelector {
        model_glob: "claude-opus-4-6*",
        litellm_key: "claude-opus-4-6",
        models_dev_model: "claude-opus-4-6",
        min_prefix_tokens: 4096,
    },
    TieredSelector {
        model_glob: "claude-opus-4-5*",
        litellm_key: "claude-opus-4-5",
        models_dev_model: "claude-opus-4-5",
        min_prefix_tokens: 4096,
    },
    TieredSelector {
        model_glob: "claude-haiku-4-5*",
        litellm_key: "claude-haiku-4-5",
        models_dev_model: "claude-haiku-4-5",
        min_prefix_tokens: 4096,
    },
];

/// Bedrock Claude selectors (real Bedrock ids carry a vendor prefix, so
/// `model_glob` is trailing-only). `models_dev` provider key is
/// `"amazon-bedrock"` for all of these.
pub const BEDROCK_SELECTORS: &[TieredSelector] = &[
    TieredSelector {
        model_glob: "anthropic.claude-sonnet-4-6*",
        litellm_key: "anthropic.claude-sonnet-4-6",
        models_dev_model: "anthropic.claude-sonnet-4-6",
        min_prefix_tokens: 1024,
    },
    TieredSelector {
        model_glob: "anthropic.claude-sonnet-4-5*",
        litellm_key: "anthropic.claude-sonnet-4-5-20250929-v1:0",
        models_dev_model: "anthropic.claude-sonnet-4-5-20250929-v1:0",
        min_prefix_tokens: 4096,
    },
    TieredSelector {
        model_glob: "anthropic.claude-haiku-4-5*",
        litellm_key: "anthropic.claude-haiku-4-5-20251001-v1:0",
        models_dev_model: "anthropic.claude-haiku-4-5-20251001-v1:0",
        min_prefix_tokens: 4096,
    },
    TieredSelector {
        model_glob: "anthropic.claude-opus-4-5*",
        litellm_key: "anthropic.claude-opus-4-5-20251101-v1:0",
        models_dev_model: "anthropic.claude-opus-4-5-20251101-v1:0",
        min_prefix_tokens: 4096,
    },
];

/// One auto-cacher-shaped selector: a single tier-agnostic row (OpenAI
/// Responses, or one openai-compat sub-provider). When neither source
/// publishes an explicit cache-write price but a cache-read price
/// confirms caching is supported, `wm` defaults to `1.0` (no premium --
/// the write is folded into ordinary input billing, the documented
/// behavior of every auto-cacher this table covers).
pub struct AutoCacherSelector {
    pub model_glob: &'static str,
    pub litellm_key: &'static str,
    pub models_dev_provider: &'static str,
    pub models_dev_model: &'static str,
    pub ttl_seconds: u32,
    pub min_prefix_tokens: u32,
    pub auto_cacher: bool,
    /// When `true`, no source publishes cache-pricing economics for this
    /// family: `wm` / `rm` / `ttl_seconds` / `min_prefix_tokens` /
    /// `auto_cacher` on the generated row mirror
    /// [`crate::catalog::CatalogRow::sentinel`] verbatim (never a
    /// fabricated number); `max_context_tokens` and `capabilities` are
    /// still derived.
    pub economics_unconfirmed: bool,
    /// When `true`, `model_glob` is a vendor-wide prefix that matches
    /// models with GENUINELY different confirmed windows in the vendored
    /// snapshots (e.g. `grok-*` spans a 131K-token mini model and a 2M
    /// -token fast model) -- baking the one flagship's window under the
    /// shared glob would be a confidently-wrong number for every other
    /// model the glob matches. `max_context_tokens` on the generated row
    /// is forced `None` (no cross-check either); every other field is
    /// still derived normally, matching
    /// [`crate::catalog::CatalogRow::max_context_tokens`]'s own
    /// fail-closed documented behavior for a broad, ambiguous glob.
    pub context_ambiguous: bool,
    /// When `true`, `model_glob` matches models the snapshots price very
    /// differently, so no single base per-token rate is right for the glob
    /// (e.g. a bare `"*"` spanning a $0.02/M embedding model and a $150/M
    /// reasoning model, or a vendor prefix spanning a flash and a flagship
    /// tier). `input_cost_per_token` / `output_cost_per_token` on the
    /// generated row are forced `None`; every other field is still derived.
    /// Same posture as [`Self::context_ambiguous`], applied to price: a
    /// wrong dollar rate compounds per token, so ABSENT beats a guess.
    ///
    /// Independent of `context_ambiguous` -- a glob can be coherent in one
    /// dimension and not the other (`grok-*` prices within 2x but spans a
    /// 131K-to-2M window range).
    pub price_ambiguous: bool,
}

pub const OPENAI_RESPONSES_SELECTORS: &[AutoCacherSelector] = &[AutoCacherSelector {
    model_glob: "*",
    litellm_key: "gpt-5.6",
    models_dev_provider: "openai",
    models_dev_model: "gpt-5.6",
    ttl_seconds: 86_400,
    min_prefix_tokens: 1024,
    auto_cacher: true,
    economics_unconfirmed: false,
    context_ambiguous: false,
    // The openai-responses `*` glob serves every OpenAI model, which the
    // snapshots price from $0.02/M (embeddings) to $150/M (o1-pro) -- no
    // single rate is defensible for the glob.
    price_ambiguous: true,
}];

pub const OPENAI_COMPAT_SELECTORS: &[AutoCacherSelector] = &[
    AutoCacherSelector {
        model_glob: "deepseek-v4-pro*",
        litellm_key: "deepseek-v4-pro",
        models_dev_provider: "deepseek",
        models_dev_model: "deepseek-v4-pro",
        ttl_seconds: 3_600,
        min_prefix_tokens: 1,
        auto_cacher: true,
        economics_unconfirmed: false,
        context_ambiguous: false,
        // Pinned to one model (unlike the `deepseek-*` catch-all below):
        // every id this glob matches prices identically in both snapshots.
        price_ambiguous: false,
    },
    AutoCacherSelector {
        model_glob: "deepseek-*",
        litellm_key: "deepseek-v4-flash",
        models_dev_provider: "deepseek",
        models_dev_model: "deepseek-v4-flash",
        ttl_seconds: 3_600,
        min_prefix_tokens: 1,
        auto_cacher: true,
        economics_unconfirmed: false,
        context_ambiguous: false,
        price_ambiguous: true,
    },
    AutoCacherSelector {
        model_glob: "gemini-*",
        litellm_key: "gemini/gemini-3.5-flash",
        models_dev_provider: "google",
        models_dev_model: "gemini-3.5-flash",
        ttl_seconds: 300,
        min_prefix_tokens: 4096,
        auto_cacher: true,
        economics_unconfirmed: false,
        context_ambiguous: false,
        price_ambiguous: true,
    },
    AutoCacherSelector {
        model_glob: "grok-*",
        litellm_key: "xai/grok-4.5",
        models_dev_provider: "xai",
        models_dev_model: "grok-4.5",
        ttl_seconds: 300,
        min_prefix_tokens: 4096,
        auto_cacher: true,
        economics_unconfirmed: false,
        // grok-* also matches xai/grok-4-fast-reasoning (2M context) and
        // xai/grok-3-mini (131K) in the vendored snapshot -- a single
        // number under the shared glob would be confidently wrong for
        // most of the family.
        context_ambiguous: true,
        price_ambiguous: true,
    },
    AutoCacherSelector {
        model_glob: "kimi-*",
        litellm_key: "moonshot/kimi-k2-thinking",
        models_dev_provider: "moonshotai",
        models_dev_model: "kimi-k2-thinking",
        ttl_seconds: 300,
        min_prefix_tokens: 4096,
        auto_cacher: true,
        economics_unconfirmed: false,
        context_ambiguous: false,
        price_ambiguous: true,
    },
    AutoCacherSelector {
        model_glob: "moonshot-*",
        litellm_key: "moonshot/kimi-k2-thinking",
        models_dev_provider: "moonshotai",
        models_dev_model: "kimi-k2-thinking",
        ttl_seconds: 300,
        min_prefix_tokens: 4096,
        auto_cacher: true,
        economics_unconfirmed: false,
        context_ambiguous: false,
        price_ambiguous: true,
    },
    AutoCacherSelector {
        model_glob: "mistral-*",
        litellm_key: "mistral/mistral-large-latest",
        models_dev_provider: "mistral",
        models_dev_model: "mistral-large-latest",
        ttl_seconds: 300,
        min_prefix_tokens: 64,
        auto_cacher: false,
        economics_unconfirmed: true,
        // mistral-* spans embedding models, code models, and chat models
        // of very different sizes in the vendored snapshot.
        context_ambiguous: true,
        price_ambiguous: true,
    },
    AutoCacherSelector {
        model_glob: "qwen-*",
        litellm_key: "dashscope/qwen-max",
        models_dev_provider: "alibaba",
        models_dev_model: "qwen-max",
        ttl_seconds: 300,
        min_prefix_tokens: 1024,
        auto_cacher: false,
        economics_unconfirmed: true,
        // qwen-* spans dashscope SKUs from 30K (qwen-max) to 1M
        // (qwen-turbo / qwen-coder) tokens in the vendored snapshot.
        context_ambiguous: true,
        price_ambiguous: true,
    },
    AutoCacherSelector {
        model_glob: "minimax-m3*",
        litellm_key: "minimax/MiniMax-M3",
        models_dev_provider: "minimax",
        models_dev_model: "MiniMax-M3",
        ttl_seconds: 300,
        min_prefix_tokens: 512,
        auto_cacher: true,
        economics_unconfirmed: false,
        context_ambiguous: false,
        // Pinned to the M3 generation, which prices identically in both
        // snapshots; the `minimax-*` catch-all below spans a 2x range.
        price_ambiguous: false,
    },
    AutoCacherSelector {
        model_glob: "minimax-*",
        litellm_key: "minimax/MiniMax-M2",
        models_dev_provider: "minimax",
        models_dev_model: "MiniMax-M2",
        ttl_seconds: 300,
        min_prefix_tokens: 512,
        auto_cacher: false,
        economics_unconfirmed: false,
        // The minimax-* catch-all spans vendored models whose context
        // windows genuinely differ (196608 vs 1_000_000), so no single
        // window can be baked for the glob.
        context_ambiguous: true,
        price_ambiguous: true,
    },
];

/// Structural fallback rows with no single backing model: a provider-kind
/// `"*"` catch-all. Not vendor-derived (there is no "generic model" to
/// look up); carried verbatim from the pre-codegen hand table.
pub struct CatchAllRow {
    pub provider_kind: &'static str,
    pub wm: f32,
    pub rm: f32,
    pub ttl_seconds: u32,
    pub min_prefix_tokens: u32,
    pub auto_cacher: bool,
}

pub const CATCH_ALL_ROWS: &[CatchAllRow] = &[
    CatchAllRow {
        provider_kind: "anthropic-api",
        wm: 1.25,
        rm: 0.10,
        ttl_seconds: 300,
        min_prefix_tokens: 4096,
        auto_cacher: false,
    },
    CatchAllRow {
        provider_kind: "bedrock",
        wm: 1.25,
        rm: 0.10,
        ttl_seconds: 300,
        min_prefix_tokens: 4096,
        auto_cacher: false,
    },
    CatchAllRow {
        provider_kind: "openai-compat",
        wm: 1.0,
        rm: 0.10,
        ttl_seconds: 300,
        min_prefix_tokens: SENTINEL_MIN_PREFIX_TOKENS,
        auto_cacher: true,
    },
];
