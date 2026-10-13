#!/bin/sh
# Linker for build-arm64.sh. zig (the cross linker cargo-lambda uses) rejects
# --symbol-ordering-file, so this links through zig as usual with ZIG_VERBOSE_LINK=1,
# then re-runs the ld.lld command zig printed with rustc's own lld and the ordering file
# added. Without LINK_ORDER_FILE it is a plain zig link. Needs ZIGCC (cargo-zigbuild's
# aarch64 zigcc wrapper) and RUST_LLD.
set -eu
if [ -z "${LINK_ORDER_FILE:-}" ]; then exec "$ZIGCC" "$@"; fi
log=$(mktemp)
if ! ZIG_VERBOSE_LINK=1 "$ZIGCC" "$@" 2>"$log"; then
  cat "$log" >&2
  rm -f "$log"
  exit 1
fi
cmd=$(grep '^ld.lld ' "$log" | tail -n 1 | sed 's/^ld.lld //')
rm -f "$log"
[ -n "$cmd" ] || { echo "zig-order-link: no ld.lld line from zig" >&2; exit 1; }
# shellcheck disable=SC2086 # zig prints one unquoted, space-separated argv
"$RUST_LLD" -flavor gnu $cmd --symbol-ordering-file="$LINK_ORDER_FILE" --no-warn-symbol-ordering
