package lambdabench;

import com.amazonaws.services.lambda.runtime.Context;
import com.amazonaws.services.lambda.runtime.RequestHandler;
import com.fasterxml.jackson.annotation.JsonCreator;
import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import org.crac.Core;
import org.crac.Resource;
import software.amazon.awssdk.awscore.retry.AwsRetryStrategy;
import software.amazon.awssdk.core.client.config.ClientOverrideConfiguration;
import software.amazon.awssdk.http.urlconnection.UrlConnectionHttpClient;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.model.GetObjectRequest;

/**
 * Scenario "batch": a deserialize-heavy batch record processor (the canonical
 * Kinesis/SQS-consumer shape). At init the handler fetches a ~16 MB JSON array of
 * event records from S3 and keeps the raw text in memory. Each warm invoke parses
 * the whole batch into records, groups-by {@code key} into a map of running
 * sum+count, and returns the per-group totals.
 *
 * <p>Read this scenario on two independent axes. The MEDIAN tracks each
 * language's standard JSON-parser speed (here Jackson, Java's de-facto standard
 * decoder; a faster parser would compare libraries, not languages). The TAIL
 * (P99/P99.9) at the smaller memory tiers tracks allocation + GC: the parsed
 * records and the group map are all live simultaneously for the whole invoke, a
 * large transient object graph the JVM's tracing GC must promote then collect,
 * where a non-GC runtime just drops it. Java's tail is among the most pronounced
 * of the runtimes (large heap representation + a generational tracing collector).
 *
 * <p>Contrast lettercount, which counts into a fixed {@code long[26]} (nothing
 * grows). Fetching the batch at init keeps the warm measurement pure compute. The
 * group-by (parse + map insert/update + arithmetic) is in-language work, not a
 * native library, so the comparison is fair.
 *
 * <p>For SnapStart, the handler primes itself via a CRaC {@code beforeCheckpoint}
 * hook (one real parse+group-by during init, baking Jackson's class loading and
 * the group-by JIT into the snapshot). The hook fires only when a snapshot is
 * taken; a plain function uses org.crac's no-op context, so the same jar is
 * unprimed.
 */
public final class BatchHandler implements RequestHandler<Object, Map<String, Object>>, Resource {

    private static final ObjectMapper MAPPER = new ObjectMapper();
    /** Raw batch text fetched once at init and reused across warm invokes. */
    private static final String PAYLOAD;

    /** One event record in the batch. Deserialized into a typed object (not a lazy
     * {@code JsonNode} tree) so the parse's live object graph matches the other
     * handlers, the quantity this GC probe compares.
     *
     * <p>{@code value} stays a primitive {@code long} on purpose: a boxed
     * {@code Long} would retain one extra heap object per record (~573k promoted
     * boxes per invoke) on the exact path the scenario measures, inflating Java's
     * live heap relative to Rust ({@code i64}) and Node. The {@code required = true}
     * creator properties reject a missing field at parse time without forcing a box,
     * still failing loud on a malformed batch. */
    @JsonIgnoreProperties(ignoreUnknown = true)
    private static final class Record {
        public final String key;
        public final long value;

        @JsonCreator
        Record(
                @JsonProperty(value = "key", required = true) String key,
                @JsonProperty(value = "value", required = true) long value) {
            this.key = key;
            this.value = value;
        }
    }

    /** Running aggregate per group. */
    private static final class Agg {
        long sum;
        long count;
    }

    private static String env(String name) {
        String v = System.getenv(name);
        if (v == null || v.isEmpty()) {
            throw new IllegalStateException(name + " not set");
        }
        return v;
    }

    static {
        // Init phase: fetch the batch from S3 once and hold it in memory. Retries
        // disabled (doNotRetry): a throttle on the init-time fetch must surface as a
        // hard failure, not be silently retried into an inflated init_ms. A failed
        // run beats wrong data.
        String bucket = env("LAMBDABENCH_BUCKET");
        String key = env("LAMBDABENCH_BATCH_KEY");
        try (S3Client s3 = S3Client.builder()
                .httpClient(UrlConnectionHttpClient.create())
                .overrideConfiguration(ClientOverrideConfiguration.builder()
                        .retryStrategy(AwsRetryStrategy.doNotRetry())
                        .build())
                .build()) {
            PAYLOAD = s3.getObjectAsBytes(GetObjectRequest.builder()
                    .bucket(bucket).key(key).build())
                    .asUtf8String();
        }
    }

    public BatchHandler() {
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
        try {
            Record[] records = MAPPER.readValue(PAYLOAD, Record[].class);
            Map<String, Agg> groups = new HashMap<>();
            long total = 0;
            int recordCount = 0;
            for (Record r : records) {
                // required=true rejects a missing key/value at parse time, but an
                // explicit "key": null still deserializes; reject it here so a
                // malformed batch fails loud instead of grouping under a null key.
                if (r.key == null) {
                    throw new IllegalStateException("batch record has null key");
                }
                Agg agg = groups.computeIfAbsent(r.key, k -> new Agg());
                agg.sum += r.value;
                agg.count++;
                total += r.value;
                recordCount++;
            }
            // Emit per-group totals plus headline figures. Building the output
            // allocates proportional to group count, mirroring what a real batch
            // processor hands downstream.
            List<Map<String, Object>> perGroup = new ArrayList<>(groups.size());
            for (Map.Entry<String, Agg> e : groups.entrySet()) {
                Map<String, Object> g = new HashMap<>();
                g.put("key", e.getKey());
                g.put("sum", e.getValue().sum);
                g.put("count", e.getValue().count);
                perGroup.add(g);
            }
            Map<String, Object> result = new HashMap<>();
            result.put("scenario", "batch");
            result.put("records", recordCount);
            result.put("groups", groups.size());
            result.put("total", total);
            result.put("per_group", perGroup);
            return result;
        } catch (Exception e) {
            throw new RuntimeException("aggregating batch payload", e);
        }
    }
}
