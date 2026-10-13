#!/usr/bin/env python3
"""map-order.py ORDER SRC_BIN DST_BIN > ORDER_FOR_DST

Translates a symbol ordering file profiled on one target (SRC_BIN, unstripped) to
another target's symbol names (DST_BIN, unstripped, same source). Rust symbol names
carry crate hashes that differ per target, so symbols are matched by their demangled
name (legacy `::h<hash>` suffixes dropped). Coverage goes to stderr.
"""

import collections
import re
import subprocess
import sys


def names(path):
    """Demangled key -> mangled names, in nm's symbol-table order."""
    def nm(*flags):
        out = subprocess.run(["nm", "--defined-only", "-p", *flags, path],
                             capture_output=True, text=True, check=True).stdout
        return [line.split(" ", 2)[2] for line in out.splitlines() if line.count(" ") >= 2]
    keys = collections.defaultdict(list)
    for raw, nice in zip(nm(), nm("--demangle")):
        keys[re.sub(r"::h[0-9a-f]{16}$", "", nice)].append(raw)
    return keys


order, src, dst = sys.argv[1:]
src_keys, dst_keys = names(src), names(dst)
raw_to_key = {raw: key for key, raws in src_keys.items() for raw in raws}
seen, hit, miss = set(), 0, 0
for sym in open(order).read().split():
    key = raw_to_key.get(sym, sym)
    targets = dst_keys.get(key)
    if not targets:
        miss += 1
        continue
    hit += 1
    for t in targets:
        if t not in seen:
            seen.add(t)
            print(t)
print(f"mapped {hit} of {hit + miss} symbols ({len(seen)} names on the target)", file=sys.stderr)
