# Link-time ordering of cold-start code

Tests whether placing the functions a cold start runs next to each other in the binary reduces the
compressed 128 KB blocks Lambda has to decompress (see `site/src/tuning.md`, "Read the executable at
init"). The profile and the x86_64 builds run in an `amazonlinux:2023` x86_64 container (glibc
2.34, as on `provided.al2023`) with Rust's bundled lld; arm64 builds reuse that profile (see
[arm64](#arm64)).

All paths assume `.tmp/` of the repo mounted at `/w` and this tooling at `/env`:

1. `setup.sh` (inside `amazonlinux:2023`, then `docker commit` the container as `al-build:1`):
   gcc, cmake, valgrind and Rust 1.97.1 into `/env`.
2. `build.sh NAME [ORDER_FILE [SUFFIX]]`: builds `/w/sf-order` (a copy of
   `.tmp/sf-cnt-new`, the `smithyfull` copy with the counters module from `../smithyfull/`) with
   lld, stripped as `out/NAME` and unstripped as `out/NAME-sym`; with an ordering file it passes
   `--symbol-ordering-file`.
3. `profile.sh BIN OUT`: runs the unstripped build under callgrind (`--demangle=no`) behind
   `fakeapi.py`, a minimal Lambda runtime API that holds the first event until the Init profile
   is dumped (`OUT.1`), then sends `event.json` (a copy of `../smithyfull-event.json`) and dumps
   the first request (`OUT.2`). Needs the probe resources (`../resources.sh up`), their
   `LAMBDABENCH_*` variables and AWS credentials as environment variables.
4. The ordering file lists the mangled names from `fn=` lines of the executable's object (`ob=`),
   Init's functions first, then the request's new ones, filtered to names `nm` finds.
5. `blocks.py BIN` (reads `sf-order/reqonly.txt` and `sf-order/order2.txt`) counts the 128 KB
   blocks Init's and the request's functions span.

Results are in the scratchpad notes (phase AE) and on the tuning page.

## arm64

cargo-lambda cross-links arm64 through zig, which rejects `--symbol-ordering-file`.
`zig-order-link.sh` works around that as the linker: it links through zig as usual with
`ZIG_VERBOSE_LINK=1`, then re-runs the `ld.lld` command zig printed with rustc's own `rust-lld` and
the ordering file added. `build-arm64.sh SRC OUT [ORDER]` builds a smithyfull copy that way
(stripped `OUT` plus unstripped `OUT-sym`).

Two ways to get the arm64 ordering file:

- **Native profile (the one that counts).** `profile.sh` with `ONE_DUMP=1` inside an
  `amazonlinux:2023` arm64 container with valgrind and python3 (`--platform linux/arm64`, runs
  under Docker's qemu emulation; Init plus one request took about a minute). `callgrind_control`
  cannot reach valgrind under emulation, so in that mode `fakeapi.py` drops the second `/next`,
  the function exits, and callgrind writes the profile at exit. `cg-order.py BIN PROFILE...` turns
  the profile into the ordering file (it reproduces the x86_64 `order.txt` and `order2.txt`).
- **The x86_64 list, mapped.** `map-order.py ORDER X86_SYM ARM64_SYM` translates the x86_64 list
  to arm64 symbol names by demangled name (Rust's symbol hashes differ per target): 2,258 of
  2,427 functions mapped with the same compiler (1.97.1), but 279 functions of the arm64 run had
  no counterpart, and this layout made the first request slower.

Results (phases UD to UH in the scratchpad notes; 20 cold starts per cell in `eu-central-1` and
`us-east-1`, Rust 1.97.1): default layout 374 / 241 / 191 ms at 128 / 256 / 1,024 MB, native
order 365 / 244 / 198, mapped x86_64 order 425 / 261 / 195. Page traces: Init touched 913 pages
with the native order against 1,591; the first request loaded 12 new blocks with the default
layout, 13 with the native order and 20 with the mapped one.
