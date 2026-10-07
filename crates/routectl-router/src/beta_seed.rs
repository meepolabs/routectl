//! The shipped prior for client beta flags a Bedrock lane rejects.
//!
//! A prior consulted outside the learned registry, never an evidence source:
//! nothing here is written to the registry, the events ledger, or any doctor
//! surface. A learned verdict for the same lane and flag beats it, and an
//! operator override beats both.

/// The provider kind ([`crate::config::ProviderEntry::kind_str`]) the seed
/// applies to.
pub const BEDROCK_SEED_PROVIDER_KIND: &str = "bedrock";

/// Client-lifted beta flags AWS Bedrock rejects outright: each one alone 400s
/// the whole request on every Claude model measured, so forwarding any of them
/// can never succeed.
pub const BEDROCK_BETA_SEED: &[&str] = &[
    "advanced-tool-use-2025-11-20",
    "advisor-tool-2026-03-01",
    "prompt-caching-scope-2026-01-05",
];
