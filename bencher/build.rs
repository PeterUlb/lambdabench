//! Generates the `authz` scenario crypto fixtures before compilation.
//!
//! `src/aws/lambda.rs` embeds `fixtures/authz_token.txt` via `include_str!`, so
//! that file must exist before the crate compiles. The fixtures (RSA private
//! key, public JWK, signed JWT) are NOT committed (they trip secret scanners),
//! so they are generated here at build time by `fixtures/generate.mjs`
//! (idempotent: a no-op if they already exist). Node is a required toolchain dep.

use std::process::Command;

fn main() {
    let script = "fixtures/generate.mjs";
    // Re-run if the generator changes; the fixtures themselves are gitignored and
    // generated, so we do not track them as rerun-if-changed inputs.
    println!("cargo:rerun-if-changed={script}");

    let status = Command::new("node")
        .arg(script)
        .status()
        .expect("failed to run node for authz fixture generation (is node on PATH?)");
    if !status.success() {
        panic!("authz fixture generation ({script}) failed");
    }
}
