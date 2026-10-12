---
title: "AWS Lambda cold-start tuning: measured levers and their trade-offs"
toc: true
---

# Cold-start tuning

Settings and code changes that shorten Lambda cold starts, each backed by a measurement from the benchmark or from a dated one-off probe. The effect depends on memory tier, architecture, and handler, so test any change against your own function before relying on it.

Several levers below move work into the Init phase rather than removing it. That only shortens an on-demand cold start below about 1.8 GB, where Init gets more CPU than the handler. At larger sizes the work costs about the same in either phase, and it is still on the caller's path. The exception is an environment initialized before any request arrives (provisioned concurrency, SnapStart, or Lambda's proactive initialization), where init work is off the request path at any size.

<div class="tldr">
<div class="tldr-label">At a glance</div>

| Lever                                                                       | Runtime | Measured effect                                                             | Watch out for                            |
| --------------------------------------------------------------------------- | ------- | --------------------------------------------------------------------------- | ---------------------------------------- |
| [Do one-time setup at init](#do-one-time-setup-at-init)                     | All     | Rust `aws_config::load()` at 128 MB: ~55 ms at init, ~900 ms in the handler | Relies on undocumented CPU behavior      |
| [Keep `.zip` packages small](#keep-zip-packages-small)                      | All     | Roughly 3-8 ms per MB beyond a few MB, invisible to every metric            | An image moves the cost into init        |
| [Seed the aws-lc-rs RNG at init](#seed-the-aws-lc-rs-rng-at-init)           | Rust    | First-call RNG seeding: ~528 ms in the handler, ~38 ms at init              | Needs a single `aws-lc-sys` version      |
| [Disable jitter entropy](#disable-jitter-entropy-at-build-time)             | Rust    | Removes the seeding cost in either phase                                    | Drops an entropy source                  |
| [Load roots from the CA bundle](#load-root-certificates-from-the-ca-bundle) | Rust    | Root loading at init: ~28 ms to ~2 ms (arm64)                               | Changes which CAs are trusted            |
| [Compare `opt-level` z and 3](#compare-opt-level-z-and-3)                   | Rust    | z usually lowers cold init, 3 usually speeds warm work                      | No direction holds for every binary      |
| [Prime SnapStart](#prime-snap-start)                                        | Java    | SDK-heavy `threeclient`: ~5.0 s plain, ~2.3 s primed SnapStart              | Slower than plain Java on light handlers |
| [Warm the SDK's lazy setup at init](#warm-the-sdks-lazy-setup-at-init)      | Go      | ~1.3 s of lazy SDK setup lands on the first request at 128 MB               | Warm-up itself is not measured here      |

</div>

## Every runtime

### Do one-time setup at init

Create clients, read configuration, and do other one-time work outside the handler, so it runs during the Init phase.

Below about 1.8 GB, the Init phase appears to get more CPU than the handler, which runs on the tier's share of a vCPU. The same work therefore finishes sooner at init, and the difference is largest at the smallest tiers: the Rust SDK's `aws_config::load()` takes ~55 ms at init but ~900 ms in the handler at 128 MB ([same work, different phase](./lifecycle#rust-vs-go-same-work-different-phase)).

At 1,769 MB and above the handler has at least a full vCPU, so moving work to init no longer shortens an on-demand cold start. It still helps with provisioned concurrency, SnapStart, and proactive initialization, where Init runs before the request.

<div class="note" label="Undocumented behavior">

AWS does not document the Init-phase CPU boost, so it can change. If it does, the work costs what it would in the handler. Details: [work placed at init is subsidized](./lifecycle#work-placed-at-init-is-subsidized).

</div>

### Keep `.zip` packages small

Lambda downloads a `.zip` in full before Init starts, so its size cost appears in no `REPORT` metric: a flat floor up to a few MB, then roughly 3-8 ms per MB. A container image loads lazily: only the blocks that are read get fetched, that cost appears in `Init Duration`, and it grows with how much the function reads at init. For a container image, the amount read at init matters more than the image size. Measurements: [zip vs container image](./lifecycle#zip-vs-container-image-where-the-size-cost-lands).

## Rust

### Seed the aws-lc-rs RNG at init

**Applies to:** `aws-lc-rs` (the default crypto backend for `rustls` and the AWS SDK for Rust), built without `AWS_LC_SYS_NO_JITTER_ENTROPY=1`.

`aws-lc-rs` collects CPU jitter entropy the first time anything in the process uses its random number generator. A TLS handshake does that, so if the first TLS connection happens in the handler, the cost lands in the first invocation. One call at init moves it, without a network call:

```rust
// In main(), before starting the Lambda runtime.
let mut seed = [0u8; 32];
aws_lc_rs::rand::fill(&mut seed).expect("aws-lc-rs RNG");
```

A dated one-off characterization (taken 2026-10, single account, `eu-central-1`, arm64, 128 MB, 15 forced cold starts per row, timed internally; not committed data, so treat as rough magnitudes):

| Median time of `rand::fill` | At init | First call in the handler |
| --------------------------- | ------- | ------------------------- |
| No call at init             | n/a     | ~528 ms                   |
| One call at init            | ~38 ms  | ~0.01 ms                  |

The handler-side call ran on a newly spawned thread, as a tokio worker would, so seeding on the main thread at init covers other threads too.

Check that `cargo tree -i aws-lc-sys` shows a single version. Each `aws-lc-sys` version links its own copy of AWS-LC with its own RNG, so seeding one copy leaves the one your TLS stack uses unseeded. The [jitter-entropy panel](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax) shows how the cost grows as memory shrinks.

### Disable jitter entropy at build time

Build with `AWS_LC_SYS_NO_JITTER_ENTROPY=1`. AWS-LC then skips jitter entropy collection, which removes the one-time cost in whichever phase it would have landed.

<div class="warning" label="Security trade-off">

The flag removes one of AWS-LC's two independent entropy sources. Whether that is acceptable depends on the workload; the [Rust page](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax) explains what replaces it.

</div>

### Load root certificates from the CA bundle

**Applies to:** `rustls-native-certs`, which the AWS SDK for Rust uses by default to load the OS root certificates, on `provided.al2023`.

By default, `rustls-native-certs` loads the CA bundle plus every file in `/etc/ssl/certs` and `/etc/pki/tls/certs`. On `provided.al2023` each root appears there several times: in the bundle, as its own file, and as two hash symlinks to that file, and both directories also link to the bundle. Every one of those is read and parsed. Setting `SSL_CERT_FILE=/etc/pki/tls/certs/ca-bundle.crt` makes it read only the bundle. In this test both configurations returned the identical set of 251 roots.

A dated one-off characterization (taken 2026-10, single account, `eu-central-1`, 128 MB, 20 forced cold starts per cell, `load_native_certs()` timed internally; not committed data, so treat as rough magnitudes):

| Median time to load the roots | Init, arm64 | Init, x86_64 | Handler, arm64 | Handler, x86_64 |
| ----------------------------- | ----------- | ------------ | -------------- | --------------- |
| Default                       | ~28 ms      | ~46 ms       | ~460 ms        | ~607 ms         |
| `SSL_CERT_FILE` set           | ~2 ms       | ~3 ms        | ~2 ms          | ~3-38 ms        |

The x86_64 handler cell with `SSL_CERT_FILE` split into two groups, about half the samples near 3 ms and half near 38 ms, consistent with the load sometimes crossing a CPU-throttling boundary. The arm64 cell did not split. The Rust SDK builds its HTTPS client inside `aws_config::load()`, so for a handler that loads its config at init, the init columns apply.

<div class="warning" label="Check what your function trusts first">

This changes the set of root certificates the function trusts.

- **Private or custom CAs.** A CA added with `update-ca-trust` is written into the bundle and still loads. A CA copied only into a certificate directory is not in the bundle, and connections to servers that rely on it will fail.
- **Other base images.** The path above is the Amazon Linux 2023 one. On other images the bundle lives elsewhere or does not exist.
- **The whole process reads it.** OpenSSL and any other TLS library in the function also use `SSL_CERT_FILE`.

Compare the number of loaded roots with and without the variable, and make a test call to each service the function depends on, before rolling it out.

</div>

Other runtimes load their roots differently and are not measured here. Go's `crypto/x509`, for example, still scans both directories when only `SSL_CERT_FILE` is set; only `SSL_CERT_DIR` replaces that list.

### Compare `opt-level` z and 3

`opt-level=z` usually produces a smaller binary and a lower cold init. `opt-level=3` usually produces faster warm invocations, by a wide margin for CPU-bound handlers. Neither holds for every binary, so build both and measure. Results per scenario: [opt-level: speed vs size](./rust#opt-level-speed-vs-size).

## Java

### Prime SnapStart

Enable SnapStart and register a CRaC `beforeCheckpoint` hook that runs one representative invocation during init, so the AWS SDK's lazy class loading and JIT are captured in the snapshot. Unprimed SnapStart on an SDK-heavy handler can be slower than no SnapStart at all.

A dated one-off characterization (taken 2026-06, arm64, 512 MB, `threeclient`; not committed data, so treat as rough magnitudes):

| Configuration            | Total cold start |
| ------------------------ | ---------------- |
| Plain JVM (no SnapStart) | ~5.0 s           |
| SnapStart, unprimed      | ~8.4 s           |
| SnapStart, primed        | ~2.3 s           |

<div class="note" label="Not a win on every handler">

Restoring a snapshot is itself work. SnapStart wins where there is a large init to skip and loses on light handlers, and framework code that no hook can reach stays on the first restored request. A crash or timeout also drops the function back to a full JVM init. Which scenarios win and lose: [when SnapStart wins, and when it loses](./java-snapstart#when-snap-start-wins-and-when-it-loses).

</div>

## Go

### Warm the SDK's lazy setup at init

The AWS SDK for Go v2 loads the OS certificate pool, builds its HTTP transport, and resolves credentials on the first service call, not when the client is created. In a dated one-off probe (taken 2026-07, arm64, 128 MB) that setup cost ~1.3 s on the first request ([same work, different phase](./lifecycle#rust-vs-go-same-work-different-phase)). As with any init placement, this only shortens an on-demand cold start below about 1.8 GB.

Most of that setup is local, so it can be triggered at init without a request:

```go
// In main() or init(), after config.LoadDefaultConfig.
// Load the OS root certificates; crypto/x509 caches them for the process.
if _, err := x509.SystemCertPool(); err != nil {
	log.Fatal(err)
}
// Resolve credentials into the config's cache; on Lambda they come from
// environment variables, so this makes no network call.
if _, err := cfg.Credentials.Retrieve(ctx); err != nil {
	log.Fatal(err)
}
```

The first TLS handshake to each service still happens on the first request to it.

<div class="note" label="Not measured as a variant here">

This benchmark's Go handlers construct their clients at init but do no warm-up, so the effect is inferred from the phase difference, not measured. The probes do not break the ~1.3 s down, so how much of it the warm-up above removes is not known.

</div>

<div class="warning" label="If you make a real SDK call at init instead">

A throwaway call also opens a connection, but it has costs the local warm-up does not:

- **Connections are per host.** A call to a different service (for example `sts:GetCallerIdentity`, which needs no IAM permission) does not warm the connection your handler uses.
- **It adds work.** Above about 1.8 GB the call's round trip is pure added cold-start time, since the first real request still makes its own.
- **The connection can go stale.** With provisioned concurrency or proactive initialization, an environment can sit idle long after init, and the first request may reconnect or retry anyway.
- **Startup depends on the service.** If the call fails or is slow, init fails (within Init's 10 s limit) for every cold start, including requests that never use that service.
- **Bursts of cold starts mean bursts of calls**, which count against the service's rate limits and any per-request charges.

</div>

<style>
/* Same summary panel as lifecycle.md's "The short version". */
.tldr {
  margin: 20px 0 24px; padding: 14px 18px 6px;
  background: color-mix(in srgb, var(--panel) 60%, transparent);
  border: 1px solid var(--frame); border-left: 4px solid var(--accent);
  border-radius: 0 8px 8px 0;
}
.tldr-label { font-size: 12px; letter-spacing: .14em; text-transform: uppercase; color: var(--faint); margin-bottom: 4px; }
.tldr table { width: 100%; max-width: none; margin: 0 0 8px; }
.tldr + h2 { margin-top: 32px; }
.tldr a { color: var(--accent); }
</style>
