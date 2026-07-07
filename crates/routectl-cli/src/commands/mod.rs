//! CLI subcommand implementations: test, config, login, logout, refresh,
//! whoami, seat, usage, prompt_size, pricing, rc.
//!
//! `serve` is in `crate::server`. Each module here exposes one entry function
//! called from `main.rs`'s clap match arms.

pub mod config;
pub mod login;
pub mod logout;
pub mod pricing;
pub mod prompt_size;
pub mod rc;
pub mod refresh;
pub mod seat;
pub mod test;
pub mod usage;
pub mod whoami;
