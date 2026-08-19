//! CLI subcommand implementations: test, config, login, logout, refresh,
//! whoami, seat, usage, prompt_size, catalog, catalog_import, rc.
//!
//! `serve` is in `crate::server`. Each module here exposes one entry function
//! called from `main.rs`'s clap match arms.

pub mod capability_legacy;
pub mod catalog;
pub mod catalog_import;
pub mod config;
pub mod config_edit;
pub mod config_effective;
pub mod config_migrate_cmd;
pub mod doctor;
pub mod doctor_panels;
pub mod edit_pipeline;
pub mod init;
pub mod login;
pub mod login_provider_block;
pub mod logout;
pub mod parse_error_redaction;
pub mod probe;
pub mod prompt_size;
pub mod provider_add;
pub mod provider_env;
pub mod rc;
pub mod refresh;
pub mod seat;
pub mod seat_report;
pub mod staleness_hint;
pub mod test;
pub mod usage;
pub mod whoami;
