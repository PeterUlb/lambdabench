package lambdabench;

import com.amazonaws.services.lambda.runtime.Context;
import com.amazonaws.services.lambda.runtime.RequestHandler;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.util.HashMap;
import java.util.Map;
import software.amazon.awssdk.awscore.retry.AwsRetryStrategy;
import software.amazon.awssdk.core.client.config.ClientOverrideConfiguration;
import software.amazon.awssdk.http.urlconnection.UrlConnectionHttpClient;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.model.GetObjectRequest;

/**
 * Scenario "lettercount": CPU-bound work with no per-invoke I/O. At init the
 * handler fetches a ~1 MB JSON array of ASCII strings from S3 and keeps the raw
 * text in memory. Each warm invoke parses the array and counts the occurrences
 * of each lowercase ASCII letter (a..z) into 26 totals.
 *
 * <p>In-language CPU work (the count is a tight loop over the decoded string's
 * chars, not a shared native library; for the ASCII-only payload each char equals
 * one byte). A hashing- or stringify-heavy workload would instead run in native
 * C/OpenSSL every runtime shares, measuring the library not the language. There is
 * a fresh object graph rebuilt per invoke: under a constrained heap
 * the tracing collector may add pauses in the warm tail while a non-GC runtime
 * stays flat. Counting is restricted to ASCII a..z so every language does
 * identical work and yields identical totals.
 *
 * <p>Not primed via CRaC {@code beforeCheckpoint} (see DESIGN.md §10). lettercount
 * has no SDK to hoist; the init-time S3 fetch already runs ahead of the snapshot.
 * A prime would warm Jackson and the count loop's JIT, an advantage no other
 * runtime gets an analog of. Leaving cold unprimed makes the SnapStart cold sample
 * measure restore + first-touch JIT cost, the same shape a non-SnapStart cold
 * start pays.
 */
public final class LetterCountHandler implements RequestHandler<Object, Map<String, Object>> {

    private static final ObjectMapper MAPPER = new ObjectMapper();
    /** Raw JSON text fetched once at init and reused across warm invokes. */
    private static final String PAYLOAD;

    private static String env(String name) {
        String v = System.getenv(name);
        if (v == null || v.isEmpty()) {
            throw new IllegalStateException(name + " not set");
        }
        return v;
    }

    static {
        String bucket = env("LAMBDABENCH_BUCKET");
        String key = env("LAMBDABENCH_LETTERCOUNT_KEY");
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

    @Override
    public Map<String, Object> handleRequest(Object event, Context context) {
        long[] totals = countLetters(PAYLOAD);
        Map<String, Object> result = new HashMap<>();
        result.put("scenario", "lettercount");
        result.put("letter_counts", totals);
        return result;
    }

    /**
     * Parses the JSON string array and counts lowercase-ASCII letters across all
     * entries, returning 26 totals (index 0 = 'a' .. 25 = 'z'). The parse is the
     * allocation source; the per-character count is the in-language CPU work.
     *
     * <p>Deserialized into a typed {@code String[]} (not a lazy {@code JsonNode}
     * tree) so the parse's object graph matches the other handlers, whose parsers
     * produce a flat string array. Same fairness rule as the {@code batch} handler.
     */
    private static long[] countLetters(String payload) {
        long[] totals = new long[26];
        try {
            String[] entries = MAPPER.readValue(payload, String[].class);
            for (String s : entries) {
                for (int i = 0; i < s.length(); i++) {
                    char c = s.charAt(i);
                    if (c >= 'a' && c <= 'z') {
                        totals[c - 'a']++;
                    }
                }
            }
        } catch (Exception e) {
            throw new RuntimeException("parsing lettercount payload", e);
        }
        return totals;
    }
}
