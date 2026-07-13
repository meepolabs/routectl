//! Regenerates the committed `routectl.schema.json` at the repo root from
//! the `Config` type's `schemars` derivation.
//!
//! `cargo run --bin gen_schema` (a repo binary, not `build.rs`). The golden
//! test in `crate::schema_gen` pins the committed file to
//! `render_schema_json`'s output, so this must be re-run and the result
//! committed whenever the config surface changes.

use std::path::PathBuf;

fn main() {
    let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../routectl.schema.json");
    let schema = routectl_router::schema_gen::render_schema_json();
    std::fs::write(&out_path, schema)
        .unwrap_or_else(|e| panic!("gen_schema: failed to write {}: {e}", out_path.display()));
    println!("wrote {}", out_path.display());
    println!("run `cargo test -p routectl-router` to verify the golden test.");
}
