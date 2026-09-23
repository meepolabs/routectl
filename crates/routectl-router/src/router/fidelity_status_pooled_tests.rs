// POOLED-SEAT identity parity: every status row fact a pooled `nick#member` key
// resolves must come from the SAME identity a live `DispatchTarget` for that seat
// carries -- the member provider entry for kind, base URL, override mask, and
// prefix-impact opt-in, and the base model nickname for the model-scoped tier.
//
// Why this needs its own section rather than one case beside the non-pooled rows:
// a pooled key has THREE plausible resolutions (the member suffix, the base model
// nickname, the pool the base names) and two of them are wrong in ways no
// non-pooled fixture can see. A pooled fixture that lets the pool name double as a
// provider entry cannot see them either -- both resolutions then land on the same
// entry -- and config validation refuses exactly that shape, so the fixtures here
// keep the pool out of `[providers]` and give the member and the fallback DIFFERENT
// provider kinds, which is what makes each assertion discriminate.
//
// An `include!`d FRAGMENT of `fidelity_status_tests.rs` -- no top-level `use`, no
// `mod`; see that host's note.

/// The pooled model nickname every fixture below keys on.
const POOLED_NICK: &str = "sonnet";

/// The pool the model's `provider` names. Deliberately NOT a `[providers]` entry:
/// `validate_pools` refuses a pool whose name collides with one, so a fixture that
/// made it double as a provider entry would collapse the member and pool
/// resolutions onto one answer and could not tell them apart.
const POOL: &str = "anthropic-pool";

/// The pool member under test -- an `anthropic-api` entry on a remote-looking base
/// URL, so the lane the member resolves admits pre-flight.
const MEMBER: &str = "seat-a";

/// A SECOND member, for the opt-in tier controls: a spec naming this one must not
/// opt the member above in.
const OTHER_MEMBER: &str = "seat-b";

/// `nick#member`, the state key a live pooled `DispatchTarget` carries.
fn pooled_key() -> String {
    crate::seat_pool::seat_state_key(POOLED_NICK, Some(MEMBER))
}

/// The prefix-impacting closed-table path, whose class is the one that requires a
/// target opt-in. Read from the table's own constant, never spelled here.
fn prefix_path() -> &'static str {
    crate::router::field_repair::TEST_PREFIX_IMPACTING_PATH
}

/// The capability key the prefix-impacting path mints.
fn prefix_key() -> String {
    crate::field_capability::field_capability_key(prefix_path())
        .expect("the test table's prefix-impacting path is well-formed")
}

/// A config for one pool-backed model: two `anthropic-api` member entries on
/// remote-looking base URLs, a `[pools]` block naming both, and a model whose
/// `provider` is the POOL.
///
/// Parsed from toml rather than hand-mutated so the provider entries are shaped as
/// an operator's would be -- a hand-built variant can carry a combination config
/// validation refuses, which is the kind of fixture that proves a resolution
/// against a state no deployment reaches. The member credentials are `oauth://`
/// refs because `validate_pools` requires it of every pool member this release, and
/// [`assert_pool_config_is_valid`] is what holds the whole fixture to that standard
/// rather than leaving it to the comment.
fn pooled_config() -> Config {
    use crate::config::{AliasValue, ModelEntry, PoolEntry};
    let toml_text = format!(
        "\n[providers.{MEMBER}]\nkind = \"anthropic-api\"\napi_key_ref = \"oauth://{MEMBER}\"\n\
         base_url = \"https://api.anthropic.com\"\n\
         \n[providers.{OTHER_MEMBER}]\nkind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://{OTHER_MEMBER}\"\n\
         base_url = \"https://api.anthropic.com\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config.pools.insert(
        POOL.to_string(),
        PoolEntry::new(vec![MEMBER.to_string(), OTHER_MEMBER.to_string()]),
    );
    config.models.insert(
        POOLED_NICK.to_string(),
        ModelEntry::new(POOL, "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(POOLED_NICK.to_string()),
    );
    config
}

/// Refuse a fixture the real config validator would reject.
///
/// Every case below asserts what a pooled identity resolves to, and that claim is
/// only about a deployment if the config could BE a deployment. A pool with an
/// API-key member, a mixed-kind pool, a name colliding with a provider entry, or an
/// opt-in spec naming a target outside its pool are each refused at load -- so a
/// fixture carrying one proves a resolution for a state no daemon ever serves, which
/// is exactly the shape that makes a green test say nothing.
///
/// Both validators run, because they refuse different things and neither implies the
/// other: `validate_pools` owns the pool table's own coherence, while
/// `collect_config_validation` is the whole suite `config check` and the serve gate
/// apply -- and it is the one that checks a `[fidelity] prefix_impact_opt_in` entry
/// against the provider/model directory, which is the surface half these fixtures
/// vary.
fn assert_pool_config_is_valid(config: &Config) {
    crate::factory::validate_pools(config).expect("the pooled fixture must be a servable pool");
    let validation = crate::factory::collect_config_validation(config);
    assert!(
        validation.errors.is_empty(),
        "the pooled fixture must pass the same validation suite a deployment does, or \
         it proves a resolution for a config no daemon serves: {:?}",
        validation.errors,
    );
}

/// A Router over `config` with the pool-backed model INSTALLED as a resolved
/// multi-seat model, exactly as the factory compiles one.
///
/// Installed rather than left config-only, and that is the half a config-only
/// fixture cannot cover: a live daemon's status read resolves through the resolved
/// table first, so a resolution that only works on a cold-boot config would still
/// report the wrong identity on every running daemon. The converse gap is covered by
/// the config-only cases at the end of this fragment.
fn pooled_router(config: Config) -> Router {
    use crate::resolved::ResolvedModel;

    assert_pool_config_is_valid(&config);
    let provider: std::sync::Arc<dyn routectl_core::Provider> = std::sync::Arc::new(InertProvider);
    let seats: Vec<crate::seat_pool::SeatTarget> = [MEMBER, OTHER_MEMBER]
        .iter()
        .map(|member| crate::seat_pool::SeatTarget {
            provider_name: (*member).to_string(),
            provider: std::sync::Arc::clone(&provider),
            auth_secret_ref: None,
        })
        .collect();
    let resolved = ResolvedModel::new(
        POOLED_NICK,
        POOL,
        std::sync::Arc::clone(&provider),
        "claude-sonnet-4-5",
    )
    .with_seats(seats.into());
    let mut router = router_assuming_durable_writes(config);
    let mut models: std::collections::BTreeMap<String, std::sync::Arc<ResolvedModel>> =
        std::collections::BTreeMap::new();
    models.insert(POOLED_NICK.to_string(), std::sync::Arc::new(resolved));
    router.install_resolved_models(models);
    router
}

/// A provider that never answers. The fixtures here drive the STATUS read, which
/// dispatches nothing -- so a double that could answer would only invite a test to
/// depend on a dispatch this surface must never make.
struct InertProvider;

#[async_trait::async_trait]
impl routectl_core::Provider for InertProvider {
    fn id(&self) -> &'static str {
        "inert"
    }
    fn normalize_request(
        &self,
        _: &routectl_core::ChatRequest,
    ) -> routectl_core::Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    fn normalize_response(
        &self,
        _: serde_json::Value,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::normalize_response("inert", "unused"))
    }
    async fn complete(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::upstream("inert", 500, "unused"))
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    > {
        Err(routectl_core::Error::upstream("inert", 500, "unused"))
    }
    async fn count_tokens(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::TokenCount> {
        Err(routectl_core::Error::upstream("inert", 500, "unused"))
    }
}

/// The LIVE `DispatchTarget` for the pooled seat, built through the router's own
/// chain expansion.
///
/// The oracle for every parity assertion below: what pre-flight reads is a target
/// this function returns, so an identity the status surface resolves differently is
/// an identity the two stages disagree on.
fn live_seat_target(router: &Router) -> crate::router::DispatchTarget {
    let model = std::sync::Arc::clone(
        router
            .resolved_models
            .get(POOLED_NICK)
            .expect("the fixture installed the pooled model"),
    );
    router
        .expand_chain_to_targets(vec![model], None)
        .into_iter()
        .find(|target| target.state_key == pooled_key())
        .expect("the pooled model expands to one target per seat")
}

/// Plant a resident ACTING verdict for `capability_key` on the pooled seat key and
/// acknowledge `confirmations` for it AT THE LIVE TARGET'S OWN PROVIDER KIND.
///
/// The kind is taken from the live `DispatchTarget` rather than from the status
/// surface's own resolution, and that is the whole point: the confirmation half is
/// keyed on `(state_key, capability_key, provider_kind)`, so a status read that
/// resolves a different kind looks up a key nothing seeded and reports an
/// acknowledged verdict as unacknowledged.
fn plant_pooled_verdict(router: &Router, capability_key: &str, confirmations: u32) {
    let live_kind = live_seat_target(router)
        .provider_kind
        .expect("a live pooled target carries its member entry's kind");
    plant_verdict_on(router, &pooled_key(), capability_key.to_string());
    let key = crate::field_verdict::FieldVerdictKey::from_capability_key(
        pooled_key(),
        capability_key.to_string(),
        live_kind.to_string(),
    );
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        &pooled_key(),
        capability_key,
        live_kind,
    );
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&key, incarnation, confirmations, false);
}

// ---------------------------------------------------------------------------
// Identity parity against the live DispatchTarget
// ---------------------------------------------------------------------------

/// A pooled seat's status row resolves the SAME provider identity a live
/// `DispatchTarget` for that seat carries, so its kind, confirmation count, canary
/// state, and blocked reason all describe the lane pre-flight actually acts on.
///
/// The four facts are asserted TOGETHER because they share one cause: every one of
/// them is read through a `FieldVerdictKey` whose third component is the resolved
/// provider kind. A resolution that answers the pool's kind (or the empty token)
/// mints a key nothing seeded, and then the confirmation count reads zero, the
/// canary posture reads never-started, the remaining interval reads full, and the
/// blocked reason reads `not_eligible` -- for a verdict pre-flight is at that
/// moment rewriting traffic with. Each of those readings is individually plausible,
/// which is why none of them alone would be recognized as a resolution bug.
///
/// Mutation check: drop the member-suffix arm from `provider_kind_for_state_key` ->
/// the kind parity assertion reds, and the three downstream facts red with it.
#[test]
fn a_pooled_seats_row_resolves_the_member_identity_a_live_dispatch_target_carries() {
    let router = pooled_router(pooled_config());
    let target = live_seat_target(&router);
    // PREMISE, stated against the live target rather than assumed: the seat's own
    // target is keyed by `nick#member`, names the MEMBER provider entry (not the
    // pool), and carries that entry's kind.
    assert_eq!(target.state_key, pooled_key());
    assert_eq!(
        target.provider_name, MEMBER,
        "a pooled seat's target names the member entry it dispatches",
    );
    let live_kind = target
        .provider_kind
        .expect("the member entry's kind is resolved at chain expansion");
    assert_eq!(live_kind, "anthropic-api");

    plant_pooled_verdict(&router, &grounded_key(), ENVELOPE_QUORUM);

    assert_eq!(
        router.provider_kind_for_state_key(&pooled_key()),
        live_kind,
        "the status surface must resolve the pooled key to the MEMBER entry's kind: \
         the verdict identity is keyed on it, so any other answer looks up a key the \
         learn path never wrote",
    );
    let rows = router.field_verdict_status();
    let row = only_row(&rows);
    assert_eq!(
        row.confirmations, ENVELOPE_QUORUM,
        "so the acknowledged count seeded at the live identity is the count reported",
    );
    assert_eq!(
        row.canary,
        CanaryPosture::Counting,
        "and the canary posture is the seeded identity's, not a never-started default",
    );
    assert_eq!(
        row.canary_remaining_requests, CANARY_INTERVAL,
        "with the seeded identity's own cadence rather than a fresh interval that \
         happens to equal it -- pinned alongside the count above, which a missed \
         lookup would report as zero",
    );
    assert_eq!(
        row.blocked_reason, None,
        "and the row reads UNBLOCKED, which is the state in which pre-flight rewrites \
         this seat's traffic: a missed identity reports `not_eligible` for exactly \
         that state",
    );
}

/// The FEATURE-ABSENT control for the row above: with the confirmation seeded at a
/// DIFFERENT provider kind, the same fixture reports `not_eligible`.
///
/// Without this the assertions above could pass on a surface that reported an
/// unblocked row regardless of what was seeded. Here the only thing changed is the
/// kind the acknowledgement was written under, and the row must refuse -- which is
/// what makes "unblocked" above a statement about the identity match rather than
/// about the fixture.
#[test]
fn a_pooled_row_whose_confirmation_was_seeded_under_another_kind_is_not_eligible() {
    let router = pooled_router(pooled_config());
    plant_verdict_on(&router, &pooled_key(), grounded_key());
    // A kind no resolution of this key can produce, so the seeded acknowledgement
    // belongs to an identity the row cannot reach.
    let foreign_kind = "openai-compat";
    let key = crate::field_verdict::FieldVerdictKey::from_capability_key(
        pooled_key(),
        grounded_key(),
        foreign_kind.to_string(),
    );
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        &pooled_key(),
        &grounded_key(),
        foreign_kind,
    );
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&key, incarnation, ENVELOPE_QUORUM, false);

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(
        row.confirmations, 0,
        "an acknowledgement written under another kind backs no identity this row can \
         reach",
    );
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::NotEligible),
        "so the row refuses -- which is what makes the unblocked sibling a statement \
         about the identity match",
    );
}

// ---------------------------------------------------------------------------
// Member-scoped prefix-impact opt-in
// ---------------------------------------------------------------------------

/// Plant a prefix-impacting pooled verdict at its class quorum and report the
/// blocked reason under `opt_in`.
///
/// One driver for every opt-in tier case below, so the cases differ ONLY in the
/// spec list -- which is what makes the comparison between them the evidence.
fn pooled_prefix_blocked_reason(opt_in: &[&str]) -> Option<PreflightBlockedReason> {
    let mut config = pooled_config();
    config.fidelity.prefix_impact_opt_in = opt_in.iter().map(|s| (*s).to_string()).collect();
    let router = pooled_router(config);
    plant_pooled_verdict(&router, &prefix_key(), PREFIX_QUORUM);

    let rows = router.field_verdict_status();
    let row = only_row(&rows);
    assert!(
        row.prefix_impacting,
        "premise: the planted row is prefix-impacting, so the target opt-in is the \
         gate under test rather than an inert one",
    );
    assert_eq!(
        row.confirmations, PREFIX_QUORUM,
        "and it clears its class's quorum, so a refusal below is the opt-in's and not \
         a confirmation shortfall",
    );
    row.blocked_reason
}

/// A MEMBER-scoped, model-scoped opt-in (`member:nickname`) opts the pooled seat in.
///
/// The spec shape an operator actually writes for one seat of a pool, and the one
/// config validation accepts for it (the model belongs to the pool, and the member
/// belongs to that pool). The live planner resolves it against the target's own
/// `provider_name` -- the MEMBER -- and its `nickname` -- the BASE model -- so a
/// status read resolving either half differently reports a seat as dormant while
/// its traffic is being rewritten.
///
/// Mutation check: resolve the opt-in's provider half through the pool base instead
/// of the `#member` suffix -> red here with `no_target_opt_in`.
#[test]
fn a_member_scoped_model_scoped_opt_in_unblocks_a_pooled_seat() {
    let spec = format!("{MEMBER}:{POOLED_NICK}");

    let blocked = pooled_prefix_blocked_reason(&[&spec]);

    assert_eq!(
        blocked, None,
        "the member-scoped opt-in is found for the pooled key: the provider half \
         resolves to the MEMBER suffix and the model half to the BASE nickname, \
         exactly as a live DispatchTarget carries them",
    );
}

/// A MEMBER-scoped, provider-scoped opt-in (a bare `member`) opts every model on
/// that member in, the pooled seat included.
///
/// The second tier of the same grammar, asserted separately because the tiers match
/// on different halves: this one ignores the nickname entirely, so a resolution
/// that got the provider half right and the model half wrong passes here and fails
/// the case above -- and the pair localizes which half moved.
#[test]
fn a_member_scoped_provider_scoped_opt_in_unblocks_a_pooled_seat() {
    let blocked = pooled_prefix_blocked_reason(&[MEMBER]);

    assert_eq!(
        blocked, None,
        "a bare member entry opts in every model dispatched through that member",
    );
}

/// The MISSING-OPT-IN control: with no opt-in at all, the same fully-confirmed
/// pooled row reports `no_target_opt_in`.
///
/// Without it every case above would pass against a surface that never consulted
/// the opt-in list, which is a fidelity defect (a prefix-impacting rewrite running
/// on a target the operator never named) rather than the feature.
#[test]
fn a_pooled_seat_with_no_opt_in_reports_no_target_opt_in() {
    let blocked = pooled_prefix_blocked_reason(&[]);

    assert_eq!(
        blocked,
        Some(PreflightBlockedReason::NoTargetOptIn),
        "a prefix-impacting rewrite stays dormant until the operator names the target, \
         however well confirmed the verdict",
    );
}

/// An opt-in naming the OTHER member of the same pool does not opt this seat in.
///
/// THE discriminating case for the provider half, and the one a pool-scoped
/// resolution cannot survive: both members sit in one pool and behind one base
/// model, so a resolution keyed on the pool or on the base nickname matches this
/// spec and opts in a seat the operator opted out of -- running a cache-prefix
/// rewrite on an account they deliberately left alone.
#[test]
fn an_opt_in_naming_a_sibling_pool_member_does_not_opt_this_seat_in() {
    let blocked = pooled_prefix_blocked_reason(&[OTHER_MEMBER]);

    assert_eq!(
        blocked,
        Some(PreflightBlockedReason::NoTargetOptIn),
        "one member's opt-in is not the pool's: a resolution through the pool or the \
         base nickname would rewrite traffic on an account the operator left out",
    );
}

/// An opt-in naming the right member but a DIFFERENT model does not opt this seat
/// in.
///
/// The discriminating case for the model half. A resolution that dropped the
/// nickname comparison -- or compared the pooled `nick#member` key against the
/// spec's bare nickname and fell through to the provider tier -- would match this
/// spec and opt in a model the operator named nowhere.
#[test]
fn an_opt_in_naming_another_model_on_the_same_member_does_not_opt_this_seat_in() {
    let mut config = pooled_config();
    // A second model on the SAME member, so the spec below is one config validation
    // accepts and the only thing distinguishing it is the nickname.
    config.models.insert(
        "haiku".to_string(),
        crate::config::ModelEntry::new(MEMBER, "claude-haiku-4-5"),
    );
    config.fidelity.prefix_impact_opt_in = vec![format!("{MEMBER}:haiku")];
    let router = pooled_router(config);
    plant_pooled_verdict(&router, &prefix_key(), PREFIX_QUORUM);

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::NoTargetOptIn),
        "a model-scoped opt-in for a sibling model on the same member matches neither \
         tier for this target, so it opts it in not at all",
    );
}

// ---------------------------------------------------------------------------
// Pool fallback: a suffix that names no provider entry
// ---------------------------------------------------------------------------

/// A config with ONE openai-compat provider-backed model, for the fallback pair
/// below: `nick#<member>` where the suffix names a real `anthropic-api` entry, and
/// `nick#<unknown>` where it names nothing.
///
/// The two provider kinds are deliberately DIFFERENT, which is what makes the pair
/// discriminate: the member resolution admits the lane and the base resolution
/// refuses it, so a swap between them flips both rows at once.
fn fallback_config() -> Config {
    use crate::config::{AliasValue, ModelEntry};
    let toml_text = format!(
        "\n[providers.base-oc]\nkind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\napi_key_ref = \"env://K\"\n\
         \n[providers.{MEMBER}]\nkind = \"anthropic-api\"\napi_key_ref = \"env://K2\"\n\
         base_url = \"https://api.anthropic.com\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config.models.insert(
        POOLED_NICK.to_string(),
        ModelEntry::new("base-oc", "gpt-4o"),
    );
    config.aliases.insert(
        "default".to_string(),
        AliasValue::Single(POOLED_NICK.to_string()),
    );
    config
}

/// A suffix naming a real provider entry resolves THAT entry, not the base model's.
///
/// Half of the fallback pair. The base model here is openai-compat, whose lane
/// pre-flight refuses, and the suffix is an attributable `anthropic-api` entry whose
/// lane it admits -- so this row reads as a supported lane only if the suffix won.
#[test]
fn a_suffix_naming_a_provider_entry_resolves_that_entry_rather_than_the_base_model() {
    let router = router_assuming_durable_writes(fallback_config());
    let state_key = crate::seat_pool::seat_state_key(POOLED_NICK, Some(MEMBER));
    plant_verdict_on(&router, &state_key, grounded_key());

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_ne!(
        row.blocked_reason,
        Some(PreflightBlockedReason::UnsupportedLane),
        "the suffix names an attributable anthropic-api entry, so the lane is \
         supported -- the base model's openai-compat entry is not what was resolved",
    );
}

/// A suffix naming NO provider entry falls back to the base model's own provider.
///
/// The other half of the pair, and the reason the fallback exists: a `#`-suffixed
/// key whose suffix resolves nothing is not unattributable, it is a key whose model
/// half still resolves. Here the base is openai-compat, so the lane is refused --
/// which is the correct answer for that entry and the opposite of the row above.
#[test]
fn a_suffix_naming_no_provider_entry_falls_back_to_the_base_models_provider() {
    let router = router_assuming_durable_writes(fallback_config());
    let state_key = crate::seat_pool::seat_state_key(POOLED_NICK, Some("no-such-entry"));
    plant_verdict_on(&router, &state_key, grounded_key());

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::UnsupportedLane),
        "with no provider entry for the suffix the base model's provider is what \
         resolves, and its openai-compat lane is one pre-flight never acts on",
    );
}

// ---------------------------------------------------------------------------
// CONFIG-ONLY pooled identity (no resolved models installed)
// ---------------------------------------------------------------------------

/// Plant a prefix-impacting pooled verdict on a CONFIG-ONLY router and report the
/// blocked reason under `opt_in`.
///
/// Config-only is not a contrived state: it is what a cold boot looks like before
/// the resolved table is installed, and what a Router whose provider build FAILED
/// leaves behind for the rest of its life. The learn path keys entries on such a
/// target either way, so its identity must still resolve -- and the sibling driver
/// above cannot see this branch at all, because an installed resolved model
/// short-circuits ahead of it.
///
/// The verdict is planted at the kind the CONFIG resolves, since there is no live
/// target to read one off: that is the same resolution the seed path uses on a cold
/// boot, so what this plants is a state a real replay produces.
fn config_only_pooled_blocked_reason(opt_in: &[&str]) -> Option<PreflightBlockedReason> {
    let mut config = pooled_config();
    config.fidelity.prefix_impact_opt_in = opt_in.iter().map(|s| (*s).to_string()).collect();
    assert_pool_config_is_valid(&config);
    // No `install_resolved_models`: THE branch under test.
    let router = router_assuming_durable_writes(config);
    let state_key = pooled_key();
    let capability_key = prefix_key();
    plant_verdict_on(&router, &state_key, capability_key.clone());
    let kind = router.provider_kind_for_state_key(&state_key).to_string();
    assert_eq!(
        kind, "anthropic-api",
        "premise: even with no resolved table the pooled key resolves its MEMBER \
         entry's kind -- an empty token here would mean the row below is blocked by \
         the lane rather than by the opt-in, and every case would read the same",
    );
    let key = crate::field_verdict::FieldVerdictKey::from_capability_key(
        state_key.clone(),
        capability_key.clone(),
        kind.clone(),
    );
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        &state_key,
        &capability_key,
        &kind,
    );
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&key, incarnation, PREFIX_QUORUM, false);

    let rows = router.field_verdict_status();
    let row = only_row(&rows);
    assert_eq!(
        row.confirmations, PREFIX_QUORUM,
        "and the acknowledgement is reachable, so a refusal below is the opt-in's \
         rather than a confirmation shortfall",
    );
    row.blocked_reason
}

/// On a CONFIG-ONLY router, a member-scoped model-scoped opt-in (`member:nickname`)
/// opts the pooled seat in.
///
/// The gap the installed-model cases cannot reach. A pooled seat's identity has to
/// resolve from `[models]` and `[providers]` alone, because that is the state a cold
/// boot replays its ledger in -- and a resolution that only consulted the resolved
/// table would report every such row blocked, telling an operator their opt-in was
/// ignored on exactly the boot where the verdict was restored.
///
/// Mutation check: delete the configured-model fallback arm from
/// `override_identity_for` -> red here with `no_target_opt_in`.
#[test]
fn a_config_only_pooled_seat_honours_a_member_scoped_model_scoped_opt_in() {
    let spec = format!("{MEMBER}:{POOLED_NICK}");

    let blocked = config_only_pooled_blocked_reason(&[&spec]);

    assert_eq!(
        blocked, None,
        "with no resolved table the pooled key still resolves the MEMBER for the \
         provider half and the BASE nickname for the model half, so the operator's \
         opt-in is honoured",
    );
}

/// The MISSING-OPT-IN control for the config-only branch.
///
/// Without it the case above would pass against a resolution that never consulted
/// the opt-in list at all on this branch -- a prefix-impacting rewrite running on a
/// target the operator never named.
#[test]
fn a_config_only_pooled_seat_with_no_opt_in_reports_no_target_opt_in() {
    let blocked = config_only_pooled_blocked_reason(&[]);

    assert_eq!(
        blocked,
        Some(PreflightBlockedReason::NoTargetOptIn),
        "no opt-in means no prefix-impacting rewrite, on this branch as on the other",
    );
}

/// The SIBLING-MEMBER control for the config-only branch: an opt-in naming the other
/// member of the same pool does not opt this seat in.
///
/// The discriminating case, and the one a pool-scoped or base-scoped resolution
/// cannot survive on this branch either: both members sit in one pool behind one
/// base model, so a resolution keyed on either would match this spec and rewrite
/// traffic on an account the operator deliberately left out.
#[test]
fn a_config_only_opt_in_naming_a_sibling_pool_member_does_not_opt_this_seat_in() {
    let blocked = config_only_pooled_blocked_reason(&[OTHER_MEMBER]);

    assert_eq!(
        blocked,
        Some(PreflightBlockedReason::NoTargetOptIn),
        "one member's opt-in is not the pool's, with or without a resolved table",
    );
}

// ---------------------------------------------------------------------------
// The documented blocked-reason vocabulary
// ---------------------------------------------------------------------------

/// EVERY `PreflightBlockedReason` token this surface can emit is documented in the
/// `blocked_reason` row of `docs/LOGGING.md`'s field-verdict snapshot section.
///
/// A vocabulary an operator's dashboard matches on is only usable if it is written
/// down, and the two tokens this guard was added for -- `capability_disabled` and
/// `unsupported_lane` -- were emitted by the surface and absent from the doc, so an
/// operator meeting either had nothing to look up. Binding the needles to the enum's
/// own `as_str` makes the check work in both directions: a new variant fails until it
/// is documented, and a token renamed in the doc fails until the rename is matched.
///
/// The doc is embedded at compile time, so a moved or renamed `LOGGING.md` is a build
/// error rather than a silently-skipped test, and the search is SCOPED to the row
/// that owns the contract -- unrelated prose elsewhere in the file (the planner's own
/// DEBUG reason table uses several of the same words) must not be able to satisfy it.
#[test]
fn every_blocked_reason_token_is_documented_in_the_logging_contract() {
    const LOGGING_DOC: &str = include_str!("../../../../docs/LOGGING.md");

    let row_start = LOGGING_DOC
        .find("| `blocked_reason` |")
        .expect("LOGGING.md must carry the status surface's blocked_reason row");
    let row_end = LOGGING_DOC[row_start..]
        .find('\n')
        .map_or(LOGGING_DOC.len(), |idx| row_start + idx);
    let row = &LOGGING_DOC[row_start..row_end];

    for reason in [
        PreflightBlockedReason::CapabilityDisabled,
        PreflightBlockedReason::UnsupportedLane,
        PreflightBlockedReason::MaskedByOverride,
        PreflightBlockedReason::CanarySuspended,
        PreflightBlockedReason::NotEligible,
        PreflightBlockedReason::BelowQuorum,
        PreflightBlockedReason::NoTargetOptIn,
    ] {
        let documented = format!("`{}`", reason.as_str());
        assert!(
            row.contains(&documented),
            "the blocked_reason row must document {documented}: a token an operator's \
             dashboard can match on but cannot look up is a vocabulary they have to \
             reverse-engineer from source",
        );
    }
    // The unblocked literal too -- it is the value the row carries most of the time,
    // and an operator reading it needs to know it means "nothing is blocking" rather
    // than "no data".
    assert!(
        row.contains(&format!("`{BLOCKED_REASON_NONE}`")),
        "and the unblocked token, which is what the field reads while pre-flight is \
         actively rewriting traffic",
    );
    // POSITIVE CONTROL: the scoped row would genuinely fail to contain a token it
    // does not document. Without this, a mis-scoped slice (an empty row, or one cut
    // short) would satisfy every assertion above by matching nothing.
    assert!(
        !row.contains("`no_such_blocked_reason`"),
        "control: the scoped row is real text that does NOT contain an undocumented \
         token, so the assertions above are reading a row rather than an empty slice",
    );
    assert!(
        row.len() > 200,
        "control: the row really is the full documented row rather than a truncated \
         slice that trivially contains every short needle",
    );
}
