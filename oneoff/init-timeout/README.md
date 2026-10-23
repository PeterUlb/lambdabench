# init-timeout: one-off init time-limit probes

Source for the init time-limit sections of [`site/src/lifecycle.md`](../../site/src/lifecycle.md)
("How much time a re-init gets" through "How long a re-init loop lasts") and the SnapStart notes in
[`site/src/java-snapstart.md`](../../site/src/java-snapstart.md). It is not part of the benchmark
matrix: `bencher` never builds or deploys it, and its numbers are dated one-off characterizations
(2026-10, `eu-central-1`, arm64), not committed data.

## What is here

| File                  | Role                                                                                                                                                                                                                          |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `fn/index.mjs`        | The Node function. Init sleeps `INIT_MS` and runs `INIT_WORK` million loop iterations (reporting its speed per second); a `{"hang"}`, `{"exit"}` or `{"oom"}` payload crashes the invoke and leaves a `/tmp` marker so the re-init uses `REINIT_MS` / `REINIT_WORK` instead. Every response reports `getRemainingTimeInMillis()` at the handler's start |
| `rust/`               | A Rust function with the same init sleep and hang (workspace member, built with cargo-lambda)                                                                                                                                 |
| `java/Probe.java`     | A Java function with the same init sleep, hang and exit, for SnapStart (no dependencies, built with `javac`)                                                                                                                  |
| `ext/`                | An external extension, published as a layer, that is slow to become ready (`EXT_INIT_MS`, `EXT_REINIT_MS`)                                                                                                                   |
| `run.sh`              | `up` / `down`, and one function per `fn`, plus provisioned concurrency (`pc`), SnapStart versions (`publish`), the extension layer (`ext`) and single invokes with their log tail (`invoke`)                                   |
| `loop.sh`             | How long a re-init loop lasts: one caller at a fixed gap, or several back to back with one timeout injected                                                                                                                   |
| `loopstats.py`        | Summary of a `loop.sh par` run: the failure timeline and, per log stream, the stuck environment's timed-out re-inits                                                                                                          |

Every resource name starts with `oneoff-inittimeout-`, outside bencher's `lambdabench-` names.
`AWS_REGION` picks the region (default `eu-central-1`); builds and responses go to
`.tmp/oneoff-init-timeout/`.

## Prerequisites

AWS CLI v2, Node.js 24, Python 3, `cargo lambda` (for the Rust function) and a JDK with `javac`
21 or later (for the Java one).

## Reproducing

```sh
./run.sh up                                  # role, Node zip, Rust and Java builds
./run.sh fn a 100 3 14000                    # 3 s timeout, 0.1 s init, 14 s re-init
./run.sh invoke a; ./run.sh invoke a '' hang; ./run.sh invoke a   # the re-init is cut off at 13 s

PROBE_MEMORY=128 PROBE_ENV="INIT_WORK=8600" ./run.sh fn cpu 0 300
./run.sh invoke cpu                          # an init over 10 s, retried; see initWork.perSecond

./run.sh fn d 25000 10 && ./run.sh pc d      # provisioned concurrency, 25 s init
./run.sh invoke d live; ./run.sh invoke d live hang; ./run.sh invoke d live   # cut off at 20 s

./loop.sh par a 20 8 > par.txt && python3 -I loopstats.py par.txt oneoff-inittimeout-a
./run.sh down                                # functions, log groups, layer, role
```

`PROBE_RUNTIME=rust` or `java` (with `PROBE_SNAPSTART=1`) switches the runtime, and `PROBE_LAYER`
adds the layer that `./run.sh ext` prints. A provisioned-concurrency configuration bills while it
exists, so run `down` (or delete it) when done.
