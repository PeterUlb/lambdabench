package lambdabench;

import com.amazonaws.services.lambda.runtime.Context;
import com.amazonaws.services.lambda.runtime.RequestHandler;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.nimbusds.jose.JWSVerifier;
import com.nimbusds.jose.crypto.RSASSAVerifier;
import com.nimbusds.jose.jwk.RSAKey;
import com.nimbusds.jwt.SignedJWT;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Date;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import org.crac.Core;
import org.crac.Resource;

/**
 * Scenario "authz": the realistic JWT-authorizer hot path, mixing native crypto
 * with in-language work. The signed RS256 JWT arrives in the invoke payload as
 * {@code {"token":"<jwt>"}}, exactly as a real authorizer receives the token with
 * the request. The RSA public key is embedded as a JWK resource (the same fixture
 * the other handlers use). Each invoke verifies the signature and the iss/aud/exp
 * claims, then extracts a configured set of claims with type mapping.
 *
 * <p>RS256 verification runs in the JVM's native JCA crypto (SunRsaSign) via
 * nimbus-jose-jwt, mirroring Rust's AWS-LC and Node's WebCrypto, configured
 * symmetrically across runtimes (see README "Fairness note"). The claim set and
 * mapping mirror the other handlers so the comparison is like-for-like.
 *
 * <p>But RS256 verify is a cheap public-key op, so native crypto is only a small
 * slice; the surrounding in-language glue (base64url decode, JSON parse of
 * header/payload, claim extraction/type-mapping) dominates and is what spreads the
 * runtimes apart. The warm gap stays moderate and flat across memory tiers (the
 * native verify is not CPU-starved at 128 MB the way pure in-language work is),
 * placing authz between lettercount (all in-language, wide spread) and a
 * pure-crypto tie. Cold start spreads more strongly, by how lean each runtime is
 * at startup.
 *
 * <p>For SnapStart, the handler primes itself via a CRaC {@code beforeCheckpoint}
 * hook (one real verify of the bundled token fixture during init, baking the
 * nimbus-jose-jwt / JCA class loading and JIT into the snapshot). The hook fires
 * only when a snapshot is taken; a plain function uses org.crac's no-op context,
 * so the same jar is unprimed.
 */
public final class AuthzHandler implements RequestHandler<Map<String, Object>, Map<String, Object>>, Resource {

    private static final String EXPECTED_AUD = "lambdabench-gateway";
    private static final String EXPECTED_ISS = "https://idp.lambdabench.example/";

    /** Reused for JSON-serializing structured claim values in {@link #mapToString}. */
    private static final ObjectMapper MAPPER = new ObjectMapper();

    private enum ClaimType { STRING, BOOL, SET }

    /**
     * The claims a real authorizer extracts and type-maps, with target type.
     * Fairness: no STRING entry holds a numeric token value, since each language
     * stringifies numbers differently; every claim here is a string, boolean, or
     * set in the fixture token.
     */
    private static final Map<String, ClaimType> CLAIM_CONFIG = new LinkedHashMap<>();
    static {
        CLAIM_CONFIG.put("sub", ClaimType.STRING);
        CLAIM_CONFIG.put("email", ClaimType.STRING);
        CLAIM_CONFIG.put("email_verified", ClaimType.BOOL);
        CLAIM_CONFIG.put("cognito:groups", ClaimType.SET);
        CLAIM_CONFIG.put("scope", ClaimType.STRING);
        CLAIM_CONFIG.put("tenant_id", ClaimType.STRING);
        CLAIM_CONFIG.put("roles", ClaimType.SET);
    }

    /** The RSA verifier built once at init from the embedded public JWK. */
    private static final JWSVerifier VERIFIER;

    static {
        try (InputStream in = AuthzHandler.class.getResourceAsStream("/authz_public_jwk.json")) {
            if (in == null) {
                throw new IllegalStateException("authz_public_jwk.json not on classpath");
            }
            String jwkJson = new String(in.readAllBytes(), StandardCharsets.UTF_8);
            RSAKey rsaKey = RSAKey.parse(jwkJson);
            VERIFIER = new RSASSAVerifier(rsaKey.toRSAPublicKey());
        } catch (Exception e) {
            throw new RuntimeException("building authz verifier from JWK", e);
        }
    }

    public AuthzHandler() {
        Core.getGlobalContext().register(this);
    }

    @Override
    public void beforeCheckpoint(org.crac.Context<? extends Resource> context) {
        // Prime with the bundled token fixture (the same committed token the
        // bencher sends in the invoke payload), so the verify path's class
        // loading and JIT are captured in the snapshot.
        try (InputStream in = AuthzHandler.class.getResourceAsStream("/authz_token.txt")) {
            if (in == null) {
                throw new IllegalStateException("authz_token.txt not on classpath");
            }
            String token = new String(in.readAllBytes(), StandardCharsets.UTF_8).trim();
            handleRequest(Map.of("token", token), null);
        } catch (Exception e) {
            throw new RuntimeException("priming authz verify", e);
        }
    }

    @Override
    public void afterRestore(org.crac.Context<? extends Resource> context) {}

    @Override
    public Map<String, Object> handleRequest(Map<String, Object> event, Context context) {
        Object token = event == null ? null : event.get("token");
        if (!(token instanceof String tokenStr) || tokenStr.isEmpty()) {
            throw new IllegalArgumentException("invoke payload missing string field 'token'");
        }
        Map<String, Object> claims = authorize(tokenStr);

        Map<String, Object> result = new LinkedHashMap<>();
        result.put("scenario", "authz");
        result.put("authorized", true);
        result.put("claims", claims);
        return result;
    }

    /**
     * Verifies one token's RS256 signature + iss/aud/exp/nbf and extracts the
     * configured claims with type mapping. The verify is native-crypto-bound;
     * the parse + extraction is in-language work.
     */
    private static Map<String, Object> authorize(String token) {
        try {
            SignedJWT jwt = SignedJWT.parse(token);
            // Pin RS256 before verifying: an RSASSAVerifier accepts any RSA-family
            // alg the key supports (RS384/RS512/PS256/…), so without this the
            // header could downgrade the algorithm. The other languages pin it too.
            if (!com.nimbusds.jose.JWSAlgorithm.RS256.equals(jwt.getHeader().getAlgorithm())) {
                throw new SecurityException("unexpected JWS algorithm (expected RS256)");
            }
            if (!jwt.verify(VERIFIER)) {
                throw new SecurityException("JWT signature verification failed");
            }
            var c = jwt.getJWTClaimsSet();
            // Standard-claim validation, exactly as an authorizer pins its
            // expected issuer/audience and rejects expired tokens.
            if (!EXPECTED_ISS.equals(c.getIssuer())) {
                throw new SecurityException("unexpected issuer");
            }
            if (c.getAudience() == null || !c.getAudience().contains(EXPECTED_AUD)) {
                throw new SecurityException("unexpected audience");
            }
            // Canonical rules shared across all languages (see
            // bencher/fixtures/README.md "validation rules"): require `exp` and
            // reject an expired token, and reject a not-yet-valid token when `nbf`
            // is present, both with a 60 s leeway. nimbus does no claim validation
            // itself, so these checks are hand-rolled to match the other languages.
            long leewayMs = 60_000L;
            Date now = new Date();
            Date exp = c.getExpirationTime();
            if (exp == null) {
                throw new SecurityException("token missing required exp claim");
            }
            if (exp.getTime() + leewayMs < now.getTime()) {
                throw new SecurityException("token expired");
            }
            Date nbf = c.getNotBeforeTime();
            if (nbf != null && nbf.getTime() - leewayMs > now.getTime()) {
                throw new SecurityException("token not yet valid");
            }

            Map<String, Object> raw = c.getClaims();
            Map<String, Object> extracted = new LinkedHashMap<>();
            for (Map.Entry<String, ClaimType> entry : CLAIM_CONFIG.entrySet()) {
                Object value = raw.get(entry.getKey());
                if (value == null) {
                    continue;
                }
                Object mapped = switch (entry.getValue()) {
                    case STRING -> mapToString(value);
                    case BOOL -> mapToBool(value);
                    case SET -> mapToSet(value);
                };
                if (mapped != null) {
                    extracted.put(entry.getKey(), mapped);
                }
            }
            return extracted;
        } catch (java.text.ParseException | com.nimbusds.jose.JOSEException e) {
            throw new RuntimeException("verifying JWT", e);
        }
    }

    /**
     * String mapping: strings pass through; objects/arrays are JSON-serialized;
     * other primitives are stringified. JSON-serializing structured values (rather
     * than a language-specific {@code toString}) matches the other handlers.
     */
    private static Object mapToString(Object value) {
        if (value instanceof String s) {
            return s;
        }
        if (value instanceof List<?> || value instanceof Map<?, ?>) {
            try {
                return MAPPER.writeValueAsString(value);
            } catch (com.fasterxml.jackson.core.JsonProcessingException e) {
                throw new RuntimeException("serializing claim value", e);
            }
        }
        return String.valueOf(value);
    }

    /** Real booleans pass through; "true"/"false" strings map to booleans. */
    private static Object mapToBool(Object value) {
        if (value instanceof Boolean b) {
            return b;
        }
        if ("true".equals(value)) {
            return Boolean.TRUE;
        }
        if ("false".equals(value)) {
            return Boolean.FALSE;
        }
        return null;
    }

    /** Arrays are filtered to string elements; a single string is wrapped. */
    private static Object mapToSet(Object value) {
        if (value instanceof List<?> list) {
            List<String> strings = new ArrayList<>();
            for (Object o : list) {
                if (o instanceof String s) {
                    strings.add(s);
                }
            }
            return strings;
        }
        if (value instanceof String s) {
            return List.of(s);
        }
        return null;
    }
}
