//! Benchmark configuration: the matrix, naming conventions, and tunable counts.
//!
//! Everything that defines "what gets built, deployed, and measured" lives here
//! so the rest of the driver stays mechanism, not policy.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. Single source for wall-clock stamping
/// across the driver (run ids, meta timestamps, result rows). Panics only for a
/// pre-1970 system clock, which never occurs in practice.
pub fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis()
}

/// A sortable run id: `<unix_ms>-<8 hex>`. The millisecond clock sorts newest
/// last; the random suffix breaks ties within a millisecond. Shared by the matrix
/// run and the probes so the site's newest-file discovery (which parses the
/// `<unix_ms>` segment) works identically for both.
pub fn run_id() -> String {
    let ms = now_unix_ms();
    let suffix = &uuid::Uuid::new_v4().to_string()[..8];
    format!("{ms}-{suffix}")
}

/// AWS region the benchmark is pinned to. Hardcoded so an ambient `AWS_REGION` /
/// profile mismatch can never silently change where we deploy. The in-Lambda
/// REPORT latencies are region-independent, but warm invokes run serially per
/// cell, so the client↔region round-trip dominates wall-clock; running near the
/// driver shortens a full run substantially.
pub const REGION: &str = "eu-central-1";

/// Shared stem every AWS resource the bencher creates carries: the Lambda
/// functions, IAM role, DynamoDB table, KMS alias, and S3 bucket. Fixed, not
/// configurable: names are derived deterministically from it and teardown
/// reconstructs that exact set, so a mismatch between `run` and `teardown` would
/// orphan resources. The derived names below repeat the literal rather than
/// concatenate it (not allowed for a `const`), so all must change together on a
/// rename. Mirrored in deploy/cdk/lib/constants.ts (kept in sync by hand).
pub const PREFIX: &str = "lambdabench";

/// `PREFIX` plus the trailing hyphen `Cell::function_name` always emits, so the
/// CDK's `lambdabench-*` IAM wildcard matches only `lambdabench-<...>` resources,
/// never a sibling sharing the bare stem (e.g. `lambdabenchmark-*`). Every managed
/// name carries it (asserted by tests), keeping that wildcard a correct upper
/// bound on what the runner can touch. Spelled as its own literal (a `const`
/// cannot be concatenated); same rename caveat as `PREFIX`.
pub const RESOURCE_PREFIX: &str = "lambdabench-";

/// IAM role assumed by every benchmarked function.
pub const ROLE_NAME: &str = "lambdabench-lambda-exec";
/// Inline policy on `ROLE_NAME` granting the data-plane access the scenarios
/// need: DDB Get/Put, KMS Encrypt, and S3 Get/Put (see `iam.rs` for the exact,
/// resource-scoped statements). The create and teardown paths both reference
/// this constant, so it is the single source for the name.
pub const INLINE_POLICY_NAME: &str = "lambdabench-scenario-access";

/// DynamoDB table holding the single seeded item read by the DDB-using
/// scenarios (`oneclient`, `threeclient`, `smithyfull`).
pub const TABLE_NAME: &str = "lambdabench-table";
pub const TABLE_PK: &str = "pk";
/// Partition-key value of the seeded item.
pub const SEED_KEY: &str = "lambdabench-item";
/// String payload stored on the seeded item and echoed back by handlers.
pub const SEED_PAYLOAD: &str = "lambdabench-seeded-payload-value";
/// S3 object-metadata key stamping the seed payload's schema version, so
/// `ensure_seed_object` can HeadObject to decide whether the stored payload is
/// current before re-uploading.
pub const SEED_VERSION_META_KEY: &str = "lambdabench-seed-version";

/// KMS key alias used by the three_client scenario to address the encrypt key.
/// Carries the `lambdabench-` prefix (after the required `alias/` segment) so the
/// runner's IAM `lambdabench-*` wildcard covers it like the other resources.
pub const KMS_ALIAS: &str = "alias/lambdabench-key";
/// Tag applied to the KMS key at creation. The runner's IAM policy scopes
/// `kms:ScheduleKeyDeletion` by this tag rather than by alias, because teardown
/// removes the alias before deleting, and both the creation-time orphan cleanup
/// and teardown's orphan sweep (`reclaim_orphaned_kms_keys`) schedule a key that
/// never received an alias; the tag is present from key creation in every case.
/// Teardown's sweep additionally calls `kms:ListKeys` and `kms:ListResourceTags`.
/// The deploy CDK (deploy/cdk/lib/bench-runner-stack.ts) must keep this key/value
/// in sync with that policy condition and grant those two list actions.
///
/// Unlike every other resource, the orphan sweep finds candidates by scanning
/// every KMS key in the account/region (see `reclaim_orphaned_kms_keys`), not by
/// an exact, predetermined name, so this tag is the only thing distinguishing a
/// lambdabench-owned key from someone else's in a shared account. Deliberately a
/// specific, namespaced phrase (not a bare word like `"lambdabench"`, which a
/// coincidental unrelated tag could realistically match) to keep that sweep's
/// blast radius as narrow as the exact-name deletes everywhere else.
pub const KMS_TAG_KEY: &str = "lambdabench-managed-kms-key";
pub const KMS_TAG_VALUE: &str = "true";
pub const S3_BUCKET_SUFFIX: &str = PREFIX;
pub const S3_OBJECT_KEY: &str = "lambdabench-object";
pub const S3_OBJECT_BODY: &str = "lambdabench-seeded-s3-object-body";

/// S3 object read once at init by the `lettercount` scenario. Unlike the tiny
/// three_client object, this is a ~1 MB ASCII JSON document so the per-invoke
/// parse/transform/serialize does real work (see `S3_LETTERCOUNT_PAYLOAD_BYTES`).
pub const S3_LETTERCOUNT_KEY: &str = "lambdabench-lettercount.json";
/// Minimum serialized size of the seeded `lettercount` payload, in bytes. The
/// bencher generates a deterministic ASCII JSON document at least this large.
/// Tunable: a larger payload means more allocation per invoke (more GC fuel for
/// the Node runtime) at the cost of a slower init fetch.
pub const S3_LETTERCOUNT_PAYLOAD_BYTES: usize = 1_000_000;

/// S3 object read once at init by the `batch` scenario: a large JSON array of
/// event records (`{ "key": "...", "value": N }`). Each warm invoke parses the
/// whole batch and groups-by key into a HashMap of running sum+count. See the
/// `Scenario::Batch` doc for the two axes it measures (parser-speed median,
/// low-memory GC tail).
pub const S3_BATCH_KEY: &str = "lambdabench-batch.json";
/// Minimum serialized size of the seeded `batch` payload (~16 MB). Big enough
/// that the parsed batch + group map is a substantial live heap at the low memory
/// tiers (where used ≈ allocated, so a tracing GC runs near its limit).
pub const S3_BATCH_PAYLOAD_BYTES: usize = 16_000_000;
/// Number of distinct group keys in the `batch` payload. Bounds the group map
/// size; moderate cardinality keeps the map meaningfully large without dominating
/// over the parsed-record graph.
pub const S3_BATCH_KEY_CARDINALITY: usize = 1_000;

/// Lambda's fixed log-group prefix. Lambda auto-creates `/aws/lambda/<function>`
/// on a function's first invoke, so the benchmark's log-group names are exactly
/// its function names under this prefix. Teardown deletes them by that exact
/// name (see `all_managed_log_group_names`), never by a listing sweep.
pub const LAMBDA_LOG_GROUP_PREFIX: &str = "/aws/lambda/";

/// Artifact sizes (MB) the synthetic download-scaling probe sweeps. Methodology,
/// not a CLI knob: the exact set the published chart is built from AND the set
/// teardown enumerates. The probe reads this const directly, so it can only create
/// names teardown knows. `probe/synthetic.rs::sizes_in_range` pins every entry
/// within Lambda's package limit.
pub const SYNTH_DEFAULT_SIZES_MB: &[u32] = &[1, 10, 50, 100, 200];

/// Family labels for the synthetic ZIP download-scaling probe: one function per
/// (family, size). Mirrors `SynthRuntime::family` in `probe/synthetic.rs`, pinned
/// equal by a test there.
pub const SYNTH_ZIP_FAMILIES: &[&str] = &["python", "rust"];

/// Family labels for the synthetic CONTAINER-IMAGE download-scaling probe: two
/// functions per size from one image. Listed in the probe's create order
/// (`[false, true]` → untouched, touched), pinned equal by a test in
/// `probe/image.rs`.
pub const SYNTH_IMAGE_FAMILIES: &[&str] = &["image-untouched", "image-touched"];

/// The synthetic probe's function name for a `(family, size_mb)`:
/// `lambdabench-synthdl-<family>-<mb>mb`. Shared by the probe create sites and
/// the teardown enumerator so both format identically.
pub fn synth_function_name(family: &str, size_mb: u32) -> String {
    format!("{RESOURCE_PREFIX}synthdl-{family}-{size_mb}mb")
}

/// The single ECR repository the synthetic image family pushes to. Carries the
/// `lambdabench-` stem for naming consistency with every other benchmark
/// resource; teardown deletes it by this exact name (see `Aws::delete_ecr_repo`).
pub const ECR_REPO: &str = "lambdabench-synthdl";

/// Throwaway function name the download probes invoke to pre-warm the shared
/// data-plane HTTPS/TLS connection. Never created: the invoke is expected to fail
/// with ResourceNotFound once the connection is established. The name cannot
/// collide with a real matrix or synthetic function.
pub const PREWARM_NONEXISTENT_FN: &str = "lambdabench-probe-prewarm-nonexistent";

// The `authz` scenario needs no S3 object or config constant: it receives the
// signed JWT in the invoke payload (a build-time fixture; see `bencher/fixtures/`,
// gitignored) as a real authorizer would, and embeds the public verification key
// in the handler binary. See the `Scenario::Authz` doc for what it measures.

/// Fixed write targets for the `smithyfull` realistic CreateOrder flow. Fixed
/// keys make the writes idempotent (each invoke overwrites the same item /
/// object), so however many cold/warm invocations run never accumulate data.
pub const ORDER_PK: &str = "lambdabench-order";
pub const S3_RECEIPT_KEY: &str = "lambdabench-receipt";

/// Languages under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Rust,
    Node,
    Java,
    Python,
    Go,
}

impl Lang {
    pub const ALL: [Lang; 5] = [Lang::Rust, Lang::Node, Lang::Java, Lang::Python, Lang::Go];

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Node => "node",
            Lang::Java => "java",
            Lang::Python => "python",
            Lang::Go => "go",
        }
    }

    /// Parses a CLI language name, or errors listing the valid names.
    pub fn parse(s: &str) -> Result<Lang, String> {
        Lang::ALL
            .into_iter()
            .find(|l| l.as_str() == s.trim())
            .ok_or_else(|| {
                let names: Vec<&str> = Lang::ALL.iter().map(|l| l.as_str()).collect();
                format!("unknown lang '{s}' (valid: {})", names.join(", "))
            })
    }

    /// Lambda managed runtime identifier. Go compiles to a native executable and
    /// ships on the OS-only `provided.al2023` custom runtime (the same family
    /// Rust uses), since it needs no language runtime of its own.
    pub fn runtime(self) -> &'static str {
        match self {
            Lang::Rust => "provided.al2023",
            Lang::Node => "nodejs24.x",
            Lang::Java => "java25",
            Lang::Python => "python3.14",
            Lang::Go => "provided.al2023",
        }
    }

    /// Whether this language hosts a given scenario. Python and Go both skip the
    /// two Smithy scenarios (`smithy`, `smithyfull`): neither has a usable,
    /// fair SERVER SDK. smithy-python emits clients/types only; smithy-go ships
    /// only a work-in-progress, unpublished `go-server-codegen` that targets
    /// `awsJson1_0` (the shared CoffeeShop model is `restJson1`, so a Go server
    /// would do different (de)serialization work than the Rust/Node/Java servers,
    /// breaking the same-task fairness rule) and provides no Lambda adapter. So
    /// there is no fair way to host the Smithy server in either; those cells are
    /// never generated. The other languages host every scenario.
    pub fn supports(self, scenario: Scenario) -> bool {
        match self {
            Lang::Python | Lang::Go => !matches!(scenario, Scenario::Smithy | Scenario::SmithyFull),
            _ => true,
        }
    }
}

/// Scenarios under test; each does the same task in every language. The full
/// reader-facing description (what each measures and how to read it) is in
/// README.md#scenarios and the design intent is in DESIGN.md; this comment records
/// only the mechanics an edit here must preserve.
///
/// The first five are handler shapes read on cold start. `hello` is the bare
/// baseline; the other four add a layer on top of it. Compare each to the shape it
/// builds on, never by subtracting across the set: each initializes its own way,
/// so the layers do not additively decompose.
///   hello        = runtime baseline (no framework, no AWS client)
///   smithy       = framework only (Smithy server, no AWS call)
///   one_client   = construct + call 1 AWS client (DDB GetItem)
///   three_client = construct + call 3 AWS clients (DDB + KMS + S3)
///   smithy_full  = Smithy server + 3 AWS clients (realistic write flow)
///
/// The last four are CPU probes read on warm latency, each isolating where the CPU
/// time goes. Read them by direct cross-language comparison, not by subtraction.
///   lettercount = pure in-language CPU: parse ~1 MB JSON and count a..z, no native
///                 lib, no retained heap. Widest, cliff-shaped warm spread.
///   authz       = the lettercount counterpart with a small NATIVE-crypto slice:
///                 RS256 verify (symmetric across languages) then in-language claim
///                 mapping. Verify is cheap, so the glue still dominates; warm
///                 spread stays moderate and flat.
///   batch       = deserialize-heavy record processor. Two axes: the MEDIAN is each
///                 language's stdlib JSON-parser speed (the dominant signal;
///                 keeping the idiomatic parser avoids the SHA trap of comparing
///                 libraries), the low-memory TAIL is transient allocation + GC (the
///                 parsed batch and group map are live at once). Read the tail in
///                 absolute ms, not a P99.9/median ratio.
///   cache       = the dedicated GC probe batch cannot be: a ~100 MB RETAINED live
///                 set held across invokes with a churned fraction each invoke, so a
///                 tracing GC re-traces the whole set every cycle (tracing cost
///                 scales with the live heap, not the garbage). The tail separates
///                 from a flat median at the fractional-vCPU tiers. An indexed ring
///                 of buffers, not a hashmap, to isolate the GC, not map impls.
///
/// `batch` floors at `min_memory_mb` (the ~16 MB batch OOMs the smallest tiers);
/// `cache` floors at 512 MB (its retained set OOMs / CPU-starves below).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    Hello,
    Smithy,
    OneClient,
    ThreeClient,
    SmithyFull,
    LetterCount,
    Authz,
    Batch,
    Cache,
}

impl Scenario {
    pub const ALL: [Scenario; 9] = [
        Scenario::Hello,
        Scenario::Smithy,
        Scenario::OneClient,
        Scenario::ThreeClient,
        Scenario::SmithyFull,
        Scenario::LetterCount,
        Scenario::Authz,
        Scenario::Batch,
        Scenario::Cache,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Scenario::Hello => "hello",
            Scenario::Smithy => "smithy",
            Scenario::OneClient => "oneclient",
            Scenario::ThreeClient => "threeclient",
            Scenario::SmithyFull => "smithyfull",
            Scenario::LetterCount => "lettercount",
            Scenario::Authz => "authz",
            Scenario::Batch => "batch",
            Scenario::Cache => "cache",
        }
    }

    /// Parses a CLI scenario name, or errors listing the valid names. Matches
    /// against `as_str` so a new scenario needs no second mapping.
    pub fn parse(s: &str) -> Result<Scenario, String> {
        Scenario::ALL
            .into_iter()
            .find(|sc| sc.as_str() == s.trim())
            .ok_or_else(|| {
                let names: Vec<&str> = Scenario::ALL.iter().map(|sc| sc.as_str()).collect();
                format!("unknown scenario '{s}' (valid: {})", names.join(", "))
            })
    }

    /// Minimum memory tier (MB) this scenario can run on for a given language;
    /// tiers below it are never built, deployed, or run. Language- and
    /// variant-aware because baseline footprints differ, and each floor is set
    /// empirically at the lowest tier the (scenario, runtime) reliably completes.
    ///
    /// - `batch`: 256 on Rust/Node (128 MB ≈ 0.07 vCPU CPU-starves Node past the
    ///   30 s timeout; Rust uses ~117 MB at 256). 512 on plain Java (OOMs at 256,
    ///   where Node survives at ~210 MB). 1024 on Java SnapStart (the restored
    ///   snapshot's resident heap/metaspace OOMs it at 512, where plain Java fits).
    /// - `smithyfull` on Java SnapStart: 256 (at 128 MB the restore competes with
    ///   the first invoke's CreateOrder write flow and times out at ~30.2 s; plain
    ///   Java completes at 128, so the floor is SnapStart-specific).
    /// - `lettercount` on all Java: 512 (256 OOMs, 128 times out, both intermittent).
    /// - `cache` on every language: 512 (its ~100 MB retained set OOMs/CPU-starves
    ///   below, worst on the GC'd runtimes).
    pub fn min_memory_mb(self, lang: Lang, snapstart: bool) -> i32 {
        match (self, lang) {
            (Scenario::Batch, Lang::Java) => {
                if snapstart {
                    1024
                } else {
                    512
                }
            }
            (Scenario::Batch, _) => 256,
            (Scenario::SmithyFull, Lang::Java) if snapstart => 256,
            (Scenario::LetterCount, Lang::Java) => 512,
            (Scenario::Cache, _) => 512,
            _ => 128,
        }
    }

    /// Whether this scenario is fronted by a Smithy server SDK behind an API
    /// Gateway Lambda adapter, so its response is an HTTP envelope
    /// (`{ "statusCode": N, ..., "body": ... }`) rather than the raw handler
    /// return. The benchmark validates that envelope's status (see
    /// `check_platform_ok`): the framework serializes an internal error as a 500
    /// INSIDE the envelope while the Lambda invoke still returns normally, so
    /// without this the failure would be recorded as a clean invoke.
    pub fn is_http_fronted(self) -> bool {
        matches!(self, Scenario::Smithy | Scenario::SmithyFull)
    }

    /// Whether this scenario reads the seeded DynamoDB item.
    pub fn needs_ddb(self) -> bool {
        matches!(
            self,
            Scenario::OneClient | Scenario::ThreeClient | Scenario::SmithyFull
        )
    }

    /// Whether this scenario additionally uses KMS Encrypt and S3 GetObject.
    /// Gates the KMS key id plus the three_client/smithyfull S3 object and
    /// write-target env vars. `lettercount`/`batch` use S3 but not KMS and read
    /// their own objects, so they go through `needs_s3` instead.
    pub fn needs_kms_s3(self) -> bool {
        matches!(self, Scenario::ThreeClient | Scenario::SmithyFull)
    }

    /// Whether this scenario reads an object from the benchmark S3 bucket
    /// (`needs_kms_s3` plus `lettercount`/`batch`, which read their own objects).
    /// Decides which functions get the `LAMBDABENCH_BUCKET` env var; the IAM role
    /// already grants `s3:GetObject` bucket-wide, so no extra permission wiring.
    pub fn needs_s3(self) -> bool {
        self.needs_kms_s3() || matches!(self, Scenario::LetterCount | Scenario::Batch)
    }

    /// The full-profile cold/warm iteration counts for this scenario at the given
    /// lang/memory, BEFORE the cell-level jitter-A/B bypass and SnapStart clamp
    /// (both in `Cell::iterations`, which composes this). Every scenario returns an
    /// explicit `(cold, warm)`, so no count can silently fall through to a default.
    ///
    /// Light / I/O scenarios use `FULL_LIGHT_COUNTS` (cold start is their axis).
    /// The CPU probes use long warm runs so a GC'd runtime builds enough heap
    /// pressure to show its tail. Those counts are also thinned on the slowest
    /// runtime (Python) at the starved low-CPU tiers, where the full count would
    /// run tens of minutes per cell (measured: Python `lettercount`@128 ≈ 50 min,
    /// `batch`@256 ≈ 35 min). We thin the sample count, not the tiers: the
    /// low-memory cliff is the most interesting data and the cross-runtime gap is
    /// already wide there, so fewer samples still show it.
    pub fn full_base_counts(self, lang: Lang, memory_mb: i32) -> (u32, u32) {
        let starved_python = lang == Lang::Python && memory_mb <= 256;
        match self {
            Scenario::LetterCount if starved_python => (5, 300),
            // cache's signal IS the warm tail, so it needs the dense warm run at
            // every (512+) tier; its churn+scan is fast enough not to need thinning.
            Scenario::Cache => (5, 1500),
            Scenario::LetterCount | Scenario::Authz => (5, 1500),
            // batch's GC pressure is per-invoke, so 200 warm fully expresses the
            // tail; cold is cut to 15 (still characterizes cold start) because at
            // the starved tiers 50 cold × 200 warm runs ~5 h/cell, long enough to
            // risk session-token expiry. Starved Python is slower still, so cut cold
            // to 5 there.
            Scenario::Batch if starved_python => (5, 200),
            Scenario::Batch => (15, 200),
            // Light / I/O scenarios: cold start is the axis of interest.
            Scenario::Hello
            | Scenario::Smithy
            | Scenario::OneClient
            | Scenario::ThreeClient
            | Scenario::SmithyFull => FULL_LIGHT_COUNTS,
        }
    }
}

/// CPU architectures under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    Arm64,
    X86_64,
}

impl Arch {
    pub const ALL: [Arch; 2] = [Arch::Arm64, Arch::X86_64];

    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::X86_64 => "x86_64",
        }
    }

    /// Parses a CLI architecture name, or errors listing the valid names. Matches
    /// against `as_str` so accepted spellings cannot drift from the canonical ones.
    pub fn parse(s: &str) -> Result<Arch, String> {
        Arch::ALL
            .into_iter()
            .find(|a| a.as_str() == s.trim())
            .ok_or_else(|| {
                let names: Vec<&str> = Arch::ALL.iter().map(|a| a.as_str()).collect();
                format!("unknown arch '{s}' (valid: {})", names.join(", "))
            })
    }

    /// Lambda `Architectures` value.
    pub fn lambda_arch(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::X86_64 => "x86_64",
        }
    }

    /// Canonical OCI/containerd architecture name for a `linux/<arch>` platform
    /// string (used when assembling a container image). Distinct from
    /// `lambda_arch`: the OCI world spells x86-64 as `amd64` (Lambda spells it
    /// `x86_64`), so `crane mutate --platform linux/<arch>` needs the OCI spelling
    /// to select the right variant from the multi-arch base manifest.
    pub fn oci_arch(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::X86_64 => "amd64",
        }
    }
}

/// Memory sizes (MB) swept across.
pub const MEMORY_MB: [i32; 6] = [128, 256, 512, 1024, 2048, 3008];

/// Validates each `--memory` value against the swept tiers, so a typo (e.g.
/// `1000` for `1024`) fails loud rather than silently selecting zero cells.
/// Shared by `select_cells` and the download-start probe's target resolution.
pub fn validate_memory_tiers(values: &[i32]) -> Result<(), String> {
    for m in values {
        if !MEMORY_MB.contains(m) {
            return Err(format!(
                "--memory {m} is not a swept tier (valid: {})",
                MEMORY_MB
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(())
}

/// Cold cycles per Java SnapStart cell, overriding the scenario count. Each cold
/// cycle publishes a new version (a fresh snapshot + restore), far slower than the
/// env-bump cold-force other cells use, so the count is kept small. SnapStart
/// still sweeps the full `MEMORY_MB` range; only the cold-cycle count is reduced.
pub const SNAPSTART_COLD_CYCLES: u32 = 10;

/// Iteration-count profile for a run. A run picks a named profile rather than raw
/// counts; each scenario's count is methodology (chosen for a documented reason in
/// `Scenario::full_base_counts`) and lives in code next to that reason. Given a
/// profile, every cell's count is deterministic (`Cell::iterations`), so a run is
/// fully described by the profile name plus the matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// The published methodology: per-scenario counts (light scenarios at
    /// `FULL_LIGHT_COUNTS`, the CPU probes at their long-warm overrides), with the
    /// jitter-A/B bypass and the SnapStart cold-cycle clamp applied. What the
    /// hosted site is built from.
    #[default]
    Full,
    /// A tiny flat count over the entire matrix for a quick end-to-end pipeline
    /// sanity pass (build + deploy + invoke + parse + record). NOT statistically
    /// meaningful and never published.
    Smoke,
}

/// Full-profile cold/warm counts for the light / I/O scenarios. Cold start is
/// their story; 50 cold cycles bounds the warm tail's confidence (warm samples
/// within a cycle share a sandbox, so the cold-cycle count is the
/// independent-replicate count), and 50 warm/cycle is already past the P99/P99.9
/// thresholds in `site/src/lib/stats.js`. The CPU probes trade this for
/// cold-sparse / warm-dense counts; see `Scenario::full_base_counts`.
pub const FULL_LIGHT_COUNTS: (u32, u32) = (50, 50);

/// Smoke-profile counts: a couple of cold cycles and a couple of warm invokes for
/// every cell, just enough to exercise the whole pipeline end to end.
pub const SMOKE_COUNTS: (u32, u32) = (2, 2);

/// Compiler optimization level, a Rust-only benchmark dimension. `opt-level=3`
/// maximizes runtime speed at the cost of larger binaries; `opt-level=z`
/// minimizes binary size, which can load (and thus cold-start) faster. Which
/// wins for cold start is scenario-dependent, so we measure both. Node has no
/// equivalent knob (esbuild minification is always on), so it carries no opt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Opt {
    /// `opt-level = 3`: all optimizations, larger binary.
    O3,
    /// `opt-level = "z"`: optimize for size.
    Oz,
}

impl Opt {
    pub const ALL: [Opt; 2] = [Opt::O3, Opt::Oz];

    /// Short tag used in names and labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Opt::O3 => "o3",
            Opt::Oz => "oz",
        }
    }

    /// The cargo `opt-level` profile value this maps to.
    pub fn cargo_opt_level(self) -> &'static str {
        match self {
            Opt::O3 => "3",
            Opt::Oz => "z",
        }
    }
}

/// Whether the Rust binary keeps `aws-lc-rs`'s CPU jitter-entropy seeding (`On`)
/// or disables it at build time via `AWS_LC_SYS_NO_JITTER_ENTROPY=1` (`Off`).
///
/// Rust-only diagnostic A/B. The standing matrix is `Off`; a scoped `On` variant
/// is emitted for `oneclient` and `lettercount` (see `JITTER_AB_SCENARIOS`) to
/// quantify the seeding's cost without doubling the matrix. Those two place the
/// same one-time cost in opposing lifecycle phases (the `oneclient` Invoke-phase
/// cliff vs the `lettercount` Init-phase flat bump).
///
/// Disabling it is a latency/security trade-off, not a default: it drops one of
/// AWS-LC's entropy sources. One independent source always remains (the OS); a
/// second (`RDRAND`/`RNDR`) only where the CPU has a hardware RNG, which arm64
/// Graviton2 lacks, so on that part the two seeding slots both fall back to the
/// OS and are no longer independent (see the README jitter Finding / rust.md).
/// Java SnapStart gets the same opt-out automatically via AWS-LC's VM-UBE
/// detection, so no jitter cliff shows there. Full story in the README "Finding:
/// the AWS-LC jitter-entropy cold-start tax"; references: aws/aws-lc-rs#899 and
/// the AWS Rust SDK Lambda mitigation announcement (smithy-lang/smithy-rs#4541).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Jitter {
    /// `aws-lc-rs` jitter-entropy seeding ENABLED (no build flag set).
    /// Diagnostic A/B variant only.
    On,
    /// `aws-lc-rs` jitter-entropy seeding DISABLED via
    /// `AWS_LC_SYS_NO_JITTER_ENTROPY=1` at build time. The standing matrix
    /// default.
    Off,
}

impl Jitter {
    /// Short tag used in names and labels (`on` / `off`).
    pub fn as_str(self) -> &'static str {
        match self {
            Jitter::On => "on",
            Jitter::Off => "off",
        }
    }
}

/// Scenarios that carry the diagnostic jitter=On variant alongside the default
/// jitter=Off cells. Kept to these two because they place the tax in opposing
/// lifecycle phases (see `Jitter`); more scenarios would only repeat one of them.
const JITTER_AB_SCENARIOS: [Scenario; 2] = [Scenario::OneClient, Scenario::LetterCount];

/// One point in the benchmark matrix: a single deployed Lambda function.
#[derive(Debug, Clone, Copy)]
pub struct Cell {
    pub lang: Lang,
    pub scenario: Scenario,
    pub arch: Arch,
    pub memory_mb: i32,
    /// Rust-only optimization level; `None` for Node and Java.
    pub opt: Option<Opt>,
    /// Java-only SnapStart dimension: when true, the function has SnapStart
    /// enabled (`ApplyOn=PublishedVersions`) and is measured via published
    /// versions rather than `$LATEST`. Always `false` for Rust and Node, which
    /// have no SnapStart equivalent.
    pub snapstart: bool,
    /// Rust-only `aws-lc-rs` jitter-entropy build dimension. `None` for non-Rust
    /// cells; `Some(Jitter::Off)` for the standing Rust matrix; `Some(Jitter::On)`
    /// only for the diagnostic A/B cells (see `JITTER_AB_SCENARIOS`).
    pub jitter: Option<Jitter>,
}

impl Cell {
    /// Whether this cell participates in the `aws-lc-rs` jitter-entropy A/B
    /// (Rust + o3 + a `JITTER_AB_SCENARIOS` scenario). Both jitter values match
    /// here, so on/off pairs get identical iteration counts, a precondition for a
    /// fair cliff/bump P50 comparison.
    pub fn is_jitter_ab(&self) -> bool {
        self.lang == Lang::Rust
            && self.opt == Some(Opt::O3)
            && JITTER_AB_SCENARIOS.contains(&self.scenario)
    }

    /// Structural invariants every constructed cell must hold: `opt`/`jitter` are
    /// Rust-only, `snapstart` is Java-only, `snapstart` and `jitter == On` are
    /// mutually exclusive, and `jitter == On` is scoped to the A/B. Called per cell
    /// in `all_cells` so a matrix-generation regression fails loud in debug/test
    /// builds rather than shipping a mis-shaped cell.
    fn assert_valid(&self) {
        debug_assert_eq!(
            self.opt.is_some(),
            self.lang == Lang::Rust,
            "opt is Rust-only: {self:?}"
        );
        debug_assert_eq!(
            self.jitter.is_some(),
            self.lang == Lang::Rust,
            "jitter is Rust-only: {self:?}"
        );
        if self.snapstart {
            debug_assert_eq!(self.lang, Lang::Java, "snapstart is Java-only: {self:?}");
        }
        debug_assert!(
            !(self.snapstart && self.jitter == Some(Jitter::On)),
            "snapstart and jitter=On are mutually exclusive: {self:?}"
        );
        if self.jitter == Some(Jitter::On) {
            debug_assert!(
                self.is_jitter_ab(),
                "jitter=On must be scoped to the A/B (Rust + o3 + JITTER_AB_SCENARIOS): {self:?}"
            );
        }
    }

    /// The cold/warm cycle counts that apply to THIS cell under the given profile.
    /// The single source of truth: the planned-invocation estimate and the run loop
    /// both call it, so they cannot disagree, and a run is fully described by its
    /// profile plus the matrix.
    ///
    /// `Smoke` gives every cell the same tiny flat count. `Full` composes three
    /// rules:
    ///   1. the scenario's `full_base_counts` (lang/memory-aware), EXCEPT
    ///   2. jitter-A/B cells use `FULL_LIGHT_COUNTS` instead: the lettercount
    ///      override (5 cold) starves the jitter chart's P50, and 50 cold matches
    ///      `oneclient`'s density so both panels carry comparable uncertainty; then
    ///   3. SnapStart cells clamp cold cycles to `SNAPSTART_COLD_CYCLES` (keeping
    ///      warm), since each SnapStart cold sample publishes a fresh version
    ///      (~10-30 s). The clamp only ever reduces.
    pub fn iterations(&self, profile: Profile) -> (u32, u32) {
        let (cold, warm) = match profile {
            Profile::Smoke => SMOKE_COUNTS,
            Profile::Full if self.is_jitter_ab() => FULL_LIGHT_COUNTS,
            Profile::Full => self.scenario.full_base_counts(self.lang, self.memory_mb),
        };
        if self.snapstart {
            (cold.min(SNAPSTART_COLD_CYCLES), warm)
        } else {
            (cold, warm)
        }
    }

    /// The deployed Lambda function name. Includes the opt segment for Rust
    /// (e.g. `lambdabench-rust-hello-arm64-o3-1024`), a `snap` segment for Java
    /// SnapStart cells (e.g. `lambdabench-java-hello-arm64-snap-1024`), and a
    /// `jitter` segment for Rust cells built with `aws-lc-rs` jitter-entropy
    /// seeding ENABLED (the diagnostic A/B variant only).
    pub fn function_name(&self) -> String {
        let mut parts = vec![
            PREFIX.to_string(),
            self.lang.as_str().to_string(),
            self.scenario.as_str().to_string(),
            self.arch.as_str().to_string(),
        ];
        if let Some(opt) = self.opt {
            parts.push(opt.as_str().to_string());
        }
        if self.snapstart {
            parts.push("snap".to_string());
        }
        if matches!(self.jitter, Some(Jitter::On)) {
            parts.push("jitter".to_string());
        }
        parts.push(self.memory_mb.to_string());
        parts.join("-")
    }

    /// Lambda handler string for this cell. Rust custom runtimes ignore it (the
    /// bootstrap is the entrypoint); Node points at the bundled ESM module
    /// export. Java is scenario-aware: the Smithy scenarios run behind the
    /// smithy-java `LambdaEndpoint`, while the others use a per-scenario handler
    /// class under the `lambdabench` package.
    pub fn handler(&self) -> String {
        match self.lang {
            // Go and Rust both deploy a custom-runtime `bootstrap` executable,
            // which is the entrypoint; the handler string is ignored.
            Lang::Rust | Lang::Go => "bootstrap".to_string(),
            Lang::Node => "index.handler".to_string(),
            Lang::Java => match self.scenario {
                Scenario::Smithy | Scenario::SmithyFull => {
                    "software.amazon.smithy.java.aws.integrations.lambda.LambdaEndpoint::handleRequest"
                        .to_string()
                }
                other => format!("lambdabench.{}Handler::handleRequest", scenario_class(other)),
            },
            // Python: `<module>.<function>`. Every scenario ships a `handler.py`
            // exposing a `handler` function. Python never hosts a Smithy
            // scenario (no server SDK), so there is no Smithy-adapter handler.
            Lang::Python => "handler.handler".to_string(),
        }
    }

    /// The build artifact this cell deploys. Many cells share one artifact:
    /// Rust binaries differ by (scenario, arch, opt, jitter); Node and Java zips
    /// differ by scenario only (arch-independent bytecode/JS). SnapStart is a pure
    /// function-config knob (the jar is identical), so it does not key the
    /// artifact.
    pub fn artifact_key(&self) -> ArtifactKey {
        ArtifactKey {
            lang: self.lang,
            scenario: self.scenario,
            // Collapse arch for artifacts whose bytes are identical across
            // arches, so they are built once and deployed to both. Node JS and
            // Java bytecode are arch-independent; so are the pure-Python bundles.
            // Arch-specific artifacts (Rust binaries; the Python `authz` bundle,
            // which carries a native `cryptography` wheel) keep their arch.
            arch: if arch_significant(self.lang, self.scenario) {
                self.arch
            } else {
                Arch::Arm64
            },
            opt: self.opt,
            jitter: self.jitter,
        }
    }
}

/// Whether a (lang, scenario) artifact's bytes differ by architecture, so the
/// build must produce one artifact per arch rather than a single shared one.
/// Rust binaries are always arch-specific. The Python `authz` bundle carries a
/// native `cryptography` wheel (compiled per arch), so it is too; every other
/// Python bundle is pure-Python and arch-independent. Node and Java are always
/// arch-independent. Single source of truth so `artifact_key` (which collapses
/// arch) and `ArtifactKey::label` (which encodes it) cannot disagree.
fn arch_significant(lang: Lang, scenario: Scenario) -> bool {
    match lang {
        // Rust and Go compile to a native per-arch executable, so every scenario
        // is arch-specific (one binary per arch).
        Lang::Rust | Lang::Go => true,
        Lang::Python => scenario == Scenario::Authz,
        Lang::Node | Lang::Java => false,
    }
}

/// PascalCase class-name stem for a scenario's Java handler (e.g. `OneClient`
/// for the `oneclient` scenario, yielding `lambdabench.OneClientHandler`).
fn scenario_class(scenario: Scenario) -> &'static str {
    match scenario {
        Scenario::Hello => "Hello",
        Scenario::Smithy => "Smithy",
        Scenario::OneClient => "OneClient",
        Scenario::ThreeClient => "ThreeClient",
        Scenario::SmithyFull => "SmithyFull",
        Scenario::LetterCount => "LetterCount",
        Scenario::Authz => "Authz",
        Scenario::Batch => "Batch",
        Scenario::Cache => "Cache",
    }
}

impl fmt::Display for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.function_name())
    }
}

/// Identifies a unique build artifact (deduped across memory configs, and
/// across arch for Node).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArtifactKey {
    pub lang: Lang,
    pub scenario: Scenario,
    pub arch: Arch,
    /// Rust-only optimization level; `None` for Node.
    pub opt: Option<Opt>,
    /// Rust-only `aws-lc-rs` jitter-entropy build dimension; `None` for non-Rust.
    /// `Some(Jitter::Off)` is the standing matrix default; only `Some(Jitter::On)`
    /// adds a `-jitter` segment to the label.
    pub jitter: Option<Jitter>,
}

impl ArtifactKey {
    pub fn label(&self) -> String {
        match self.lang {
            Lang::Rust => {
                let base = format!(
                    "rust-{}-{}-{}",
                    self.scenario.as_str(),
                    self.arch.as_str(),
                    self.opt.map(|o| o.as_str()).unwrap_or("o3"),
                );
                // Only the diagnostic jitter=On variant tags the label.
                if matches!(self.jitter, Some(Jitter::On)) {
                    format!("{base}-jitter")
                } else {
                    base
                }
            }
            Lang::Node => format!("node-{}", self.scenario.as_str()),
            Lang::Java => format!("java-{}", self.scenario.as_str()),
            // Go is a native per-arch binary (no opt-level dimension), so its
            // label always carries the arch, matching the per-arch artifacts.
            Lang::Go => format!("go-{}-{}", self.scenario.as_str(), self.arch.as_str()),
            // Python is arch-specific only for `authz` (native crypto wheel);
            // its label then carries the arch, matching the per-arch artifacts.
            // The pure-Python scenarios collapse to a single arch-independent
            // label (the arch is forced to arm64 in `artifact_key`).
            Lang::Python => {
                if arch_significant(Lang::Python, self.scenario) {
                    format!("python-{}-{}", self.scenario.as_str(), self.arch.as_str())
                } else {
                    format!("python-{}", self.scenario.as_str())
                }
            }
        }
    }
}

/// Generates the full benchmark matrix, sweeping scenario × arch × opt(Rust) ×
/// memory.
pub fn all_cells() -> Vec<Cell> {
    let mut cells = Vec::new();

    for lang in Lang::ALL {
        for scenario in Scenario::ALL {
            // A language may not host every scenario (Python skips the Smithy
            // scenarios: smithy-python has no server SDK). Skip those entirely
            // so no cell, artifact, or function is ever produced for them.
            if !lang.supports(scenario) {
                continue;
            }
            for arch in Arch::ALL {
                for memory_mb in MEMORY_MB {
                    // A scenario's memory floor is language- AND variant-aware
                    // (e.g. batch's payload OOMs the JVM below 512; SnapStart
                    // smithyfull times out below 256). Tiers below the floor are
                    // never built, deployed, or run. The check is per-variant
                    // because the floor differs between plain and SnapStart Java.
                    let below_floor =
                        |snapstart: bool| memory_mb < scenario.min_memory_mb(lang, snapstart);
                    match lang {
                        Lang::Rust => {
                            if below_floor(false) {
                                continue;
                            }
                            for opt in Opt::ALL {
                                cells.push(Cell {
                                    lang,
                                    scenario,
                                    arch,
                                    memory_mb,
                                    opt: Some(opt),
                                    snapstart: false,
                                    jitter: Some(Jitter::Off),
                                });
                            }
                            // Diagnostic jitter=On A/B: emit only for the two
                            // scoped scenarios, o3 only, full arch+memory sweep.
                            // The cells are scoped narrowly so the matrix grows
                            // by ~24 functions, not ~hundreds (see
                            // `JITTER_AB_SCENARIOS`).
                            if JITTER_AB_SCENARIOS.contains(&scenario) {
                                cells.push(Cell {
                                    lang,
                                    scenario,
                                    arch,
                                    memory_mb,
                                    opt: Some(Opt::O3),
                                    snapstart: false,
                                    jitter: Some(Jitter::On),
                                });
                            }
                        }
                        Lang::Node => {
                            if below_floor(false) {
                                continue;
                            }
                            cells.push(Cell {
                                lang,
                                scenario,
                                arch,
                                memory_mb,
                                opt: None,
                                snapstart: false,
                                jitter: None,
                            });
                        }
                        Lang::Java => {
                            // Java runs twice per cell: plain and SnapStart, both
                            // over the full memory sweep (SnapStart is a full peer
                            // series, not a reduced sub-sweep). Each variant is
                            // gated by its own floor, so e.g. SnapStart smithyfull
                            // drops 128 MB while plain smithyfull keeps it.
                            for snapstart in [false, true] {
                                if below_floor(snapstart) {
                                    continue;
                                }
                                cells.push(Cell {
                                    lang,
                                    scenario,
                                    arch,
                                    memory_mb,
                                    opt: None,
                                    snapstart,
                                    jitter: None,
                                });
                            }
                        }
                        Lang::Python | Lang::Go => {
                            // Python and Go are each a single plain runtime per
                            // cell (no opt-level dimension, no SnapStart), like
                            // Node. Go's per-arch native binary is handled by
                            // `artifact_key`, not by a separate cell here.
                            if below_floor(false) {
                                continue;
                            }
                            cells.push(Cell {
                                lang,
                                scenario,
                                arch,
                                memory_mb,
                                opt: None,
                                snapstart: false,
                                jitter: None,
                            });
                        }
                    }
                }
            }
        }
    }

    for c in &cells {
        c.assert_valid();
    }
    cells
}

/// Every Lambda function name the project can create: the full matrix plus the
/// synthetic download-scaling probe families at every `SYNTH_DEFAULT_SIZES_MB`
/// size. This is the exact set teardown deletes (never a prefix sweep of a live
/// account listing), so it can only ever touch names the project defines. The
/// probe reads the same size const, so no run can create a name this omits.
pub fn all_managed_function_names() -> Vec<String> {
    let mut names: Vec<String> = all_cells().iter().map(Cell::function_name).collect();
    for family in SYNTH_ZIP_FAMILIES.iter().chain(SYNTH_IMAGE_FAMILIES) {
        for &mb in SYNTH_DEFAULT_SIZES_MB {
            names.push(synth_function_name(family, mb));
        }
    }
    // The matrix and synthetic name spaces are disjoint today
    // (`<lang>-<scenario>-...` vs `synthdl-...`); dedup is cheap insurance against
    // a future overlap so teardown never issues a redundant delete.
    names.sort_unstable();
    names.dedup();
    names
}

/// The CloudWatch log-group name for every managed function: each
/// `all_managed_function_names` entry under `LAMBDA_LOG_GROUP_PREFIX`. Teardown
/// deletes these by exact name rather than sweeping `/aws/lambda/lambdabench-*`.
pub fn all_managed_log_group_names() -> Vec<String> {
    all_managed_function_names()
        .into_iter()
        .map(|name| format!("{LAMBDA_LOG_GROUP_PREFIX}{name}"))
        .collect()
}

/// The benchmark matrix restricted to the given languages. An empty slice means
/// "no restriction" (the full matrix), so callers can pass the CLI value
/// through unconditionally. Used to scope build, deploy, and run to a subset of
/// languages (e.g. only rust + node when a third language is also defined).
pub fn cells_for_langs(langs: &[Lang]) -> Vec<Cell> {
    all_cells()
        .into_iter()
        .filter(|c| langs.is_empty() || langs.contains(&c.lang))
        .collect()
}

/// The set of unique artifacts that need building for the given cells.
pub fn unique_artifacts(cells: &[Cell]) -> Vec<ArtifactKey> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cell in cells {
        let key = cell.artifact_key();
        if seen.insert(key) {
            out.push(key);
        }
    }
    out
}

/// Number of functions benchmarked concurrently (serial within a function,
/// independent across). Pool size governs only invoke (data-plane) concurrency,
/// which is exempt from the 15 req/s control-plane quota; control-plane pressure
/// is bounded separately by `aws::CONTROL_PLANE_TPS`, not the pool. 32 is the
/// validated speed/safety sweet spot (see RUNBOOK.md); if you see control-plane
/// throttling, lower `CONTROL_PLANE_TPS` rather than the pool.
pub const DEFAULT_POOL: usize = 32;

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire for README.md's Matrix function-count bullet ("= 674 Lambda
    /// functions: Rust 228 ..., Node 102, Java ... plain 96 and SnapStart 92,
    /// Python 78, and Go 78", plus the 24 jitter=On diagnostic cells). Any
    /// config change that moves the matrix (a scenario, tier, floor, or A/B
    /// cell) fails here until the README numbers are updated to match.
    #[test]
    fn all_cells_count_matches_readme() {
        let cells = all_cells();
        let by_lang: std::collections::BTreeMap<&str, usize> =
            cells
                .iter()
                .fold(std::collections::BTreeMap::new(), |mut acc, c| {
                    *acc.entry(c.lang.as_str()).or_insert(0) += 1;
                    acc
                });
        let snap = cells.iter().filter(|c| c.snapstart).count();
        let plain_java = cells
            .iter()
            .filter(|c| c.lang == Lang::Java && !c.snapstart)
            .count();
        let jitter_on = cells
            .iter()
            .filter(|c| matches!(c.jitter, Some(Jitter::On)))
            .count();
        let msg = "matrix size changed; update README.md's Matrix function-count bullet";
        assert_eq!(cells.len(), 674, "{msg} (total, found {})", cells.len());
        assert_eq!(by_lang.get("rust"), Some(&228), "{msg} (rust)");
        assert_eq!(by_lang.get("node"), Some(&102), "{msg} (node)");
        assert_eq!(plain_java, 96, "{msg} (plain java)");
        assert_eq!(snap, 92, "{msg} (java snapstart)");
        assert_eq!(by_lang.get("python"), Some(&78), "{msg} (python)");
        assert_eq!(by_lang.get("go"), Some(&78), "{msg} (go)");
        assert_eq!(jitter_on, 24, "{msg} (rust jitter=On diagnostic cells)");
    }

    /// `Scenario::parse` round-trips every variant's `as_str`, trims whitespace,
    /// and rejects unknown names. This guards the `--only` parser, which relies on
    /// it so a newly added scenario is accepted without a second mapping.
    #[test]
    fn scenario_parse_round_trips_all_and_rejects_unknown() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::parse(s.as_str()), Ok(s));
            assert_eq!(Scenario::parse(&format!("  {}  ", s.as_str())), Ok(s));
        }
        assert!(Scenario::parse("nope").is_err());
    }

    /// `Arch::parse` round-trips every variant's `as_str`, trims whitespace, and
    /// rejects unknown names. Guards the `--arch` filter, which validates its
    /// value through this so a typo (e.g. `x86`, `amd64`) fails loud rather than
    /// silently selecting zero cells.
    #[test]
    fn arch_parse_round_trips_all_and_rejects_unknown() {
        for a in Arch::ALL {
            assert_eq!(Arch::parse(a.as_str()), Ok(a));
            assert_eq!(Arch::parse(&format!("  {}  ", a.as_str())), Ok(a));
        }
        assert!(Arch::parse("x86").is_err());
        assert!(Arch::parse("amd64").is_err());
    }

    /// Python never hosts the two Smithy scenarios (smithy-python has no server
    /// SDK), so no `python` cell may exist for them, and every other scenario
    /// must be present.
    #[test]
    fn python_skips_smithy_scenarios_only() {
        let py: Vec<Scenario> = all_cells()
            .into_iter()
            .filter(|c| c.lang == Lang::Python)
            .map(|c| c.scenario)
            .collect();
        assert!(!py.contains(&Scenario::Smithy));
        assert!(!py.contains(&Scenario::SmithyFull));
        for s in [
            Scenario::Hello,
            Scenario::OneClient,
            Scenario::ThreeClient,
            Scenario::LetterCount,
            Scenario::Authz,
            Scenario::Batch,
        ] {
            assert!(py.contains(&s), "python should host {s:?}");
        }
    }

    /// Go, like Python, never hosts the two Smithy scenarios (smithy-go's server
    /// codegen is a WIP, unpublished, `awsJson1_0`-only generator with no Lambda
    /// adapter, no fair counterpart to the restJson1 servers the other languages
    /// host), so no `go` cell may exist for them, and every other scenario must
    /// be present.
    #[test]
    fn go_skips_smithy_scenarios_only() {
        let go: Vec<Scenario> = all_cells()
            .into_iter()
            .filter(|c| c.lang == Lang::Go)
            .map(|c| c.scenario)
            .collect();
        assert!(!go.contains(&Scenario::Smithy));
        assert!(!go.contains(&Scenario::SmithyFull));
        for s in [
            Scenario::Hello,
            Scenario::OneClient,
            Scenario::ThreeClient,
            Scenario::LetterCount,
            Scenario::Authz,
            Scenario::Batch,
        ] {
            assert!(go.contains(&s), "go should host {s:?}");
        }
    }

    /// Go runs on the OS-only `provided.al2023` runtime (it compiles to a native
    /// executable, like Rust) and uses the custom-runtime `bootstrap` handler.
    #[test]
    fn go_uses_provided_runtime_and_bootstrap_handler() {
        assert_eq!(Lang::Go.runtime(), "provided.al2023");
        let cell = Cell {
            lang: Lang::Go,
            scenario: Scenario::Hello,
            arch: Arch::Arm64,
            memory_mb: 128,
            opt: None,
            snapstart: false,
            jitter: None,
        };
        assert_eq!(cell.handler(), "bootstrap");
    }

    /// Every Go scenario is arch-specific (a native per-arch binary): the two
    /// arches yield distinct artifact keys and arch-tagged labels, and Go cells
    /// carry no opt-level or SnapStart dimension (like Node/Python).
    #[test]
    fn go_artifacts_are_per_arch_with_no_opt_or_snapstart() {
        let arm = Cell {
            lang: Lang::Go,
            scenario: Scenario::Hello,
            arch: Arch::Arm64,
            memory_mb: 128,
            opt: None,
            snapstart: false,
            jitter: None,
        };
        let x86 = Cell {
            arch: Arch::X86_64,
            ..arm
        };
        assert_ne!(arm.artifact_key(), x86.artifact_key());
        assert_eq!(arm.artifact_key().label(), "go-hello-arm64");
        assert_eq!(x86.artifact_key().label(), "go-hello-x86_64");
        for c in all_cells().into_iter().filter(|c| c.lang == Lang::Go) {
            assert!(c.opt.is_none(), "go cells carry no opt-level");
            assert!(!c.snapstart, "go cells are never SnapStart");
            assert!(c.jitter.is_none(), "go cells carry no jitter dimension");
        }
    }

    /// Only `authz` is arch-specific for Python (it carries a native crypto
    /// wheel): its artifact key keeps the arch and its label encodes it, while
    /// every other Python scenario collapses to one arch-independent artifact
    /// shared by both arches.
    #[test]
    fn python_authz_is_arch_specific_others_are_not() {
        let arm = Cell {
            lang: Lang::Python,
            scenario: Scenario::Authz,
            arch: Arch::Arm64,
            memory_mb: 512,
            opt: None,
            snapstart: false,
            jitter: None,
        };
        let x86 = Cell {
            arch: Arch::X86_64,
            ..arm
        };
        assert_ne!(arm.artifact_key(), x86.artifact_key());
        assert_eq!(arm.artifact_key().label(), "python-authz-arm64");
        assert_eq!(x86.artifact_key().label(), "python-authz-x86_64");

        let hello_arm = Cell {
            scenario: Scenario::Hello,
            ..arm
        };
        let hello_x86 = Cell {
            scenario: Scenario::Hello,
            ..x86
        };
        assert_eq!(hello_arm.artifact_key(), hello_x86.artifact_key());
        assert_eq!(hello_arm.artifact_key().label(), "python-hello");
    }

    /// Python carries no opt-level or SnapStart dimension (like Node): every
    /// Python cell is a single plain runtime.
    #[test]
    fn python_cells_have_no_opt_or_snapstart() {
        for c in all_cells().into_iter().filter(|c| c.lang == Lang::Python) {
            assert!(c.opt.is_none(), "python cells carry no opt-level");
            assert!(!c.snapstart, "python cells are never SnapStart");
            assert!(c.jitter.is_none(), "python cells carry no jitter dimension");
        }
    }

    /// The diagnostic jitter=On A/B is generated only for `oneclient` and
    /// `lettercount`, only on Rust, only at o3, and never SnapStart. Every other
    /// Rust cell carries `Some(Jitter::Off)`; non-Rust cells carry `None`. Only
    /// jitter=On adds a `-jitter` segment to the function name / artifact label,
    /// so a jitter=Off Rust cell is indistinguishable from a non-A/B Rust cell.
    #[test]
    fn jitter_on_variant_is_scoped_and_named() {
        let cells = all_cells();
        let jitter_on: Vec<&Cell> = cells
            .iter()
            .filter(|c| matches!(c.jitter, Some(Jitter::On)))
            .collect();
        assert!(!jitter_on.is_empty(), "expected diagnostic jitter=On cells");
        for c in &jitter_on {
            assert_eq!(c.lang, Lang::Rust, "jitter=On is Rust-only");
            assert!(
                JITTER_AB_SCENARIOS.contains(&c.scenario),
                "jitter=On scenario {:?} is not in JITTER_AB_SCENARIOS",
                c.scenario
            );
            assert_eq!(c.opt, Some(Opt::O3), "jitter=On uses o3 only");
            assert!(!c.snapstart, "jitter=On is never SnapStart");
            assert!(
                c.function_name().contains("-jitter-"),
                "jitter=On function name must include the `-jitter` segment: {}",
                c.function_name()
            );
            assert!(
                c.artifact_key().label().ends_with("-jitter"),
                "jitter=On artifact label must end with `-jitter`: {}",
                c.artifact_key().label()
            );
        }
        // Every Rust cell carries Some(jitter); every non-Rust cell carries
        // None. jitter=Off must NOT add a tag to names/labels, so the headline
        // matrix is named the same way regardless of whether the diagnostic A/B
        // is in scope (otherwise jitter=Off + jitter=On would split into two
        // chart cells under the same `series|scenario|memory` key without the
        // site noticing).
        for c in &cells {
            match c.lang {
                Lang::Rust => assert!(c.jitter.is_some(), "rust cells must carry jitter"),
                _ => assert!(c.jitter.is_none(), "non-rust cells carry no jitter"),
            }
            if matches!(c.jitter, Some(Jitter::Off)) {
                let name = c.function_name();
                assert!(
                    !name.split('-').any(|seg| seg == "jitter"),
                    "jitter=Off cells must not carry the `jitter` segment: {name}"
                );
                let label = c.artifact_key().label();
                assert!(
                    !label.split('-').any(|seg| seg == "jitter"),
                    "jitter=Off labels must not carry the `jitter` segment: {label}"
                );
            }
        }
    }

    /// Every cell in `all_cells()` deploys to a distinct Lambda function, so
    /// `function_name()` must be unique across the full matrix, not just
    /// within a (lang, scenario) group. This catches future tagging
    /// regressions: any new dimension that fails to extend the function name
    /// (a SnapStart-style or jitter-style suffix) would silently collide and
    /// the second cell's deploy would overwrite the first's, indistinguishable
    /// in the recorded results.
    #[test]
    fn function_names_are_unique_across_full_matrix() {
        let cells = all_cells();
        let mut seen: std::collections::HashMap<String, &Cell> = std::collections::HashMap::new();
        for c in &cells {
            let name = c.function_name();
            if let Some(prev) = seen.insert(name.clone(), c) {
                panic!(
                    "function name collision on {name}: {prev:?} vs {c:?}, a new \
                     matrix dimension is missing its name segment"
                );
            }
        }
        assert_eq!(seen.len(), cells.len());
    }

    /// `all_managed_function_names` is the exact set teardown deletes. It must
    /// cover every matrix function and every synthetic probe default-size family,
    /// and carry no duplicates.
    #[test]
    fn managed_function_names_cover_matrix_and_synthetic() {
        let managed: std::collections::HashSet<String> =
            all_managed_function_names().into_iter().collect();

        // Every matrix function is present.
        for c in all_cells() {
            assert!(
                managed.contains(&c.function_name()),
                "managed set missing matrix function {}",
                c.function_name()
            );
        }

        // Every synthetic default-size family (zip + image) is present.
        for family in SYNTH_ZIP_FAMILIES.iter().chain(SYNTH_IMAGE_FAMILIES) {
            for &mb in SYNTH_DEFAULT_SIZES_MB {
                let name = synth_function_name(family, mb);
                assert!(
                    managed.contains(&name),
                    "managed set missing synthetic {name}"
                );
            }
        }

        let expected = all_cells().len()
            + (SYNTH_ZIP_FAMILIES.len() + SYNTH_IMAGE_FAMILIES.len())
                * SYNTH_DEFAULT_SIZES_MB.len();
        assert_eq!(
            managed.len(),
            expected,
            "managed set size {} != matrix + synthetic {expected} (unexpected overlap or gap)",
            managed.len()
        );
        // Vec is deduped: unique count equals returned length.
        assert_eq!(all_managed_function_names().len(), managed.len());
    }

    /// Every managed function name carries `RESOURCE_PREFIX`, so the CDK's
    /// `lambdabench-*` IAM wildcard remains a correct upper bound on what teardown
    /// (and the runner generally) can address. Parity with the ECR repo test.
    #[test]
    fn managed_function_names_carry_resource_prefix() {
        for name in all_managed_function_names() {
            assert!(
                name.starts_with(RESOURCE_PREFIX),
                "managed function name {name} does not start with {RESOURCE_PREFIX}"
            );
        }
    }

    /// Log-group names are exactly the function names under `/aws/lambda/`, and
    /// one-to-one with them.
    #[test]
    fn managed_log_group_names_map_from_functions() {
        let fns = all_managed_function_names();
        let groups = all_managed_log_group_names();
        assert_eq!(fns.len(), groups.len());
        for (f, g) in fns.iter().zip(&groups) {
            assert_eq!(g, &format!("{LAMBDA_LOG_GROUP_PREFIX}{f}"));
        }
    }

    /// Jitter A/B cells (Rust + o3 + `JITTER_AB_SCENARIOS`) bypass the
    /// scenario-level lettercount override (5 × 1500) under the Full profile. The
    /// cliff/bump P50 comparison needs matched cold-sample density between
    /// `oneclient` and `lettercount`, and the lettercount override would leave the
    /// `lettercount` panel at n=5 cold samples while `oneclient` ran with the
    /// light count, too lopsided to claim "flat bump" with. Non-jitter Rust
    /// lettercount cells, and the Oz lettercount cells, keep the override.
    #[test]
    fn jitter_ab_cells_use_light_iteration_counts_under_full() {
        let make = |scenario, opt, jitter| Cell {
            lang: Lang::Rust,
            scenario,
            arch: Arch::Arm64,
            memory_mb: 512,
            opt: Some(opt),
            snapstart: false,
            jitter: Some(jitter),
        };
        // Both halves of the jitter A/B fall through to the light counts.
        let on = make(Scenario::LetterCount, Opt::O3, Jitter::On);
        let off = make(Scenario::LetterCount, Opt::O3, Jitter::Off);
        assert!(on.is_jitter_ab());
        assert!(off.is_jitter_ab());
        assert_eq!(on.iterations(Profile::Full), FULL_LIGHT_COUNTS);
        assert_eq!(off.iterations(Profile::Full), FULL_LIGHT_COUNTS);
        // OneClient is also part of the A/B and is a light scenario anyway, so it
        // uses the light counts under Full.
        assert_eq!(
            make(Scenario::OneClient, Opt::O3, Jitter::Off).iterations(Profile::Full),
            FULL_LIGHT_COUNTS
        );
        // The Oz lettercount cell is NOT in the A/B (jitter is Rust o3 only),
        // so it still picks up the lettercount override (5 cold × 1500 warm).
        let oz_letter = make(Scenario::LetterCount, Opt::Oz, Jitter::Off);
        assert!(!oz_letter.is_jitter_ab());
        assert_eq!(oz_letter.iterations(Profile::Full), (5, 1500));
        // A non-jitter scenario at o3 (e.g. authz) keeps its own override.
        let authz = make(Scenario::Authz, Opt::O3, Jitter::Off);
        assert!(!authz.is_jitter_ab());
        assert_eq!(authz.iterations(Profile::Full), (5, 1500));
    }

    /// The CPU-probe counts are thinned on the slowest runtime (Python) at the
    /// starved low tiers (≤256 MB), but unchanged for compiled runtimes and for
    /// Python at the larger tiers. All memory tiers stay in the matrix (we thin
    /// samples, not tiers).
    #[test]
    fn cpu_probe_counts_thin_only_starved_python() {
        // Python at starved tiers: reduced.
        assert_eq!(
            Scenario::LetterCount.full_base_counts(Lang::Python, 128),
            (5, 300)
        );
        assert_eq!(
            Scenario::Batch.full_base_counts(Lang::Python, 256),
            (5, 200)
        );
        // Python above the starved tiers: full counts.
        assert_eq!(
            Scenario::LetterCount.full_base_counts(Lang::Python, 512),
            (5, 1500)
        );
        assert_eq!(
            Scenario::Batch.full_base_counts(Lang::Python, 512),
            (15, 200)
        );
        // Compiled runtimes are never thinned, even at the smallest tier.
        assert_eq!(
            Scenario::LetterCount.full_base_counts(Lang::Rust, 128),
            (5, 1500)
        );
        assert_eq!(Scenario::Batch.full_base_counts(Lang::Java, 512), (15, 200));
        // Light scenarios use the light counts, regardless of lang/memory.
        assert_eq!(
            Scenario::Hello.full_base_counts(Lang::Python, 128),
            FULL_LIGHT_COUNTS
        );
    }

    /// The SnapStart cold-cycle clamp applies under any profile and only ever
    /// reduces the cold count, keeping the resolved warm count.
    #[test]
    fn snapstart_clamps_cold_cycles_keeping_warm() {
        let snap = Cell {
            lang: Lang::Java,
            scenario: Scenario::Hello,
            arch: Arch::Arm64,
            memory_mb: 512,
            opt: None,
            snapstart: true,
            jitter: None,
        };
        // Full light cold count (50) is clamped to SNAPSTART_COLD_CYCLES (10);
        // warm is unchanged.
        let (cold, warm) = snap.iterations(Profile::Full);
        assert_eq!(cold, SNAPSTART_COLD_CYCLES);
        assert_eq!(warm, FULL_LIGHT_COUNTS.1);
        // Smoke's tiny cold count is already below the clamp, so it is unchanged.
        assert_eq!(snap.iterations(Profile::Smoke), SMOKE_COUNTS);
    }
}
