package lambdabench;

import com.amazonaws.services.lambda.runtime.Context;
import com.amazonaws.services.lambda.runtime.RequestHandler;
import java.util.HashMap;
import java.util.Map;

/**
 * Scenario "cache": a long-lived in-memory working set, churned every invoke,
 * the dedicated garbage-collection probe.
 *
 * <p>At init the handler allocates a large RETAINED live set: {@code ENTRIES}
 * byte buffers of {@code ENTRY_BYTES} each (~100 MB), held in static state that
 * persists across every warm invocation, the way a real handler holds an
 * in-process cache, an LRU, a buffer pool, or loaded reference data for the life
 * of the sandbox.
 *
 * <p>Each warm invoke (1) replaces {@code CHURN} entries with freshly allocated
 * buffers (eviction + insert: the replaced buffers become garbage while the live
 * set stays full, generating garbage against a large permanently-live heap), then
 * (2) scans every 10th entry summing a byte, keeping the whole retained set
 * genuinely live and read so the JIT cannot elide it.
 *
 * <p>Why this workload (contrast {@code batch}): the JVM's tracing GC pays a
 * per-cycle cost that scales with the live heap it must trace, not with the
 * garbage. Keeping a large live set permanently resident and churning it makes
 * every collection expensive, the path {@code batch} never reaches (its object
 * graph is transient). At the smaller fractional-vCPU tiers GC competes with the
 * handler for the one core, so the warm P99/P99.9 tail opens up while the median
 * stays flatter, worst at the starved low-memory tiers and easing as vCPU grows. A
 * non-GC runtime frees each replaced buffer immediately and stays flat. Read the
 * absolute tail latencies on the dashboard, not a P99/P50 ratio. Java's tail is
 * among the most pronounced (large heap + generational tracing collector).
 *
 * <p>Deliberately an indexed array of {@code byte[]}, not a {@code HashMap}: the
 * point is to isolate the GC / allocator, not to compare map implementations or
 * hashing speed. No AWS clients, no payload: fully self-contained.
 *
 * <p>Not primed via CRaC {@code beforeCheckpoint} (see DESIGN.md §10). cache has
 * no SDK to hoist, so a prime would warm only the hot loop's JIT, an advantage no
 * other runtime gets an analog of. Leaving the cold path unprimed makes the
 * SnapStart cold sample measure restore + first-touch JIT cost, the same shape a
 * non-SnapStart cold start pays.
 */
public final class CacheHandler implements RequestHandler<Object, Map<String, Object>> {

    /** Number of buffers in the retained live set. */
    private static final int ENTRIES = 200_000;
    /** Bytes per buffer; ENTRIES * ENTRY_BYTES ~= 100 MB of permanently-live heap. */
    private static final int ENTRY_BYTES = 512;
    /** Buffers replaced per invoke (garbage generated + new live, set stays full). */
    private static final int CHURN = 40_000;

    /** The retained working set, allocated once at init and held forever. That
     * permanence is what keeps the tracing GC's per-cycle cost high. */
    private static final byte[][] LIVE = new byte[ENTRIES][];
    /** Ring cursor, carried across warm invokes. */
    private static int rot = 0;

    static {
        for (int i = 0; i < ENTRIES; i++) {
            LIVE[i] = new byte[ENTRY_BYTES];
            LIVE[i][0] = (byte) (i & 0xff);
        }
    }

    /** Replace CHURN entries then scan every 10th entry. The replaced buffers
     * become garbage while the live set stays full, so the GC keeps tracing the
     * whole ~100 MB set. The scan keeps it genuinely live. */
    private static long churnAndScan() {
        for (int c = 0; c < CHURN; c++) {
            rot = (rot + 1) % ENTRIES;
            byte[] b = new byte[ENTRY_BYTES];
            b[0] = (byte) (c & 0xff);
            LIVE[rot] = b;
        }
        long sum = 0;
        for (int i = 0; i < ENTRIES; i += 10) {
            sum += LIVE[i][0] & 0xff;
        }
        return sum;
    }

    @Override
    public Map<String, Object> handleRequest(Object event, Context context) {
        long checksum = churnAndScan();
        Map<String, Object> result = new HashMap<>();
        result.put("scenario", "cache");
        result.put("entries", ENTRIES);
        result.put("churned", CHURN);
        result.put("checksum", checksum);
        return result;
    }
}
