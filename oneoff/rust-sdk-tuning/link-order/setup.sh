#!/bin/bash
set -euxo pipefail
dnf install -y -q gcc gcc-c++ make cmake perl-FindBin perl-File-Compare perl-IPC-Cmd perl-lib valgrind tar gzip xz findutils which >/dev/null
export RUSTUP_HOME=/env/rustup CARGO_HOME=/env/cargo
[ -x /env/cargo/bin/cargo ] || curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.97.1 --no-modify-path
/env/cargo/bin/rustc -vV | grep host
ls /env/rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld/
