#!/usr/bin/env python3
"""cg-order.py BIN CALLGRIND_OUT... > ORDER

Writes a symbol ordering file: the functions of BIN's own object that ran, from
callgrind dumps written with --compress-strings=no (profile.sh) of a binary with
BIN's file name, in dump order
(Init's dump first, then the request's new functions), keeping only names `nm`
finds in BIN.
"""

import os
import subprocess
import sys

binp, dumps = sys.argv[1], sys.argv[2:]
known = {line.split()[-1] for line in subprocess.run(
    ["nm", "--defined-only", binp], capture_output=True, text=True, check=True).stdout.splitlines()}
seen = []
seen_set = set()
for dump in dumps:
    ob = ""
    for line in open(dump, errors="replace"):
        if line.startswith("ob="):
            ob = line[3:].strip()
        elif line.startswith("fn=") and os.path.basename(ob) == os.path.basename(binp):
            name = line[3:].strip()
            if name in known and name not in seen_set:
                seen_set.add(name)
                seen.append(name)
print("\n".join(seen))
print(f"{len(seen)} functions", file=sys.stderr)
