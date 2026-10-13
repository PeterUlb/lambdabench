// Package cpuinfo logs the host CPU when the bencher asks for it.
//
// After each cold+warm cycle the bencher sends one extra, unrecorded invoke whose
// ClientContext carries {"custom":{"lambdabench":"cpu"}}. On that invoke
// ReportIfRequested logs one `LAMBDABENCH_CPU <cpu>` line, which the bencher reads
// from the log tail and stamps on the cycle's rows (Lambda mixes Graviton
// generations on arm64). Recorded invokes carry no ClientContext, so for them this
// is a map lookup and /proc/cpuinfo is never read. Same output as the Rust
// scenarios' lambdabench-cpuinfo crate.
package cpuinfo

import (
	"context"
	"fmt"
	"os"
	"strings"

	"github.com/aws/aws-lambda-go/lambdacontext"
)

// ReportIfRequested logs the host CPU if the invoke's ClientContext asks for it.
func ReportIfRequested(ctx context.Context) {
	if !requested(ctx) {
		return
	}
	info, _ := os.ReadFile("/proc/cpuinfo")
	fmt.Fprintf(os.Stderr, "LAMBDABENCH_CPU %s\n", describe(string(info)))
}

// describe names the CPU: the x86 model name, which Firecracker (Lambda's VM
// layer) normalizes to "Intel(R) Xeon(R) Processor" plus the host's nominal clock
// (family/model/stepping come from its CPU template and are fixed: model 63 in every probe), or the arm64
// implementer, part and variant, which keep their host values (part 0xd0c is Neoverse N1 / Graviton2, 0xd40 V1 / Graviton3, 0xd4f V2 /
// Graviton4), plus the vCPU count.
func describe(info string) string {
	field := func(name string) string {
		for _, line := range strings.Split(info, "\n") {
			key, value, found := strings.Cut(line, ":")
			if found && strings.TrimSpace(key) == name {
				return strings.TrimSpace(value)
			}
		}
		return ""
	}
	vcpus := 0
	for _, line := range strings.Split(info, "\n") {
		if strings.HasPrefix(line, "processor") {
			vcpus++
		}
	}
	cpu := field("model name")
	if cpu == "" {
		cpu = fmt.Sprintf("arm implementer=%s part=%s variant=%s",
			field("CPU implementer"), field("CPU part"), field("CPU variant"))
	}
	return fmt.Sprintf("%s; vcpus=%d", cpu, vcpus)
}

// requested reports whether the invoke's ClientContext asks for the host CPU.
func requested(ctx context.Context) bool {
	lc, ok := lambdacontext.FromContext(ctx)
	return ok && lc.ClientContext.Custom["lambdabench"] == "cpu"
}
