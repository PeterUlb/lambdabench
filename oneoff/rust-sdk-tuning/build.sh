#!/bin/bash
# Build the probe's binary variants (cargo feature sets) with cargo-lambda into
# .tmp/oneoff-rust-sdk-tuning/<variant>-<arch>.zip. Usage:
#   oneoff/rust-sdk-tuning/build.sh [arch] [variant...]   (default: arm64, all)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
OUT="$REPO/.tmp/oneoff-rust-sdk-tuning"
ARCH="${1:-arm64}"
shift || true
# variant<TAB>crate dir (relative to here)<TAB>cargo feature flags
ALL="default	.	--features sdk-defaults
slim	.	--no-default-features --features slim
slim-sigv4a	.	--no-default-features --features slim,sigv4a
slim-pad	.	--no-default-features --features slim,pad
ring	.	--no-default-features --features ring
slim-mimalloc	.	--no-default-features --features slim,mimalloc
diag	.	--features sdk-defaults,diag
first-touch	first-touch	
first-touch-jitter	first-touch	"
case "$ARCH" in
  arm64) ARCH_FLAG=--arm64 ;;
  x86_64) ARCH_FLAG=--x86-64 ;;
  *) echo "unknown arch $ARCH (arm64 or x86_64)" >&2; exit 1 ;;
esac
mkdir -p "$OUT"
while IFS=$'\t' read -r variant crate flags; do
  if [ $# -gt 0 ] && [[ " $* " != *" $variant "* ]]; then continue; fi
  echo "+ build $variant-$ARCH ($flags)"
  dir="$OUT/lambda-$variant-$ARCH"
  # Same flag-set baseline as the matrix (DESIGN.md, Fairness #4), except the
  # `-jitter` variants, which keep AWS-LC's jitter entropy on. Separate target
  # dirs: AWS_LC_SYS_NO_JITTER_ENTROPY is not in cargo's fingerprint.
  jitter_env=(AWS_LC_SYS_NO_JITTER_ENTROPY=1)
  target="$OUT/target-${crate//./sdk}-$ARCH"
  if [[ "$variant" == *-jitter ]]; then
    jitter_env=(-u AWS_LC_SYS_NO_JITTER_ENTROPY)
    target="$target-jitter"
  fi
  (cd "$HERE/$crate" && env "${jitter_env[@]}" CARGO_TARGET_DIR="$target" \
    cargo lambda build -q --profile lambda "$ARCH_FLAG" $flags --lambda-dir "$dir")
  rm -f "$OUT/$variant-$ARCH.zip"
  (cd "$dir/bootstrap" && zip -q "$OUT/$variant-$ARCH.zip" bootstrap)
  ls -l "$dir/bootstrap/bootstrap" "$OUT/$variant-$ARCH.zip" | awk '{print $5, $9}'
done <<<"$ALL"
