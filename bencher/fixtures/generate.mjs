// Generates the `authz` scenario crypto fixtures, idempotently.
//
// Run by build.rs (bencher + the authz crate) before compilation, so the
// `include_str!`/JSON-import of the public JWK and the token resolve. Also
// runnable by hand: `node bencher/fixtures/generate.mjs [--force]`.
//
// WHY a generator instead of committing the files: the fixtures include an RSA
// PRIVATE KEY (PEM) and a signed JWT, which trip secret-scanning push protection
// even though they are a throwaway benchmark key that guards nothing. Generating
// them at build time keeps them out of git entirely (they are gitignored).
//
// The key is deterministic ONLY in purpose, not in bytes: each machine generates
// its own keypair. That is fine: the signer (this script) and the verifiers
// (the handlers, which import the generated public JWK) always share one local
// key, and the benchmark never compares tokens across machines.

import { generateKeyPairSync, createPrivateKey, sign } from "node:crypto";
import {
  writeFileSync,
  existsSync,
  mkdirSync,
  renameSync,
  openSync,
  closeSync,
  unlinkSync,
  statSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const PRIV = join(here, "authz_signing_key.pem");
const JWK = join(here, "authz_public_jwk.json");
const TOKEN = join(here, "authz_token.txt");
const LOCK = join(here, ".authz-fixtures.lock");

// cargo builds the bencher and the authz crate in parallel, and both run this
// script (each crate's build.rs). Without coordination, both would see the
// fixtures missing and each generate its OWN keypair, and the renames could
// interleave to leave a JWK and a token signed by different keys: a
// broken authz fixture baked into every language's artifact. So generation runs
// under a cross-process exclusive lock: exactly one process generates one
// keypair; the other waits for it to finish and then no-ops.
const LOCK_STALE_MS = 60_000;
const sleepSync = (ms) => {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
};

// Runs `fn` while holding an exclusive lock. `openSync(..., "wx")` is an atomic
// create-if-absent, so at most one process holds the lock at a time. A waiter
// spins until the holder releases; a lock left behind by a crashed process is
// reclaimed once it goes stale, so a failed build can never deadlock the next.
const withLock = (fn) => {
  const deadline = Date.now() + LOCK_STALE_MS;
  for (;;) {
    let fd;
    try {
      fd = openSync(LOCK, "wx");
    } catch (e) {
      if (e.code !== "EEXIST") throw e;
      try {
        if (Date.now() - statSync(LOCK).mtimeMs > LOCK_STALE_MS) {
          unlinkSync(LOCK); // stale holder; reclaim it
          continue;
        }
      } catch {
        continue; // lock vanished between open and stat; retry the acquire
      }
      if (Date.now() > deadline) {
        throw new Error(`timed out waiting for ${LOCK}`);
      }
      sleepSync(50);
      continue;
    }
    try {
      return fn();
    } finally {
      closeSync(fd);
      try {
        unlinkSync(LOCK);
      } catch {
        // already reclaimed as stale by another process; nothing to do
      }
    }
  }
};

const KID = "lambdabench-key-1";
const ISS = "https://idp.lambdabench.example/";
const AUD = "lambdabench-gateway";

const force = process.argv.includes("--force");

// Idempotent: regenerate only if forced or any output is missing. The common
// warm-build case (all three present) is a no-op. NOTE: must NOT call
// process.exit() here: this module is `await import()`ed by build.mjs, and
// exiting would kill that whole process before its esbuild loop runs. Use a
// plain guard around the generation body instead so importers continue cleanly.
const missing = () => !existsSync(PRIV) || !existsSync(JWK) || !existsSync(TOKEN);

if (force || missing()) {
  mkdirSync(here, { recursive: true });
  withLock(() => {
    // Re-check under the lock: a concurrent process may have generated the
    // fixtures while we waited for the lock, in which case there is nothing to
    // do (never overwrite a valid keypair, since another artifact may already
    // embed it). `--force` still regenerates once, under the lock.
    if (!force && !missing()) return;
    generate();
  });
}

// Generates one RSA keypair and writes all three fixtures. Callers must hold the
// lock so the keypair and the token it signs are always written as a set.
function generate() {
  // 1. RSA-2048 keypair (what a real OIDC IdP uses for RS256).
  const { publicKey, privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });

  // 2. Public key as a JWK, tagged with the kid/alg the handlers expect.
  const jwk = publicKey.export({ format: "jwk" });
  jwk.kid = KID;
  jwk.alg = "RS256";
  jwk.use = "sig";

  // 3. Private key PEM (PKCS#8), used to sign the token below.
  const privPem = privateKey.export({ format: "pem", type: "pkcs8" });

  // 4. A single signed RS256 JWT with a realistic OIDC-style claim set, mirroring
  //    what the bencher sends in the invoke payload. Fixed wide iat/exp bounds so
  //    it stays valid for the life of the benchmark (these are benchmark tokens).
  const b64u = (o) => Buffer.from(JSON.stringify(o)).toString("base64url");
  const header = { alg: "RS256", typ: "JWT", kid: KID };
  const payload = {
    iss: ISS,
    aud: AUD,
    sub: "user-00042",
    iat: 1700000000,
    nbf: 1700000000,
    exp: 4102444800, // 2100-01-01
    email: "user42@example.com",
    email_verified: true,
    "cognito:groups": ["admin", "developer", "readonly"],
    scope: "openid profile email mcp:invoke",
    tenant_id: "tenant-7",
    org: "acme-corp",
    roles: ["editor", "viewer"],
  };
  const signingInput = `${b64u(header)}.${b64u(payload)}`;
  const signature = sign("RSA-SHA256", Buffer.from(signingInput), createPrivateKey(privPem)).toString(
    "base64url"
  );
  const token = `${signingInput}.${signature}`;

  // The lock guarantees a single writer, so the three files are always a
  // consistent set (one keypair, and the token it signed). Each file is still
  // written temp-then-rename so a reader that races the writer (e.g. the
  // dependent crate's `include_str!`) sees a complete file rather than a
  // half-written one; rename() is atomic on the same filesystem.
  const tmp = (p) => `${p}.tmp.${process.pid}`;
  const atomicWrite = (p, data) => {
    const t = tmp(p);
    writeFileSync(t, data);
    renameSync(t, p);
  };
  atomicWrite(PRIV, privPem);
  atomicWrite(JWK, JSON.stringify(jwk));
  atomicWrite(TOKEN, token); // no trailing newline (handlers .trim() defensively anyway)

  console.error(`[authz fixtures] generated ${PRIV}, ${JWK}, ${TOKEN}`);
}
