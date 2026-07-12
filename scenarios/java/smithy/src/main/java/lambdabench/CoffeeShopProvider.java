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
import java.util.List;
import java.util.concurrent.atomic.AtomicLong;
import software.amazon.smithy.java.aws.integrations.lambda.SmithyServiceProvider;
import software.amazon.smithy.java.server.RequestContext;
import software.amazon.smithy.java.server.Service;

/**
 * Scenario "smithy": the Smithy server framework hosted behind a Lambda handler,
 * with NO AWS call. {@code GetMenu} increments a shared in-memory counter
 * (exercising shared state across warm invokes, as a real server would) and
 * returns a constant menu. {@code CreateOrder}/{@code GetOrder} are stubs the
 * service contract requires but the benchmark does not invoke. The cold-start
 * delta versus {@code hello} isolates the Smithy server framework overhead.
 *
 * <p>The bencher drives this with a synthetic API Gateway proxy event (GET
 * /menu); no live API Gateway is involved.
 *
 * <p>Discovered by the smithy-java {@code LambdaEndpoint} via {@code ServiceLoader}
 * (registered with {@code @AutoService}). The service is built in a static
 * initializer so Lambda reuses it across warm invocations.
 *
 * <p><b>SnapStart: deliberately not primed.</b> The framework's first-request cost
 * (protocol resolution, request (de)serialization, constraint validation, response
 * serialization, JIT) is reachable only through {@code LambdaEndpoint::handleRequest},
 * which smithy-java exposes no public hook to drive before a checkpoint (the
 * {@code ProxyRequest} event type is package-private). Priming {@code getMenu}
 * directly would warm only the operation method, not the framework path, and this
 * scenario has no AWS SDK to warm either. So the SnapStart cold start legitimately
 * carries the framework first-request cost, the same as plain Java: itself a
 * finding (see README / DESIGN §10), not a gap to work around.
 */
@AutoService(SmithyServiceProvider.class)
public final class CoffeeShopProvider implements SmithyServiceProvider {

    /** Shared request counter, exercised by GetMenu across warm invokes. */
    private static final AtomicLong REQUESTS = new AtomicLong();

    private static final Service SERVICE = CoffeeShop.builder()
            .addCreateOrderOperation(CoffeeShopProvider::createOrder)
            .addGetMenuOperation(CoffeeShopProvider::getMenu)
            .addGetOrderOperation(CoffeeShopProvider::getOrder)
            .build();

    @Override
    public Service get() {
        return SERVICE;
    }

    /** Bumps the shared counter and returns a constant menu item. */
    private static GetMenuOutput getMenu(GetMenuInput input, RequestContext context) {
        long n = REQUESTS.incrementAndGet();
        CoffeeItem item = CoffeeItem.builder()
                .type(CoffeeType.DRIP)
                .description("lambdabench #" + n)
                .build();
        return GetMenuOutput.builder().items(List.of(item)).build();
    }

    /** Stub: the benchmark only invokes GetMenu in this scenario. */
    private static CreateOrderOutput createOrder(CreateOrderInput input, RequestContext context) {
        return CreateOrderOutput.builder()
                .id("00000000-0000-0000-0000-000000000000")
                .coffeeType(input.getCoffeeType())
                .status(OrderStatus.IN_PROGRESS)
                .build();
    }

    /** Stub for the same reason as createOrder. */
    private static GetOrderOutput getOrder(GetOrderInput input, RequestContext context) {
        throw OrderNotFound.builder()
                .message("not implemented in benchmark")
                .build();
    }
}
