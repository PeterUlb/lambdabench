package lambdabench;

import com.amazonaws.services.lambda.runtime.Context;
import com.amazonaws.services.lambda.runtime.RequestHandler;
import java.util.Map;

/**
 * Scenario "hello": the runtime baseline, with no AWS clients and no framework.
 * Ignores the event and returns a constant object, mirroring the other
 * languages' {@code hello} handlers. Isolates the bare JVM cold-start and per-invoke
 * overhead.
 */
public final class HelloHandler implements RequestHandler<Object, Map<String, String>> {

    @Override
    public Map<String, String> handleRequest(Object event, Context context) {
        // The bencher's unrecorded host-CPU invoke (see CpuInfo).
        if (context != null && context.getClientContext() != null
                && context.getClientContext().getCustom() != null
                && "cpu".equals(context.getClientContext().getCustom().get("lambdabench"))) {
            CpuInfo.report();
        }
        return Map.of("message", "hello", "scenario", "hello");
    }
}
