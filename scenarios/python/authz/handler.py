# Scenario "authz": the realistic JWT-authorizer hot path, mixing native crypto
# with in-language work (the counterpart to lettercount).
#
# The signed RS256 JWT arrives in the invoke payload as { "token": "<jwt>" },
# exactly as a real authorizer receives the token with the request. The RSA
# public key is loaded once at init from the embedded JWK. Each invoke reads the
# token, verifies its RS256 signature and the iss/aud/exp claims, then extracts a
# configured set of claims with type-mapping the way a real authorizer does before
# handing them to a policy engine.
#
# The fairness split: RSA verification runs in a native crypto library (PyJWT ->
# cryptography -> OpenSSL here; WebCrypto/OpenSSL on Node; AWS-LC on Rust; JCA on
# Java), configured symmetrically across runtimes (see README "Fairness note").
# But RS256 verify is a cheap public-key op, so native crypto is only a small
# slice; the surrounding in-language glue (base64url decode, JSON parse of
# header/payload, claim extraction/type-mapping) dominates and is what spreads the
# runtimes apart. The warm gap stays moderate and flat across memory tiers (the
# native verify is not CPU-starved at 128 MB the way pure in-language work is),
# placing authz between lettercount (all in-language, wide spread) and a
# pure-crypto tie. Cold start spreads more strongly, by how lean each runtime is
# at startup.

import json
import os

import jwt

EXPECTED_AUD = "lambdabench-gateway"
EXPECTED_ISS = "https://idp.lambdabench.example/"

# The claims a real authorizer extracts and type-maps before handing them to a
# policy engine (modeled on a typical API-gateway JWT claims extractor). [name, type].
# Fairness: no "String" entry holds a numeric token value, since each language
# stringifies numbers differently; every claim below is a string, boolean, or set
# in the fixture token.
CLAIM_CONFIG = [
    ("sub", "String"),
    ("email", "String"),
    ("email_verified", "Boolean"),
    ("cognito:groups", "Set"),
    ("scope", "String"),
    ("tenant_id", "String"),
    ("roles", "Set"),
]

# Init phase: load the verification key once from the embedded JWK (the same
# generated public key the other languages embed). No S3, no AWS clients; the
# per-request token arrives in the invoke payload.
_here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(_here, "authz_public_jwk.json"), encoding="utf-8") as _f:
    _KEY = jwt.PyJWK.from_json(_f.read())


def map_to_string(value):
    # Strings pass through; objects/arrays are JSON-serialized; other primitives
    # are stringified. Mirrors the other languages' map_to_string. Booleans are
    # rendered lowercase (true/false) to match JSON/JS, not Python's True/False.
    if isinstance(value, str):
        return value
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (dict, list)):
        return json.dumps(value, separators=(",", ":"))
    return str(value)


def map_to_boolean(value):
    # Real booleans pass through; "true"/"false" strings map to booleans;
    # anything else is dropped. Mirrors the other languages' map_to_bool.
    if isinstance(value, bool):
        return value
    if value == "true":
        return True
    if value == "false":
        return False
    return None


def map_to_set(value):
    # Arrays filtered to their string elements; a single string is wrapped in a
    # one-element list. Mirrors the other languages' map_to_set.
    if isinstance(value, list):
        return [v for v in value if isinstance(v, str)]
    if isinstance(value, str):
        return [value]
    return None


def extract_claims(payload):
    extracted = {}
    for name, kind in CLAIM_CONFIG:
        value = payload.get(name)
        if value is None:
            continue
        if kind == "String":
            mapped = map_to_string(value)
        elif kind == "Boolean":
            mapped = map_to_boolean(value)
        else:  # "Set"
            mapped = map_to_set(value)
        if mapped is not None:
            extracted[name] = mapped
    return extracted


def handler(event, context):
    # The JWT arrives in the invoke payload as { token: "<jwt>" }, exactly as a
    # real authorizer receives the token with the request.
    token = event.get("token") if isinstance(event, dict) else None
    if not isinstance(token, str):
        raise RuntimeError("invoke payload missing string field 'token'")

    # Signature + standard-claim verification (RS256, native RSA via OpenSSL).
    # Canonical rules shared across all languages (see
    # bencher/fixtures/README.md "validation rules"): RS256 only, exp/iss/aud
    # required, 60 s leeway on exp/nbf, nbf validated when present, aud/iss pinned.
    # PyJWT validates aud/iss only when present, so listing them in `require` makes
    # a token missing iss/aud fail loud, matching the other languages. Pinning
    # algorithms forbids confusion downgrades.
    payload = jwt.decode(
        token,
        _KEY,
        algorithms=["RS256"],
        audience=EXPECTED_AUD,
        issuer=EXPECTED_ISS,
        leeway=60,
        options={"require": ["exp", "iss", "aud"]},
    )

    # Claim extraction + type mapping (in-language).
    claims = extract_claims(payload)
    return {"scenario": "authz", "authorized": True, "claims": claims}
