// Scenario "authz": the realistic JWT-authorizer hot path, mixing native crypto
// with in-language work (the counterpart to lettercount).
//
// The signed RS256 JWT arrives in the invoke payload as { "token": "<jwt>" },
// exactly as a real authorizer receives the token with the request. The RSA
// public key is imported once at init from the embedded JWK. Each invoke reads
// the token, verifies its RS256 signature and the iss/aud/exp claims, then
// extracts a configured set of claims with type-mapping the way a real authorizer
// does before handing them to a policy engine.
//
// The fairness split: RSA verification runs in a native crypto library (jose ->
// Node's WebCrypto/OpenSSL here, AWS-LC on Rust), configured symmetrically across
// runtimes (see README "Fairness note"). But RS256 verify is a cheap public-key
// op, so native crypto is only a small slice; the surrounding in-language glue
// (base64url decode, JSON.parse of header/payload, claim extraction/type-mapping)
// dominates and is what spreads the runtimes apart. The warm gap stays moderate
// and flat across memory tiers (the native verify is not CPU-starved at 128 MB
// the way pure JS work is), placing authz between lettercount (all in-language,
// wide spread) and a pure-crypto tie. Cold start spreads more strongly, by how
// lean each runtime is at startup.

import { importJWK, jwtVerify } from "jose";

// The public verification key, embedded at build time as a JWK, matching the
// fixture private key the token is signed with (see bencher/fixtures/README.md).
// esbuild inlines this JSON import at bundle time.
import PUBLIC_JWK from "../../../bencher/fixtures/authz_public_jwk.json" with { type: "json" };

const EXPECTED_AUD = "lambdabench-gateway";
const EXPECTED_ISS = "https://idp.lambdabench.example/";

// The claims a real authorizer extracts and type-maps before handing them to a
// policy engine (modeled on a typical API-gateway JWT claims extractor). [name, type].
// Fairness: no "String" entry holds a numeric token value, since each language
// stringifies numbers differently; every claim below is a string, boolean, or set
// in the fixture token.
const CLAIM_CONFIG = [
  ["sub", "String"],
  ["email", "String"],
  ["email_verified", "Boolean"],
  ["cognito:groups", "Set"],
  ["scope", "String"],
  ["tenant_id", "String"],
  ["roles", "Set"],
];

// Init phase: import the verification key once. No S3, no AWS clients; the
// per-request token arrives in the invoke payload.
const key = await importJWK(PUBLIC_JWK, "RS256");

// String mapping: strings pass through; objects/arrays are JSON-serialized;
// other primitives are stringified. Mirrors the Rust side's map_to_string.
function mapToString(value) {
  if (typeof value === "string") return value;
  if (typeof value === "object" && value !== null) return JSON.stringify(value);
  return String(value);
}

// Boolean mapping: real booleans pass through; "true"/"false" strings map to
// booleans; anything else is dropped. Mirrors the Rust side's map_to_bool.
function mapToBoolean(value) {
  if (typeof value === "boolean") return value;
  if (value === "true") return true;
  if (value === "false") return false;
  return undefined;
}

// Set mapping: arrays filtered to their string elements; a single string is
// wrapped in a one-element array. Mirrors the Rust side's map_to_set.
function mapToSet(value) {
  if (Array.isArray(value)) return value.filter((v) => typeof v === "string");
  if (typeof value === "string") return [value];
  return undefined;
}

function extractClaims(payload) {
  const extracted = {};
  for (const [name, type] of CLAIM_CONFIG) {
    const value = payload[name];
    if (value === null || value === undefined) continue;
    let mapped;
    if (type === "String") mapped = mapToString(value);
    else if (type === "Boolean") mapped = mapToBoolean(value);
    else if (type === "Set") mapped = mapToSet(value);
    if (mapped !== undefined) extracted[name] = mapped;
  }
  return extracted;
}

export const handler = async (event) => {
  // The JWT arrives in the invoke payload as { token: "<jwt>" }, exactly as a
  // real authorizer receives the token with the request.
  const token = event?.token;
  if (typeof token !== "string") {
    throw new Error("invoke payload missing string field 'token'");
  }

  // Signature + standard-claim verification (RS256, native RSA via WebCrypto).
  // Canonical rules shared across all languages (see
  // bencher/fixtures/README.md "validation rules"): RS256 only, `exp` required,
  // 60 s clock tolerance on `exp`/`nbf`, `nbf` validated when present, `aud`/`iss`
  // pinned. jose does not require `exp` or apply a tolerance by default, so both
  // are set explicitly. `algorithms` must pin RS256: otherwise jose accepts any
  // algorithm the RSA key can verify (PS256, RS384, …), an algorithm-confusion
  // gap the other languages close explicitly.
  const { payload } = await jwtVerify(token, key, {
    algorithms: ["RS256"],
    audience: EXPECTED_AUD,
    issuer: EXPECTED_ISS,
    requiredClaims: ["exp"],
    clockTolerance: 60,
  });

  // Claim extraction + type mapping (in-language).
  const claims = extractClaims(payload);
  return { scenario: "authz", authorized: true, claims };
};
