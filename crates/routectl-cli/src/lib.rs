//! Integration-test surface of routectl-cli.
//!
//! This crate is a binary (entry point `src/main.rs`); the items re-exported
//! here exist so the crate's own `tests/*.rs` integration binaries can compile
//! against them. This is test scaffolding, not a stable library API: modules
//! and items are exposed purely to satisfy in-repo consumers (the test binaries
//! and `main.rs`), and may change without notice. Do not depend on this crate
//! as a library.

#[cfg(test)]
pub(crate) mod alloc_probe;
#[cfg(test)]
#[global_allocator]
static ALLOC_PROBE: alloc_probe::ProbeAllocator = alloc_probe::ProbeAllocator;
pub mod commands;
pub(crate) mod config_classify;
pub mod config_folder;
#[doc(hidden)]
pub mod handlers;
pub mod ingress;
pub(crate) mod lane_cell;
pub mod log_sink;
#[doc(hidden)]
pub mod proxy;
pub mod server;
#[cfg(test)]
pub(crate) mod test_secret;
#[cfg(test)]
pub(crate) mod test_usage_dir;
