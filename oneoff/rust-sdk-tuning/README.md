# rust-sdk-tuning: one-off AWS SDK for Rust cold-start probe

Source for the AWS SDK for Rust levers in [`site/src/tuning.md`](../../site/src/tuning.md). It is
not part of the benchmark matrix: `bencher` never builds or deploys it, and its numbers are dated
one-off characterizations, not committed data. Like the matrix it measures the cold start as Init
Duration + the first request's Duration from the `REPORT` line.

## What it measures

The SDK probe (`src/main.rs`) is a `lambda_runtime` function in two flows:

| `PROBE_FLOW` | Work per request                                                                       |
| ------------ | -------------------------------------------------------------------------------------- |
| `one`        | DynamoDB `GetItem`, the matrix's `oneclient` work                                      |
| `three`      | KMS `Encrypt` + DynamoDB `PutItem` + S3 `PutObject`, the matrix's `smithyfull` SDK work |

Cargo feature sets build one binary per variant (`build.sh`):

| Variant         | SDK crates                                                                         |
| --------------- | ---------------------------------------------------------------------------------- |
| `default`       | the SDK crates' default features, as the matrix scenarios use them                 |
| `slim`          | `default-features = false` plus only the hyper 1.x + rustls/aws-lc client          |
| `ring`          | as `slim`, but rustls on ring, passed to the loader as an explicit HTTP client     |
| `slim-mimalloc` | `slim` with mimalloc as the global allocator                                       |
| `slim-sigv4a`   | `slim` plus S3's `sigv4a` feature, to isolate its cost                              |
| `slim-pad`      | `slim` plus ~2 MiB of incompressible data the process never reads (file size alone) |
| `diag`          | `default` plus timestamped SDK tracing, for reading where the first request's time goes |

Within a binary, every runtime lever is an environment variable (`PROBE_REGION`, `PROBE_CREDS`,
`PROBE_RT`, `PROBE_PREWARM`, `PROBE_PREFAULT`, `PROBE_CHECKSUM`; see the header of `src/main.rs`),
plus `SSL_CERT_FILE`, so one build covers many variants. Each response also carries process CPU time, major and minor page faults
for Init (since process start) and for the request, and how much of the executable is resident in
memory after each (`/proc/self/smaps`).

`first-touch/` is a second, minimal custom-runtime binary behind the page-cache finding. It times
the first call of each crypto primitive (aws-lc-rs and ring) and two real rustls handshakes, with
and without the executable read into the page cache first. Each step also reports process CPU
time, major and minor page faults, and bytes read from the device (`/proc/self/io`), plus where
`/var/task` is mounted. It builds as the `first-touch` variant (and as `first-touch-jitter` with
AWS-LC's jitter entropy left on) and needs only the IAM role (`resources.sh role`).
`squashfs-fingerprint.sh` builds `sq-*` variants from it that identified the code image's
compression (gzip, 128 KB blocks) from the code device's size in `/proc/partitions`.
With `PROBE_MADVISE=random|sequential|normal` it calls `madvise` on the executable's mappings first
thing in `main()`, which showed that the first-use cost follows the KB read from the code device
(~7 µs of CPU per KB on arm64), not the number of faults. `PROBE_TOUCH=N` measures single block loads the same way, `PROBE_STEPS=dns` times repeated
lookups of AWS endpoints, and `PROBE_READ_FILE` takes a comma-separated list of packaged files to
read at init and reports `/proc/meminfo`, which showed that page cache counts toward "Max Memory
Used"; `PROBE_REREAD=1` reads them twice, which showed eviction at the memory limit.

The matrix's real `smithyfull` (Smithy server SDK + three clients) was also measured as a variant,
built outside this directory from copies of `scenarios/rust/smithyfull` with the levers applied by
hand (slim features plus a concurrent prewarm behind `SF_PREWARM=1`, and the executable read behind
`SF_PREFAULT=1`). The invoke payload for it is `smithyfull-event.json` (`@payload=` in a config line). The patches and the counters module are in `smithyfull/`.

`other-runtimes/` holds copies of the matrix's Go and Java `oneclient` handlers with the same
counters (major/minor faults, `/proc/self/io` read bytes, CPU for Go), printed as one `PROBE` line on
the first request. They checked whether the executable read carries over (it does not) and measured
the Go levers on the tuning page:

- `go/main.go`: `GB_PREREAD=exe` reads the executable first thing in `main()`, `GB_LOCALWARM=1` runs
  the tuning page's local warm-up, `GB_PREWARM=1` makes one `GetItem` at init; `SSL_CERT_DIR` is set
  per function. It also reports how many roots the system pool holds. Build it in place of
  `scenarios/go/oneclient/main.go` (same module) with the bencher's flags:
  `GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -tags lambda.norpc -trimpath -ldflags='-s -w' -o bootstrap ./oneclient`
  (a `golang:1.26` container works), then zip `bootstrap` as `go-oneclient-<arch>.zip`.
- `java/OneClientHandler.java`: `JB_PREREAD=none|task|lang|all` reads the jars in `/var/task`, the
  JDK's `lib/modules` image, or both at init. Build it in place of the matrix's handler with
  `./gradlew :oneclient:buildZip` and copy the zip as `java-oneclient-<arch>.zip`. Config lines need
  `@runtime=java25 @handler=lambdabench.OneClientHandler::handleRequest`.

- `go/stat.go` (built with `go/main.go`): `GB_STARTONLY=1` skips the SDK and reports the state at
  the first line of `main()` (major faults, how much of the executable is resident from
  `/proc/self/smaps`, the virtio disk sizes). `go-min/main.go` reports the same from a function
  without the SDK; copy `go/stat.go` next to it. With `GODEBUG=inittrace=1` the logs show each
  package's init time. `go/pages.go` adds a `PAGES main` log line with a bitmap of the executable's
  pages mapped at that point (`/proc/self/pagemap` present bits; its `mincore` page-cache bitmap
  came back empty on Lambda). `go-pages.py SECTIONS PAGES` breaks it down by ELF section, where
  SECTIONS is `readelf -SW bootstrap` reduced to name, offset and size per line; copy
  `go/pages.go` next to `go-min/main.go` as well.
  `go/touch.go`: `GB_TOUCH=N` (with `GB_STARTONLY=1`) reads one byte at N 128 KB blocks of the
  executable that have nothing mapped nearby and reports thread CPU, major faults and KB read per
  touch, the same test as the first-touch probe's `PROBE_TOUCH`. `/proc/self/io` `read_bytes` at that point (~7-8 MB) exceeds the whole code
  device (2.5-3.5 MB), so it also counts reads that are not the function's code and is not used.

Details behind the tuning page's "Other runtimes" note (2026-10):

- Go: at the first line of `main()` 6.8 of the 8.5 MB SDK binary was mapped (4.7 of 5.7 MB for the
  minimal function), touching 28 of 29 128 KB code blocks, all of `.rodata` and 18 of 26 blocks of
  `.gopclntab`; `GODEBUG=inittrace=1` showed 108 package inits taking 26 ms with almost no
  allocations. With `SSL_CERT_DIR` skipped, the read removed the first request's ~7 major faults
  (~1.7 ms of CPU together). A fresh block load (`GB_TOUCH`) cost Go ~0.55 ms of CPU and ~77 KB
  read, the same per KB as Rust. For comparison the Rust SDK probe had ~5.0 of 10.0 MB mapped
  after all of Init.
- Java: `/` is an overlay root (`/var/rootfs/operator/runtime`) holding the JDK; its 127 MB
  `lib/modules` read in ~370 ms at 512 MB and up. The first request had ~16 major faults and read
  ~6 MB, mostly from that image.

Both read `LAMBDABENCH_KEY`, so their config lines add `LAMBDABENCH_KEY=seed`.

## Running it

```sh
oneoff/rust-sdk-tuning/resources.sh up            # role, table, KMS key, bucket (AWS_REGION, default eu-central-1)
oneoff/rust-sdk-tuning/build.sh arm64             # all variants; or name some: build.sh x86_64 slim ring
python3 -I oneoff/rust-sdk-tuning/probe.py deploy  CONFIGS
python3 -I oneoff/rust-sdk-tuning/probe.py measure CONFIGS 15 5   # rounds, warm invokes per round
python3 -I oneoff/rust-sdk-tuning/probe.py delete  CONFIGS
oneoff/rust-sdk-tuning/resources.sh down          # schedules the KMS key for deletion (7 days)
```

`AWS_REGION` picks the region for both scripts. Several regions can be up at once: the role is
global and carries one data policy per region, and each region gets its own
`resources-<region>.env` and `results-<timestamp>-<region>.jsonl`. Run `measure` once per region,
at the same time, to compare regions on the same schedule.

`CONFIGS` is a TSV with one function per line (name, variant, arch, memory, environment); the
format is in `probe.py`'s docstring. A line looks like:

```
three-128-pwnet	slim	arm64	128	PROBE_FLOW=three PROBE_PREWARM=network SSL_CERT_FILE=/etc/pki/tls/certs/ca-bundle.crt
```

Builds use cargo-lambda with the matrix's `[profile.lambda]` and `AWS_LC_SYS_NO_JITTER_ENTROPY=1`.
Each measure round forces a cold start on every function by changing an environment variable,
then invokes it once cold and N times warm, all functions in parallel. Raw results land in
`.tmp/oneoff-rust-sdk-tuning/results-<timestamp>-<region>.jsonl`; `probe.py summary FILE...` prints
medians. Resource and function names use the `oneoff-sdk-tuning` prefix, so this probe and
`bencher teardown` never touch each other's resources.

Retries are disabled as in the matrix, so a transient service error fails the invoke instead of
inflating it. About 3 in 100 cold invokes of the three-call flow failed that way, with a
generic `service error`; the summary drops them.

## Host CPU

The first-touch probe's environment report and the smithyfull counters (`probe_counters.rs`)
include the host CPU from `/proc/cpuinfo`: the arm64 part number (`0xd0c` Neoverse N1 /
Graviton2, `0xd40` V1 / Graviton3, `0xd4f` V2 / Graviton4) or the x86 model name. Lambda placed
arm64 functions on all three generations, chosen per execution environment, with a different mix
per region, and the generation moved cold starts by about 30% (Graviton2 against Graviton4, combining the gaps measured within each region). Group results by it when a
comparison looks noisy.

`cpu-features/` is a Python 3.14 function behind the instruction-set findings on the site. Each
cold start returns the first `/proc/cpuinfo` block (model name, cache size, the full `flags` or
`Features` list, the kernel's `bugs` line) and times SHA-256, SHA-512 and SHA3-256 over 1 MiB
buffers and a CPU-bound Python loop, and says whether the invoke was a cold start; `run.sh` keeps
only cold starts. 2026-10-14 and 2026-10-15, `eu-central-1`, 1,769 MB, two runs of 30 cold starts
per architecture: every x86_64 host showed one identical flag list (AVX2; no AVX-512, ADX, SHA or VAES;
family 6 model 63), while each Graviton generation showed its own. To repeat it from this
directory:

```sh
. ../../.tmp/oneoff-rust-sdk-tuning/resources-eu-central-1.env   # ROLE_ARN, from `resources.sh role`
export OUT=../../.tmp/oneoff-rust-sdk-tuning/cpu-features
mkdir -p "$OUT" && zip -qj "$OUT/f.zip" cpu-features/handler.py
for a in x86_64 arm64; do
  aws lambda create-function --function-name oneoff-sdk-tuning-flags-$a --runtime python3.14 \
    --architectures $a --handler handler.handler --memory-size 1769 --timeout 60 \
    --role "$ROLE_ARN" --zip-file "fileb://$OUT/f.zip"
done
cpu-features/run.sh x86_64 30 & cpu-features/run.sh arm64 30; wait   # appends to $OUT/res-<arch>.jsonl
```

Delete the two functions and their log groups afterwards.

## Last run

SDK probe: 2026-10-12/13, `eu-central-1`, 128 and 1024 MB, arm64 (x86_64 for the three-call flow), 15 forced
cold starts per configuration, `aws-config` 1.12.0, `aws-lc-rs` 1.18.1 (`aws-lc-sys` 0.45.0),
`rustls` 0.23.45, `lambda_runtime` 1.4.0. The probe binary links all three SDK clients in both
flows, so its absolute `one` numbers sit above the matrix's `oneclient`; compare variants with each
other, not with the matrix. When re-running, update the tables and their date in
`site/src/tuning.md` together with this section.

Go and Java (`other-runtimes/`): 2026-10-13, `eu-central-1`, Go 1.26 with the matrix's
`scenarios/go/go.sum`, Java 25 with the matrix's SDK BOM, 15 (Go) and 10 (Java) forced cold starts
per configuration.

The `smithyfull` runs for the second region, the host-CPU split and the benchmark's own lockfile
(the tuning page's "putting it together" and measurement notes): 2026-10-13, `eu-central-1` and `us-east-1` interleaved, 10 to 30
forced cold starts per configuration (the page gives the count per table), `smithyfull` on the
matrix's lockfile (`aws-config` 1.8.18, `rustls-native-certs` 0.8.5). CPU feature probe (`cpu-features/`): 2026-10-14 and
2026-10-15, `eu-central-1`, two runs of 30 cold starts per architecture.
