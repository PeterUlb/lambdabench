package lambdabench;

import com.amazonaws.services.lambda.runtime.Context;
import com.amazonaws.services.lambda.runtime.RequestHandler;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;
import org.crac.Core;
import org.crac.Resource;
import software.amazon.awssdk.awscore.retry.AwsRetryStrategy;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.core.client.config.ClientOverrideConfiguration;
import software.amazon.awssdk.http.urlconnection.UrlConnectionHttpClient;
import software.amazon.awssdk.services.dynamodb.DynamoDbClient;
import software.amazon.awssdk.services.dynamodb.model.AttributeValue;
import software.amazon.awssdk.services.dynamodb.model.GetItemResponse;
import software.amazon.awssdk.services.kms.KmsClient;
import software.amazon.awssdk.services.kms.model.EncryptResponse;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.model.GetObjectRequest;

/**
 * Scenario "threeclient": construct THREE AWS SDK clients (DynamoDB, KMS, S3) at
 * init and call all three per invoke (GetItem + Encrypt + GetObject). Mirrors
 * the Rust and Node handlers; read by direct comparison against {@code oneclient}
 * (not subtraction) to show what additional AWS clients add to cold start (extra
 * middleware stacks plus a first TLS handshake per distinct endpoint). Retries
 * disabled.
 *
 * <p>For SnapStart, the handler primes itself via a CRaC {@code beforeCheckpoint}
 * hook (one real invocation during init, baking the three SDK clients' first-call
 * costs into the snapshot, the realistic config for an operator who enables
 * SnapStart). The hook fires only when a snapshot is taken; a plain function uses
 * org.crac's no-op context, so the same jar is unprimed.
 */
public final class ThreeClientHandler implements RequestHandler<Object, Map<String, Object>>, Resource {

    private static final DynamoDbClient DDB;
    private static final KmsClient KMS;
    private static final S3Client S3;
    private static final String TABLE = env("LAMBDABENCH_TABLE");
    private static final String KEY = env("LAMBDABENCH_KEY");
    private static final String KMS_KEY_ID = env("LAMBDABENCH_KMS_KEY_ID");
    private static final String BUCKET = env("LAMBDABENCH_BUCKET");
    private static final String OBJECT_KEY = env("LAMBDABENCH_OBJECT_KEY");

    private static String env(String name) {
        String v = System.getenv(name);
        if (v == null || v.isEmpty()) {
            throw new IllegalStateException(name + " not set");
        }
        return v;
    }

    static {
        // Retries disabled: a throttle/transient must surface as a hard failure,
        // not be silently retried into an inflated Duration. A failed run beats
        // wrong data.
        ClientOverrideConfiguration noRetry = ClientOverrideConfiguration.builder()
                .retryStrategy(AwsRetryStrategy.doNotRetry())
                .build();
        // Each SDK client gets its own HTTP client (its own transport stack),
        // matching the other languages' three independent clients. A single shared
        // instance would make Java pay for one transport at init where the others
        // pay for three, biasing the cross-language cold-init comparison.
        DDB = DynamoDbClient.builder()
                .httpClient(UrlConnectionHttpClient.create())
                .overrideConfiguration(noRetry).build();
        KMS = KmsClient.builder()
                .httpClient(UrlConnectionHttpClient.create())
                .overrideConfiguration(noRetry).build();
        S3 = S3Client.builder()
                .httpClient(UrlConnectionHttpClient.create())
                .overrideConfiguration(noRetry).build();
    }

    public ThreeClientHandler() {
        Core.getGlobalContext().register(this);
    }

    @Override
    public void beforeCheckpoint(org.crac.Context<? extends Resource> context) {
        handleRequest(Map.of(), null);
    }

    @Override
    public void afterRestore(org.crac.Context<? extends Resource> context) {}

    @Override
    public Map<String, Object> handleRequest(Object event, Context context) {
        // 1. DynamoDB GetItem. Fail loud if the seeded item is absent: a missing
        // item means a broken benchmark setup, never a null fallback (matches
        // oneclient and the other languages).
        GetItemResponse ddbOut = DDB.getItem(b -> b.tableName(TABLE)
                .key(Map.of("pk", AttributeValue.fromS(KEY))));
        if (!ddbOut.hasItem() || ddbOut.item().isEmpty()) {
            throw new IllegalStateException("seeded item not found");
        }
        AttributeValue payload = ddbOut.item().get("payload");

        // 2. KMS Encrypt of a short constant ("hello").
        EncryptResponse kmsOut = KMS.encrypt(b -> b.keyId(KMS_KEY_ID)
                .plaintext(SdkBytes.fromString("hello", StandardCharsets.UTF_8)));
        int ciphertextLen = kmsOut.ciphertextBlob().asByteArray().length;

        // 3. S3 GetObject of a small seeded object. Measure the raw byte length,
        // not a decoded String's length: a decode would add per-invoke work the
        // other languages do not do and count UTF-16 code units rather than bytes.
        int objectLen = S3.getObjectAsBytes(GetObjectRequest.builder()
                .bucket(BUCKET).key(OBJECT_KEY).build())
                .asByteArray().length;

        Map<String, Object> result = new HashMap<>();
        result.put("scenario", "threeclient");
        result.put("ddb_payload", payload == null ? null : payload.s());
        result.put("kms_ciphertext_len", ciphertextLen);
        result.put("s3_object_len", objectLen);
        return result;
    }
}
