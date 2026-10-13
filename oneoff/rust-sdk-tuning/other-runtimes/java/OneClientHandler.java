package lambdabench;

import com.amazonaws.services.lambda.runtime.Context;
import com.amazonaws.services.lambda.runtime.RequestHandler;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import software.amazon.awssdk.awscore.retry.AwsRetryStrategy;
import software.amazon.awssdk.core.client.config.ClientOverrideConfiguration;
import software.amazon.awssdk.http.urlconnection.UrlConnectionHttpClient;
import software.amazon.awssdk.services.dynamodb.DynamoDbClient;
import software.amazon.awssdk.services.dynamodb.model.AttributeValue;
import software.amazon.awssdk.services.dynamodb.model.GetItemResponse;

/**
 * Probe copy of the matrix's Java oneclient. JB_PREREAD picks what to read once
 * at init before building the client: none, task (every file under /var/task),
 * lang (the JDK's lib/modules image), or all. The first request prints one
 * PROBE line with page-fault and disk-read counters for init and the request.
 */
public final class OneClientHandler implements RequestHandler<Object, Map<String, Object>> {

    private static final DynamoDbClient DDB;
    private static final String TABLE = System.getenv("LAMBDABENCH_TABLE");
    private static final String KEY = System.getenv("LAMBDABENCH_KEY");
    private static final long[] START;
    private static final long[] INIT;
    private static final double PREREAD_MS;
    private static final double CLIENT_MS;
    private static final long PREREAD_BYTES;
    private static final String MOUNTS;
    private static int n = 0;

    /** majflt, minflt, read_bytes, wall ns. */
    static long[] counters() {
        long[] c = new long[4];
        try {
            String stat = Files.readString(Path.of("/proc/self/stat"));
            String[] f = stat.substring(stat.lastIndexOf(')') + 2).split(" ");
            // Fields after the comm: state is f[0], so field k (1-based) is f[k-3].
            c[0] = Long.parseLong(f[12 - 3]);
            c[1] = Long.parseLong(f[10 - 3]);
            for (String line : Files.readAllLines(Path.of("/proc/self/io"))) {
                if (line.startsWith("read_bytes:")) {
                    c[2] = Long.parseLong(line.substring(11).trim());
                }
            }
        } catch (IOException | RuntimeException e) {
            c[0] = -1;
        }
        c[3] = System.nanoTime();
        return c;
    }

    static long readAll(File f) {
        if (f.isDirectory()) {
            long sum = 0;
            File[] kids = f.listFiles();
            if (kids != null) {
                for (File k : kids) {
                    sum += readAll(k);
                }
            }
            return sum;
        }
        long sum = 0;
        byte[] buf = new byte[1 << 20];
        try (FileInputStream in = new FileInputStream(f)) {
            int r;
            while ((r = in.read(buf)) > 0) {
                sum += r;
            }
        } catch (IOException e) {
            // unreadable file: skip
        }
        return sum;
    }

    static String mounts() {
        StringBuilder sb = new StringBuilder();
        try {
            for (String line : Files.readAllLines(Path.of("/proc/self/mountinfo"))) {
                String[] p = line.split(" ");
                if (p.length > 9 && (p[4].startsWith("/var") || p[4].equals("/") || p[4].startsWith("/opt"))) {
                    int dash = line.indexOf(" - ");
                    sb.append(p[4]).append(':').append(line.substring(dash + 3).split(" ")[0])
                            .append(':').append(line.substring(dash + 3).split(" ")[1]).append(';');
                }
            }
        } catch (IOException e) {
            sb.append("err");
        }
        return sb.toString();
    }

    static {
        START = counters();
        String mode = System.getenv().getOrDefault("JB_PREREAD", "none");
        long t = System.nanoTime();
        long bytes = 0;
        if (mode.equals("task") || mode.equals("all")) {
            bytes += readAll(new File("/var/task"));
        }
        if (mode.equals("lang") || mode.equals("all")) {
            bytes += readAll(new File(System.getProperty("java.home"), "lib/modules"));
        }
        PREREAD_BYTES = bytes;
        PREREAD_MS = (System.nanoTime() - t) / 1e6;
        t = System.nanoTime();
        DDB = DynamoDbClient.builder()
                .httpClient(UrlConnectionHttpClient.create())
                .overrideConfiguration(ClientOverrideConfiguration.builder()
                        .retryStrategy(AwsRetryStrategy.doNotRetry())
                        .build())
                .build();
        CLIENT_MS = (System.nanoTime() - t) / 1e6;
        MOUNTS = mounts();
        INIT = counters();
    }

    @Override
    public Map<String, Object> handleRequest(Object event, Context context) {
        long[] before = counters();
        GetItemResponse out = DDB.getItem(b -> b.tableName(TABLE)
                .key(Map.of("pk", AttributeValue.fromS(KEY))));
        if (!out.hasItem() || out.item().isEmpty()) {
            throw new IllegalStateException("seeded item not found");
        }
        long[] after = counters();
        n++;
        if (n == 1) {
            System.err.println("PROBE {\"n\":1"
                    + ",\"init_majflt\":" + (INIT[0] - START[0])
                    + ",\"init_minflt\":" + (INIT[1] - START[1])
                    + ",\"init_read_kb\":" + (INIT[2] - START[2]) / 1024
                    + ",\"start_majflt\":" + START[0]
                    + ",\"start_read_kb\":" + START[2] / 1024
                    + ",\"preread_ms\":" + PREREAD_MS
                    + ",\"preread_kb\":" + PREREAD_BYTES / 1024
                    + ",\"client_ms\":" + CLIENT_MS
                    + ",\"handler_ms\":" + (after[3] - before[3]) / 1e6
                    + ",\"handler_majflt\":" + (after[0] - before[0])
                    + ",\"handler_minflt\":" + (after[1] - before[1])
                    + ",\"handler_read_kb\":" + (after[2] - before[2]) / 1024
                    + ",\"java_home\":\"" + System.getProperty("java.home") + "\""
                    + ",\"mounts\":\"" + MOUNTS + "\"}");
        }
        Map<String, Object> result = new HashMap<>();
        result.put("scenario", "oneclient");
        result.put("n", n);
        return result;
    }
}
