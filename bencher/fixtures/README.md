# authz scenario crypto fixtures

These files are **generated at build time, not committed**: they contain an RSA
private key (PEM) and a signed JWT, which could trip secret-scanning push protection
even though they are a throwaway benchmark key that guards nothing.

Tracked (committed):
- `generate.mjs`: the generator. Creates the keypair, public JWK, and signed
  token. Idempotent: a no-op if all three outputs already exist (so re-builds
  never mint a new key, which would invalidate the JWK embedded in already
  deployed functions). Force a fresh key with `node generate.mjs --force`.
- `README.md`: this file.

Generated (gitignored via the repo-root `.gitignore`):
- `authz_signing_key.pem`: RSA-2048 private key used to sign the token.
- `authz_public_jwk.json`: matching public key (JWK), embedded into every
  language's authorizer handler so each can verify the token.
- `authz_token.txt`: a single signed RS256 JWT (compact form, no trailing
  newline). The bencher sends it in the invoke payload of every `authz`
  invocation, exactly as a real authorizer receives a token with the request.

## How it gets generated

`generate.mjs` runs automatically before compilation. The bencher's own
`bencher/build.rs` runs it once (so the bencher embeds the token), and each
language's authz build pipeline either re-runs it (idempotent, a no-op if the
outputs exist) or copies the already-generated JWK in:

- **Rust:** `scenarios/rust/authz/build.rs` runs it; the handler embeds the
  JWK via `include_str!`.
- **Node:** `scenarios/node/build.mjs` imports the JWK before bundling.
- **Java:** `scenarios/java/authz/build.gradle.kts`'s `generateAuthzFixture`
  Gradle task runs it before compile; the handler reads the JWK from the
  classpath at init.
- **Python:** the bencher's own `bencher/src/build.rs` stages the JWK into the
  Python deployment zip alongside `handler.py`; the handler reads it at init.
- **Go:** same staging path: `bencher/src/build.rs` copies the JWK into the
  authz package dir before `go build` (Go's `//go:embed` cannot reference paths
  outside the module).

Node is a required toolchain dependency, so there is no new prerequisite.

The key is per-machine (each clone generates its own), which is fine: the signer
(`generate.mjs`) and the verifiers (the handlers, which embed the locally-
generated public JWK) always share one local key, and the benchmark never
compares tokens across machines. The token guards nothing, grants no access, and
is not used by any real system.

## Validation rules (kept identical across languages)

For the `authz` measurement to be fair, every language's verifier must do the
same validation work per invoke, so every language's handler enforces one canonical rule
set. The generated token satisfies all of them (it is signed RS256, carries a
far-future `exp`, a past `nbf`, and the expected `iss`/`aud`), so each verifier
takes the accept path and does equivalent work:

- **Algorithm:** RS256 only. Each handler pins it explicitly so a token-header
  algorithm downgrade (RS384/PS256/…) is rejected, not just whatever the RSA key
  can verify.
- **`exp`:** required and validated (an absent or expired `exp` is rejected).
- **`nbf`:** validated when present (a not-yet-valid token is rejected).
- **Clock leeway:** 60 s, applied to both `exp` and `nbf`.
- **`iss` / `aud`:** pinned to `https://idp.lambdabench.example/` and `lambdabench-gateway`,
  and required to be present (a token omitting either is rejected).

The libraries differ in their defaults, so each handler sets only the options
needed to reach this rule set. That per-library detail lives at each handler
(`scenarios/*/authz/`), which references this section as the canonical rule set.
