package lambdabench;

import com.amazonaws.services.lambda.runtime.Context;
import com.amazonaws.services.lambda.runtime.RequestHandler;
import java.util.HashMap;
import java.util.Map;
import org.crac.Core;
import org.crac.Resource;
import software.amazon.awssdk.awscore.retry.AwsRetryStrategy;
import software.amazon.awssdk.core.client.config.ClientOverrideConfiguration;
import software.amazon.awssdk.http.urlconnection.UrlConnectionHttpClient;
import software.amazon.awssdk.services.dynamodb.DynamoDbClient;
import software.amazon.awssdk.services.dynamodb.model.AttributeValue;
import software.amazon.awssdk.services.dynamodb.model.GetItemResponse;

/**
 * Scenario "oneclient": construct ONE AWS SDK client (DynamoDB) at init and call
 * it once ({@code GetItem}) per invoke. Mirrors the Rust and Node handlers: the
 * client is built during the Lambda init phase so the cold-start measurement
 * includes AWS config resolution and client construction.
 *
 * <p>Retries are disabled (max 1 attempt): a throttle or transient must surface
 * as a hard failure rather than be silently retried into an inflated Duration.
 * A failed run beats wrong data.
 *
 * <p>For SnapStart, the handler primes itself via a CRaC {@code beforeCheckpoint}
 * hook (running one real invocation during init, so the SDK's lazy class loading,
 * marshaller construction, and JIT are baked into the snapshot rather than paid
 * on the first restored invoke. The realistic config an operator who enables
 * SnapStart would ship). The hook fires only when a snapshot is taken; on a plain
 * function {@code org.crac} uses a no-op context, so the same jar is unprimed.
 */
public final class OneClientHandler implements RequestHandler<Object, Map<String, Object>>, Resource {

    private static final DynamoDbClient DDB;
    private static final String TABLE = env("LAMBDABENCH_TABLE");
    private static final String KEY = env("LAMBDABENCH_KEY");

    /** Reads a required environment variable, failing loud if it is unset. */
    private static String env(String name) {
        String v = System.getenv(name);
        if (v == null || v.isEmpty()) {
            throw new IllegalStateException(name + " not set");
        }
        return v;
    }

    static {
        DDB = DynamoDbClient.builder()
                .httpClient(UrlConnectionHttpClient.create())
                .overrideConfiguration(ClientOverrideConfiguration.builder()
                        .retryStrategy(AwsRetryStrategy.doNotRetry())
                        .build())
                .build();
    }

    public OneClientHandler() {
        // Register for SnapStart priming. Lambda holds a strong reference to this
        // handler instance, satisfying CRaC's WeakReference requirement.
        Core.getGlobalContext().register(this);
    }

    @Override
    public void beforeCheckpoint(org.crac.Context<? extends Resource> context) {
        // Prime the exact per-invoke path so its first-call costs land in the
        // snapshot. Runs against the real provisioned resources at publish time;
        // a failure fails the snapshot loud rather than shipping an unprimed one.
        handleRequest(Map.of(), null);
    }

    @Override
    public void afterRestore(org.crac.Context<? extends Resource> context) {}

    @Override
    public Map<String, Object> handleRequest(Object event, Context context) {
        GetItemResponse out = DDB.getItem(b -> b.tableName(TABLE)
                .key(Map.of("pk", AttributeValue.fromS(KEY))));
        // Fail loud if the seeded item is absent: a missing item means a broken
        // benchmark setup, never a null fallback (matches the other languages).
        if (!out.hasItem() || out.item().isEmpty()) {
            throw new IllegalStateException("seeded item not found");
        }
        AttributeValue payload = out.item().get("payload");

        Map<String, Object> result = new HashMap<>();
        result.put("scenario", "oneclient");
        result.put("key", KEY);
        result.put("payload", payload == null ? null : payload.s());
        return result;
    }
}
