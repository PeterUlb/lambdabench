---
title: "Java SnapStart on AWS Lambda: plain JVM vs snapshot-restore cold start"
toc: false
---

# Java SnapStart on AWS Lambda: plain JVM vs snapshot-restore cold start

```js
const stats = await FileAttachment("data/stats.json").json();
import { filterForm } from "./components/filters.js";
import { colorModel } from "./lib/series.js";
import * as C from "./components/charts.js";
const cm = colorModel(
  stats.dimensions.languages,
  stats.dimensions.architectures,
);
```

## How SnapStart and priming work

SnapStart restores a pre-initialized snapshot instead of running JVM init on every cold start, replacing Init Duration with a Restore Duration. That restore marker's size depends on the handler: usually smaller than init on a heavy handler, though on a light one it can exceed init outright, so the chart's total for such a handler lands to the right of plain Java's. The chart below is the direct within-Java A/B: plain JVM vs SnapStart cold-start P50 across scenarios, architectures, and memory tiers. Plain Java and Java SnapStart are treated as two separate runtimes here, not a tuning knob, because snapshot restore and JVM init are different execution models.

**Where priming is meaningful, the SnapStart variant is primed** (`oneclient`, `threeclient`, `smithyfull`, `authz`, `batch`; the others are left unprimed), which is what makes the comparison fair. SnapStart snapshots the JVM _after_ init, but it cannot snapshot work that only happens on the first invocation: for an SDK-heavy handler that is chiefly the AWS SDK v2's lazy class loading, the JIT of that code, and the first marshaller construction. Left alone, an SDK-heavy handler pays all of that on the measured invoke, so its unprimed SnapStart would still carry the lazy SDK work on the first restored invoke; this run does not include that unprimed SDK-heavy A/B. The fix an operator ships is to force that work to run before the checkpoint: a CRaC `beforeCheckpoint` hook runs one representative invocation during init, baking the warm-up into the snapshot. Priming can only reach first-invoke cost that a code path the benchmark-owned handler drives before the checkpoint can exercise: the SDK-heavy handlers (`oneclient`, `threeclient`, `smithyfull`), plus `authz` (no SDK, but it warms its RS256-verify path) and `batch` (its first parse).

Where priming reaches the dominant lazy first-invoke work (the SDK-heavy handlers), the expected ordering is `primed < plain < unprimed`. That ordering is not universal: on handlers where the init being skipped is small or a framework path survives priming, primed SnapStart can still trail plain Java ([which scenarios win and lose](#when-snap-start-wins-and-when-it-loses) is taken up below). Only primed and plain are benchmarked here, so the chart below is primed vs plain; the unprimed leg is inferred, not measured. The dashboard measures the primed config (the same jar runs the hook as a no-op when SnapStart is off). The handlers with nothing for priming to reach are left unprimed: the probes with no first-invoke SDK cost for priming to hoist (`hello`, `lettercount`, `cache`) and the framework-only `smithy`, whose first-request cost sits behind a smithy-java path no public hook can drive.

A residual first-request cost survives priming on every scenario: restore is lazy, so the first invocation likely still pages the snapshot's memory back in and JITs any path not exercised before the checkpoint. SnapStart's first request is therefore never as fast as a warm one.

## A SnapStart crash or timeout re-runs Init, it does not re-restore

SnapStart's whole premise is that a cold start restores a snapshot instead of running JVM init. That
holds for any _healthy_ new environment: one brought up to meet load (scaling), and one Lambda brings
up after [periodically recycling](https://docs.aws.amazon.com/lambda/latest/dg/lambda-runtime-environment.html)
a long-lived environment for maintenance (it terminates them every few hours, even under continuous
traffic) both resume from the snapshot. It does **not** hold for the environment Lambda brings up
after a process-ending failure. When an invocation kills the sandbox (an `OutOfMemoryError`, a
function timeout, or a process exit, the process-ending failures that trigger a
[suppressed init](./lifecycle#when-a-crash-or-timeout-re-runs-init-the-suppressed-init)
on any runtime, plus a `StackOverflowError`, which ends the process on the JVM, each verified
here on a SnapStart function), the replacement environment runs a
**full from-scratch JVM init**, not a snapshot restore. The recovery path is exactly the plain-JVM
cold start that SnapStart exists to avoid.

Direct test confirms it on two independent signals. A probe whose static initializer records an id
baked into the snapshot at publish time keeps that id across every genuine restore (and across warm
reuse); after a crash the id **changed**, proving the static init ran again rather than being restored
from the snapshot. CloudWatch shows the same thing from the platform side: a healthy SnapStart cold
start reports a [`Restore Duration`](https://docs.aws.amazon.com/lambda/latest/dg/snapstart-monitoring.html)
(a restore, no `Init Duration`, since SnapStart runs init at version-create), whereas each post-crash
recovery emitted an **`INIT_REPORT`** (the full-init marker) instead, with an Init duration in the
range of plain JVM init, not the smaller restore marker.

That `INIT_REPORT` is also where SnapStart's recovery is _more visible_ than a plain function's: a
plain suppressed init emits no `INIT_REPORT` and hides its cost inside the next invocation's
`Duration` (see the [lifecycle page](./lifecycle#when-a-crash-or-timeout-re-runs-init-the-suppressed-init)),
whereas SnapStart always writes an explicit `INIT_REPORT` when it runs a full init, so here the
re-init shows up as its own line rather than folded away. As with the general suppressed-init case,
an _ordinary_ handler exception did **not** trigger this: the process survived and the next
invocation was a normal warm one. It is specifically the process-ending failures that drop the
function onto the full-init recovery path.

The consequence is sharper than the general suppressed-init story. On a plain function a crash
re-runs the init a cold start was already paying. On SnapStart a crash replaces a restore
with a full init, so it does not merely re-pay the fast path, it **abandons the fast path** for that
recovery, and the from-scratch JVM init it runs instead is the _larger_ of the two costs (on an
SDK-heavy handler, exactly the init SnapStart+priming was configured to skip). So a single such
failure converts what should be a fast snapshot restore into a full cold-start init on that sandbox's
next invocation, on the runtime where init is most expensive.

## When SnapStart wins, and when it loses

**SnapStart is not universally faster, and on light handlers it is slower.** Restoring a snapshot is
itself work: the restore marker is not negligible, and on a lightweight handler it can exceed plain
Java's entire JVM init (above). On top of it sits whatever the snapshot could not carry onto the
first restored request: the residual restore cost every scenario pays, any lazy first-invoke work
priming did not hoist, and, for a network handler, likely some client or connection state to
re-establish. Plain Java pays JVM init plus that same lazy work in one summed cold-start total, so
SnapStart is faster only when the init plus first-request work it removes is _larger_ than the
restore cost plus its heavier first request.

That trade-off splits the scenarios cleanly in the chart below. SnapStart **wins** where there is a
large init to skip: the SDK-heavy handlers that call an AWS client per invoke (`oneclient`,
`threeclient`), where priming hoists the full lazy SDK loading cost into the snapshot and the
restored first request comes out faster than plain Java's; and the two that do a heavy one-time
load at init (`lettercount` unprimed, `batch` primed), where plain Java's init-time S3 fetch plus
JVM/SDK class loading dwarfs the restore cost. It **loses** on `hello`, `cache`, `smithy`,
`smithyfull`, and (bar a stray near-tie tier) `authz`. These cold cells rest on only a handful of
cycles each, so read a single close tier off the chart rather than this prose. For most of these
there is little to skip: the small init plus a restored first request that costs _more_ than plain
Java's never outweighs the restore. `smithyfull` is the exception to that reasoning; it has a large
SDK init that priming does hoist, but the framework request path stays on the first restored invoke
(see the details under the chart), and that residue is enough to keep it behind plain Java net.

```js
// Java SnapStart A/B page: the chart pairs plain vs SnapStart for Java only
// (scoped at the snapDumbbell call below), so the language toggle is omitted.
// The meaningful dimensions here are arch, scenario, and memory.
const sel = view(
  filterForm(stats, {
    colorModel: cm,
    groups: ["architectures", "scenarios", "memories"],
  }),
);
```

```js
const v = C.makeView(stats, sel, invalidation);
```

```js
if (!stats.dimensions.hasSnapStart) {
  display(
    html`<p class="caption">This dataset has no SnapStart dimension.</p>`,
  );
}
```

## Cold start P50: plain JVM vs SnapStart

<div class="chart-sub">Each row is scenario × arch × memory; the dots are the total cold-start latency P50 (Init or Restore + first request), left dot = faster. SnapStart has a higher memory floor on <code>batch</code> and <code>smithyfull</code>, so a row appears only where both variants ran that tier.</div>

```js
// This is the Java SnapStart page, so the A/B is scoped to Java explicitly: a
// future runtime that gains a SnapStart variant gets its OWN page rather than
// leaking rows onto this one.
display(
  stats.dimensions.hasSnapStart ? C.snapDumbbell(v, { only: ["java"] }) : null,
);
```

<details class="chart-details">
<summary><strong>The extra penalty on the two Smithy scenarios: unprimable framework marshalling</strong></summary>

The Smithy scenarios carry the restore-overhead cost above _plus_ a second penalty that priming cannot remove, which is why they sit among the worst SnapStart results. Priming works by driving representative work through a CRaC `beforeCheckpoint` hook. For a plain `RequestHandler` an operator calls the handler's own entrypoint, which warms everything the measured invoke will hit. The Smithy scenarios are different: they are fronted by smithy-java's `LambdaEndpoint`, and the framework's first-request cost (protocol resolution, request/response (de)serialization, constraint validation, and the JIT of all of it) is reachable only through `LambdaEndpoint::handleRequest`, which smithy-java exposes no supported hook to drive before a checkpoint. So an operator can warm the SDK clients but **not the framework marshalling**. The benchmark records only restore/init/first-request timing, not which component runs when, so the likely explanation for the surviving first-request cost is that this framework marshalling stays on the first restored request.

- **`smithyfull`** primes its three SDK clients, so the restore marker drops below plain Java's init at every tier, but the unprimable framework marshalling survives into the first restored request. The surviving framework cost is large enough that SnapStart still trails plain Java net on this scenario, unlike the SDK-only `threeclient` where priming reaches everything and SnapStart wins.
- **`smithy`** has no SDK and no AWS call, so there is nothing to hoist; it is left unprimed by design. Its SnapStart cold start carries the full framework first-request cost on top of restore overhead, which is why it trails plain Java. This is the expected result for a framework-only handler on SnapStart with the current smithy-java, not a defect.

The framework residue is a property of the smithy-java API surface used in this run (1.4.0), not of SnapStart itself: a future supported framework-level warmup hook would shrink this second penalty, though the restore-overhead floor above would remain.

</details>
