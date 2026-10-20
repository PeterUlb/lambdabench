# web-functions: one-off Lambda Web Functions probes

Source for [`site/src/web-functions.md`](../../site/src/web-functions.md). It is not part of the
benchmark matrix: `bencher` never builds or deploys it, and its numbers are dated one-off
characterizations, not committed data. It compares Web Functions with Event Functions running the
benchmark's own handlers, timed from an in-region client.

## What is here

| File                      | Role                                                                                                                                                                                                             |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `server/server.mjs`       | The Web Function: a `node:http` server that calls a scenario handler per request and reports, in response headers, which environment served it and when that process started, listened and saw its first request |
| `server/background.mjs`   | Background-work probe, loaded on `/bg/start`: timers, a worker thread, a child process, network calls                                                                                                            |
| `client/`                 | The in-region client, an Event Function that times requests (`seq`, `burst`, `load`) and forced Event Function cold starts (`event-invoke`, `event-url`)                                                         |
| `resources.sh`            | `up` / `down`: IAM role, S3 bucket, DynamoDB table, client; `down` also deletes every probe function                                                                                                             |
| `web.sh`                  | Create a Web Function from a scenario, add endpoints, and create revisions of the same or changed code (each starts fresh environments)                                                                          |
| `event.sh`                | Create an Event Function baseline (Node bundle or Rust zip) with a Function URL                                                                                                                                  |
| `run.sh`                  | One subcommand per probe; results go to `.tmp/oneoff-web-functions/results/`                                                                                                                                     |
| `analyze.py`              | Summaries of those results and of the Web Functions' platform logs                                                                                                                                               |
| `events/smithy.json`      | The synthetic API Gateway event the `smithy` handler expects                                                                                                                                                     |
| `wrapper-probe/`          | A minimal Web Function that reports Node's startup marks, the wrapper's runtime API calls (via a `NODE_OPTIONS` preload) and its own first line, and times bare `node` loading `children/*`                      |
| `wrapper.sh`              | Create, revise, burst and time the wrapper probe                                                                                                                                                                 |
| `fixsim/`, `fixsim.sh`    | Express + DynamoDB Web Function, deployed twice: with a no-op preload and with one that keeps the wrapper's `import http` from loading undici, to measure what a wrapper fix would save                          |
| `rust-execve/`, `rust.sh` | An axum server (`oneclient` handler) that the Node process `execve`s into, from the entry module (`index.mjs`) or from a `NODE_OPTIONS=--require` preload (`pre.cjs`)                                            |
| `cloudfront.sh`           | A CloudFront distribution (caching off) in front of `hello`, logging to CloudWatch in us-east-1, to compare its `time-to-first-byte` with the client's waits                                                     |

Every resource name starts with `oneoff-webfn-`, outside bencher's `lambdabench-` names, so neither
side touches the other's resources. `AWS_REGION` picks the region (default `eu-central-1`).

## Prerequisites

- The benchmark's artifacts: the Node bundles (`cd scenarios/node && npm install && npm run build`,
  which writes `scenarios/node/dist/`) and, for the Rust comparison, `dist/rust-smithy-arm64-o3.zip`
  (`cargo run -p bencher -- build --lang rust`).
- AWS CLI v2 with the `aws lambda-web` commands, Node.js 24, Python 3.
- An account whose Web Functions vCPU quota (`aws lambda-web get-web-account-settings`) covers the
  bursts: the 1,000-request burst needs ~16 environments on a learned endpoint, but a new endpoint
  starts up to one environment per two requests.

The forced Event Function cold starts call `UpdateFunctionConfiguration`, which counts against the
account-wide 15 requests/s control-plane quota that a running `bencher` also uses. The client polls
with `GetFunction` (its own quota) and waits at least 3 s between updates, but avoid running the
Event Function cold probes during a benchmark run.

## Reproducing the page

```sh
cd oneoff/web-functions
./resources.sh up

./web.sh create hello hello                  # Web Functions (name, scenario)
./web.sh create oneclient oneclient
./web.sh create smithy smithy
./web.sh create load hello                   # fresh function for the concurrency probes
./web.sh create big hello 64 50              # hello + a 50 MB incompressible file
./event.sh create ev-hello node hello 2048   # Event Function baselines
./event.sh create ev-oneclient node oneclient 2048
./event.sh create ev-smithy node smithy 2048
./event.sh create rust-smithy-2048 rust smithy 2048
./event.sh create rust-smithy-128 rust smithy 128
U() { cat ../../.tmp/oneoff-web-functions/$1.url; }

# Concurrency is learned per endpoint: run this first, on the fresh `load` function
./run.sh burst new64 "$(U load)/sleep?ms=1000" 64 && python3 -I analyze.py burst new64

# Cold starts
for n in hello oneclient smithy big; do ./run.sh cold-web $n 10; python3 -I analyze.py cold-web $n; done
./run.sh cold-event-invoke ev-hello 10; ./run.sh cold-event-invoke ev-oneclient 10
./run.sh cold-event-invoke ev-smithy 10 smithy
./run.sh cold-event-url rust-smithy-2048 10 menu; ./run.sh cold-event-url rust-smithy-128 10 menu
for n in ev-hello ev-oneclient ev-smithy; do python3 -I analyze.py cold-event-invoke $n; done
for n in hello oneclient smithy big; do python3 -I analyze.py init $n; done

# Warm front door
./web.sh endpoint hello iam IamAuth
./run.sh warm web-noauth "$(U hello)/run" 300;  ./run.sh warm web-iam "$(U hello-iam)/run" 300 sign
./run.sh warm furl "$(U ev-hello)/" 300;        ./run.sh warm-invoke invoke-notail ev-hello 300
./run.sh warm-invoke invoke-tail ev-hello 300 tail
python3 -I analyze.py warm web-noauth web-iam furl invoke-notail invoke-tail

# Learning, per-endpoint state, from zero
./run.sh load learn "$(U load)/sleep?ms=200" 40 240 && python3 -I analyze.py load learn
./web.sh endpoint load e1 ApplicationManaged
./run.sh burst newendpoint64 "$(U load-e1)/sleep?ms=1000" 64
#   wait ~7 min without traffic to `load`, then:
./run.sh burst learned-fromzero64 "$(U load)/sleep?ms=1000" 64

# Learning across code changes: a fresh function, learned, then a same-code revision, a
# comment-only code change, and (after 15 min idle) /sleep made CPU-bound with 100 ms per request
./web.sh create rc hello && ./run.sh burst rc-new64 "$(U rc)/sleep?ms=1000" 64
./run.sh load rc-learn "$(U rc)/sleep?ms=200" 40 240
./web.sh revise rc && sleep 30 && ./run.sh burst rc-learned64 "$(U rc)/sleep?ms=1000" 64
./web.sh recode rc && sleep 30 && ./run.sh burst rc-recode64 "$(U rc)/sleep?ms=1000" 64
sleep 900; ./web.sh recode rc 64 100 && sleep 30 && ./run.sh burst rc-cpu64 "$(U rc)/sleep?ms=1000" 64
for l in rc-new64 rc-learned64 rc-recode64 rc-cpu64; do python3 -I analyze.py burst $l; done

# CPU-bound handlers
./run.sh load cpu-default "$(U load)/cpu?ms=50" 40 180
./web.sh revise load 1 && ./run.sh load cpu-mc1 "$(U load)/cpu?ms=50" 40 120

# Larger bursts (a new revision first, then 30 s)
./web.sh revise load 128 && sleep 30 && ./run.sh burst mc128-256 "$(U load)/sleep?ms=1000" 256
./web.sh revise load 64 && sleep 30 && ./run.sh burst b1000 "$(U load)/sleep?ms=1000" 1000 4

# CPU per request, idle CPU, spare environments, reclaim times (from platform logs)
./run.sh load hello-c20 "$(U hello)/run" 20 120; ./run.sh load oneclient-c20 "$(U oneclient)/run" 20 120
python3 -I analyze.py cpu hello; python3 -I analyze.py cpu oneclient
python3 -I analyze.py lifetimes load    # after the environments have been idle ~7 min

# Spare environments: a new function at the default 64, then bursts and steady traffic on the
# learned `load`, each on a fresh revision and ~8 min apart so its spares spin down first
./web.sh create sp10 hello && ./run.sh burst sp10 "$(U sp10)/sleep?ms=1000" 10
./web.sh revise load 64 && sleep 30 && ./run.sh burst sp256 "$(U load)/sleep?ms=1000" 256 2
sleep 480; ./web.sh revise load 64 && sleep 30 && ./run.sh burst sp512 "$(U load)/sleep?ms=1000" 512 4
sleep 480; ./web.sh revise load 64 && sleep 30 && ./run.sh load c800 "$(U load)/sleep?ms=200" 800 180 8
#   after ~8 min idle; `lifetimes` lists spares per revision, so per burst
python3 -I analyze.py lifetimes sp10; python3 -I analyze.py lifetimes load

# Rust Event Function vs Node Web Function, warm
./run.sh warm rust-2048 "$(U rust-smithy-2048)/menu" 300; ./run.sh warm web-smithy "$(U smithy)/run" 300

./resources.sh down    # and again ~7 min later, for log groups recreated at shutdown
```

### Background work

```sh
./web.sh create bg-idle hello; ./web.sh create bg-spin hello
curl "$(U bg-idle)/bg/start?net=1"; curl "$(U bg-spin)/bg/start?spin=1&net=1"
curl "$(U bg-idle)/bg/status"           # any time; the environment's state so far
```

Then send nothing for ~10 minutes. At spin-down each environment logs a final summary with
`evt: "sigterm"`, and its `platform.invokeSummary` lines show the CPU the background work used per
minute (`aws logs filter-log-events --log-group-name /aws/lambda/web/oneoff-webfn-bg-spin.local`).

### Startup before the entry module

```sh
./wrapper.sh create
./wrapper.sh burst 40      # ~20 environments: Node bootstrap, wrapper /load and /ready, first line, listen
./wrapper.sh children      # bare `node` in an environment: empty CJS/ESM vs `require`/`import` of `http`
./wrapper.sh revise && sleep 10 && ./wrapper.sh burst 40
```

The function sets `NODE_OPTIONS=--import /var/task/pre.mjs`; the preload runs before the wrapper and
records its runtime API calls through `diagnostics_channel`. `resources.sh down` deletes it with the
rest.

### What a wrapper fix would save

```sh
(cd fixsim && npm install)
./fixsim.sh create && sleep 10 && ./fixsim.sh burst 40
./fixsim.sh revise && sleep 10 && ./fixsim.sh burst 40    # repeat for more environments
```

`burst` prints, per host CPU and variant, the first line, the first request and when the first
`GetItem` finished, and whether undici was loaded.

### Rust via `execve`

```sh
./rust.sh create                       # builds rust-execve/ with cargo lambda (benchmark's lambda profile)
./rust.sh cold index 10;   python3 -I analyze.py cold-web rs-index
./rust.sh cold preload 10; python3 -I analyze.py cold-web rs-preload
./web.sh create oneclient oneclient && ./run.sh cold-web oneclient 10        # Node baseline
./event.sh create ev-rs-oneclient rust oneclient 2048                         # Rust Event baseline
./run.sh cold-event-url ev-rs-oneclient 10; ./run.sh cold-event-invoke ev-rs-oneclient 10
```

The Web Function servers report their host CPU in an `x-cpu` header, which the client records.

### CloudFront in front

```sh
./web.sh create hello hello
./cloudfront.sh create                 # distribution + log delivery, waits until deployed (~5 min)
./cloudfront.sh run 10                 # warm via CloudFront and direct, then 10 forced cold starts
#   wait a few minutes for the log delivery, then:
./cloudfront.sh logs && python3 -I analyze.py cloudfront
./cloudfront.sh down                   # before `resources.sh down`; disabling takes ~5 min
```

The page cites two runs, kept as `results/cf-run1/` and `results/cf-run2/`; `analyze.py cloudfront`
reads the files in `results/` itself, so copy a run's files there to re-analyze it.

## Cost

The repeat run in 2026-10, with all of the above, used ~6,800 vCPU-seconds and ~1 million Web
Function requests, about $0.55 at `eu-central-1` prices, plus the Web Functions' memory and small
amounts for the Event Functions, DynamoDB, S3 and CloudWatch Logs: around a dollar in total.
