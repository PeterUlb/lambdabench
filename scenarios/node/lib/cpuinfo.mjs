// Logs the host CPU when the bencher asks for it.
//
// After each cold+warm cycle the bencher sends one extra, unrecorded invoke whose
// ClientContext carries {"custom":{"lambdabench":"cpu"}}. On that invoke
// reportIfRequested logs one `LAMBDABENCH_CPU <cpu>` line, which the bencher reads
// from the log tail and stamps on the cycle's rows (Lambda mixes Graviton
// generations on arm64). Recorded invokes carry no ClientContext, so for them this
// is one synchronous property check (no await, so no extra microtask), and node:fs
// is only loaded on the flagged invoke. Same output as the Rust scenarios'
// lambdabench-cpuinfo crate.

export function reportIfRequested(context) {
  if (context?.clientContext?.custom?.lambdabench !== "cpu") return;
  const { readFileSync } = process.getBuiltinModule("node:fs");
  process.stderr.write(`LAMBDABENCH_CPU ${describe(readFileSync("/proc/cpuinfo", "utf8"))}\n`);
}

// The x86 model name, which Firecracker (Lambda's VM layer) normalizes to
// "Intel(R) Xeon(R) Processor" plus the host's nominal clock (family/model/stepping
// come from its CPU template and are fixed: model 63 in every probe), or the arm64 implementer, part and
// variant, which keep their host values (part 0xd0c is
// Neoverse N1 / Graviton2, 0xd40 V1 / Graviton3, 0xd4f V2 / Graviton4), plus the
// vCPU count.
export function describe(info) {
  const lines = info.split("\n");
  const field = (name) => {
    for (const line of lines) {
      const i = line.indexOf(":");
      if (i >= 0 && line.slice(0, i).trim() === name) return line.slice(i + 1).trim();
    }
    return "";
  };
  const vcpus = lines.filter((l) => l.startsWith("processor")).length;
  const cpu =
    field("model name") ||
    `arm implementer=${field("CPU implementer")} part=${field("CPU part")} variant=${field("CPU variant")}`;
  return `${cpu}; vcpus=${vcpus}`;
}
