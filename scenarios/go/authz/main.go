// Scenario "authz": the realistic JWT-authorizer hot path, mixing native crypto
// with in-language work (the counterpart to lettercount).
//
// The signed RS256 JWT arrives in the invoke payload as { "token": "<jwt>" },
// exactly as a real authorizer receives the token with the request. The RSA
// public key used to verify it is embedded at build time as a JWK. Each invoke
// reads the token, verifies its RS256 signature and the iss/aud/exp claims, then
// extracts a configured set of claims with type-mapping the way a real authorizer
// does before handing them to a policy engine.
//
// The fairness split: RSA verification runs in a native crypto path (Go's
// crypto/rsa + crypto/sha256, the runtime's assembly-optimized big-integer and
// SHA routines), symmetric with Rust's AWS-LC, Node's WebCrypto/OpenSSL, Java's
// JCA, and Python's cryptography/OpenSSL (see README "Fairness note"). Go's
// standard-library crypto is the production RSA path the ecosystem ships. But
// RS256 verify is a cheap public-key op, so native crypto is only a small slice;
// the surrounding in-language glue (base64url decode, JSON parse of
// header/payload, claim extraction/type-mapping) dominates and is what spreads
// the runtimes apart. The warm gap stays moderate and flat across memory tiers
// (the native verify is not CPU-starved at 128 MB the way pure in-language work
// is), placing authz between lettercount (all in-language, wide spread) and a
// pure-crypto tie. Cold start spreads more strongly, by how lean each runtime is
// at startup.
package main

import (
	"context"
	"crypto/rsa"
	_ "embed"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"log"
	"math/big"
	"time"

	"github.com/aws/aws-lambda-go/lambda"
	"github.com/golang-jwt/jwt/v5"
)

// The public verification key, embedded at build time as a JWK. The bencher's
// build copies the generated fixture into this package dir before compiling (Go
// embed cannot reference a path outside the module). Matches the fixture private
// key the token is signed with. See bencher/fixtures/README.md.
//
//go:embed authz_public_jwk.json
var publicJWK []byte

// The audience and issuer the tokens are minted with; validated on every verify,
// exactly as a real authorizer pins its expected issuer/audience.
const (
	expectedAud = "lambdabench-gateway"
	expectedIss = "https://idp.lambdabench.example/"
)

// claimType is the target type a configured claim is mapped to.
type claimType int

const (
	claimString claimType = iota
	claimBool
	claimSet
)

// claimConfig is the set of claims a real authorizer extracts and type-maps
// before handing them to a policy engine (modeled on a typical API-gateway JWT
// claims extractor). Order matches the other languages.
//
// Fairness: no claimString entry holds a numeric token value. Each language
// stringifies numbers differently, which would break the same-task invariant, so
// every claim below is a string, boolean, or set in the fixture token.
var claimConfig = []struct {
	name string
	kind claimType
}{
	{"sub", claimString},
	{"email", claimString},
	{"email_verified", claimBool},
	{"cognito:groups", claimSet},
	{"scope", claimString},
	{"tenant_id", claimString},
	{"roles", claimSet},
}

// state holds the verification key + parser, built once at init and reused
// across warm invokes (the per-request token arrives in the event payload).
type state struct {
	key    *rsa.PublicKey
	parser *jwt.Parser
}

type event struct {
	Token string `json:"token"`
}

type response struct {
	Scenario   string                 `json:"scenario"`
	Authorized bool                   `json:"authorized"`
	Claims     map[string]interface{} `json:"claims"`
}

// jwk is the subset of the JWK we need to rebuild the RSA public key.
type jwk struct {
	N string `json:"n"`
	E string `json:"e"`
}

// keyFromJWK rebuilds the RSA public key from the embedded JWK's base64url n/e
// components, the same way an authorizer builds a key after fetching a JWKS.
func keyFromJWK(raw []byte) (*rsa.PublicKey, error) {
	var k jwk
	if err := json.Unmarshal(raw, &k); err != nil {
		return nil, err
	}
	nBytes, err := base64.RawURLEncoding.DecodeString(k.N)
	if err != nil {
		return nil, fmt.Errorf("decoding JWK n: %w", err)
	}
	eBytes, err := base64.RawURLEncoding.DecodeString(k.E)
	if err != nil {
		return nil, fmt.Errorf("decoding JWK e: %w", err)
	}
	// The exponent is a big-endian unsigned integer of variable length; left-pad
	// to 8 bytes so it can be read as a uint64.
	padded := make([]byte, 8)
	copy(padded[8-len(eBytes):], eBytes)
	return &rsa.PublicKey{
		N: new(big.Int).SetBytes(nBytes),
		E: int(binary.BigEndian.Uint64(padded)),
	}, nil
}

// authorize verifies one token's signature + iss/aud/exp and extracts the
// configured claims with type mapping. Returns the extracted claim map. This is
// the full authorizer hot path: the verify is native-crypto-bound, the
// extraction is in-language.
func (s *state) authorize(token string) (map[string]interface{}, error) {
	// 1. Signature + standard-claim verification (RS256, native RSA + SHA-256).
	claims := jwt.MapClaims{}
	_, err := s.parser.ParseWithClaims(token, claims, func(*jwt.Token) (interface{}, error) {
		return s.key, nil
	})
	if err != nil {
		return nil, err
	}

	// 2. Claim extraction + type mapping (in-language), mirroring a real
	//    authorizer preparing claims for a policy engine.
	extracted := make(map[string]interface{})
	for _, c := range claimConfig {
		value, ok := claims[c.name]
		if !ok || value == nil {
			continue
		}
		var mapped interface{}
		switch c.kind {
		case claimString:
			mapped = mapToString(value)
		case claimBool:
			mapped = mapToBool(value)
		case claimSet:
			mapped = mapToSet(value)
		}
		if mapped != nil {
			extracted[c.name] = mapped
		}
	}
	return extracted, nil
}

// mapToString: strings pass through; objects/arrays are JSON-serialized; other
// primitives are stringified. Booleans render lowercase (true/false) to match
// JSON/JS. Mirrors the other languages' map_to_string.
func mapToString(value interface{}) interface{} {
	switch v := value.(type) {
	case string:
		return v
	case bool:
		if v {
			return "true"
		}
		return "false"
	case []interface{}, map[string]interface{}:
		b, err := json.Marshal(v)
		if err != nil {
			return nil
		}
		return string(b)
	case float64:
		// JSON numbers decode to float64; render without a trailing ".0" for
		// integers, matching the other languages' stringification.
		return new(big.Float).SetFloat64(v).Text('f', -1)
	default:
		return fmt.Sprintf("%v", v)
	}
}

// mapToBool: real booleans pass through; the strings "true"/"false" map to
// booleans; anything else is dropped (nil). Mirrors the other languages'
// map_to_bool.
func mapToBool(value interface{}) interface{} {
	switch v := value.(type) {
	case bool:
		return v
	case string:
		if v == "true" {
			return true
		}
		if v == "false" {
			return false
		}
	}
	return nil
}

// mapToSet: arrays are filtered to their string elements; a single string is
// wrapped in a one-element slice. Mirrors the other languages' map_to_set.
func mapToSet(value interface{}) interface{} {
	switch v := value.(type) {
	case []interface{}:
		out := make([]interface{}, 0, len(v))
		for _, e := range v {
			if s, ok := e.(string); ok {
				out = append(out, s)
			}
		}
		return out
	case string:
		return []interface{}{v}
	}
	return nil
}

func (s *state) handle(_ context.Context, e event) (response, error) {
	if e.Token == "" {
		return response{}, fmt.Errorf("invoke payload missing string field 'token'")
	}
	claims, err := s.authorize(e.Token)
	if err != nil {
		return response{}, err
	}
	return response{Scenario: "authz", Authorized: true, Claims: claims}, nil
}

func main() {
	// Init phase: build the verification key + parser from the embedded JWK. No
	// S3, no AWS clients; the per-request token arrives in the payload.
	//
	// Canonical validation shared across all languages (see
	// bencher/fixtures/README.md "validation rules"): RS256 only, exp required,
	// 60 s leeway on exp/nbf, nbf validated when present, aud/iss pinned and
	// required. golang-jwt: WithValidMethods pins the algorithm,
	// WithExpirationRequired makes exp mandatory, WithLeeway sets the clock
	// tolerance, WithAudience/WithIssuer pin those claims and require their
	// presence. nbf is validated when present by default.
	key, err := keyFromJWK(publicJWK)
	if err != nil {
		log.Fatalf("building verification key from JWK: %v", err)
	}
	parser := jwt.NewParser(
		jwt.WithValidMethods([]string{"RS256"}),
		jwt.WithExpirationRequired(),
		jwt.WithLeeway(60*time.Second),
		jwt.WithAudience(expectedAud),
		jwt.WithIssuer(expectedIss),
	)
	s := &state{key: key, parser: parser}
	lambda.Start(s.handle)
}
