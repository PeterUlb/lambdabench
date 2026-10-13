#!/bin/bash
# build-arm64.sh SRC_DIR OUT [ORDER_FILE]: builds the smithyfull copy in SRC_DIR for arm64
# through cargo-lambda's zig toolchain, ordered by ORDER_FILE if given (see zig-order-link.sh).
# Writes OUT (stripped) and OUT-sym (unstripped, for page tracing and map-order.py).
set -euo pipefail
SRC=$(cd "$1" && pwd); OUT=$(realpath -m "$2"); ORDER=${3:+$(realpath "$3")}
HERE=$(cd "$(dirname "$0")" && pwd)
W=$(ls -d ~/.cache/cargo-zigbuild/*/wrappers/* | tail -n 1)
T=aarch64-unknown-linux-gnu; TU=AARCH64_UNKNOWN_LINUX_GNU
export ZIGCC=$(ls "$W"/zigcc-$T-*.sh) AWS_LC_SYS_NO_JITTER_ENTROPY=1
export RUST_LLD="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld"
export "CC_${T//-/_}=$ZIGCC" "CXX_${T//-/_}=$(ls "$W"/zigcxx-$T-*.sh)" "AR_${T//-/_}=$W/ar" \
  "RANLIB_${T//-/_}=$W/zigranlib.sh" "CARGO_TARGET_${TU}_LINKER=$HERE/zig-order-link.sh"
export LINK_ORDER_FILE=${ORDER:-}
TD="$SRC/target-arm64-$(basename "${ORDER:-plain}" .txt)"
cd "$SRC"
for strip in false true; do
  CARGO_PROFILE_LAMBDA_STRIP=$strip CARGO_TARGET_DIR="$TD-$strip" cargo build -q --profile lambda --target $T
done
cp "$TD-false/$T/lambda/bootstrap" "$OUT-sym"
cp "$TD-true/$T/lambda/bootstrap" "$OUT"
ls -la "$OUT" "$OUT-sym"
