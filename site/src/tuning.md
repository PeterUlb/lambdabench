---
title: "AWS Lambda cold-start tuning: measured levers and their trade-offs"
toc: true
---

# Cold-start tuning

Settings and code changes that shorten Lambda cold starts, each backed by a measurement from the benchmark or from a dated one-off probe. The effect depends on memory tier, architecture, and handler, so test any change against your own function before relying on it.

Some levers remove work. Others only move it into the Init phase, which shortens an on-demand cold start only below about 1.8 GB, where Init gets more CPU than the handler; at larger sizes the work costs about the same in either phase. That extra CPU is [undocumented behavior](./lifecycle#setup-is-cheaper-at-init-than-in-the-handler) that AWS can change. Environments initialized before any request arrives (provisioned concurrency, SnapStart, Lambda's proactive initialization) take init work off the request path at any size. The table marks each lever as "Removes work" or "Moves to Init".

<div class="tldr">
<div class="tldr-label">At a glance</div>

| Lever                                                                            | Runtime | Kind                          | Measured effect                                                                                                                 | Watch out for                                                                                       |
| -------------------------------------------------------------------------------- | ------- | ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| [Do one-time setup at init](#do-one-time-setup-at-init)                          | All     | Moves to Init                 | Rust `aws_config::load()` at 128 MB: [~55 ms at init, ~900 ms in the handler](./lifecycle#rust-vs-go-same-work-different-phase) | Gains only below ~1.8 GB, on demand                                                                 |
| [Keep `.zip` packages small](#keep-zip-packages-small)                           | All     | Removes work                  | Roughly 3-8 ms per MB beyond a few MB, invisible to every metric                                                                | [An image moves the cost into init](./lifecycle#zip-vs-container-image-where-the-size-cost-lands)   |
| [Load roots from the CA bundle](#load-root-certificates-from-the-ca-bundle)      | Rust    | Removes work                  | Init: ~16 ms less with `rustls-native-certs` 0.8.5 (~27 ms with 0.8.4)                                                          | Changes which CAs are trusted                                                                       |
| [Disable jitter entropy](#disable-jitter-entropy-at-build-time)                  | Rust    | Removes work                  | Removes the RNG seeding cost in either phase                                                                                    | [Drops an entropy source](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax)                       |
| [Turn off the SDK's default features](#turn-off-the-sdk-crates-default-features) | Rust    | Removes work                  | 10.0 MB to 8.1 MB binary; ~330 ms to ~300 ms cold at 128 MB                                                                     | You choose the HTTP client features                                                                 |
| [Consider rustls on ring](#consider-rustls-on-ring)                              | Rust    | Removes work                  | Three-client handler at 128 MB: ~371 ms to ~239 ms cold                                                                         | No post-quantum key exchange, no FIPS                                                               |
| [Open each client's connection at init](#open-each-clients-connection-at-init)   | Rust    | Moves to Init, overlaps calls | Three-client handler at 128 MB: ~371 ms to ~156 ms cold                                                                         | Real service calls on every cold start                                                              |
| [Read the executable at init](#read-the-executable-at-init)                      | Rust    | Moves to Init                 | `smithyfull` at 128 MB: ~382 ms to ~290 ms cold                                                                                 | Pays only up to ~256 MB                                                                             |
| [Seed the aws-lc-rs RNG at init](#seed-the-aws-lc-rs-rng-at-init)                | Rust    | Moves to Init                 | First-call RNG seeding: ~528 ms in the handler, ~38 ms at init                                                                  | Needs a single `aws-lc-sys` version                                                                 |
| [Order cold-start code at link time](#order-cold-start-code-at-link-time)        | Rust    | Removes work                  | x86_64 `smithyfull` at 128 MB: ~490 ms to ~388 ms cold; no gain on arm64                                                        | Needs lld and a profile per build and architecture                                                  |
| [Compare `opt-level` z and 3](#compare-opt-level-z-and-3)                        | Rust    | Removes work                  | z usually lowers cold init, 3 usually speeds warm work                                                                          | [No direction holds for every binary](./rust#opt-level-speed-vs-size)                               |
| [Prime SnapStart](#prime-snap-start)                                             | Java    | Moves to snapshot             | SDK-heavy `threeclient`: ~5.0 s plain, ~2.3 s primed SnapStart                                                                  | [Slower than plain Java on light handlers](./java-snapstart#when-snap-start-wins-and-when-it-loses) |
| [Skip the certificate directory scan](#skip-the-certificate-directory-scan)      | Go      | Removes work                  | `oneclient` at 128 MB: ~1,053 ms to ~265 ms cold                                                                                | Changes which CAs are trusted                                                                       |
| [Warm the SDK's lazy setup at init](#warm-the-sdks-lazy-setup-at-init)           | Go      | Moves to Init                 | `oneclient` at 128 MB: ~1,053 ms to ~166 ms cold                                                                                | First TLS handshake still in the handler                                                            |

</div>

<div class="note" label="How these numbers were measured">

Unless a section says otherwise, the numbers come from one-off probes taken in 2026-10 in a single account in `eu-central-1`, on arm64, with AWS-LC's jitter entropy disabled. They are not committed benchmark data, so treat them as rough magnitudes. A cold start is Init Duration plus the first request's Duration, as the median of 10 to 25 forced cold starts per cell; each table says how many.

- **The SDK probe** is a Rust function with KMS, DynamoDB and S3 clients. Its "three-client handler" makes one request to each service, its "DynamoDB `GetItem`" handler one request.
- **The crypto probe** times single steps (the first random-number call, a TLS handshake) with page-fault, CPU and disk-read counters.
- **`smithyfull`, `oneclient`** are copies of this benchmark's scenarios with the lever applied.

Baselines differ by up to ~40 ms between runs, so compare rows within one table. The probe code is in the repository under `oneoff/rust-sdk-tuning`.

- **A second region.** The main levers were measured again in `us-east-1`, interleaved with a repeat in `eu-central-1`. Every lever moved the cold start the same way in both regions, by amounts within about a factor of two, although `us-east-1` was slower across the board ([putting it together](#putting-it-together) shows both).
- **The host CPU varies.** Lambda ran the same arm64 function on Graviton2, Graviton3 and Graviton4 hosts, chosen per execution environment, and a cold start took about 30% longer on Graviton2 than on Graviton4. `smithyfull` at 128 MB took ~354 ms on Graviton3 and ~292 ms on Graviton4 in `eu-central-1`, and ~402 ms on Graviton2 and ~376 ms on Graviton3 in `us-east-1`; the 30% combines those two per-region gaps, since no region had enough of both Graviton2 and Graviton4. The mix differed by region (mostly Graviton3 in `eu-central-1`, mostly Graviton2 in `us-east-1`), which explains part of the regional gap; `us-east-1` was still ~20 ms slower on the same generation. Every cell in a table mixes generations. Interleaving the rows exposes them to the same mix over time, but each cell's draw still varies. On x86_64, Firecracker (Lambda's VM layer) reports a generic Intel Xeon name with only the host's nominal clock, which hides the generation; most probe cold starts showed the same clock, and the few that did not were faster.
- **Lambda hides newer instructions on x86_64.** In a probe, every x86_64 host showed the same reduced instruction set, without AVX-512, ADX or the SHA extensions even on hosts that have them, while arm64 showed each Graviton's own extensions ([reading the cold numbers](./lifecycle#reading-the-cold-numbers) has the details). The benchmark's Rust binaries are built for each architecture's generic baseline CPU (cargo-lambda adds a target CPU only for the `release` profile, and the benchmark builds with its own `lambda` profile), so this mainly affects code that picks its instructions at run time, such as the crypto in AWS-LC and ring.

</div>

## Every runtime

### Do one-time setup at init

Create clients, read configuration, and do other one-time work outside the handler, so it runs during the Init phase.

Below about 1.8 GB, the Init phase appears to get more CPU than the handler, which runs on the tier's share of a vCPU. The same work therefore finishes sooner at init, and the difference is largest at the smallest tiers: the Rust SDK's `aws_config::load()` takes ~55 ms at init but ~900 ms in the handler at 128 MB ([same work, different phase](./lifecycle#rust-vs-go-same-work-different-phase)).

At 1,769 MB and above the handler has at least a full vCPU, so moving work to init no longer shortens an on-demand cold start. It still helps with provisioned concurrency, SnapStart, and proactive initialization, where Init runs before the request.

<div class="note" label="Undocumented behavior">

AWS does not document the Init-phase CPU boost, so it can change. If it does, the work costs what it would in the handler. Details: [setup is cheaper at init than in the handler](./lifecycle#setup-is-cheaper-at-init-than-in-the-handler).

</div>

### Keep `.zip` packages small

Lambda downloads a `.zip` in full before Init starts, so its size cost appears in no `REPORT` metric: a flat floor up to a few MB, then roughly 3-8 ms per MB. A container image loads lazily: only the blocks that are read get fetched, that cost appears in `Init Duration`, and it grows with how much the function reads at init. For a container image, the amount read at init matters more than the image size. Measurements: [zip vs container image](./lifecycle#zip-vs-container-image-where-the-size-cost-lands).

### Why the first request is slow: loading code

Linux loads a program's code lazily, in 4 KB pages. The first use of a page triggers a page fault, and the kernel brings the page into memory. It then stays in the page cache, the kernel's in-memory copy of file contents, so later uses are cheap. A fault that has to read from disk first is a major fault.

On Lambda, a `.zip` function's code sits on a read-only squashfs image (`/var/task`) compressed with gzip in 128 KB blocks. The first touch of a page therefore reads the compressed block containing it from a virtual disk and decompresses the whole 128 KB in the kernel, even if only one small function in it is needed. That costs about 7 µs of CPU per KB read on arm64, roughly 0.5 ms per block. It is CPU time charged to the function, so at 128 MB the handler's small CPU share stretches it about tenfold.

An example from the crypto probe: aws-lc's first random-number call in the handler caused 3 major faults, read 478 KB from the disk and used ~4.5 ms of CPU. With a full vCPU (1,024 MB and up, or in Init) it took about that long in wall time too, so the time is computation rather than waiting on I/O. At 128 MB it took ~48 ms; on a local machine, where the file is cached, ~27 µs.

- **Only code the cold start runs costs time.** Code that never runs is never loaded, and code that first runs in the handler costs the most.
- **Where the linker places code changes the cost.** Code first used by the first request costs one block load per block it lands in, so where the linker happens to place it can add or save tens of ms at 128 MB (see [putting it together](#putting-it-together)).
- **The fault count is a poor measure.** The kernel reads ahead around a faulting page, so one fault can load one or several blocks; the cost follows the KB read.
- **The page cache counts toward the function's memory.** Reading a 20 MB packaged file raised "Max Memory Used" by exactly 20 MB. At the limit the kernel evicts cached pages instead of failing the function: a 128 MB function that read 150 MB of packaged files peaked at ~117 MB, and its own code pages had been evicted too, so the next steps faulted again (9 major faults instead of 5).

<div class="note" label="Observed, not documented">

That the code sits on a gzip-compressed squashfs image is observed, not documented, and can change. The compression was identified by deploying files whose repeats only some algorithms can compress and reading the code device's size from `/proc/partitions`. The compression is what makes the handler's CPU share relevant: decompression is CPU work, so a lower CPU share stretches it, while on uncompressed storage a first touch would mostly wait on I/O, which a lower CPU share does not slow down.

</div>

## Rust

Several levers attack the same cost, [loading code](#why-the-first-request-is-slow-loading-code), so their gains overlap rather than add up; [putting it together](#putting-it-together) shows the combinations on `smithyfull`.

### Load root certificates from the CA bundle

**Applies to:** `rustls-native-certs`, which the AWS SDK for Rust uses by default to load the OS root certificates, on `provided.al2023`.

By default, `rustls-native-certs` loads the CA bundle plus every file in `/etc/ssl/certs` and `/etc/pki/tls/certs`. On `provided.al2023` each root appears there several times: in the bundle, as its own file, and as two hash symlinks to that file, and both directories also link to the bundle. Every one of those is read and parsed. Setting the function's environment variable `SSL_CERT_FILE=/etc/pki/tls/certs/ca-bundle.crt` makes it read only the bundle. In this test both configurations returned the identical set of 251 roots.

| Median time to load the roots, 128 MB | Init, arm64 | Init, x86_64 | Handler, arm64 | Handler, x86_64 |
| ------------------------------------- | ----------- | ------------ | -------------- | --------------- |
| Default                               | ~28 ms      | ~46 ms       | ~460 ms        | ~607 ms         |
| `SSL_CERT_FILE` set                   | ~2 ms       | ~3 ms        | ~2 ms          | ~3-38 ms        |

(`rustls-native-certs` 0.8.4, `load_native_certs()` timed inside the function, 20 forced cold starts per cell.)

The Rust SDK builds its HTTPS client inside `aws_config::load()`, so for a handler that loads its config at init, the Init columns apply.

`rustls-native-certs` 0.8.5 (released 2026-10-13, [#256](https://github.com/rustls/rustls-native-certs/pull/256)) reads each file in the certificate directories only once, without changing which roots load. In the SDK probe (25 forced cold starts per cell), root loading inside `aws_config::load()` dropped from ~27 ms with 0.8.4 to ~16 ms with 0.8.5 at both 128 MB and 1,024 MB, and Init dropped by the same ~11 ms. With 0.8.5, setting `SSL_CERT_FILE` therefore saves ~16 ms instead of ~27 ms. On the benchmark's `smithyfull` with 0.8.5 it took ~13 ms off the cold start in `eu-central-1` and ~22 to ~26 ms in `us-east-1`, at 128 MB and 1,024 MB alike ([putting it together](#putting-it-together)). The single-lever Rust measurements elsewhere on this page used 0.8.4.

The x86_64 handler cell with `SSL_CERT_FILE` split into two groups, about half the samples near 3 ms and half near 38 ms, consistent with the load sometimes crossing a CPU-throttling boundary. In the crypto probe at 128 MB, such a wait lasted ~19 ms and landed on whichever step crossed the boundary: DNS lookups that otherwise took ~1 ms took ~19 ms while using ~0.2 ms of CPU.

<div class="warning" label="Check what your function trusts first">

This changes the set of root certificates the function trusts.

- **Private or custom CAs.** A CA added with `update-ca-trust` is written into the bundle and still loads. A CA copied only into a certificate directory is not in the bundle, and connections to servers that rely on it will fail.
- **Other base images.** The path above is the Amazon Linux 2023 one. On other images the bundle lives elsewhere or does not exist.
- **The whole process reads it.** OpenSSL and any other TLS library in the function also use `SSL_CERT_FILE`.

Compare the number of loaded roots with and without the variable, and make a test call to each service the function depends on, before rolling it out.

</div>

Go's `crypto/x509` loads its roots differently: it still scans both directories when only `SSL_CERT_FILE` is set; see [skip the certificate directory scan](#skip-the-certificate-directory-scan).

### Disable jitter entropy at build time

Build with `AWS_LC_SYS_NO_JITTER_ENTROPY=1`. AWS-LC then skips CPU jitter entropy collection, which otherwise runs the first time anything in the process uses its random number generator, typically the first TLS handshake. That removes the one-time cost in whichever phase it would have landed, roughly half a second in the handler at 128 MB. If you keep jitter entropy on, [seed the RNG at init](#seed-the-aws-lc-rs-rng-at-init) instead.

[Reading the executable at init](#read-the-executable-at-init) does not replace this. Jitter collection is CPU work, which the read does not remove. With jitter entropy on, the first `rand::fill` in the handler at 128 MB took ~630 ms without the read and still ~485 ms (~35 ms of CPU) with it.

<div class="warning" label="Security trade-off">

The flag removes one of AWS-LC's two independent entropy sources. Whether that is acceptable depends on the workload; the [Rust page](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax) explains what replaces it.

</div>

### Turn off the SDK crates' default features

The SDK crates' default features include a legacy HTTP client (hyper 0.14 with rustls 0.21 and ring), and `aws-config`'s defaults add SSO and `credential_process` support. A Lambda function needs none of these:

```toml
aws-config = { version = "1", default-features = false, features = ["rt-tokio", "default-https-client"] }
aws-sdk-dynamodb = { version = "1", default-features = false, features = ["rt-tokio", "default-https-client"] }
```

This leaves the SDK's default hyper 1.x client with rustls on aws-lc, so behavior is unchanged unless you used SSO, `credential_process` or SigV4a (an S3 default feature, needed for Multi-Region Access Points). In the SDK probe (25 forced cold starts per cell, interleaved) the three-client binary shrank from 10.0 MB to 8.1 MB, Init dropped by ~7 ms, and the cold start went from ~330 ms to ~300 ms at 128 MB and from ~166 ms to ~152 ms at 1,024 MB.

The removed code never runs, and with the executable already in the page cache both builds took the same time, so the gain comes from [loading less code](#why-the-first-request-is-slow-loading-code):

- **Unread bytes cost nothing.** 2 MiB of padding the process never reads left the cold start unchanged.
- **The default build loads more of itself for the same work.** It had ~600 KB more of its executable in memory after Init (5.0 MB against 4.4 MB) and ~850 KB more after the first request (6.8 MB against 5.9 MB), with a few more major faults and ~6 ms more CPU at Init. Which code those extra pages hold was not traced.
- **It overlaps with the executable read.** With the executable read at init, turning off default features saved only ~6 to ~18 ms more, mostly because the smaller file is quicker to read.
- **SigV4a alone made no measurable difference** when added back.

### Consider rustls on ring

The SDK's HTTPS client can use rustls on ring instead of aws-lc. Leave the SDK crates' default features off as above, drop `default-https-client`, and pass the client to the loader:

```toml
aws-smithy-http-client = { version = "1", features = ["rustls-ring"] }
```

```rust
use aws_smithy_http_client::{Builder, tls};

let http = Builder::new()
    .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::Ring))
    .build_https();
let config = aws_config::defaults(BehaviorVersion::latest()).http_client(http).load().await;
```

The three-client binary shrank to 6.7 MB, and the cold start went from ~371 ms to ~239 ms at 128 MB and from ~166 ms to ~146 ms at 1,024 MB. Most of that is [less code to load](#why-the-first-request-is-slow-loading-code): setting up rustls and making the first two handshakes caused 8 major faults reading 877 KB from the disk with aws-lc, and 4 faults reading 399 KB with ring, while warm handshakes took the same time with either library (~2 ms at init).

With the executable read at init, `smithyfull` was still ~27 ms faster with ring at 128 MB and ~5 ms faster at 1,024 MB. Most of that remainder is about 1 ms more CPU in aws-lc's first request, partly from mapping more already-cached pages, stretched by the handler's CPU share at 128 MB. With connections opened at init instead, ring was 6 to 11 ms faster than aws-lc.

<div class="warning" label="What you give up">

- **Post-quantum key exchange.** With aws-lc, rustls negotiated the hybrid `X25519MLKEM768` with DynamoDB and KMS; ring offers only classical groups. In this probe the post-quantum exchange cost nothing measurable.
- **FIPS.** ring has no FIPS-validated mode; aws-lc does.
- **The SDK's default path.** You now own the HTTP client's construction and TLS settings.

</div>

### Open each client's connection at init

**Applies to:** the AWS SDK for Rust, which builds its HTTPS connection to a service on the first request to it.

That first request resolves DNS, opens the TCP and TLS connection, and runs code for the first time, all at the handler's CPU share. Making one real, side-effect-free call per client at init moves that work into the Init phase, and running the calls concurrently overlaps their round trips:

```rust
// In main(), after building the clients and before lambda_runtime::run.
// An error response (such as an S3 404) still leaves the connection open.
let _ = tokio::join!(
    kms.encrypt().key_id(&key_id).plaintext(Blob::new(b"warm".to_vec())).send(),
    ddb.get_item().table_name(&table).key("pk", AttributeValue::S(pk.clone())).send(),
    s3.head_object().bucket(&bucket).key(&receipt_key).send(),
);
```

| Cold start, SDK probe with default features | 128 MB, before | 128 MB, after | 1,024 MB, before | 1,024 MB, after |
| ------------------------------------------- | -------------- | ------------- | ---------------- | --------------- |
| KMS + DynamoDB + S3, one request each       | ~371 ms        | ~156 ms       | ~166 ms          | ~155 ms         |
| DynamoDB `GetItem`                          | ~207 ms        | ~109 ms       | ~110 ms          | ~113 ms         |

(15 forced cold starts per cell.)

Init grew by ~18 ms (one client) to ~36 ms (three), and the first request dropped to about a warm request's time.

- **At small memory sizes** the gain relies on the Init CPU boost, like any [one-time setup at init](#do-one-time-setup-at-init).
- **With a single client**, expect no gain at 1 GB and above, where the handler has enough CPU: opening the connection costs about the same in either phase.
- **With several clients** the calls at init run concurrently, while the handler's first requests open their connections one after another. This overlap does not depend on the Init CPU boost; `smithyfull` still gained ~10 to ~16 ms at 1,769 MB (one full vCPU) and 3,008 MB.

<div class="warning" label="A real call at init has costs of its own">

- **Connections are per host.** A call to a different service (for example `sts:GetCallerIdentity`, which needs no IAM permission) does not warm the connection your handler uses.
- **It adds a round trip.** With enough CPU, opening the connection costs about the same in Init as in the handler, so one call at init only adds its own request's round trip. Several calls come out ahead only because they overlap.
- **The connection can go stale.** With provisioned concurrency or proactive initialization, an environment can sit idle long after init, and the first request may reconnect or retry anyway.
- **Startup depends on the service.** If the call fails or is slow, init fails (within Init's 10 s limit) for every cold start, including requests that never use that service. Ignoring the call's result, as above, keeps init from failing, but init still waits for the call.
- **Bursts of cold starts mean bursts of calls**, which count against the service's rate limits and any per-request charges.

</div>

### Read the executable at init

**Applies to:** Rust binaries on `provided.al2023`, when nothing else at init touches most of the code. Go gained ~25 ms at 128 MB on arm64 and nothing at larger sizes, and Java gained nothing (see [other runtimes](#other-runtimes-go-and-java)).

Reading the whole executable once, first thing in `main()`, [loads and decompresses all of it](#why-the-first-request-is-slow-loading-code) in one sequential pass while Init still has plenty of CPU. Later first uses then find their pages already in the page cache instead of decompressing them in the handler:

```rust
#[tokio::main]
async fn main() -> Result<(), Error> {
    // First thing: read the executable once so its pages are in the page cache.
    if let Ok(mut exe) = std::fs::File::open("/proc/self/exe") {
        let _ = std::io::copy(&mut exe, &mut std::io::sink());
    }
    // ... create the clients, then lambda_runtime::run(service_fn(handler)).await
}
```

After the read the crypto probe saw no major faults at all:

| Median                                         | Plain   | Executable read at init |
| ---------------------------------------------- | ------- | ----------------------- |
| aws-lc first `rand::fill`, handler, 128 MB     | ~48 ms  | ~0.06 ms                |
| First TLS handshake, handler, 128 MB           | ~80 ms  | ~1.9 ms                 |
| Three-client SDK handler, cold start, 128 MB   | ~340 ms | ~265 ms                 |
| DynamoDB `GetItem` handler, cold start, 128 MB | ~216 ms | ~149 ms                 |
| Three-client SDK handler, cold start, 1,024 MB | ~167 ms | ~171 ms                 |

(15 forced cold starts per cell.)

The read itself costs Init time, mostly decompression: ~8 ms for the 3.2 MB crypto probe, ~32 ms for a 6.7 MB binary, ~48 ms for the 10 MB SDK probe, and ~15 ms of Init growth on `smithyfull` (11.5 MB). Whether that pays depends on the memory size, since the handler's CPU share decides how expensive the first touches would have been:

| `smithyfull` cold start, arm64                             | 128 MB  | 256 MB  | 512 MB  | 768 MB  | 1,024 MB |
| ---------------------------------------------------------- | ------- | ------- | ------- | ------- | -------- |
| As in the benchmark                                        | ~382 ms | ~239 ms | ~186 ms | ~177 ms | ~175 ms  |
| Executable read at init                                    | ~290 ms | ~197 ms | ~183 ms | ~182 ms | ~182 ms  |
| Default features off, `SSL_CERT_FILE` set                  | ~332 ms | ~187 ms | ~153 ms | n/a     | ~141 ms  |
| Default features off, `SSL_CERT_FILE` set, executable read | ~248 ms | ~167 ms | ~147 ms | n/a     | ~146 ms  |

(25 forced cold starts per cell, interleaved.)

- **Where it pays.** At 128 and 256 MB. It breaks even around 512 MB and costs ~5 to ~10 ms from 768 MB up, also at 1,769 and 3,008 MB. On x86_64 it paid more: `smithyfull` at 128 MB went from ~487 ms to ~332 ms, and at 1,024 MB from ~231 ms to ~243 ms.
- **It held in a second region and on every host generation.** Built with the benchmark's lockfile, `smithyfull` at 128 MB went from ~361 ms to ~276 ms in `eu-central-1` and from ~395 ms to ~298 ms in `us-east-1`. Split by host CPU, with both regions pooled: ~401 to ~295 ms on Graviton2, ~363 to ~282 ms on Graviton3, ~292 to ~226 ms on Graviton4.
- **It adds nothing once [connections are opened at init](#open-each-clients-connection-at-init)**, since those calls already touch the code at init. Use it when you cannot make calls at init.
- **Memory.** The read keeps the whole binary in the page cache, which counts toward the function's memory: "Max Memory Used" rose by ~2 MB on `smithyfull`, which would otherwise have cached about 9 of its 11.5 MB anyway. At small sizes it is only worth it while the binary plus the function's own memory stay well under the limit, or the kernel [evicts the pages again](#why-the-first-request-is-slow-loading-code).
- **Alternatives that did not help:** reading the file on a background thread, and asking for readahead with `posix_fadvise(POSIX_FADV_WILLNEED)`.

<div class="note" label="Relies on undocumented behavior">

The read pays off only because Init gets more CPU than the handler (see [do one-time setup at init](#do-one-time-setup-at-init)). Decompressing during the read cost about as much CPU per byte as decompressing on a page fault, and the read also covers pages the function never touches, so without that boost expect it to cost time at every memory size.

</div>

#### Other runtimes: Go and Java

The same test on copies of this benchmark's Go and Java `oneclient` handlers (10 to 20 forced cold starts per cell) found little or no gain:

- **Go.** About 80% of a Go binary was already in memory when `main()` started, in a minimal Go function as well as with the SDK: Go's runtime and package initialization run before `main()` and touch nearly every 128 KB block of code. With the [certificate directory scan](#skip-the-certificate-directory-scan) skipped, so that the rest of the cold start varied little, the read removed the first request's ~7 remaining major faults and gained ~25 ms at 128 MB on arm64 (~294 ms to ~268 ms). At 256 MB, at 1,024 MB, and on x86_64 at 128 MB the change was under ~5 ms.
- **Java.** The JVM and its class library are not on the compressed code image, and the JVM already reads the function's jars while Init builds the client. The first request was limited by CPU instead (class loading and interpreted code: ~10 s at 128 MB, ~2.3 s at 512 MB, ~1.1 s at 1,024 MB). Reading the jars or the JDK's class image at init left it unchanged, and the class image raised "Max Memory Used" from ~145 MB to ~259 MB. For Java, see [prime SnapStart](#prime-snap-start) instead.

### Seed the aws-lc-rs RNG at init

**Applies to:** `aws-lc-rs` (the default crypto backend for `rustls` and the AWS SDK for Rust), built without `AWS_LC_SYS_NO_JITTER_ENTROPY=1`.

`aws-lc-rs` collects CPU jitter entropy the first time anything in the process uses its random number generator. A TLS handshake does that, so if the first TLS connection happens in the handler, the cost lands in the first invocation. One call at init moves it, without a network call:

```rust
// In main(), before starting the Lambda runtime.
let mut seed = [0u8; 32];
aws_lc_rs::rand::fill(&mut seed).expect("aws-lc-rs RNG");
```

| Median time of `rand::fill`, 128 MB, jitter entropy on | At init | First call in the handler |
| ------------------------------------------------------ | ------- | ------------------------- |
| No call at init                                        | n/a     | ~528 ms                   |
| One call at init                                       | ~38 ms  | ~0.01 ms                  |

(Timed inside the function, 15 forced cold starts per row.)

The call moves the work rather than removing it, so the gain relies on the Init CPU boost like any [one-time setup at init](#do-one-time-setup-at-init) and disappears at about 1.8 GB and above. [Disabling jitter entropy](#disable-jitter-entropy-at-build-time) removes the cost at every size.

The handler-side call ran on a newly spawned thread, as a tokio worker would, so seeding on the main thread at init covers other threads too.

Check that `cargo tree -i aws-lc-sys` shows a single version. Each `aws-lc-sys` version links its own copy of AWS-LC with its own RNG, so seeding one copy leaves the one your TLS stack uses unseeded. The [jitter-entropy panel](./rust#the-aws-lc-rs-jitter-entropy-cold-start-tax) shows how the cost grows as memory shrinks.

### Order cold-start code at link time

**Applies to:** Rust binaries linked with lld, on x86_64. On arm64 it did not help (see [arm64](#on-arm64-it-did-not-help)).

The functions a cold start runs are spread across the whole binary, so Init and the first request [decompress many 128 KB blocks](#why-the-first-request-is-slow-loading-code) that hold mostly code they never run. A linker can be told to place a list of functions next to each other (lld's `--symbol-ordering-file`), which packs the cold-start code into fewer blocks.

In the experiment, one Init and first request of `smithyfull` ran locally under valgrind's callgrind, against the real services and behind a stand-in for the Lambda runtime API. That listed the 2,427 functions of the binary that ran. Linking with that list moved the 2.6 MB of cold-start code from 64 of the binary's 75 code blocks into 23:

| `smithyfull` cold start, x86_64    | 128 MB  | 256 MB  | 1,024 MB |
| ---------------------------------- | ------- | ------- | -------- |
| lld, default layout                | ~490 ms | ~318 ms | ~235 ms  |
| Cold-start functions ordered       | ~388 ms | ~257 ms | ~224 ms  |
| Default layout, executable read    | ~336 ms | n/a     | n/a      |
| Functions ordered, executable read | ~328 ms | n/a     | n/a      |

(20 forced cold starts per cell, two runs.)

- **No Init cost.** Unlike the executable read, ordering costs nothing at larger sizes.
- **It cannot remove every block load.** The first request ran 1.7 MB of code that Init did not, which needs about 13 blocks in any layout; its major faults went from 22 to 15. At 128 MB the read still did better, and the two together were about the same as the read alone.
- **One combined list worked best.** Listing Init's functions first and the request's after them did no better, because request code mixed in with Init's lands in blocks Init loads anyway.
- **It held in a repeat.** A repeat in `eu-central-1` and `us-east-1` took the 128 MB cold start from ~504 ms to ~394 ms and from ~510 ms to ~398 ms.

#### On arm64 it did not help

The same source built for arm64, ordered by a profile of an arm64 run (callgrind in an emulated arm64 container), was no faster at 128, 256 or 1,024 MB, measured in both regions (20 forced cold starts per cell and region):

| `smithyfull` cold start, arm64              | 128 MB  | 256 MB  | 1,024 MB |
| ------------------------------------------- | ------- | ------- | -------- |
| Default layout                              | ~374 ms | ~241 ms | ~191 ms  |
| Ordered by an arm64 profile                 | ~365 ms | ~244 ms | ~198 ms  |
| Ordered by the x86_64 list (mapped by name) | ~425 ms | ~261 ms | ~195 ms  |

In a trace of the pages each phase touched, the arm64 default layout already left the first request with 12 blocks to load (x86_64: 17), and ordering did not get it lower: the code only the first request runs needs about that many blocks in any layout. Ordering still shrank the code Init touched by about 40%, but Init runs on boosted CPU, so that saved only ~3 ms.

A list does not carry over between architectures. Mapped onto the arm64 build by name, the x86_64 list packed Init's code but missed much of the first request's: 279 of the functions the arm64 run executed had no counterpart in it, most likely because the compiler inlines differently per architecture. That code ended up in blocks of its own, and the first request had 20 blocks to load instead of 12.

<div class="warning" label="Not a drop-in setting">

- **cargo-lambda's default linker does not support it.** cargo-lambda links through zig, which rejects `--symbol-ordering-file`. The experiment built in an Amazon Linux 2023 container with Rust's bundled lld instead.
- **The list belongs to one build and one architecture.** Rust symbol names include hashes that change with crate versions, compiler versions and the target. lld only warns about names it does not find, and cargo does not show linker warnings by default, so a stale list quietly falls back to the default layout. Regenerate it whenever dependencies change.
- **It covers the profiled path only.** Code that only other requests or error paths run stays where it was.

</div>

### Compare `opt-level` z and 3

`opt-level=z` usually produces a smaller binary and a lower cold init, partly because there is [less code to load](#why-the-first-request-is-slow-loading-code). `opt-level=3` usually produces faster warm invocations, by a wide margin for CPU-bound handlers. Neither holds for every binary, so build both and measure. Results per scenario: [opt-level: speed vs size](./rust#opt-level-speed-vs-size).

### Putting it together

Applied to the benchmark's `smithyfull` handler (Smithy server SDK plus KMS, DynamoDB and S3 clients), the levers cut its cold start at 128 MB by about half with calls at init, and by about a quarter to a third without. After tuning, the cold start depends much less on the memory tier. Gains overlap where levers cut the same loading cost: opening connections at init makes the executable read redundant, and once the executable is read, turning off default features or switching to ring adds only ~5 to ~30 ms. `SSL_CERT_FILE` targets a separate cost and adds up with the others.

| `smithyfull` cold start, arm64                                               | 128 MB, eu‑central‑1 | 1,024 MB, eu‑central‑1 | 128 MB, us‑east‑1 | 1,024 MB, us‑east‑1 |
| ---------------------------------------------------------------------------- | -------------------- | ---------------------- | ----------------- | ------------------- |
| As in the benchmark                                                          | ~330 ms              | ~158 ms                | ~369 ms           | ~200 ms             |
| `SSL_CERT_FILE` set                                                          | ~317 ms              | ~144 ms                | ~343 ms           | ~178 ms             |
| Default features off, `SSL_CERT_FILE` set                                    | ~317 ms              | ~140 ms                | ~331 ms           | ~172 ms             |
| No calls at init: default features off, `SSL_CERT_FILE` set, executable read | ~244 ms              | ~143 ms                | ~261 ms           | ~173 ms             |
| No calls at init: the same, with rustls on ring                              | ~229 ms              | ~137 ms                | ~243 ms           | ~167 ms             |
| Calls at init: default features off, `SSL_CERT_FILE` set, connections opened | ~149 ms              | ~131 ms                | ~180 ms           | ~159 ms             |
| Calls at init: the same, with rustls on ring                                 | ~140 ms              | ~123 ms                | ~169 ms           | ~153 ms             |

(The benchmark's lockfile: `aws-config` 1.8.18, `rustls-native-certs` 0.8.5. 20 forced cold starts per cell, both regions at the same time.)

The same combinations measured a day earlier with `aws-config` 1.12 gave ~373 ms as in the benchmark, ~246 ms with no calls at init and ~147 ms with calls at init, at 128 MB in `eu-central-1`.

The same levers on the SDK probe's single DynamoDB client (rustls on ring, `SSL_CERT_FILE`, the connection opened at init) took its cold start at 128 MB from ~221 ms to ~68 ms.

With one full vCPU (1,769 MB) the levers that only move work into Init stop mattering, but `SSL_CERT_FILE` (an Init cost at every size), rustls on ring and overlapping the connections at init still add up. The results at 3,008 MB were the same within a few ms:

| `smithyfull` cold start at 1,769 MB                                   | Cold start |
| --------------------------------------------------------------------- | ---------- |
| As in the benchmark                                                   | ~175 ms    |
| `SSL_CERT_FILE` set                                                   | ~148 ms    |
| Default features off                                                  | ~173 ms    |
| Executable read at init                                               | ~185 ms    |
| Default features off, `SSL_CERT_FILE` set                             | ~141 ms    |
| rustls on ring, `SSL_CERT_FILE` set                                   | ~135 ms    |
| Default features off, `SSL_CERT_FILE` set, connections opened at init | ~132 ms    |
| rustls on ring, `SSL_CERT_FILE` set, connections opened at init       | ~126 ms    |

(25 forced cold starts per cell.)

<div class="note" label="Why some baselines on this page sit above the benchmark's">

The 1,769 MB table and the single-lever sections used `aws-config` 1.12, while the benchmark's lockfile has 1.8.18. In interleaved comparisons the 1.12 build's first request at 128 MB was ~10 to ~50 ms slower across three runs (~376 ms against ~325 ms cold in the largest); Init and 1,024 MB were unaffected.

The difference is in [loading code](#why-the-first-request-is-slow-loading-code); no crate does more work in the newer build. Tracing which pages each phase touched showed that the 1.12 build's first request needed 12 blocks Init had not loaded, against 8, mostly because the TLS handshake code, which first runs on that request, landed in blocks of its own instead of next to code Init had already loaded. Where code lands is a side effect of linking, so any dependency update can move it either way. Reading the executable at init removed the gap (to ~3 ms).

</div>

### Measured without effect

These did not move the cold start beyond run-to-run noise at 128 MB or 1,024 MB:

- **An explicit region** (`.region(...)` on the loader): the region provider chain finds `AWS_REGION` first and is already cheap.
- **Explicit credentials** (`EnvironmentVariableCredentialsProvider` on the loader): `load()` dropped from ~55 ms to ~13 ms, but building the first client grew by the same amount. Root certificates are loaded when the first HTTPS client is set up, which the default credential chain triggers inside `load()` and an explicit provider leaves to `Client::new`.
- **A current-thread tokio runtime** instead of the multi-threaded default.
- **mimalloc** as the global allocator.
- **S3 request checksums only when required** (`request_checksum_calculation`), for cold or warm `PutObject`.
- **X25519 instead of post-quantum `X25519MLKEM768`** key exchange, on the first handshake or later ones.
- **A newer Smithy server SDK** (`http-1x` codegen with `lambda_http` 1, instead of the benchmark's http 0.x codegen) for `smithyfull`.
- **A non-PIE executable** (`-C relocation-model=static` on the final crate only, through cargo-lambda's zig toolchain, `smithyfull`): the dynamic loader's 640 KB of relocations went away and Init dropped by ~3 ms, but the first request at 128 MB got slower, from ~279 ms to ~333 ms, because the new layout made it take 23 major faults instead of 16. At 1,024 MB both took ~174 ms cold.
- **Changing the kernel's read-around for the executable** (`madvise` with `MADV_RANDOM` or `MADV_SEQUENTIAL` on its mappings, first thing in `main()`, crypto probe): the faults got smaller or larger, but their CPU cost followed the amount read from disk, ~6 to ~8 µs per KB, and that barely changed.

## Java

### Prime SnapStart

Enable SnapStart and register a CRaC `beforeCheckpoint` hook that runs one representative invocation during init, so the AWS SDK's lazy class loading and JIT are captured in the snapshot. Unprimed SnapStart on an SDK-heavy handler can be slower than no SnapStart at all.

| `threeclient`, arm64, 512 MB (2026-06) | Total cold start |
| -------------------------------------- | ---------------- |
| Plain JVM (no SnapStart)               | ~5.0 s           |
| SnapStart, unprimed                    | ~8.4 s           |
| SnapStart, primed                      | ~2.3 s           |

<div class="note" label="Not a win on every handler">

Restoring a snapshot is itself work. SnapStart wins where there is a large init to skip and loses on light handlers, and framework code that no hook can reach stays on the first restored request. A crash or timeout also drops the function back to a full JVM init. Which scenarios win and lose: [when SnapStart wins, and when it loses](./java-snapstart#when-snap-start-wins-and-when-it-loses).

</div>

## Go

### Skip the certificate directory scan

**Applies to:** Go's `crypto/x509`, which the AWS SDK for Go v2 uses for TLS, on `provided.al2023`.

On Linux, Go loads the root certificates from the first CA bundle file it finds and then from every file in `/etc/ssl/certs` and `/etc/pki/tls/certs`. On `provided.al2023` those directories hold each root again as its own file, plus both bundle files, so the same roots are read and parsed several times. Setting the function's environment variable `SSL_CERT_DIR` to a directory that does not exist makes Go skip the scan and load only the bundle (Go ignores a missing directory):

```
SSL_CERT_DIR=/nonexistent
```

Setting only `SSL_CERT_FILE`, the Rust lever, does not help here, because Go scans the directories either way.

With the variable, the function loaded the same 251 roots, and the first request read ~1.8 MB less from disk and used ~36 ms less CPU on arm64. Go loads the roots on the first HTTPS call, so without a warm-up that saving lands in the handler, where the 128 MB CPU share stretched it to ~775 ms: the cold start went from ~1,053 ms to ~265 ms (x86_64: ~1,407 ms to ~339 ms). It removes the work rather than moving it, so it also helps at 1,024 MB (~160 ms to ~88 ms), and "Max Memory Used" dropped by ~5 MB. The table under [warm the SDK's lazy setup](#warm-the-sdks-lazy-setup-at-init) has all the numbers.

The saving happens once per process, however many clients there are. On the benchmark's `threeclient` (DynamoDB, KMS and S3 clients) it was about the same as on `oneclient`: ~1,382 ms to ~427 ms at 128 MB and ~211 ms to ~140 ms at 1,024 MB in `eu-central-1`. A repeat run reproduced the `oneclient` result in both `eu-central-1` (~1,148 ms to ~275 ms) and `us-east-1` (~1,338 ms to ~343 ms).

The trade-offs are those of [loading roots from the CA bundle](#load-root-certificates-from-the-ca-bundle) in Rust: a CA copied only into a certificate directory, rather than added with `update-ca-trust`, no longer loads, and on a base image without a bundle at a path Go knows, no roots load at all and every TLS call fails with an unknown-authority error. Make a test call to each service the function depends on before rolling it out.

### Warm the SDK's lazy setup at init

The AWS SDK for Go v2 loads the OS certificate pool, builds its HTTP transport, and resolves credentials on the first service call, not when the client is created. In a probe taken 2026-07 (arm64, 128 MB) that setup cost ~1.3 s on the first request ([same work, different phase](./lifecycle#rust-vs-go-same-work-different-phase)); most of it is the [certificate directory scan](#skip-the-certificate-directory-scan). As with any init placement, this only shortens an on-demand cold start below about 1.8 GB.

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

The first TLS handshake to each service still happens on the first request to it. A throwaway call at init (`GetItem` here) also opens that connection, with the costs described under [open each client's connection at init](#open-each-clients-connection-at-init).

| `oneclient` cold start, Go 1.26           | arm64, 128 MB | arm64, 1,024 MB | x86_64, 128 MB |
| ----------------------------------------- | ------------- | --------------- | -------------- |
| As in the benchmark                       | ~1,053 ms     | ~160 ms         | ~1,407 ms      |
| `SSL_CERT_DIR` skipped                    | ~265 ms       | ~88 ms          | ~339 ms        |
| Local warm-up above                       | ~166 ms       | ~122 ms         | ~204 ms        |
| Local warm-up, `SSL_CERT_DIR` skipped     | ~133 ms       | ~87 ms          | ~203 ms        |
| Real call at init                         | ~124 ms       | ~125 ms         | ~187 ms        |
| Real call at init, `SSL_CERT_DIR` skipped | ~91 ms        | ~85 ms          | ~126 ms        |

(15 forced cold starts per cell.)

The local warm-up moves ~48 ms of CPU (arm64) into Init, which at 128 MB is far cheaper than paying it in the handler; at 1,024 MB it gains less than skipping the scan, because it moves the work instead of removing it. The two combine, and the fastest variant at every size made a real call at init with the scan skipped.

<style>
/* Same summary panel as lifecycle.md's "The short version". */
.tldr {
  margin: 20px 0 24px; padding: 14px 18px 6px;
  background: color-mix(in srgb, var(--panel) 60%, transparent);
  border: 1px solid var(--frame); border-left: 4px solid var(--accent);
  border-radius: 0 8px 8px 0;
}
.tldr-label { font-size: 12px; letter-spacing: .14em; text-transform: uppercase; color: var(--faint); margin-bottom: 4px; }
/* On a phone the five-column table is wider than the screen: scroll it inside the panel. */
.tldr { overflow-x: auto; }
.tldr table { width: 100%; max-width: none; margin: 0 0 8px; }
/* Long identifiers (EnvironmentVariableCredentialsProvider) would widen the page on a phone. */
p code, li code { overflow-wrap: anywhere; }
/* Wide result tables scroll inside their own box on a phone instead of widening the page. */
@media (max-width: 640px) {
  main table { display: block; overflow-x: auto; }
}
.tldr + h2 { margin-top: 32px; }
.tldr a { color: var(--accent); }
</style>
