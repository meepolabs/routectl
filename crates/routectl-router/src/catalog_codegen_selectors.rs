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
    /// When `true`, `model_glob` spans litellm entries whose
    /// `max_output_tokens` genuinely differ (a gateway or region-restricted
    /// re-listing of the same model generation caps output lower than the
    /// direct API does), so no single ceiling is right for the glob.
    /// `max_output_tokens` on the generated row is forced `None`; every
    /// other field is still derived. Same fail-closed posture as
    /// [`AutoCacherSelector::context_ambiguous`], applied to the output
    /// ceiling: a too-high ceiling gets requests rejected upstream and a
    /// too-low one silently truncates, so ABSENT beats a guess.
    ///
    /// A DOCUMENTED ASSERTION, never the derivation's source of truth:
    /// `catalog_codegen::output_ceiling_for` re-derives the verdict from
    /// every spanned snapshot entry itself, and
    /// `every_selectors_output_ambiguous_flag_matches_the_snapshots` fails
    /// when this flag and the snapshots disagree.
    pub output_ambiguous: bool,
}

/// Anthropic direct-API selectors. `models_dev` provider key is
/// `"anthropic"` for all of these (see
/// `catalog_codegen::anthropic_like_cells`).
pub const ANTHROPIC_SELECTORS: &[TieredSelector] = &[
    TieredSelector {
        model_glob: "claude-opus-4-8*",
        litellm_key: "claude-opus-4-8",
        models_dev_model: "claude-opus-4-8",
        min_prefix_tokens: 1024,
        output_ambiguous: false,
    },
    TieredSelector {
        model_glob: "claude-sonnet-4-6*",
        litellm_key: "claude-sonnet-4-6",
        models_dev_model: "claude-sonnet-4-6",
        min_prefix_tokens: 1024,
        // litellm lists this generation at three different ceilings across
        // hosts (64000 direct / vertex / azure, 16384 on snowflake) and
        // models.dev states 128000 -- no single ceiling holds for the glob.
        output_ambiguous: true,
    },
    TieredSelector {
        model_glob: "claude-sonnet-4-5*",
        litellm_key: "claude-sonnet-4-5",
        models_dev_model: "claude-sonnet-4-5",
        min_prefix_tokens: 1024,
        // The glob spans 64000 (direct / vertex / azure), 16384 (snowflake),
        // and 8192 (gov-region bedrock re-listings).
        output_ambiguous: true,
    },
    TieredSelector {
        model_glob: "claude-opus-4-7*",
        litellm_key: "claude-opus-4-7",
        models_dev_model: "claude-opus-4-7",
        min_prefix_tokens: 2048,
        output_ambiguous: false,
    },
    TieredSelector {
        model_glob: "claude-opus-4-6*",
        litellm_key: "claude-opus-4-6",
        models_dev_model: "claude-opus-4-6",
        min_prefix_tokens: 4096,
        output_ambiguous: false,
    },
    TieredSelector {
        model_glob: "claude-opus-4-5*",
        litellm_key: "claude-opus-4-5",
        models_dev_model: "claude-opus-4-5",
        min_prefix_tokens: 4096,
        output_ambiguous: false,
    },
    TieredSelector {
        model_glob: "claude-haiku-4-5*",
        litellm_key: "claude-haiku-4-5",
        models_dev_model: "claude-haiku-4-5",
        min_prefix_tokens: 4096,
        // The glob spans 64000 (direct / azure), 16384 (snowflake), and 8192
        // (vertex).
        output_ambiguous: true,
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
        output_ambiguous: false,
    },
    TieredSelector {
        model_glob: "anthropic.claude-sonnet-4-5*",
        litellm_key: "anthropic.claude-sonnet-4-5-20250929-v1:0",
        models_dev_model: "anthropic.claude-sonnet-4-5-20250929-v1:0",
        min_prefix_tokens: 4096,
        // The gov-region re-listings cap output at 8192 where the commercial
        // regions allow 64000.
        output_ambiguous: true,
    },
    TieredSelector {
        model_glob: "anthropic.claude-haiku-4-5*",
        litellm_key: "anthropic.claude-haiku-4-5-20251001-v1:0",
        models_dev_model: "anthropic.claude-haiku-4-5-20251001-v1:0",
        min_prefix_tokens: 4096,
        output_ambiguous: false,
    },
    TieredSelector {
        model_glob: "anthropic.claude-opus-4-5*",
        litellm_key: "anthropic.claude-opus-4-5-20251101-v1:0",
        models_dev_model: "anthropic.claude-opus-4-5-20251101-v1:0",
        min_prefix_tokens: 4096,
        output_ambiguous: false,
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
    ///
    /// A DOCUMENTED ASSERTION, never the derivation's source of truth:
    /// `catalog_codegen::context_window_for` re-derives the verdict from
    /// every spanned snapshot entry itself, and
    /// `every_selectors_context_ambiguous_flag_matches_the_snapshots` fails
    /// when this flag and the snapshots disagree.
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
    ///
    /// A DOCUMENTED ASSERTION, never the derivation's source of truth:
    /// `catalog_codegen::base_rates_for` re-derives the verdict from every
    /// spanned snapshot entry itself, and
    /// `every_selectors_price_ambiguous_flag_matches_the_snapshots` fails
    /// when this flag and the snapshots disagree.
    pub price_ambiguous: bool,
    /// When `true`, `model_glob` matches litellm entries whose
    /// `max_output_tokens` genuinely differ, so no single output ceiling is
    /// right for the glob. `max_output_tokens` on the generated row is
    /// forced `None`; every other field is still derived. Same fail-closed
    /// posture as [`Self::context_ambiguous`], applied to the output
    /// ceiling -- see [`TieredSelector::output_ambiguous`] for why a wrong
    /// ceiling is worse than an absent one.
    ///
    /// Every vendor-prefix glob in [`OPENAI_COMPAT_SELECTORS`] and the
    /// `openai-responses` catch-all sets this: a prefix spanning one
    /// vendor's whole lineup (and the third-party gateway re-listings of
    /// it) never lands on one ceiling.
    ///
    /// A DOCUMENTED ASSERTION, never the derivation's source of truth --
    /// see [`TieredSelector::output_ambiguous`].
    pub output_ambiguous: bool,
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
    // The bare `*` glob spans the whole OpenAI lineup, which the litellm
    // snapshot lists at dozens of distinct confirmed windows.
    context_ambiguous: true,
    // The openai-responses `*` glob serves every OpenAI model, which the
    // snapshots price from $0.02/M (embeddings) to $150/M (o1-pro) -- no
    // single rate is defensible for the glob.
    price_ambiguous: true,
    // Same reason applied to the output ceiling: the glob spans the whole
    // OpenAI lineup, which the snapshots cap anywhere from 0 to 128000.
    output_ambiguous: true,
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
        // The direct deepseek listings confirm 1000000, but the
        // azure_ai/fireworks re-listings this glob also spans state
        // 1048576 -- not the same figure.
        context_ambiguous: true,
        // The direct deepseek listings price at $0.435/M in, $0.87/M out;
        // the azure_ai/fireworks re-listings this glob also spans price at
        // roughly 4x that -- no single rate holds for the glob.
        price_ambiguous: true,
        // The two sources disagree on the DIRECT deepseek ceiling (litellm
        // 8192, models.dev 384000) and the glob also spans third-party
        // re-listings at 384000. Neither source is corroborated by the
        // other, so no figure is defensible: absent, not guessed.
        output_ambiguous: true,
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
        // The catch-all `deepseek-*` also spans deepseek-v4-pro and other
        // gateway re-listings at genuinely different confirmed windows.
        context_ambiguous: true,
        price_ambiguous: true,
        output_ambiguous: true,
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
        // gemini-* spans Gemini SKUs at genuinely different confirmed
        // windows in the vendored snapshot.
        context_ambiguous: true,
        price_ambiguous: true,
        output_ambiguous: true,
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
        output_ambiguous: true,
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
        // kimi-* spans Kimi SKUs at genuinely different confirmed windows in
        // the vendored snapshot.
        context_ambiguous: true,
        price_ambiguous: true,
        output_ambiguous: true,
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
        // moonshot-* spans the same Kimi SKUs at genuinely different
        // confirmed windows.
        context_ambiguous: true,
        price_ambiguous: true,
        output_ambiguous: true,
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
        output_ambiguous: true,
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
        output_ambiguous: true,
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
        // Pinned to the MiniMax-M3 generation, which prices identically in
        // both snapshots; the `minimax-*` catch-all below spans a 2x range.
        price_ambiguous: false,
        // Every litellm entry this glob spans agrees at 512000, so the glob
        // itself is coherent -- but models.dev states 128000 for the same
        // generation, and that cross-source gap is what withholds the
        // ceiling (see `catalog_codegen::output_ceiling_for`).
        output_ambiguous: false,
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
        output_ambiguous: true,
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
