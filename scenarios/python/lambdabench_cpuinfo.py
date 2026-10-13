# Logs the host CPU when the bencher asks for it.
#
# After each cold+warm cycle the bencher sends one extra, unrecorded invoke whose
# ClientContext carries {"custom":{"lambdabench":"cpu"}}. Each handler checks that
# flag inline and imports this module only on that invoke, so recorded invokes
# neither import it nor read /proc/cpuinfo. report() logs one
# `LAMBDABENCH_CPU <cpu>` line, which the bencher reads from the log tail and
# stamps on the cycle's rows (Lambda mixes Graviton generations on arm64). Same
# output as the Rust scenarios' lambdabench-cpuinfo crate. The bencher copies this
# file next to every Python handler.

import sys


def report():
    with open("/proc/cpuinfo") as f:
        info = f.read()
    sys.stderr.write(f"LAMBDABENCH_CPU {describe(info)}\n")
    sys.stderr.flush()


def describe(info):
    """The x86 model name, which Firecracker (Lambda's VM layer) normalizes to
    "Intel(R) Xeon(R) Processor" plus the host's nominal clock (family/model/stepping
    come from its CPU template and are fixed: model 63 in every probe), or the arm64 implementer, part and
    variant, which keep their host values (part 0xd0c is Neoverse N1 / Graviton2,
    0xd40 V1 / Graviton3, 0xd4f V2 / Graviton4), plus the vCPU count."""
    lines = info.split("\n")

    def field(name):
        for line in lines:
            key, sep, value = line.partition(":")
            if sep and key.strip() == name:
                return value.strip()
        return ""

    vcpus = sum(1 for line in lines if line.startswith("processor"))
    cpu = field("model name") or (
        f"arm implementer={field('CPU implementer')} part={field('CPU part')} "
        f"variant={field('CPU variant')}"
    )
    return f"{cpu}; vcpus={vcpus}"
