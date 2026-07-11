//! Regenerates `crates/routectl-router/src/catalog_baked.rs` from the
//! vendored snapshots under `catalog_data/`.
//!
//! `cargo run --bin gen_catalog` (a repo binary, not `build.rs` -- see
//! `crate::catalog_codegen`'s module doc for why). Fails loudly (nonzero
//! exit, message on stderr) on a parse error or an un-allowlisted
//! models.dev-vs-litellm mismatch; never writes a partial or fallback
//! file on failure.

#[cfg(not(feature = "gen-catalog"))]
compile_error!(
    "gen_catalog requires the `gen-catalog` feature: cargo run --bin gen_catalog --features gen-catalog"
);

use std::path::PathBuf;

fn main() {
    let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/catalog_baked.rs");
    let source = routectl_router::catalog_codegen::render_catalog_baked_rs();
    std::fs::write(&out_path, source)
        .unwrap_or_else(|e| panic!("gen_catalog: failed to write {}: {e}", out_path.display()));
    println!("wrote {}", out_path.display());
    println!("run `cargo fmt` next, then `cargo test -p routectl-router`.");
}
