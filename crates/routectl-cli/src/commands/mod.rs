//! CLI subcommand implementations: test, config, login, logout, refresh,
//! whoami, seat, usage, prompt_size, catalog, catalog_import, rc.
//!
//! `serve` is in `crate::server`. Each module here exposes one entry function
//! called from `main.rs`'s clap match arms.

pub mod catalog;
pub mod catalog_import;
pub mod config;
pub mod config_edit;
pub mod config_effective;
pub mod config_migrate_cmd;
pub mod edit_pipeline;
pub mod login;
pub mod logout;
pub mod prompt_size;
pub mod provider_add;
pub mod provider_env;
pub mod rc;
pub mod refresh;
pub mod seat;
pub mod test;
pub mod usage;
pub mod whoami;
