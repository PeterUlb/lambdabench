# rust-sdk-init: one-off `aws_config::load()` probe

Source for the Rust `aws_config::load()` table in
[`site/src/lifecycle.md`](../../site/src/lifecycle.md) ("Rust vs Go: same work, different
phase"). It is not part of the benchmark matrix: `bencher` never builds or
deploys it, and its numbers are dated one-off characterizations, not committed data.

## What it measures

One Lambda function per configuration, each a minimal custom-runtime binary (no
`lambda_runtime`) that times one variant with `Instant` and returns the elapsed ms:

| Config           | What is timed                                                              |
| ---------------- | -------------------------------------------------------------------------- |
| `default-init`   | first `aws_config::load()`, default settings, at init                      |
| `noop-init`      | first `load()` with a no-op HTTP client (never builds a TLS connector)     |
| `static-init`    | first `load()` with static credentials (no provider chain)                 |
| `noop-noop`      | second no-op `load()`, after an untimed first one                          |
| `static-static`  | second static-credentials `load()`, after an untimed first one             |
| `static-default` | default `load()` after an untimed static one: client + credential chain    |
| `static-certs`   | `rustls_native_certs::load_native_certs()` after the same untimed `load()` |
| `default-invoke` | first default `load()`, inside the first invocation instead of at init     |

The loader settings match `scenarios/rust/oneclient`. The binary is built with
`AWS_LC_SYS_NO_JITTER_ENTROPY=1` (the site default), so the jitter-entropy seeding cost is not in
these numbers. As a workspace member it resolves against the root `Cargo.lock`, so it measures
the same SDK versions as the scenarios. It is built with the plain `release` profile, not the
scenarios' `lambda` profile (fat LTO, `codegen-units = 1`, `panic = "abort"`) or cargo-lambda, so
codegen differs slightly from the matrix binaries.

## Running it

```sh
oneoff/rust-sdk-init/run.sh        # 20 cold starts per config; or pass a count
```

It builds inside `amazonlinux:2023` with Docker (arm64 by default, emulated on an x86 host;
the first build takes several minutes), creates an IAM role and one function per config, forces
cold starts by changing an environment variable before each invoke, prints medians, and deletes
everything it created on exit, including on failure. Raw results land in
`.tmp/oneoff-rust-sdk-init/results-<timestamp>.tsv`.

Environment overrides: `AWS_REGION` (default `eu-central-1`), `PROBE_ARCH` (`arm64` or
`x86_64`), `PROBE_MEMORY` (default `128`). Resource names use the `oneoff-sdk-init-` prefix, so
the script and `bencher teardown` never touch each other's resources.

## Last run

2026-10-12, `eu-central-1`, arm64, 128 MB, 20 cold starts per config, `aws-config` 1.8.18,
`rustls-native-certs` 0.8.4. Medians: `default-init` ~55 ms, `noop-init` ~20 ms, `static-init`
~17 ms, `noop-noop` ~0.2 ms, `static-static` ~0.1 ms, `static-default` ~38 ms, `static-certs`
~30 ms, `default-invoke` ~900 ms. When re-running, update the table and its date in
`site/src/lifecycle.md` together with this section.
