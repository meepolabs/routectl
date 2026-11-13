//! CLI subcommand implementations: test, config, login, logout, refresh,
//! whoami, seat, usage, usage_downgrade, prompt_size, prices, rc.
//!
//! `serve` is in `crate::server`. Each module here exposes one entry function
//! called from `main.rs`'s clap match arms.

pub mod capability_purge;
pub mod config;
pub mod config_edit;
pub mod config_effective;
pub mod config_member_build;
mod config_settings;
pub mod doctor;
pub mod doctor_panels;
pub mod edit_pipeline;
pub mod init;
pub mod login;
pub mod login_provider_block;
pub mod login_surface;
mod login_surface_availability;
pub mod logout;
pub mod opening_accuracy;
pub mod parse_error_redaction;
pub mod prices;
pub mod pricing;
pub mod probe;
pub mod prompt_size;
pub mod provider_add;
pub mod provider_env;
pub mod rc;
pub mod refresh;
pub mod seat;
pub mod seat_report;
pub mod test;
mod text_table;
pub mod usage;
mod usage_cache_ignore;
pub mod usage_downgrade;
pub mod whoami;
