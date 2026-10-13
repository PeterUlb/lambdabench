#!/bin/bash
# build.sh OUT_NAME [ORDER_FILE]: builds /w/sf-order with lld (unstripped copy as OUT_NAME-sym).
set -euo pipefail
export RUSTUP_HOME=/env/rustup CARGO_HOME=/env/cargo PATH=/env/cargo/bin:$PATH
export AWS_LC_SYS_NO_JITTER_ENTROPY=1
GCCLD=/env/rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld
FLAGS="-C link-arg=-fuse-ld=lld -C link-arg=-B$GCCLD"
TD=/w/sf-order/target-lld
if [ -n "${2:-}" ]; then
  FLAGS="$FLAGS -C link-arg=-Wl,--symbol-ordering-file=$2 -C link-arg=-Wl,--no-warn-symbol-ordering"
  TD=/w/sf-order/target-lld-ordered${3:-}
fi
cd /w/sf-order
mkdir -p /w/sf-order/out
CARGO_PROFILE_LAMBDA_STRIP=false RUSTFLAGS="$FLAGS" cargo build --quiet --profile lambda --target-dir $TD
cp $TD/lambda/bootstrap /w/sf-order/out/$1-sym
CARGO_PROFILE_LAMBDA_STRIP=true RUSTFLAGS="$FLAGS" cargo build --quiet --profile lambda --target-dir $TD
cp $TD/lambda/bootstrap /w/sf-order/out/$1
ls -la /w/sf-order/out/
