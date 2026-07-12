package lambdabench;

import lambdabench.coffeeshop.model.CoffeeItem;
import lambdabench.coffeeshop.model.CoffeeType;
import lambdabench.coffeeshop.model.CreateOrderInput;
import lambdabench.coffeeshop.model.CreateOrderOutput;
import lambdabench.coffeeshop.model.GetMenuInput;
import lambdabench.coffeeshop.model.GetMenuOutput;
import lambdabench.coffeeshop.model.GetOrderInput;
import lambdabench.coffeeshop.model.GetOrderOutput;
import lambdabench.coffeeshop.model.OrderNotFound;
import lambdabench.coffeeshop.model.OrderStatus;
import lambdabench.coffeeshop.service.CoffeeShop;
import com.google.auto.service.AutoService;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import org.crac.Core;
import org.crac.Resource;
import software.amazon.awssdk.awscore.retry.AwsRetryStrategy;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.core.client.config.ClientOverrideConfiguration;
import software.amazon.awssdk.core.sync.RequestBody;
import software.amazon.awssdk.http.urlconnection.UrlConnectionHttpClient;
import software.amazon.awssdk.services.dynamodb.DynamoDbClient;
import software.amazon.awssdk.services.dynamodb.model.AttributeValue;
import software.amazon.awssdk.services.kms.KmsClient;
import software.amazon.awssdk.services.kms.model.EncryptResponse;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.model.PutObjectRequest;
import software.amazon.smithy.java.aws.integrations.lambda.SmithyServiceProvider;
import software.amazon.smithy.java.server.RequestContext;
import software.amazon.smithy.java.server.Service;

/**
 * Scenario "smithyfull": the realistic shape, the Smithy server framework
 * hosting a handler that does a real AWS write flow. The benchmark invokes
 * {@code CreateOrder}: the server SDK deserializes and validates the request
 * body, the handler KMS-encrypts a signature, DDB {@code PutItem}s the order,
 * and S3 {@code PutObject}s a receipt, then the SDK serializes a
 * constraint-validated {@code Uuid} response. Framework plus multiple AWS clients
 * plus real (de)serialization: a typical production request handler.
 *
 * <p>The three AWS clients are built once in a static initializer so Lambda
 * reuses them across warm invocations. Fixed write targets make the writes
 * idempotent (each invoke overwrites the same item/object). Retries are disabled
 * so a throttle or transient surfaces as a hard failure rather than being
 * silently retried into an inflated Duration: a failed run beats wrong data.
 *
 * <p><b>SnapStart priming, and its one realistic limit.</b> The
 * {@code beforeCheckpoint} hook calls {@code createOrder} directly, the pattern
 * AWS's own guidance uses to prime an SDK-heavy SnapStart handler. That warms the
 * dominant cold cost: the three SDK clients' lazy class loading, marshaller
 * construction, JIT, and TLS/credential warmup, all baked into the snapshot. It
 * does not warm the Smithy framework's request path (protocol resolution, request
 * (de)serialization, constraint validation, response serialization), reachable
 * only through {@code LambdaEndpoint::handleRequest}, which smithy-java gives no
 * public hook to drive before a checkpoint (the {@code ProxyRequest} event type is
 * package-private). So the first restored invoke still pays the framework
 * marshalling cost: the realistic config an operator can ship, with the residual
 * framework cost a documented finding (see README / DESIGN §10). The hook fires
 * only when a snapshot is taken; a plain function uses org.crac's no-op context,
 * so the same jar deployed plain never primes.
 */
@AutoService(SmithyServiceProvider.class)
public final class CoffeeShopFullProvider implements SmithyServiceProvider, Resource {

    private static final DynamoDbClient DDB;
    private static final KmsClient KMS;
    private static final S3Client S3;
    private static final String TABLE = env("LAMBDABENCH_TABLE");
    private static final String KMS_KEY_ID = env("LAMBDABENCH_KMS_KEY_ID");
    private static final String BUCKET = env("LAMBDABENCH_BUCKET");
    private static final String ORDER_PK = env("LAMBDABENCH_ORDER_PK");
    private static final String RECEIPT_KEY = env("LAMBDABENCH_RECEIPT_KEY");

    private static final Service SERVICE = CoffeeShop.builder()
            .addCreateOrderOperation(CoffeeShopFullProvider::createOrder)
            .addGetMenuOperation(CoffeeShopFullProvider::getMenu)
            .addGetOrderOperation(CoffeeShopFullProvider::getOrder)
            .build();

    private static String env(String name) {
        String v = System.getenv(name);
        if (v == null || v.isEmpty()) {
            throw new IllegalStateException(name + " not set");
        }
        return v;
    }

    static {
        ClientOverrideConfiguration noRetry = ClientOverrideConfiguration.builder()
                .retryStrategy(AwsRetryStrategy.doNotRetry())
                .build();
        // Each SDK client gets its own HTTP client (its own transport stack),
        // matching the other languages' independent clients. A single shared
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

    public CoffeeShopFullProvider() {
        // Register for SnapStart priming. LambdaEndpoint instantiates each
        // SmithyServiceProvider during its static init (via ServiceLoader) and
        // holds a strong reference, satisfying CRaC's WeakReference requirement.
        Core.getGlobalContext().register(this);
    }

    @Override
    public void beforeCheckpoint(org.crac.Context<? extends Resource> context) {
        // Prime the CreateOrder write flow so the three SDK clients' first-call
        // costs (lazy class loading, marshaller construction, JIT, TLS warmup) are
        // baked into the snapshot: the dominant cold cost. This does not warm the
        // Smithy framework request path (see the class doc). createOrder ignores the
        // RequestContext, so a null context is fine; the write targets are the
        // idempotent fixed keys, so priming accumulates no data.
        createOrder(CreateOrderInput.builder().coffeeType(CoffeeType.LATTE).build(), null);
    }

    @Override
    public void afterRestore(org.crac.Context<? extends Resource> context) {}

    @Override
    public Service get() {
        return SERVICE;
    }

    /**
     * The realistic CreateOrder write flow. The SSDK has already deserialized and
     * validated the input (coffeeType: required enum) before this runs. The
     * returned {@code id} is a {@code @pattern}+{@code @length}-constrained Uuid,
     * so the SSDK runs constraint validation while serializing the response.
     */
    private static CreateOrderOutput createOrder(CreateOrderInput input, RequestContext context) {
        String coffeeType = input.getCoffeeType().getValue();

        // 1. KMS Encrypt a small "signature" payload for the order.
        EncryptResponse kms = KMS.encrypt(b -> b.keyId(KMS_KEY_ID)
                .plaintext(SdkBytes.fromString("order:" + coffeeType, StandardCharsets.UTF_8)));
        SdkBytes signature = kms.ciphertextBlob();

        // 2. DDB PutItem: write the order (fixed pk, idempotent overwrite).
        DDB.putItem(b -> b.tableName(TABLE).item(Map.of(
                "pk", AttributeValue.fromS(ORDER_PK),
                "coffeeType", AttributeValue.fromS(coffeeType),
                "status", AttributeValue.fromS("IN_PROGRESS"),
                "signature", AttributeValue.fromB(signature))));

        // 3. S3 PutObject: write a receipt (fixed key, idempotent overwrite).
        S3.putObject(PutObjectRequest.builder().bucket(BUCKET).key(RECEIPT_KEY).build(),
                RequestBody.fromString("receipt for " + coffeeType + " order"));

        return CreateOrderOutput.builder()
                .id("00000000-0000-0000-0000-000000000000")
                .coffeeType(input.getCoffeeType())
                .status(OrderStatus.IN_PROGRESS)
                .build();
    }

    /** Trivial stub: the benchmark exercises CreateOrder, not GetMenu. */
    private static GetMenuOutput getMenu(GetMenuInput input, RequestContext context) {
        CoffeeItem item = CoffeeItem.builder()
                .type(CoffeeType.DRIP)
                .description("lambdabench")
                .build();
        return GetMenuOutput.builder().items(List.of(item)).build();
    }

    /** Stub to satisfy the service contract. */
    private static GetOrderOutput getOrder(GetOrderInput input, RequestContext context) {
        throw OrderNotFound.builder()
                .message("not implemented in benchmark")
                .build();
    }
}
