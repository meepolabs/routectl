//! Library surface of routectl-cli used by integration tests.
//!
//! Binary entry point is `src/main.rs`. Tests import from this crate root.

pub mod commands;
pub mod config_classify;
pub mod handlers;
pub mod ingress;
pub mod proxy;
pub mod server;
#[cfg(test)]
pub(crate) mod test_secret;
