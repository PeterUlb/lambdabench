# Scenario "cache": a long-lived in-memory working set, churned every invoke,
# the dedicated garbage-collection probe.
#
# At init the handler allocates a large RETAINED live set: ENTRIES byte buffers of
# ENTRY_BYTES each (~100 MB), held in module state that persists across every warm
# invocation, the way a real handler holds an in-process cache, an LRU, a buffer
# pool, or loaded reference data for the life of the sandbox.
#
# Each warm invoke does two things:
#   1. Churn: replace CHURN of the entries with freshly allocated buffers
#      (eviction + insert). The replaced buffers become garbage while the live set
#      stays full, generating garbage against a large permanently-live heap.
#   2. Scan: read every 10th entry and sum a byte. This keeps the whole retained
#      set genuinely live and read, so it cannot be elided.
#
# Why this workload (contrast `batch`): a tracing collector's cost scales with the
# size of the live heap it must trace, not with the garbage. Keeping a large live
# set permanently resident while generating garbage against it makes reclaim
# expensive every invoke, the path `batch` never reaches (its object graph is
# transient). At the smaller fractional-vCPU tiers CPython's refcounting + cyclic
# GC, plus its large per-object overhead, competes with the handler for the one
# core, so the warm P99/P99.9 tail opens up, worst at the starved low-memory tiers
# and easing as vCPU grows. A non-GC runtime frees each replaced buffer immediately,
# so its tail stays flat. Read the absolute tail latencies on the dashboard, not a
# P99/P50 ratio.
#
# Deliberately an indexed ring of bytearrays, not a dict: the point is to isolate
# the GC/allocator, not to compare dict implementations or hashing speed. No S3,
# no AWS clients, no payload; fully self-contained.

# Number of buffers in the retained live set.
ENTRIES = 200_000
# Bytes per buffer; ENTRIES * ENTRY_BYTES ≈ 100 MB of permanently-live heap.
ENTRY_BYTES = 512
# Buffers replaced per invoke (garbage generated + new live, set stays full).
CHURN = 40_000

# The retained working set, built once at init and mutated in place across warm
# invokes. Never released. That permanence keeps the collector managing the
# whole set. _rot is the ring cursor (module-global, carried across invokes).
_live = [bytearray(ENTRY_BYTES) for _ in range(ENTRIES)]
for _i in range(ENTRIES):
    _live[_i][0] = _i & 0xFF
_rot = 0


def _churn_and_scan():
    # Replace CHURN entries then scan every 10th entry. The replaced buffers become
    # garbage while the live set stays full, so the collector keeps managing the
    # whole ~100 MB set. The scan keeps it genuinely live.
    global _rot
    for c in range(CHURN):
        _rot = (_rot + 1) % ENTRIES
        b = bytearray(ENTRY_BYTES)
        b[0] = c & 0xFF
        _live[_rot] = b
    total = 0
    for i in range(0, ENTRIES, 10):
        total += _live[i][0]
    return total


def handler(event, context):
    return {
        "scenario": "cache",
        "entries": ENTRIES,
        "churned": CHURN,
        "checksum": _churn_and_scan(),
    }
