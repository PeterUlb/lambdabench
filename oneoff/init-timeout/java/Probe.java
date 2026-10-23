// Java version of the init sleep, hang and exit in ../fn/index.mjs, for SnapStart: the static
// initializer sleeps INIT_MS (REINIT_MS once a {"hang"} or {"exit"} payload has left /tmp/hung),
// and the handler can hang or exit. No dependencies, so it builds with plain javac; the handler
// takes only the event (no Context).
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.UUID;

public class Probe {
    static final Path MARKER = Path.of("/tmp/hung");
    static final boolean REINIT = Files.exists(MARKER);
    static final String INIT_ID = UUID.randomUUID().toString().substring(0, 8);
    static final long INIT_MS;

    static {
        long t = System.currentTimeMillis();
        String ms = REINIT && System.getenv("REINIT_MS") != null ? System.getenv("REINIT_MS") : System.getenv("INIT_MS");
        try {
            Thread.sleep(Long.parseLong(ms == null ? "0" : ms));
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
        INIT_MS = System.currentTimeMillis() - t;
    }

    public Map<String, Object> handleRequest(Map<String, Object> event) throws Exception {
        boolean hang = Boolean.TRUE.equals(event.get("hang")), exit = Boolean.TRUE.equals(event.get("exit"));
        if (hang || exit) Files.writeString(MARKER, "");
        if (exit) System.exit(1);
        if (hang) Thread.sleep(900_000);
        return Map.of("initId", INIT_ID, "initMs", INIT_MS, "reinit", REINIT);
    }
}
