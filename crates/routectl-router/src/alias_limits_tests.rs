use super::*;
use crate::config::{AliasValue, ModelEntry, PoolEntry};
use crate::factory::validate_alias_chain_targets;

fn config() -> Config {
    let mut config = Config::default();
    config
        .models
        .insert("leaf".into(), ModelEntry::new("native", "vendor/model"));
    config
}

fn repeats(config: &mut Config, key: &str, entry: &str, count: usize) {
    config
        .aliases
        .insert(key.into(), AliasValue::Chain(vec![entry.into(); count]));
}

#[test]
fn expanded_size_accepts_exact_bound_and_rejects_one_more() {
    let mut cfg = config();
    repeats(&mut cfg, "root", "leaf", MAX_EXPANDED_TARGETS);
    validate_alias_chain_targets(&cfg).unwrap();
    repeats(&mut cfg, "root", "leaf", MAX_EXPANDED_TARGETS + 1);
    let err = validate_alias_chain_targets(&cfg).unwrap_err().to_string();
    assert!(err.contains("alias `root`"), "{err}");
    assert!(err.contains("4096 dispatch targets"), "{err}");
}

#[test]
fn a_six_level_ten_way_dag_is_counted_without_materializing_it() {
    let mut cfg = config();
    let mut next = "leaf".to_string();
    for i in (0..6).rev() {
        let name = format!("level-{i}");
        repeats(&mut cfg, &name, &next, 10);
        next = name;
    }
    let mut memo = BTreeMap::new();
    let (size, height) = expanded_size(&cfg, "level-0", 0, &mut memo);
    assert_eq!(size, MAX_EXPANDED_TARGETS + 1);
    assert_eq!(height, 5);
    assert_eq!(memo.len(), 6, "each shared suffix is computed once");
    assert!(validate_alias_chain_targets(&cfg).is_err());
}

#[test]
fn shared_dag_edges_count_as_repeated_retry_targets_not_unique_leaves() {
    let mut cfg = config();
    repeats(&mut cfg, "suffix", "leaf", 64);
    repeats(&mut cfg, "root", "suffix", 64);
    validate_alias_chain_targets(&cfg).unwrap();
    repeats(&mut cfg, "root", "suffix", 65);
    assert!(
        validate_alias_chain_targets(&cfg)
            .unwrap_err()
            .to_string()
            .contains("4096")
    );
}

#[test]
fn configured_pool_members_count_even_when_credentials_are_unavailable() {
    let mut cfg = config();
    cfg.models.get_mut("leaf").unwrap().provider = "pool".into();
    cfg.pools
        .insert("pool".into(), PoolEntry::new(vec!["a".into(), "b".into()]));
    repeats(&mut cfg, "root", "leaf", MAX_EXPANDED_TARGETS / 2);
    validate_alias_chain_targets(&cfg).unwrap();
    repeats(&mut cfg, "root", "leaf", MAX_EXPANDED_TARGETS / 2 + 1);
    assert!(validate_alias_chain_targets(&cfg).is_err());
}

fn nested(cfg: &mut Config, count: usize) {
    let mut next = "leaf".to_string();
    for i in (0..count).rev() {
        let key = format!("level-{i}");
        cfg.aliases.insert(key.clone(), AliasValue::Single(next));
        next = key;
    }
}

#[test]
fn exact_depth_bound_and_shared_suffix_depth_are_checked() {
    let mut cfg = config();
    nested(&mut cfg, ALIAS_MAX_RECURSION_DEPTH + 1);
    validate_alias_chain_targets(&cfg).unwrap();
    // "a" visits this suffix first, before the longer root path. Its cached
    // count must carry height or the subsequent deep path would slip through.
    cfg.aliases
        .insert("a".into(), AliasValue::Single("level-2".into()));
    cfg.aliases
        .insert("root".into(), AliasValue::Single("level-0".into()));
    let err = validate_alias_chain_targets(&cfg).unwrap_err().to_string();
    assert!(err.contains("depth 8"), "{err}");
}

#[test]
fn cycles_still_name_the_cycle_and_do_not_enter_size_recursion() {
    let mut cfg = config();
    cfg.aliases
        .insert("a".into(), AliasValue::Single("b".into()));
    cfg.aliases
        .insert("b".into(), AliasValue::Single("a".into()));
    let err = validate_alias_chain_targets(&cfg).unwrap_err().to_string();
    assert!(err.contains("cycle detected: a -> b -> a"), "{err}");
}

#[test]
fn size_error_sanitizes_the_operator_supplied_alias() {
    let mut cfg = config();
    repeats(
        &mut cfg,
        "hostile\n\x1b[31m",
        "leaf",
        MAX_EXPANDED_TARGETS + 1,
    );
    let err = validate_alias_chain_targets(&cfg).unwrap_err().to_string();
    assert!(!err.contains('\n') && !err.contains('\x1b'), "{err:?}");
    assert!(err.contains("shorten the fallback chain"), "{err}");
}

#[test]
fn enormous_but_shallow_dag_saturates_instead_of_overflowing_size() {
    let mut cfg = config();
    let mut next = "leaf".to_string();
    for i in (0..=ALIAS_MAX_RECURSION_DEPTH).rev() {
        let name = format!("level-{i}");
        repeats(&mut cfg, &name, &next, 1000);
        next = name;
    }
    let mut memo = BTreeMap::new();
    assert_eq!(
        expanded_size(&cfg, "level-0", 0, &mut memo).0,
        MAX_EXPANDED_TARGETS + 1
    );
    assert_eq!(memo.len(), ALIAS_MAX_RECURSION_DEPTH + 1);
    assert!(validate_alias_chain_targets(&cfg).is_err());
}
