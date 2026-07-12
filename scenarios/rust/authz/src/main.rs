//! Scenario "authz": the realistic JWT-authorizer hot path, mixing native crypto
//! with in-language work (the counterpart to `lettercount`).
//!
//! The signed RS256 JWT arrives in the invoke payload as `{ "token": "<jwt>" }`,
//! exactly as a real authorizer receives the token with the request. The RSA
//! public verification key is embedded at build time as a JWK. Each invoke reads
//! the token, verifies its RS256 signature and the iss/aud/exp claims, then
//! extracts a configured set of claims with type-mapping the way a real
//! authorizer does before handing them to a policy engine.
//!
//! The fairness split: RSA verification runs in a native crypto library
//! (`jsonwebtoken` -> AWS-LC here, WebCrypto/OpenSSL on Node), configured
//! symmetrically across runtimes (see README "Fairness note"). But RS256 verify
//! is a cheap public-key op, so native crypto is only a small slice; the
//! surrounding in-language glue (base64url decode, JSON parse of header/payload,
//! claim extraction/type-mapping) dominates and is what spreads the runtimes
//! apart. The warm gap stays moderate and flat across memory tiers (the native
//! verify is not CPU-starved at 128 MB the way pure-JS work is), placing `authz`
//! between lettercount (all in-language, wide spread) and a pure-crypto tie. Cold
//! start spreads more strongly, by how lean each runtime is at startup.

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};

/// The public verification key, embedded at build time as a JWK. Matches the
/// fixture private key the token is signed with. The `kid` here must equal the
/// one in the token header. See `bencher/fixtures/README.md`.
const PUBLIC_JWK: &str = include_str!("../../../../bencher/fixtures/authz_public_jwk.json");

/// The audience and issuer the tokens are minted with; validated on every
/// verify, exactly as a real authorizer pins its expected issuer/audience.
const EXPECTED_AUD: &str = "lambdabench-gateway";
const EXPECTED_ISS: &str = "https://idp.lambdabench.example/";

/// The claims a real authorizer extracts and type-maps before handing them to a
/// policy engine (modeled on a typical API-gateway claims extractor). Each entry
/// is (claim name, target type).
///
/// Fairness: never give `ClaimType::String` a claim whose token value is a JSON
/// number. Each language stringifies numbers differently (Rust
/// `Number::to_string`, Go `big.Float.Text`, Python/Java/Node their own float
/// formatting), which would break the same-task invariant. Every claim below is a
/// string, boolean, or set in the fixture token.
///
/// This is value-equivalence, not byte-for-byte: `serde_json::Map` (no
/// `preserve_order`) serializes keys alphabetically while Node/Python preserve
/// the insertion order above (Go also alphabetizes). The bencher never diffs
/// payloads across languages, so ordering does not affect fairness; keep the
/// values equivalent.
const CLAIM_CONFIG: &[(&str, ClaimType)] = &[
    ("sub", ClaimType::String),
    ("email", ClaimType::String),
    ("email_verified", ClaimType::Bool),
    ("cognito:groups", ClaimType::Set),
    ("scope", ClaimType::String),
    ("tenant_id", ClaimType::String),
    ("roles", ClaimType::Set),
];

#[derive(Clone, Copy)]
enum ClaimType {
    String,
    Bool,
    Set,
}

/// Holds the verification key + validation rules, built once at init and reused
/// across warm invokes (the per-request token arrives in the event payload).
struct State {
    decoding_key: jsonwebtoken::DecodingKey,
    validation: jsonwebtoken::Validation,
}

/// Builds the decoding key from the embedded public JWK (RSA n/e components),
/// the same way an authorizer builds a key after fetching a JWKS.
fn decoding_key_from_jwk() -> Result<jsonwebtoken::DecodingKey, Error> {
    let jwk: Value = serde_json::from_str(PUBLIC_JWK)?;
    let n = jwk["n"].as_str().ok_or("JWK missing n")?;
    let e = jwk["e"].as_str().ok_or("JWK missing e")?;
    Ok(jsonwebtoken::DecodingKey::from_rsa_components(n, e)?)
}

/// Verifies one token's signature + iss/aud/exp and extracts the configured
/// claims with type mapping. Returns the extracted claim map. This is the full
/// authorizer hot path: the verify is native-crypto-bound, the extraction is
/// in-language.
fn authorize(token: &str, state: &State) -> Result<Value, Error> {
    // 1. Signature + standard-claim verification (RS256, native RSA via AWS-LC).
    let data = jsonwebtoken::decode::<Value>(token, &state.decoding_key, &state.validation)?;
    let payload = data.claims;

    // 2. Claim extraction + type mapping (in-language), mirroring a real
    //    authorizer preparing claims for a policy engine.
    let mut extracted = serde_json::Map::new();
    for (name, ty) in CLAIM_CONFIG {
        let Some(value) = payload.get(*name) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let mapped = match ty {
            ClaimType::String => map_to_string(value),
            ClaimType::Bool => map_to_bool(value),
            ClaimType::Set => map_to_set(value),
        };
        if let Some(m) = mapped {
            extracted.insert((*name).to_string(), m);
        }
    }
    Ok(Value::Object(extracted))
}

/// String mapping: strings pass through; objects/arrays are JSON-serialized;
/// other primitives are stringified. Mirrors the Node side's `mapToString`.
fn map_to_string(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => Some(Value::String(s.clone())),
        Value::Object(_) | Value::Array(_) => serde_json::to_string(value).ok().map(Value::String),
        Value::Bool(b) => Some(Value::String(b.to_string())),
        Value::Number(n) => Some(Value::String(n.to_string())),
        Value::Null => None,
    }
}

/// Boolean mapping: real booleans pass through; the strings "true"/"false" map
/// to booleans; anything else is dropped. Mirrors the Node side's `mapToBoolean`.
fn map_to_bool(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(b) => Some(Value::Bool(*b)),
        Value::String(s) if s == "true" => Some(Value::Bool(true)),
        Value::String(s) if s == "false" => Some(Value::Bool(false)),
        _ => None,
    }
}

/// Set mapping: arrays are filtered to their string elements; a single string is
/// wrapped in a one-element array. Mirrors the Node side's `mapToSet`.
fn map_to_set(value: &Value) -> Option<Value> {
    match value {
        Value::Array(arr) => {
            let strings: Vec<Value> = arr.iter().filter(|v| v.is_string()).cloned().collect();
            Some(Value::Array(strings))
        }
        Value::String(s) => Some(Value::Array(vec![Value::String(s.clone())])),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Init phase: build the verification key + validation rules from the embedded
    // JWK. No S3, no AWS clients; the per-request token arrives in the payload.
    // Canonical validation shared across all languages (see
    // bencher/fixtures/README.md "validation rules"): RS256 only, `exp` required,
    // 60 s leeway on `exp`/`nbf`, `nbf` validated when present, `aud`/`iss` pinned
    // and required. `Validation::new` already requires `exp`, sets the 60 s
    // leeway, and pins RS256; only `nbf` validation defaults off and must be
    // enabled explicitly.
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.validate_nbf = true;
    validation.set_audience(&[EXPECTED_AUD]);
    validation.set_issuer(&[EXPECTED_ISS]);
    // jsonwebtoken validates `iss`/`aud` only when present, so a token omitting
    // them would pass. jose (Node), PyJWT (Python), and the Java handler all
    // require both to be present, so require them here too; otherwise Rust alone
    // would accept a token missing iss/aud. `set_required_spec_claims` replaces
    // the default set, so re-list `exp` to keep it required.
    validation.set_required_spec_claims(&["exp", "iss", "aud"]);

    let state = State {
        decoding_key: decoding_key_from_jwk()?,
        validation,
    };

    let state = std::sync::Arc::new(state);
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Value>| {
        let state = state.clone();
        async move {
            // The JWT arrives in the invoke payload as { "token": "<jwt>" },
            // exactly as a real authorizer receives the token with the request.
            let token = event
                .payload
                .get("token")
                .and_then(|t| t.as_str())
                .ok_or("invoke payload missing string field 'token'")?;
            let claims = authorize(token, &state)?;
            Ok::<Value, Error>(json!({ "scenario": "authz", "authorized": true, "claims": claims }))
        }
    }))
    .await
}
