// Scenario "cache": a long-lived in-memory working set, churned every invoke,
// the dedicated garbage-collection probe.
//
// At init the handler allocates a large RETAINED live set: `entries` byte buffers
// of `entryBytes` each (~100 MB), held in package state that persists across
// every warm invocation, the way a real handler holds an in-process cache, an
// LRU, a buffer pool, or loaded reference data for the life of the sandbox.
//
// Each warm invoke does two things:
//  1. Churn: replace `churn` of the entries with freshly allocated buffers
//     (eviction + insert). The replaced buffers become garbage while the live set
//     stays full, generating garbage against a large permanently-live heap.
//  2. Scan: read every 10th entry and sum a byte. This keeps the whole retained
//     set genuinely live and read, so it cannot be elided and a tracing GC has to
//     mark all of it.
//
// Why this workload (contrast `batch`): a tracing GC's per-cycle cost scales with
// the size of the live heap it must trace, not with the garbage. Keeping a large
// live set permanently resident while generating garbage against it makes every GC
// cycle expensive, the path `batch` never reaches (its object graph is transient
// and Go's compact representation stays far from the ceiling). At the smaller
// fractional-vCPU tiers the collector cannot run on a spare core and steals time
// from the handler, so the warm P99/P99.9 tail separates from the median, worst at
// the starved low-memory tiers and easing as vCPU grows. A non-GC runtime frees
// each replaced buffer immediately, so its tail stays close to its median. Read
// the absolute tail latencies on the dashboard, not a P99/P50 ratio.
//
// Deliberately an indexed ring of buffers, not a map: the point is to isolate the
// GC/allocator, not to compare map implementations or hashing speed. No S3, no AWS
// clients, no payload; fully self-contained.
package main

import (
	"context"

	"github.com/aws/aws-lambda-go/lambda"
)

const (
	// entries is the number of buffers in the retained live set.
	entries = 200_000
	// entryBytes is the size of each buffer; entries*entryBytes ≈ 100 MB live.
	entryBytes = 512
	// churn is the number of buffers replaced per invoke (set stays full).
	churn = 40_000
)

// live is the retained working set; rot is the ring cursor. Both persist across
// warm invokes (package-global state), and live is never freed. That permanence
// is what keeps the tracing GC's per-cycle cost high.
var (
	live [][]byte
	rot  int
)

func init() {
	live = make([][]byte, entries)
	for i := range live {
		live[i] = make([]byte, entryBytes)
		live[i][0] = byte(i & 0xff)
	}
}

type response struct {
	Scenario string `json:"scenario"`
	Entries  int    `json:"entries"`
	Churned  int    `json:"churned"`
	Checksum uint64 `json:"checksum"`
}

// churnAndScan replaces `churn` entries then scans every 10th entry. The replaced
// buffers become garbage while the live set stays full, so the GC keeps tracing
// the whole ~100 MB set. The scan keeps it genuinely live.
func churnAndScan() uint64 {
	for c := 0; c < churn; c++ {
		rot = (rot + 1) % entries
		b := make([]byte, entryBytes)
		b[0] = byte(c & 0xff)
		live[rot] = b
	}
	var sum uint64
	for i := 0; i < entries; i += 10 {
		sum += uint64(live[i][0])
	}
	return sum
}

func handle(_ context.Context) (response, error) {
	return response{
		Scenario: "cache",
		Entries:  entries,
		Churned:  churn,
		Checksum: churnAndScan(),
	}, nil
}

func main() {
	lambda.Start(handle)
}
