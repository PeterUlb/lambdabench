#!/bin/bash
# profile.sh BIN OUT: callgrind dump OUT.1 at the end of Init, OUT.2 after the first request.
# With ONE_DUMP=1 only one profile, OUT, written when the function exits after the first request
# (the runtime API then fails the next poll; the combined order). This also works where
# callgrind_control cannot reach valgrind, as under qemu emulation.
set -uo pipefail
BIN=$1 OUT=$2
rm -f /tmp/next* /tmp/go /tmp/posted
EXIT_AFTER_FIRST=${ONE_DUMP:-} python3 /env/fakeapi.py /env/event.json &
sleep 1
AWS_LAMBDA_RUNTIME_API=127.0.0.1:9001 AWS_LAMBDA_FUNCTION_NAME=probe AWS_LAMBDA_FUNCTION_VERSION='$LATEST' AWS_LAMBDA_FUNCTION_MEMORY_SIZE=1024 \
  valgrind --tool=callgrind --demangle=no --callgrind-out-file=$OUT --dump-instr=no --compress-strings=no --compress-pos=no $BIN > /tmp/fn.log 2>&1 &
VG=$!
for i in $(seq 1 7200); do [ -f /tmp/next1 ] && break; sleep 0.5; done
if [ -z "${ONE_DUMP:-}" ]; then callgrind_control -d >/dev/null; echo "init dumped"; fi
touch /tmp/go
for i in $(seq 1 7200); do [ -f /tmp/next2 ] && break; sleep 0.5; done
if [ -n "${ONE_DUMP:-}" ]; then wait $VG; echo "exited, profile written"; else callgrind_control -d >/dev/null; echo "request dumped"; fi
cat /tmp/posted | cut -c1-200
grep -v "^==" /tmp/fn.log | tail -3
ls -la $(dirname $OUT)/$(basename $OUT)*
