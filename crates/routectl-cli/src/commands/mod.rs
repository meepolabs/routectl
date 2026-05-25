//! CLI subcommand implementations: test, config, login, logout, refresh, whoami.
//!
//! `serve` is in `crate::server`. Each module here exposes one entry function
//! called from `main.rs`'s clap match arms.

pub mod config;
pub mod login;
pub mod logout;
pub mod refresh;
pub mod test;
pub mod whoami;
