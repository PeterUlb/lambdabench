//! Generates the `authz` crypto fixtures before compilation.
//!
//! `src/main.rs` embeds the public JWK via `include_str!` from the bencher's
//! fixtures dir, so it must exist before this crate compiles. The fixtures are
//! NOT committed (they trip secret scanners); they are generated here at build
//! time by `bencher/fixtures/generate.mjs` (idempotent). Node is a required dep.

use std::path::Path;
use std::process::Command;

fn main() {
    // This crate lives at scenarios/rust/authz; the generator is four levels up.
    let script = Path::new("../../../bencher/fixtures/generate.mjs");
    println!("cargo:rerun-if-changed={}", script.display());

    let status = Command::new("node")
        .arg(script)
        .status()
        .expect("failed to run node for authz fixture generation (is node on PATH?)");
    if !status.success() {
        panic!("authz fixture generation failed");
    }
}
