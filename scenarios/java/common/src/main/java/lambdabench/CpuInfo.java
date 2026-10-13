package lambdabench;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;

/**
 * Logs the host CPU when the bencher asks for it.
 *
 * <p>After each cold+warm cycle the bencher sends one extra, unrecorded invoke that
 * asks for the host CPU: through the ClientContext ({@code
 * {"custom":{"lambdabench":"cpu"}}}) for the plain handlers, which check it inline,
 * or as a {@code GetOrder} of {@link #SMITHY_ORDER_ID} for the Smithy scenarios,
 * whose Lambda handler is smithy-java's own. {@link #report()} logs one {@code
 * LAMBDABENCH_CPU <cpu>} line, which the bencher reads from the log tail and stamps
 * on the cycle's rows (Lambda mixes Graviton generations on arm64). The checks
 * touch only the Lambda {@code Context} interface, so this class is loaded and
 * {@code /proc/cpuinfo} read only on that invoke. Same output as the Rust
 * scenarios' lambdabench-cpuinfo crate. Compiled into every Java scenario (see the
 * root build script).
 */
public final class CpuInfo {

    /** The order id of the Smithy scenarios' host-CPU {@code GetOrder}. */
    public static final String SMITHY_ORDER_ID = "00000000-0000-0000-0000-000000000000";

    private CpuInfo() {}

    public static void report() {
        String info;
        try {
            info = Files.readString(Path.of("/proc/cpuinfo"));
        } catch (IOException e) {
            info = "";
        }
        System.err.println("LAMBDABENCH_CPU " + describe(info));
    }

    /**
     * The x86 model name, which Firecracker (Lambda's VM layer) normalizes to "Intel(R)
     * Xeon(R) Processor" plus the host's nominal clock (family/model/stepping come from
     * its CPU template and are fixed: model 63 in every probe), or the arm64 implementer, part and variant, which
     * keep their host values (part 0xd0c is Neoverse N1 / Graviton2, 0xd40 V1 /
     * Graviton3, 0xd4f V2 / Graviton4), plus the vCPU count.
     */
    static String describe(String info) {
        List<String> lines = info.lines().toList();
        long vcpus = lines.stream().filter(l -> l.startsWith("processor")).count();
        String model = field(lines, "model name");
        String cpu = !model.isEmpty()
                ? model
                : "arm implementer=" + field(lines, "CPU implementer")
                        + " part=" + field(lines, "CPU part")
                        + " variant=" + field(lines, "CPU variant");
        return cpu + "; vcpus=" + vcpus;
    }

    private static String field(List<String> lines, String name) {
        for (String line : lines) {
            int i = line.indexOf(':');
            if (i >= 0 && line.substring(0, i).trim().equals(name)) {
                return line.substring(i + 1).trim();
            }
        }
        return "";
    }
}
