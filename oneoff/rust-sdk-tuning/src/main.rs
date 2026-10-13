// One-off probe: the AWS SDK for Rust cold-start levers on the tuning page.
//
// One binary per cargo feature set (see Cargo.toml), and within a binary every
// runtime lever is an environment variable, so one build covers many variants:
//
//   PROBE_FLOW      one   = DynamoDB GetItem (the matrix's `oneclient` work)
//                   three = KMS Encrypt + DynamoDB PutItem + S3 PutObject
//                           (the matrix's `smithyfull` work, minus the server SDK)
//   PROBE_REGION    chain | explicit   region from the provider chain, or set
//   PROBE_CREDS     chain | env        default credentials chain, or env-only
//   PROBE_RT        multi | current    tokio runtime flavor
//   PROBE_PREWARM   none | local | network
//       local:   one request per client at init against a closed localhost port,
//                which builds the HTTPS connector and runs the request path
//                without any network I/O or IAM permission
//       network: one real request per client at init, concurrently, which also
//                opens each service's connection
//   PROBE_CHECKSUM  default | required  S3 request checksum calculation
//   PROBE_PREFAULT  none | read | thread | fadvise
//       Pull the executable into the page cache at process start, so code
//       touched later maps cached pages instead of faulting them in from
//       Lambda's code storage. read: synchronously; thread: on a detached
//       background thread, overlapping the rest of init; fadvise:
//       posix_fadvise(WILLNEED), asynchronous kernel readahead
//
// The invocation returns the in-process timings as JSON, plus process CPU
// time and page faults for Init (since process start) and the request; Init Duration and
// Duration come from the REPORT line.

use std::env;
use std::time::Instant;

use aws_config::BehaviorVersion;
use aws_config::retry::RetryConfig;
use aws_sdk_dynamodb::types::AttributeValue;
use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Size of the `pad` feature's never-read data: about the default-vs-slim
/// difference in .text + .rodata.
#[cfg(feature = "pad")]
const PAD_LEN: usize = 2 << 20;

/// xorshift64 output, so the data does not compress away in the zip.
#[cfg(feature = "pad")]
#[allow(long_running_const_eval)]
static PAD: [u8; PAD_LEN] = {
    let mut out = [0u8; PAD_LEN];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut i = 0;
    while i < PAD_LEN {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out[i] = x as u8;
        i += 1;
    }
    out
};

/// Closed port on localhost: the connection is refused immediately.
const DEAD_ENDPOINT: &str = "https://127.0.0.1:9";

fn var(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("{name} not set"))
}

fn opt(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

/// Process-wide (CPU ms, major faults, minor faults) since process start:
/// user + kernel CPU of all threads, and getrusage's fault counts.
fn usage() -> (f64, i64, i64) {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: both calls only write into the structs we pass.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts);
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
    }
    let cpu = ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6;
    (cpu, ru.ru_majflt, ru.ru_minflt)
}

/// KB of the executable's own mappings resident in memory (summed `Rss:` of
/// every /proc/self/smaps entry backed by /proc/self/exe's path): how much of
/// the binary has been paged in so far.
fn exe_rss_kb() -> u64 {
    let Ok(exe) = std::fs::read_link("/proc/self/exe") else {
        return 0;
    };
    let exe = exe.to_string_lossy().into_owned();
    let Ok(smaps) = std::fs::read_to_string("/proc/self/smaps") else {
        return 0;
    };
    let mut in_exe = false;
    let mut kb = 0;
    for line in smaps.lines() {
        // Mapping header lines start with an address range; the rest are fields.
        if line
            .split_whitespace()
            .next()
            .is_some_and(|f| f.contains('-'))
        {
            in_exe = line.ends_with(&exe);
        } else if in_exe && let Some(v) = line.strip_prefix("Rss:") {
            kb += v
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .unwrap_or(0);
        }
    }
    kb
}

struct State {
    ddb: aws_sdk_dynamodb::Client,
    kms: Option<aws_sdk_kms::Client>,
    s3: Option<aws_sdk_s3::Client>,
    table: String,
    key: String,
    kms_key_id: String,
    bucket: String,
    order_pk: String,
    receipt_key: String,
}

impl State {
    async fn get_item(&self, endpoint: Option<&str>) -> Result<(), Error> {
        let mut req = self
            .ddb
            .get_item()
            .table_name(&self.table)
            .key("pk", AttributeValue::S(self.key.clone()))
            .customize();
        if let Some(url) = endpoint {
            req = req.config_override(aws_sdk_dynamodb::Config::builder().endpoint_url(url));
        }
        let out = req.send().await?;
        out.item().ok_or("seeded item not found")?;
        Ok(())
    }

    async fn encrypt(&self, endpoint: Option<&str>) -> Result<Vec<u8>, Error> {
        let mut req = self
            .kms
            .as_ref()
            .expect("three flow")
            .encrypt()
            .key_id(&self.kms_key_id)
            .plaintext(aws_sdk_kms::primitives::Blob::new(b"order:drip".to_vec()))
            .customize();
        if let Some(url) = endpoint {
            req = req.config_override(aws_sdk_kms::Config::builder().endpoint_url(url));
        }
        let out = req.send().await?;
        Ok(out
            .ciphertext_blob()
            .map(|b| b.as_ref().to_vec())
            .unwrap_or_default())
    }

    async fn head_receipt(&self, endpoint: Option<&str>) -> Result<(), Error> {
        let mut req = self
            .s3
            .as_ref()
            .expect("three flow")
            .head_object()
            .bucket(&self.bucket)
            .key(&self.receipt_key)
            .customize();
        if let Some(url) = endpoint {
            req = req.config_override(
                aws_sdk_s3::Config::builder()
                    .endpoint_url(url)
                    .force_path_style(true),
            );
        }
        // A missing receipt (404) still opens and warms the connection.
        let _ = req.send().await;
        Ok(())
    }

    /// The handler's work for PROBE_FLOW.
    async fn work(&self) -> Result<(), Error> {
        if self.kms.is_none() {
            return self.get_item(None).await;
        }
        let signature = self.encrypt(None).await?;
        self.ddb
            .put_item()
            .table_name(&self.table)
            .item("pk", AttributeValue::S(self.order_pk.clone()))
            .item("coffeeType", AttributeValue::S("DRIP".to_string()))
            .item("status", AttributeValue::S("IN_PROGRESS".to_string()))
            .item(
                "signature",
                AttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new(signature)),
            )
            .send()
            .await?;
        self.s3
            .as_ref()
            .expect("three flow")
            .put_object()
            .bucket(&self.bucket)
            .key(&self.receipt_key)
            .body(aws_sdk_s3::primitives::ByteStream::from(
                b"receipt for DRIP order".to_vec(),
            ))
            .send()
            .await?;
        Ok(())
    }

    /// One request per client at init. `local` requests fail at connect by
    /// design; `network` requests are real and run concurrently.
    async fn prewarm(&self, mode: &str) {
        let endpoint = match mode {
            "none" => return,
            "local" => Some(DEAD_ENDPOINT),
            "network" => None,
            other => panic!("unknown PROBE_PREWARM {other}"),
        };
        let three = self.kms.is_some();
        let (ddb, kms, s3) = tokio::join!(
            self.get_item(endpoint),
            async {
                if three {
                    self.encrypt(endpoint).await.map(drop)
                } else {
                    Ok(())
                }
            },
            async {
                if three {
                    self.head_receipt(endpoint).await
                } else {
                    Ok(())
                }
            },
        );
        if endpoint.is_none() {
            ddb.and(kms).and(s3).expect("network prewarm failed");
        }
    }
}

async fn run(t0: Instant, prefault_ms: f64) -> Result<(), Error> {
    let three = match opt("PROBE_FLOW", "one").as_str() {
        "one" => false,
        "three" => true,
        other => panic!("unknown PROBE_FLOW {other}"),
    };

    let t = Instant::now();
    let mut loader =
        aws_config::defaults(BehaviorVersion::latest()).retry_config(RetryConfig::disabled());
    if opt("PROBE_REGION", "chain") == "explicit" {
        loader = loader.region(aws_config::Region::new(var("AWS_REGION")));
    }
    if opt("PROBE_CREDS", "chain") == "env" {
        loader = loader.credentials_provider(
            aws_config::environment::EnvironmentVariableCredentialsProvider::new(),
        );
    }
    if opt("PROBE_CHECKSUM", "default") == "required" {
        loader = loader.request_checksum_calculation(
            aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
        );
    }
    #[cfg(feature = "ring")]
    {
        use aws_smithy_http_client::tls;
        loader = loader.http_client(
            aws_smithy_http_client::Builder::new()
                .tls_provider(tls::Provider::Rustls(
                    tls::rustls_provider::CryptoMode::Ring,
                ))
                .build_https(),
        );
    }
    let config = loader.load().await;
    let load_ms = ms(t);

    let t = Instant::now();
    let state = State {
        ddb: aws_sdk_dynamodb::Client::new(&config),
        kms: three.then(|| aws_sdk_kms::Client::new(&config)),
        s3: three.then(|| aws_sdk_s3::Client::new(&config)),
        table: var("PROBE_TABLE"),
        key: var("PROBE_KEY"),
        kms_key_id: var("PROBE_KMS_KEY_ID"),
        bucket: var("PROBE_BUCKET"),
        order_pk: var("PROBE_ORDER_PK"),
        receipt_key: var("PROBE_RECEIPT_KEY"),
    };
    let clients_ms = ms(t);

    let t = Instant::now();
    state.prewarm(&opt("PROBE_PREWARM", "none")).await;
    let prewarm_ms = ms(t);
    let init_ms = ms(t0);
    let (init_cpu, init_majflt, init_minflt) = usage();
    let init_exe_rss_kb = exe_rss_kb();

    let state = &state;
    let mut n = 0u32;
    lambda_runtime::run(service_fn(move |_: LambdaEvent<Value>| {
        n += 1;
        let invocation = n;
        async move {
            let t = Instant::now();
            let (cpu0, maj0, min0) = usage();
            state.work().await?;
            let (cpu1, maj1, min1) = usage();
            let exe_rss = exe_rss_kb();
            Ok::<Value, Error>(json!({
                "n": invocation,
                "handler_ms": ms(t),
                "handler_cpu_ms": cpu1 - cpu0,
                "handler_majflt": maj1 - maj0,
                "handler_minflt": min1 - min0,
                "exe_rss_kb": exe_rss,
                "init_exe_rss_kb": init_exe_rss_kb,
                "init_cpu_ms": init_cpu,
                "init_majflt": init_majflt,
                "init_minflt": init_minflt,
                "load_ms": load_ms,
                "clients_ms": clients_ms,
                "prewarm_ms": prewarm_ms,
                "init_ms": init_ms,
                "prefault_ms": prefault_ms,
            }))
        }
    }))
    .await
}

/// Reads the executable once, discarding the bytes; returns the elapsed ms.
fn read_exe() -> f64 {
    let t = Instant::now();
    let mut exe = std::fs::File::open("/proc/self/exe").expect("open /proc/self/exe");
    std::io::copy(&mut exe, &mut std::io::sink()).expect("read /proc/self/exe");
    ms(t)
}

fn prefault(mode: &str) -> f64 {
    let t = Instant::now();
    match mode {
        "none" => {}
        "read" => {
            read_exe();
        }
        "thread" => {
            std::thread::spawn(read_exe);
        }
        "fadvise" => {
            use std::os::fd::AsRawFd;
            let exe = std::fs::File::open("/proc/self/exe").expect("open /proc/self/exe");
            // SAFETY: plain syscall on a valid, open file descriptor.
            let rc =
                unsafe { libc::posix_fadvise(exe.as_raw_fd(), 0, 0, libc::POSIX_FADV_WILLNEED) };
            assert_eq!(rc, 0, "posix_fadvise failed");
        }
        other => panic!("unknown PROBE_PREFAULT {other}"),
    }
    ms(t)
}

fn main() -> Result<(), Error> {
    let t0 = Instant::now();
    let prefault_ms = prefault(&opt("PROBE_PREFAULT", "none"));
    // Keeps PAD in the binary; it is only read when PROBE_PAD_TOUCH is set.
    #[cfg(feature = "pad")]
    if env::var("PROBE_PAD_TOUCH").is_ok() {
        let sum: u64 = PAD.iter().map(|&b| u64::from(b)).sum();
        eprintln!("pad sum {}", std::hint::black_box(sum));
    }
    #[cfg(feature = "diag")]
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("PROBE_LOG"))
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_ansi(false)
        .init();
    let runtime = match opt("PROBE_RT", "multi").as_str() {
        "multi" => tokio::runtime::Builder::new_multi_thread(),
        "current" => tokio::runtime::Builder::new_current_thread(),
        other => panic!("unknown PROBE_RT {other}"),
    }
    .enable_all()
    .build()?;
    runtime.block_on(run(t0, prefault_ms))
}
