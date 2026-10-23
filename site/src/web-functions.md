---
title: "AWS Lambda Web Functions: cold starts, warm latency and concurrency, measured"
toc: true
---

# Web Functions

[Lambda Web Functions](https://docs.aws.amazon.com/lambda/latest/dg/web-functions.html) run a
Node.js HTTP server that listens on a port. One execution environment serves many requests at
once, instead of one request per environment as with the Event Functions that the rest of this
site measures. This page collects what a set of one-off probes measured about their cold starts,
warm latency, concurrency and cost, and how they compare with Event Functions running the same
handlers.

Web Functions are not part of the benchmark matrix: they support only Node.js and have a fixed
environment size, so most of the matrix's dimensions do not apply. This is a dated investigation
from 2026-10, shortly after launch; Lambda's scaling behavior in particular may have changed since.

<div class="tldr">
<div class="tldr-label">At a glance</div>

| Question                                                              | Measured answer                                                                                                                                                                       |
| --------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [Cold start](#cold-starts)                                            | Slower than an Event Function: `hello` ~380 ms vs ~225-240 ms caller wait. Most of the extra time passes before the app's code runs                                                   |
| [Warm request](#warm-requests)                                        | Faster: ~4 ms vs ~11-14 ms caller wait                                                                                                                                                |
| [Cold starts per burst](#concurrency)                                 | Far fewer once the endpoint has learned the handler: 1,000 simultaneous requests started 16 environments                                                                              |
| [A new endpoint](#concurrency-is-learned-per-endpoint)                | Starts at ~2 requests per environment and learns over minutes of traffic                                                                                                              |
| [CPU-bound handlers](#cpu-bound-handlers)                             | One Node event loop per environment; at the default concurrency, requests queue behind it                                                                                             |
| [Idle environments](#scale-down)                                      | Reclaimed ~5.5-7 min after their last request, and billed for memory until then                                                                                                       |
| [Background work](#background-work-between-requests)                  | Timers, threads and child processes keep running between requests, billed as CPU, until the environment is reclaimed                                                                  |
| [Slow startup](#slow-startup)                                         | Startup gets ~9-10 s, and a failed one is not retried. Requests wait for a starting environment, and that wait does not count against the timeout; work deferred past `listen()` does |
| [vs a Rust Event Function](#node-web-function-vs-rust-event-function) | `smithy`: Rust cold ~160-175 ms vs ~460-480 ms; Node Web warm ~4 ms vs ~13 ms                                                                                                         |
| [Rust via `execve`](#rust-on-a-web-function-via-execve)               | `oneclient`: ~390 ms cold from the entry module, ~314 ms from a preload, vs ~441 ms for Node and ~220 ms for a Rust Event Function                                                    |

</div>

<div class="note" label="How these numbers were measured">

One-off probes taken over several days in 2026-10 in a single account in `eu-central-1`, runtime
`nodejs24.x` (Web Functions runtime version `nodejs.web24.v14`). They are not committed benchmark
data, so treat them as rough magnitudes; where a repeat run differed, the tables give both.

The handlers are this benchmark's Node `hello`, `oneclient` and `smithy` bundles, unchanged. For
the Web Function each was imported by a minimal `node:http` server (no web framework) that calls
the handler per request, and the Event Function baselines ran the same bundles as normal handlers
(arm64, 2,048 MB); the Rust comparison used this benchmark's `rust-smithy-arm64-o3.zip`. A Lambda
function in the same region timed every request as the time to the first response byte on an
already open TLS connection, which leaves out the TLS handshake to Lambda's endpoints (~27 ms
in-region). A new Web Function revision is live within 5-10 s and starts fresh environments, which
is how the Web cold starts were forced; Event Functions were forced cold by a configuration update,
and a per-process request counter confirmed each cold sample. Cold cells have 6-15 forced cold
starts each, warm rows 600-900 sequential requests.

</div>

## Inside the environment

These points come from inspecting a running environment, except where marked documented, and can
change.

The environment is fixed at 2 vCPUs and 2 GB on arm64 (documented). As with Event Functions, the
host CPU varies per environment: in three bursts, 49 of 60 environments ran on Graviton4 and 11 on
Graviton3, none on Graviton2, so the cold-start numbers here mix generations (see [reading the cold
numbers](./lifecycle#reading-the-cold-numbers)). The kernel identifies itself as a microVM build
(`…aarch64.microvm`). The process list visible to the server shows only the sandbox's init process
and one `node` process running AWS's wrapper `/var/runtime/index.js`. The wrapper imports the entry
file (`index.js` by default, or the file named in `AWS_LAMBDA_NODEJS_ENTRYPOINT`), then tries every
5 ms to connect to the address in `AWS_LAMBDA_HTTP_ENDPOINT` (`0.0.0.0:3000`), and reports the
environment ready once a connection succeeds; the documented limit for listening is 10 s, but in
practice the wrapper allowed 9 s (see [slow startup](#slow-startup)).

Requests do not reach the server directly. Lambda terminates TLS and forwards plain HTTP/1.1, and
the server sees the connection coming from `169.254.100.1`, not from the client. That link-local
address is also the one in `AWS_LAMBDA_RUNTIME_API`; which Lambda component holds it, and whether
that component runs inside the microVM, is not visible from the server.

Each environment writes `platform.initReport` (init duration, CPU and memory),
`platform.invokeSummary` about every 60 s, and `platform.invokeReport` at shutdown to the
function's log group, which together give each environment's CPU time, lifetime and peak memory.

## Cold starts

| Handler     | Web Function caller wait, p50 (p10-p90)          | Event Function `Invoke` caller wait, p50 (p10-p90) |
| ----------- | ------------------------------------------------ | -------------------------------------------------- |
| `hello`     | **~378 ms** (321-422); repeat: ~382 ms (314-435) | **~225 ms** (205-254); repeat: ~240 ms (225-396)   |
| `oneclient` | **~497 ms** (405-557); repeat: ~456 ms (432-513) | **~424 ms** (339-533); repeat: ~426 ms (420-457)   |
| `smithy`    | **~457 ms** (352-527); repeat: ~460 ms (367-493) | **~371 ms** (304-408); repeat: ~404 ms (362-431)   |

A Web Function cold start took ~30 to ~155 ms longer. The gap was largest for `hello`, whose own
init is the smallest, and smaller for the two handlers with heavier init.

### Where the time goes

The server read its own timestamps relative to the start of the `node` process
(`performance.timeOrigin`). For `hello`:

| Step                                                | Time since the Node process started |
| --------------------------------------------------- | ----------------------------------- |
| The entry module's first line runs                  | ~141-144 ms                         |
| `listen()` done                                     | ~154-158 ms                         |
| First request arrives (readiness check and routing) | ~180-185 ms                         |

An ES module's first line runs only after its static imports have loaded, here `node:http`,
`node:crypto`, `node:fs` and `node:os`. From there to listening, which covers importing the `hello`
handler and starting the server, took ~13 ms. The ~141 ms before it went to Node's startup, the
wrapper and those imports, while the same bundle as an Event Function completes its whole Init in
~97-103 ms.

A separate probe split that time up in five bursts of 20 environments each, the last three of which
also recorded the host CPU. Its entry module imported only `node:http` and `node:child_process`, and
a preload (`NODE_OPTIONS=--import`) recorded each module the wrapper loaded, its calls to the
runtime API, and CPU time along the way. Node finished bootstrapping at ~30 ms. Loading the
wrapper's file took under 1 ms, and its built-in imports up to `http` ~7-9 ms. Then importing `http`
as an ES module read every export, including the lazy `WebSocket` getter, and that getter loading
Node's bundled undici took ~43-61 ms, the largest single step. The rest of the wrapper's own work,
from there to the entry's first line, took ~12-15 ms; its first call to the runtime API, which
fetches the function's configuration, took ~1.5 ms of that. The entry's first line ran at ~109-154
ms. CPU time grew as fast as wall time throughout, so this startup was compute-bound, not waiting on
disk or network. The host CPU set the pace: on Graviton4 (49 of the 60 environments) undici took
~43.5 ms and the first line ran at ~113 ms, on Graviton3 (11) ~58.5 ms and ~150 ms, with every step
about a third longer.

`require("http")` from CommonJS does not touch the getter. Node tracks the ES module case as
[nodejs/node#59686](https://github.com/nodejs/node/issues/59686), closed with a documentation
change on how `import` and `require` load built-in modules. The same undici also backs the global
`fetch()`, so an app that calls it pays this once anyway.

An app that never loads undici would gain less than this step costs, because typical app code
loads most of undici's dependencies itself. In a simulation (a preload that keeps the wrapper's
import from loading undici), an Express app reading one DynamoDB item finished its first `GetItem`
~16 ms earlier on Graviton4 (48 fix vs 38 control environments) and ~23 ms earlier on Graviton3
(10 vs 22), and the fix variant never loaded undici.

Of the ~380 ms caller wait, ~200-215 ms lies outside the Node process's lifetime (the caller wait
minus the process's age when it responded). That remainder presumably is mostly environment setup
and the code download, plus the request's way through Lambda, but it cannot be split further from
inside.

A web framework adds its own startup before `listen()`. These probes used none, so they measured
neither that cost nor whether deferring setup until after `listen()` shortens the first request.

### Platform-reported init is not comparable to `Init Duration`

| Handler     | Web Function `initReport`, p50 | Event Function `Init Duration`, p50 |
| ----------- | ------------------------------ | ----------------------------------- |
| `hello`     | ~265 ms                        | ~97-103 ms                          |
| `oneclient` | ~330 ms                        | ~237 ms                             |
| `smithy`    | ~340 ms                        | ~231-254 ms                         |

The Web Function's init clock includes time that grows with the package size, most likely the code
download. Adding a 50 MB incompressible file to the `hello` package raised `initReport` by
~145-150 ms and the caller wait by ~150-185 ms, in line with the
[download cost of a `.zip`](./lifecycle#download-cost-at-larger-sizes), while an Event Function's
`Init Duration` starts after the download. The two columns therefore overstate the gap, and the
caller waits above are the fair comparison. `initReport` recorded ~190 ms of CPU for an empty
`hello`; the `EnvironmentCPUTime` metric includes init (documented), and Web Functions bill CPU
time, but no bill was checked against these numbers.

## Slow startup

A probe server delayed its own startup in two ways: with a top-level `await` in the entry module,
so the wrapper's import of it did not finish, and with a timer before `listen()`, after the import
had finished. Each case was a new revision of the `hello` server, with sequential requests from the
in-region client (2026-10, a few requests per case).

### Startup limits

| Limit                                     | Cutoff                                                      | Caller gets                                |
| ----------------------------------------- | ----------------------------------------------------------- | ------------------------------------------ |
| Entry module import                       | 9 s (`customer code did not finish loading within 9000ms`)  | HTTP 502, `Runtime.TimeoutImportError`     |
| `listen()`, counted from the import's end | 9 s (`nothing was listening on 0.0.0.0:3000 within 9000ms`) | HTTP 502, `Application.TimeoutListenError` |
| Whole init (`initReport`)                 | ~10 s (failed at 10,060-10,092 ms)                          | HTTP 502, `Runtime.InitTimeout`            |

An 8 s import, an 8.5 s delay before `listen()`, and a 5 s import followed by `listen()` 4.5 s
later (`initReport` 9.7 s) all succeeded. Two 8.5 s steps, whose sum is over 17 s, failed at the
10 s limit. The documentation names a 10 s limit for listening and does not list
`Runtime.TimeoutImportError`. So startup work at the top level of the entry module gets 9 s, and
all of startup gets about 10 s on the platform's clock, which also counts time before the Node
process starts, most likely the code download (see
[platform-reported init](#platform-reported-init-is-not-comparable-to-init-duration)).

### A failed startup is not retried

Each request that met a too-slow startup waited ~9.3-10.2 s and got a 502. Lambda shut that
environment down, and the next request started a new one, which failed the same way. Unlike an
Event Function, whose init over 10 s
[runs a second time inside the first invoke](./lifecycle#an-init-over-10-seconds-runs-twice),
a Web Function never ran a second startup for the same request, so a slow startup is not paid twice,
but no request succeeds until a fixed revision is deployed. Each failed startup still used CPU, which
Web Functions bill: ~190 ms when the import was held, and ~365 ms when only `listen()` was late,
probably because the wrapper tries to connect every 5 ms the whole time. After a `Runtime.InitTimeout` the
process was not stopped at once: it kept running, called `listen()` ~7 s later and then failed to
report readiness to the runtime API.

### Waiting for startup is not part of the request timeout

With `timeoutSeconds` at 3 (its minimum) and `listen()` 8 s after the import, the first request
waited 8.4 s and succeeded. The timeout started once the request reached the server.

### A crash replaces the environment

A request that ended the process (`process.exit(1)`) got HTTP 503 with
`Runtime.OriginRequestError` within ~40 ms. Lambda shut that environment down instead of starting
the server again in it, and the next request started a new environment with the full startup budget
(8 s startup: 8.4 s wait, then 200). An Event Function's
[re-init loop](./lifecycle#how-much-time-a-re-init-gets), where every
invoke re-runs a startup that cannot fit in its timeout, did not occur.

### Requests queue on a starting environment

With a 5 s startup and 1 s requests, every burst below started from zero environments, after
Lambda had reclaimed all of them, so no warm or spare environment could take a request:

| Burst                                                          | Result                                                              |
| -------------------------------------------------------------- | ------------------------------------------------------------------- |
| Learned endpoint, 64 simultaneous requests                     | **All 64 on 1 environment**, each waited ~6.5 s (startup + request) |
| Learned endpoint, 1 request, then 63 more 2 s into its startup | **All 64 on that 1 environment**; the 63 waited ~4.4 s              |
| Learned endpoint, 128 simultaneous requests                    | 2 environments × 64, each request ~6.5 s                            |
| New endpoint, 64 simultaneous requests                         | 32 environments × 2, each request ~6.4 s                            |

Requests that arrive while an environment starts wait for it, up to the endpoint's per-environment
limit, and are not throttled (documented). They also stayed with the environment they were queued
on. Lambda also started spare environments, in the simultaneous bursts one that finished starting
within ~50 ms of the busy one, and none of them served a queued request. On a learned endpoint, then, a slow startup
delays every request in a burst of up to the limit, not only the first, and a burst does not spread
out to finish sooner. In an earlier run, a new endpoint added the same way ran its first burst of 64
on one environment instead of 32 × 2; that run is not explained.

### Deferring startup work past `listen()`

AWS recommends listening first and letting requests await the remaining startup work. A server that
listened at once and made every request wait 12 s for a startup promise, longer than any limit
above, worked. A burst of 64 from zero ran on one environment, and every request succeeded after
~13.4 s. The wait then counts against `timeoutSeconds` instead of the ~9-10 s startup limit,
unlike the wait before `listen()`: with `timeoutSeconds` at 5, all 64 requests got HTTP 504
(`Runtime.RequestTimeout`) after ~5.5 s.

## Warm requests

Warm `hello`, 900 sequential requests per row (3 interleaved rounds):

| Path                                         | p50                       | p90      | p99      |
| -------------------------------------------- | ------------------------- | -------- | -------- |
| Web Function, no authentication              | **~4.1 ms** (repeat: 3.8) | ~4.4 ms  | ~5.4 ms  |
| Web Function, IAM authentication (SigV4)     | **~3.6 ms** (repeat: 3.7) | ~4.1 ms  | ~4.7 ms  |
| Event Function, Function URL, auth `NONE`    | ~11.3 ms (repeat: 12.4)   | ~14.7 ms | ~20.3 ms |
| Event Function, `Invoke` without log tail    | ~10.8 ms (repeat: 11.5)   | ~13.6 ms | ~18.9 ms |
| Event Function, `Invoke` with `LogType=Tail` | ~14.0 ms (repeat: 13.5)   | ~17.6 ms | ~22.7 ms |

IAM authentication does not explain the gap: signed Web Function requests were no slower than
unsigned ones. The Event Function's extra ~7-8 ms appears even through an unauthenticated Function
URL, and it is consistent with the per-invoke front-end lump that the
[production-caller probe](./lifecycle#the-production-caller-no-log-tail-p50-and-p99) measures for
every runtime. A Web Function request also passes through Lambda (it reaches the server from
`169.254.100.1`, see [inside the environment](#inside-the-environment)), so it is not a direct
connection. A same-region request that skips Lambda entirely, from a Lambda function to Fargate
behind an Application Load Balancer, took
[~0.7 ms at p50 and ~1 ms at p99](./lifecycle#what-a-caller-waits-for-end-to-end) (a different
client: Rust over HTTP/2), so a Web Function request still pays ~3 ms of Lambda's own per-request
cost, less than an invoke's but not zero. What it saves internally is not visible from outside.

### Seeing the caller wait from outside

Lambda publishes no latency metric for Web Functions (its documented metrics count requests and
environment resources), and neither timing inside the server nor `initReport` covers the time before
the Node process exists. The cold-start numbers on this page therefore come from client-side timing.
CloudFront in front of the endpoint sees that time too: in two separate runs with caching off and
standard logs on, its `time-to-first-byte` matched the in-region client's wait within ~2 ms on all
20 forced cold starts (~330-495 ms). The edge was in the client's region; a remote user would add
their own network time on top.

CloudFront added ~1-2.5 ms at warm p50 over a direct request (~4.5-6.4 vs ~3.6-4.0 ms), and its p99
rose from ~6-9 to ~48-51 ms. In both runs, a few requests right after each cold start took 20-40 ms
on an already warm environment, probably while CloudFront opened new connections to the origin. Its
`origin-fbl` field usually ran a few ms below `time-to-first-byte` but ~30-60 ms below on some cold
starts, so it is no substitute for `time-to-first-byte`.

## Concurrency

### Concurrency is learned per endpoint

`maxConcurrencyPerEnvironment` (default 64, 1-128) is only an upper bound. Lambda sets each
environment's actual limit from the handler's observed CPU and memory use (documented), and in these
probes the limit started low and was learned per endpoint:

| Setup, I/O-bound requests (1 s wait each)                            | Result                                                         |
| -------------------------------------------------------------------- | -------------------------------------------------------------- |
| New function, 64 simultaneous requests                               | **32 environments × 2 requests** (both runs)                   |
| Same endpoint after a few minutes of traffic, from zero environments | **1 environment × 64**                                         |
| A new endpoint on that same function, 64 requests                    | 32 × 2 again (both runs)                                       |
| Learned endpoint after 33 min with no environments, 64 requests      | 1 new environment × 64                                         |
| Learned endpoint, `maxConcurrencyPerEnvironment` 128, 256 requests   | 3 environments, ~90-110 requests on the busiest                |
| Learned endpoint, 1,000 simultaneous requests                        | **16 environments**, 64 requests on all but one; all succeeded |

With 40 clients looping on 200 ms requests against a new function, the endpoint first ran 29
environments at ~2 requests each, then 4 at ~10 after ~45 s, and 2 at ~20 from ~100 s on.

The learned limit survived new revisions, including code changes, and was still in place after 33
minutes at zero environments, the longest gap tested; hours or days without traffic were not tried.
On a second function, learned the same way, a burst of 64 ran on 1 environment after a
comment-only code change, and again after 15 minutes idle and a change that made each request burn
100 ms of CPU. In that last burst the 64 requests took p50 4.8 s and up to 7.9 s, against ~1.5 s
before: a release that makes the handler heavier starts at the limit learned from the old code.
Until an endpoint has learned, a burst starts about one environment per two requests (32 for 64),
much closer to an Event Function's one per request than to the learned 64.

Each environment counts 2 vCPUs against a regional Arm vCPU quota (200 vCPUs, so 100 environments,
in this account). Above it, requests failed with HTTP 429 and
`Platform.AccountActiveVCPUArm64Throttle`.

### Spare environments

Lambda starts environments ahead of demand (documented), and some never receive a request. At the
default `maxConcurrencyPerEnvironment` of 64, ten simultaneous requests to a new function started 8
environments: 5 served 2 requests each and 3 stayed unused. On a learned endpoint, bursts of 256 and
512 requests ran on 4 and 8 environments at 64 each and started 2 and 4 more that stayed unused. A
spare environment lives as long as any idle one (see [scale-down](#scale-down)) and uses init CPU
and memory like the rest.

Under steady traffic few requests met a cold start. With 800 concurrent clients on a learned
endpoint, 18 environments carried ~45 requests each, and of 701,510 requests only those in the
first burst from zero waited on a cold start. Of 397,000 `oneclient` requests from 20 clients,
20 (0.005%) took more than 250 ms longer than usual, and all 20 were in the opening burst, waiting
on the 15 environments it started.

### CPU-bound handlers

Node runs JavaScript on one thread, so a busy event loop blocks every request in its environment.
With 50 ms of synchronous CPU per request and 40 looping clients:

| `maxConcurrencyPerEnvironment` | Environments                                                 | Throughput                 | Latency p50                                        |
| ------------------------------ | ------------------------------------------------------------ | -------------------------- | -------------------------------------------------- |
| 64 (default)                   | 1-2 at first, then about one more per minute, 4 after ~3 min | 20-40, rising to ~80 req/s | ~1,000-1,400 ms, ~300 ms once 3-4 environments ran |
| 1                              | 58 from the start (both runs)                                | ~720-730 req/s             | ~55 ms                                             |

At the default, requests queued behind the event loop while Lambda added environments slowly, and
lowering `maxConcurrencyPerEnvironment` removed the queueing. Moving the work to `worker_threads`
could use the environment's second vCPU, but was not tested.

### Scale-down

Idle environments were reclaimed mostly 330-420 s after their last request, across several hundred
environments and including spare ones that never served a request (those lived ~350-400 s from
init); a few went sooner. Under continuing traffic, surplus environments went away within ~30 s.

A revision switch is not instantaneous. For a few seconds after a new revision went live, warm
environments of the previous revision still served requests, and in one burst 257 distinct
environments served requests, more than the account's quota of 100. The documentation mentions a
temporary overshoot of `maxEnvironments` during revision changes; whether the account quota is
treated the same way is not documented.

Before removing an environment, Lambda stops routing to it, lets in-flight requests finish and sends
`SIGTERM`, after which the process has up to 1 s (documented). Every one of ~190 environments that
spun down during the repeat run received the signal, and the log line its handler wrote reached
CloudWatch; spin-down took under 90 ms. AWS does not guarantee the signal in every failure, so it
suits flushing buffered data, not work whose loss would break correctness.

## Background work between requests

An Event Function's environment is frozen between invokes, so timers and threads stop until the
next request (documented). A Web Function environment was not. A probe started background work
with one request and then sent nothing until the environment was reclaimed, ~10 minutes later:

| Background work                                     | What happened while no requests arrived                                        |
| --------------------------------------------------- | ------------------------------------------------------------------------------ |
| `setInterval` on the main thread (1 s)              | Fired every second until `SIGTERM` (600 of 600 ticks); no gap longer than ~1 s |
| A worker thread (1 s timer, optionally a busy loop) | Kept running; the busy loop held a steady ~18 million iterations per second    |
| A child process                                     | Kept running until shutdown                                                    |
| A `setTimeout` scheduled after the response ended   | Fired on time (5 s)                                                            |
| An outbound HTTPS request every 10 s                | All succeeded (~65 ms each)                                                    |
| `console.log` every 30 s                            | Reached CloudWatch                                                             |

The process's monotonic clock advanced by 600.8 s between the start request and `SIGTERM`,
matching the ~601 s of real time between them, and no timer gap exceeded ~1 s, so the environment
was not paused. Background work shows up in the environment's CPU time, which Web Functions
bill: with only the timers and the network calls, an idle environment used ~190-200 ms of CPU per
minute instead of ~95 ms, and with a worker thread spinning, ~60 s per minute, a full vCPU for as
long as the environment lived. The background activity did not keep the environment alive,
though: both were reclaimed 358 s and 400 s after their last request, within the usual range, so
anything running in the background ends with the environment, at the latest after the 1 s that
follows `SIGTERM`.

## Node Web Function vs Rust Event Function

Both running the benchmark's `smithy` handler (cold: 10-15 forced cold starts per cell; warm: 600
requests each):

| Configuration                               | Cold caller wait, p50 (p10-p90)        | Warm caller wait, p50 / p99 |
| ------------------------------------------- | -------------------------------------- | --------------------------- |
| Node Web Function (2 vCPU, fixed)           | **~478 ms** (390-517); repeat: ~460 ms | **~3.9 / ~5.6 ms**          |
| Rust Event Function, 2,048 MB, Function URL | **~161 ms** (149-211); repeat: ~174 ms | ~13.3 / ~24.5 ms            |
| Rust Event Function, 128 MB, Function URL   | ~152 ms (131-219); repeat: ~170 ms     | ~12.0 / ~25.6 ms            |
| Rust Event Function, 2,048 MB, `Invoke`     | ~155 ms (138-203)                      | ~14.1 / ~23.8 ms            |

Rust `smithy`'s own Init is ~33 ms, so nearly all of its cold wait is Lambda's setup, and memory
size barely changed it. The Rust Event Function's cold starts were about three times faster, while
the Node Web Function was ~9 ms faster warm at p50 and ~19 ms at p99, consistent with the invoke
path's per-request cost applying to Rust as much as to any runtime.

A Web Function has 2 vCPUs before and after it reports ready. An Event Function's handler gets a CPU
share that scales with its memory size, and the benchmark's own measurements show what that costs
when the first request runs code that has not been loaded yet: the page faults decompress the
package on the CPU, so at small sizes they stretch. `smithyfull` on arm64 took ~382 ms cold at 128
MB and ~175 ms at 1,024 MB ([why the first request is
slow](./tuning#why-the-first-request-is-slow-loading-code)). Against small Event Functions with
code-heavy first requests, the fixed 2 vCPUs are therefore a cold-start advantage; against 1,024 MB
and up they make no difference, and the smaller `smithy` above showed none even at 128 MB. Reading
the executable at init, which helps small Event Functions because their Init gets more CPU than
their handler, has nothing to gain on a Web Function. Whether a Web Function's code sits on the same
compressed image was not checked.

Which is faster on average depends on the share of requests that meet a cold start. Taking each
side's p50 as typical and assuming the Rust function stays warm, the Web Function has the lower
mean latency unless more than about 2% of its requests are cold. With traffic that rarely stops
nearly every request is warm, and there the Node Web Function was faster; with bursts after long
silences most first requests are cold, and there the Rust Event Function was faster, ~160-175 ms
against ~460-480 ms. At list prices the Rust function would also cost less per request ($0.20 per
million plus a small duration charge, against $0.30 per million plus CPU and memory), but its cost
was not measured.

## Rust on a Web Function via `execve`

The runtime is Node.js only, but the Node process can replace itself with another server through
`process.execve`, as [shown for
Go](https://awsteele.com/blog/2026/10/12/go-on-lambda-web-functions-via-execve.html). The server
then listens on `AWS_LAMBDA_HTTP_ENDPOINT` and reports readiness to the runtime API itself. A probe
ran an axum server with the benchmark's `oneclient` handler (one DynamoDB client built at startup,
one `GetItem` per request) in two ways: from the entry module, after AWS's wrapper ran, and from a
`NODE_OPTIONS=--require` preload, before it. In the preload variant the Rust server also fetched the
function's configuration from the runtime API (`/load`) as the wrapper would have, and Lambda
accepted its readiness report in all 10 cycles. Neither way is documented, and the preload depends
on runtime details that can change. All rows ran in the same hour, 9-10 confirmed cold starts each:

| `oneclient`                                   | Cold caller wait, p50 (p10-p90) | Graviton4 only | First request after process start | Warm caller wait, p50 |
| --------------------------------------------- | ------------------------------- | -------------- | --------------------------------- | --------------------- |
| Node Web Function                             | ~441 ms (424-569)               | ~433 ms (n=8)  | ~190 ms                           | ~7.5 ms               |
| Rust Web Function, `execve` from entry module | ~390 ms (345-460)               | ~365 ms (n=6)  | ~136 ms                           | ~5.1 ms               |
| Rust Web Function, `execve` from preload      | ~314 ms (289-369)               | ~308 ms (n=9)  | ~70 ms                            | ~5.4 ms               |
| Rust Event Function, 2,048 MB, Function URL   | ~220 ms (206-353)               | not recorded   |                                   | ~15.9 ms              |
| Rust Event Function, 2,048 MB, `Invoke`       | ~227 ms (201-252)               | not recorded   |                                   |                       |

From the entry module, `execve` happened at ~106 ms on Graviton4, after Node's startup and the
wrapper, so Rust saved only the JavaScript handler's own startup. From the preload it happened at
~34 ms, and the first request reached the Rust server at ~70 ms, close to the Event Function's
`Init Duration` (~69 ms p50 here; ~57 ms on Graviton4 and ~71 ms on Graviton3 in the 2026-10
benchmark run). Every Web Function variant still spent ~200-220 ms before its process existed, so
even the fastest one was ~90 ms slower cold than the Rust Event Function (~314 vs ~220 ms),
though faster warm (~5.4 vs ~15.9 ms). Replacing the Node process also drops what the wrapper adds:
request IDs in logs and its `SIGTERM` handling become the server's job.

## Cost

Web Functions bill requests, active CPU time and the memory each environment uses for its whole
lifetime, idle time included ([pricing](https://aws.amazon.com/lambda/pricing/)). In `eu-central-1`
at the time of the probes that was $0.30 per million requests, $0.0000349964 per active vCPU-second
and $0.0000046334 per GB-second; Event Functions on arm64 cost $0.20 per million requests plus
$0.0000133334 per GB-second of configured memory for the invocation's duration.

| Measured                                     | Value                                                      |
| -------------------------------------------- | ---------------------------------------------------------- |
| CPU per request, `hello` / `oneclient`       | ~0.8 ms / ~1.6 ms, including the server (both runs)        |
| CPU of an idle environment                   | ~60-120 ms per minute                                      |
| Environment memory, idle / under load        | ~145-155 MB / ~250-440 MB (peak memory, so an upper bound) |
| One environment that served a single request | ~6 min alive, ~0.8 s CPU; ~$0.0003 at list prices          |

The following are list-price estimates from that consumption, not checked against a bill, and they
take peak memory for the memory Lambda meters. A cold environment costs much more than an Event
Function cold start, mostly for its idle memory until it is reclaimed: one request every 10 minutes
would cost about $1.25 a month, where the same traffic on an Event Function costs a fraction of a
cent. Short handlers cost more per request, because the request price ($0.30 against $0.20 per
million) is most of their cost. Requests that mostly wait cost less, because waiting uses no CPU
and concurrent requests share one environment's memory: a request that waits 200 ms with ~1.5 ms
of CPU works out to about $0.38 per million, against ~$1.53 for a 512 MB Event Function.
