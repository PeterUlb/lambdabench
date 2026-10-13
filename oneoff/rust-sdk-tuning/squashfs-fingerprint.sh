#!/bin/bash
# Identify how Lambda compresses the function's code image (/var/task). Builds
# zips of the first-touch bootstrap plus one 1 MiB test file each: random
# data, and random chunks of 24/40/56/100 KB repeated. An algorithm can only
# compress a repeat that fits its match window (deflate 32 KB, lzo ~48 KB, lz4
# 64 KB, zstd/xz up to the block size), so which files shrink identifies it,
# and how much they shrink gives the block size. Deploy the `sq-*` variants
# with PROBE_ENV_FULL=1 and compare the `vdc` size in the reported
# /proc/partitions against `sq-base`. Usage, after build.sh arm64 first-touch:
#   oneoff/rust-sdk-tuning/squashfs-fingerprint.sh
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$REPO/.tmp/oneoff-rust-sdk-tuning"
WORK="$OUT/sqfsprobe"
rm -rf "$WORK" && mkdir -p "$WORK"
cp "$OUT/lambda-first-touch-arm64/bootstrap/bootstrap" "$WORK/"
python3 -I - "$WORK" <<'PY'
import random, sys, os
r, size, out = random.Random(42), 1 << 20, sys.argv[1]
def write(name, data): open(os.path.join(out, name), "wb").write(data)
write("rand1m", bytes(r.getrandbits(8) for _ in range(size)))
for kb in (24, 40, 56, 100):
    chunk = bytes(r.getrandbits(8) for _ in range(kb * 1024))
    write(f"rep{kb}k", (chunk * (size // len(chunk) + 1))[:size])
PY
cd "$WORK"
for f in base rand1m rep24k rep40k rep56k rep100k; do
  rm -f "$OUT/sq-$f-arm64.zip"
  if [ "$f" = base ]; then zip -q "$OUT/sq-$f-arm64.zip" bootstrap
  else zip -q -0 "$OUT/sq-$f-arm64.zip" bootstrap "$f"; fi
done
ls -l "$OUT"/sq-*-arm64.zip | awk '{print $5, $9}'
