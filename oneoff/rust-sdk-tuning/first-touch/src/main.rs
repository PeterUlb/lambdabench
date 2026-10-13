// First-touch probe: the first-use cost of each crypto step (aws-lc-rs vs
// ring) and of real rustls handshakes inside a Lambda sandbox, with and
// without the executable pre-read into the page cache. Built by ../build.sh as
// the `first-touch` variant and driven by ../probe.py like the SDK probe.
//
// rsa_pub.der / rsa_sig.bin are a throwaway RSA-2048 public key and its
// PKCS#1 v1.5 SHA-256 signature over msg.bin, for the RSA verify step.
//
// PROBE_STEPS  micro: time each primitive's first call, in order (PROBE_LIB)
//              tls:   two handshakes (dynamodb, then kms) with PROBE_TLS
// PROBE_LIB    awslc | ring
// PROBE_TLS    awslc-pq | awslc-x25519 | ring
// PROBE_PHASE  init | invoke
// PROBE_PREFAULT  read: read /proc/self/exe once before anything else
// PROBE_AWSLC_INIT  1: call aws_lc_rs::init() before the first RNG use
// PROBE_ENV_FULL  1: also report the code image's device size, squashfs
//                 superblock, the kernel's squashfs decompressors and
//                 /proc/meminfo at the start of main() and at the end
// PROBE_READ_FILE  comma-separated paths of files to read once at init, each
//                  measured, with /proc/meminfo after each
// PROBE_REREAD  1: read the PROBE_READ_FILE files a second time (eviction test)
// PROBE_TOUCH  N: at init, read one byte at the start of up to N 128 KB blocks
//              of the executable with no page mapped nearby, each measured
//              on the calling thread
// PROBE_STEPS  dns: time repeated getaddrinfo lookups of AWS endpoints
// PROBE_MADVISE  random | sequential | normal: madvise() every mapping of the
//                executable first thing in main(); random turns off the
//                kernel's read-around on page faults, sequential widens it
// Custom runtime loop (no lambda_runtime), returns the timings as JSON.
//
// Every step reports, as deltas across the step: wall ms, process CPU ms
// (user + kernel, CLOCK_PROCESS_CPUTIME_ID), major and minor page faults
// (getrusage), bytes read from storage (/proc/self/io read_bytes) and cgroup
// CPU throttling ms, the last two only where the sandbox exposes them. Wall
// well above CPU means the step waited, on I/O or on the CPU quota.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

static RSA_PUB: &[u8] = include_bytes!("../rsa_pub.der");
static RSA_SIG: &[u8] = include_bytes!("../rsa_sig.bin");
static MSG: &[u8] = include_bytes!("../msg.bin");

fn clock_ms(clock: libc::clockid_t) -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into the timespec we pass.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6
}

/// (major, minor) page faults of the whole process so far.
fn faults() -> (i64, i64) {
    // SAFETY: getrusage fills the zeroed struct we pass.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    (ru.ru_majflt, ru.ru_minflt)
}

/// Bytes this process caused to be fetched from storage, if /proc/self/io exists.
fn read_bytes() -> Option<f64> {
    let io = std::fs::read_to_string("/proc/self/io").ok()?;
    io.lines()
        .find_map(|l| l.strip_prefix("read_bytes: "))
        .and_then(|v| v.trim().parse().ok())
}

/// Cumulative CPU throttling of this sandbox's cgroup in ms (v2, then v1).
fn throttled_ms() -> Option<f64> {
    if let Ok(stat) = std::fs::read_to_string("/sys/fs/cgroup/cpu.stat")
        && let Some(v) = stat.lines().find_map(|l| l.strip_prefix("throttled_usec "))
    {
        return v.trim().parse::<f64>().ok().map(|us| us / 1e3);
    }
    let stat = std::fs::read_to_string("/sys/fs/cgroup/cpu/cpu.stat").ok()?;
    stat.lines()
        .find_map(|l| l.strip_prefix("throttled_time "))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|ns| ns / 1e6)
}

struct Snap {
    wall: Instant,
    cpu: f64,
    maj: i64,
    min: i64,
    rb: Option<f64>,
    thr: Option<f64>,
}

impl Snap {
    fn now() -> Self {
        let (maj, min) = faults();
        Snap {
            rb: read_bytes(),
            thr: throttled_ms(),
            cpu: clock_ms(libc::CLOCK_PROCESS_CPUTIME_ID),
            wall: Instant::now(),
            maj,
            min,
        }
    }
}

/// Step timer: each `lap()` returns the deltas since the previous lap as JSON.
struct Lap(Snap);

impl Lap {
    fn new() -> Self {
        Lap(Snap::now())
    }

    fn lap(&mut self) -> String {
        let wall = self.0.wall.elapsed().as_secs_f64() * 1e3;
        let now = Snap::now();
        let opt = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => format!("{:.3}", b - a),
            _ => "null".to_string(),
        };
        let out = format!(
            r#"{{"wall":{wall:.3},"cpu":{:.3},"majflt":{},"minflt":{},"read_kb":{},"throttled":{}}}"#,
            now.cpu - self.0.cpu,
            now.maj - self.0.maj,
            now.min - self.0.min,
            opt(self.0.rb.map(|b| b / 1024.0), now.rb.map(|b| b / 1024.0)),
            opt(self.0.thr, now.thr),
        );
        self.0 = Snap::now();
        out
    }
}

/// JSON string with quotes, backslashes and control characters dropped.
fn json_str(v: &str) -> String {
    let clean: String = v
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .collect();
    format!("\"{clean}\"")
}

/// The sandbox facts the fault numbers depend on, read once.
fn environment() -> String {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let task_mount = read("/proc/self/mountinfo")
        .lines()
        .find(|l| l.split_whitespace().nth(4) == Some("/var/task"))
        .unwrap_or("")
        .to_string();
    let cpu_max = match read("/sys/fs/cgroup/cpu.max") {
        v if !v.is_empty() => v,
        _ => format!(
            "{} {}",
            read("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").trim(),
            read("/sys/fs/cgroup/cpu/cpu.cfs_period_us").trim()
        ),
    };
    let full = if std::env::var("PROBE_ENV_FULL").as_deref() == Ok("1") {
        let source = task_mount
            .split(" - ")
            .nth(1)
            .and_then(|s| s.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        let dev = source.trim_start_matches("/dev/");
        let comp_ops: Vec<String> = read("/proc/kallsyms")
            .lines()
            .filter_map(|l| l.split_whitespace().nth(2))
            .filter(|n| n.starts_with("squashfs_") && n.ends_with("_comp_ops"))
            .map(str::to_string)
            .collect();
        format!(
            r#","mem_start":{},"mem_now":{},"io_start_kb":{},"partitions":{},"dev_size_sectors":{},"superblock":{},"kallsyms_comp_ops":{}"#,
            MEM_START.get().map_or("null", String::as_str),
            meminfo(),
            IO_START
                .get()
                .map_or("null".to_string(), |b| format!("{:.0}", b / 1024.0)),
            json_str(&read("/proc/partitions").replace('\n', " | ")),
            json_str(read(&format!("/sys/class/block/{dev}/size")).trim()),
            json_str(&superblock(&source)),
            json_str(&comp_ops.join(" ")),
        )
    } else {
        String::new()
    };
    format!(
        r#"{{"kernel":{},"cpu":{},"task_mount":{},"cpu_max":{},"readahead_kb":{}{full}}}"#,
        json_str(read("/proc/sys/kernel/osrelease").trim()),
        json_str(&cpu_model()),
        json_str(&task_mount),
        json_str(cpu_max.trim()),
        json_str(&read_ahead_kb()),
    )
}

/// The host CPU as /proc/cpuinfo names it (x86 model name, or the arm64 implementer and
/// part number, 0xd0c Neoverse N1 for Graviton2 and 0xd40 Neoverse V1 for Graviton3),
/// plus the vCPU count.
fn cpu_model() -> String {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut seen: Vec<&str> = Vec::new();
    for line in info.lines() {
        let key = line.split(':').next().unwrap_or("").trim();
        if matches!(
            key,
            "model name" | "CPU implementer" | "CPU part" | "CPU variant"
        ) && !seen.contains(&line)
        {
            seen.push(line);
        }
    }
    let n = info.lines().filter(|l| l.starts_with("processor")).count();
    format!("{} | vcpus {n}", seen.join(" | ").replace('\t', ""))
}

/// Selected /proc/meminfo fields in KB, as a JSON object.
fn meminfo() -> String {
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |key: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map_or("null".to_string(), |v| v.to_string())
    };
    format!(
        r#"{{"total":{},"free":{},"available":{},"cached":{},"anon":{}}}"#,
        field("MemTotal"),
        field("MemFree"),
        field("MemAvailable"),
        field("Cached"),
        field("AnonPages"),
    )
}

/// /proc/meminfo at the start of main(), before any PROBE_READ_FILE read.
static MEM_START: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Bytes read from storage since exec, sampled first thing in main().
static IO_START: std::sync::OnceLock<f64> = std::sync::OnceLock::new();

/// Decodes the squashfs superblock of `device`: compression, block size and
/// image size, or why it could not be read.
fn superblock(device: &str) -> String {
    use std::io::Read;
    let mut sb = [0u8; 96];
    match std::fs::File::open(device).and_then(|mut f| f.read_exact(&mut sb)) {
        Err(e) => format!("unreadable {device}: {e}"),
        Ok(()) if sb[..4] != *b"hsqs" => format!("no squashfs magic on {device}"),
        Ok(()) => {
            let block = u32::from_le_bytes(sb[12..16].try_into().unwrap());
            let comp = match u16::from_le_bytes(sb[20..22].try_into().unwrap()) {
                1 => "gzip",
                2 => "lzma",
                3 => "lzo",
                4 => "xz",
                5 => "lz4",
                6 => "zstd",
                _ => "unknown",
            };
            let used = u64::from_le_bytes(sb[40..48].try_into().unwrap());
            format!("compression={comp} block_size={block} bytes_used={used}")
        }
    }
}

/// read_ahead_kb of the block device backing /var/task, if it has one.
fn read_ahead_kb() -> String {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata("/var/task") else {
        return String::new();
    };
    let dev = meta.dev();
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    std::fs::read_to_string(format!(
        "/sys/dev/block/{major}:{minor}/queue/read_ahead_kb"
    ))
    .map(|v| v.trim().to_string())
    .unwrap_or_else(|_| format!("none for {major}:{minor}"))
}

/// madvise() every mapping of the executable with `advice`; returns how many
/// mappings and KB it covered.
fn madvise_exe(advice: libc::c_int) -> (usize, usize) {
    let exe = std::fs::read_link("/proc/self/exe").unwrap_or_default();
    let exe = exe.to_string_lossy();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let (mut n, mut kb) = (0, 0);
    for line in maps.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[5] != exe {
            continue;
        }
        let Some((a, b)) = fields[0].split_once('-') else {
            continue;
        };
        let (Ok(a), Ok(b)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) else {
            continue;
        };
        // SAFETY: the range is one of this process's own mappings; advice
        // only changes the kernel's paging heuristics for it.
        if unsafe { libc::madvise(a as *mut libc::c_void, b - a, advice) } == 0 {
            n += 1;
            kb += (b - a) / 1024;
        }
    }
    (n, kb)
}

/// The madvise call's coverage, when PROBE_MADVISE is set.
static MADVISE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Per-file read metrics for PROBE_READ_FILE (and PROBE_REREAD), as JSON.
static READS: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn read_files(paths: &[&str], pass: &str) -> Vec<String> {
    paths
        .iter()
        .map(|path| {
            let mut t = Lap::new();
            let mut f = std::fs::File::open(path).expect("open PROBE_READ_FILE");
            let n = std::io::copy(&mut f, &mut std::io::sink()).expect("read PROBE_READ_FILE");
            format!(
                r#"{{"pass":"{pass}","path":{},"kb":{},"step":{},"mem":{}}}"#,
                json_str(path),
                n / 1024,
                t.lap(),
                meminfo()
            )
        })
        .collect()
}

/// Per-block touch metrics for PROBE_TOUCH, as JSON.
static TOUCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn thread_read_kb() -> f64 {
    std::fs::read_to_string("/proc/thread-self/io")
        .ok()
        .and_then(|io| {
            io.lines()
                .find_map(|l| l.strip_prefix("read_bytes: "))
                .and_then(|v| v.trim().parse::<f64>().ok())
        })
        .map_or(-1.0, |b| b / 1024.0)
}

fn thread_majflt() -> i64 {
    // SAFETY: getrusage fills the zeroed struct we pass.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) };
    ru.ru_majflt
}

/// Reads one byte at the start of up to `n` file-aligned 128 KB blocks of the
/// executable that have no page mapped in themselves or their neighbours.
fn touch_blocks(n: usize) -> String {
    use std::os::unix::fs::FileExt;
    const BLK: u64 = 128 * 1024;
    let exe = std::fs::read_link("/proc/self/exe").unwrap_or_default();
    let exe = exe.to_string_lossy().to_string();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let Ok(pm) = std::fs::File::open("/proc/self/pagemap") else {
        return r#"{"error":"pagemap"}"#.to_string();
    };
    let present = |va: u64| {
        let mut b = [0u8; 8];
        pm.read_exact_at(&mut b, va / 4096 * 8).is_err() || u64::from_le_bytes(b) >> 63 == 1
    };
    let mut out = Vec::new();
    for line in maps.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 || f[5] != exe {
            continue;
        }
        let Some((a, b)) = f[0].split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(off)) = (
            u64::from_str_radix(a, 16),
            u64::from_str_radix(b, 16),
            u64::from_str_radix(f[2], 16),
        ) else {
            continue;
        };
        let mut fo = off.div_ceil(BLK) * BLK + BLK;
        while fo + 2 * BLK <= off + (end - start) && out.len() < n {
            let va = start + (fo - off);
            if !(va - BLK..va + 2 * BLK).step_by(4096).any(present) {
                let (c0, m0, r0) = (
                    clock_ms(libc::CLOCK_THREAD_CPUTIME_ID),
                    thread_majflt(),
                    thread_read_kb(),
                );
                // SAFETY: va lies inside a readable mapping of our own executable.
                let _ = unsafe { std::ptr::read_volatile(va as *const u8) };
                let (c1, m1, r1) = (
                    clock_ms(libc::CLOCK_THREAD_CPUTIME_ID),
                    thread_majflt(),
                    thread_read_kb(),
                );
                out.push(format!(
                    r#"{{"off_kb":{},"cpu":{:.3},"majflt":{},"read_kb":{:.0}}}"#,
                    fo / 1024,
                    c1 - c0,
                    m1 - m0,
                    r1 - r0
                ));
            }
            fo += 3 * BLK;
        }
    }
    format!("[{}]", out.join(","))
}

/// Times repeated getaddrinfo lookups of AWS endpoints.
fn dns_steps() -> String {
    use std::net::ToSocketAddrs;
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "eu-central-1".into());
    let hosts: Vec<String> = [
        "dynamodb", "dynamodb", "dynamodb", "kms", "kms", "s3", "sts",
    ]
    .iter()
    .map(|svc| format!("{svc}.{region}.amazonaws.com"))
    .collect();
    let mut t = Lap::new();
    let steps: Vec<String> = hosts
        .iter()
        .map(|h| {
            let n = (h.as_str(), 443)
                .to_socket_addrs()
                .map(|a| a.count())
                .unwrap_or(0);
            format!(
                r#"{{"host":{},"addrs":{n},"step":{}}}"#,
                json_str(h),
                t.lap()
            )
        })
        .collect();
    format!(
        r#"{{"resolv":{},"nsswitch_hosts":{},"lookups":[{}]}}"#,
        json_str(
            &std::fs::read_to_string("/etc/resolv.conf")
                .unwrap_or_default()
                .replace('\n', " | ")
        ),
        json_str(
            std::fs::read_to_string("/etc/nsswitch.conf")
                .unwrap_or_default()
                .lines()
                .find(|l| l.starts_with("hosts:"))
                .unwrap_or("")
        ),
        steps.join(",")
    )
}

/// The executable read's own step metrics, when PROBE_PREFAULT=read.
static PREFAULT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn micro_awslc() -> String {
    use aws_lc_rs::{aead, agreement, digest, kem, rand, signature};
    let mut t = Lap::new();
    if std::env::var("PROBE_AWSLC_INIT").as_deref() == Ok("1") {
        aws_lc_rs::init();
    }
    let init_ms = t.lap();
    let mut buf = [0u8; 32];
    rand::fill(&mut buf).unwrap();
    let rand_ms = t.lap();
    rand::fill(&mut buf).unwrap();
    let rand2_ms = t.lap();
    std::thread::spawn(|| rand::fill(&mut [0u8; 32]).unwrap())
        .join()
        .unwrap();
    let rand_thread_ms = t.lap();
    let rng = rand::SystemRandom::new();
    let sk = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap();
    let pk = sk.compute_public_key().unwrap();
    let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, pk.as_ref().to_vec());
    agreement::agree_ephemeral(sk, peer, (), |k| Ok::<_, ()>(k.len())).unwrap();
    let x25519_ms = t.lap();
    let dk = kem::DecapsulationKey::generate(&kem::ML_KEM_768).unwrap();
    let ek = dk.encapsulation_key().unwrap();
    let (ct, _) = ek.encapsulate().unwrap();
    dk.decapsulate(ct).unwrap();
    let mlkem_ms = t.lap();
    signature::UnparsedPublicKey::new(&signature::RSA_PKCS1_2048_8192_SHA256, RSA_PUB)
        .verify(MSG, RSA_SIG)
        .unwrap();
    let rsa_ms = t.lap();
    let key =
        aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &buf[..16]).unwrap());
    let mut data = b"payload".to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key([0; 12]),
        aead::Aad::empty(),
        &mut data,
    )
    .unwrap();
    let aes_ms = t.lap();
    let _ = digest::digest(&digest::SHA256, MSG);
    let sha_ms = t.lap();
    format!(
        r#"{{"init":{init_ms},"rand":{rand_ms},"rand2":{rand2_ms},"rand_thread":{rand_thread_ms},"x25519":{x25519_ms},"mlkem":{mlkem_ms},"rsa":{rsa_ms},"aes":{aes_ms},"sha":{sha_ms}}}"#
    )
}

fn micro_ring() -> String {
    use ring::rand::SecureRandom;
    use ring::{aead, agreement, digest, rand, signature};
    let mut t = Lap::new();
    let rng = rand::SystemRandom::new();
    let mut buf = [0u8; 32];
    rng.fill(&mut buf).unwrap();
    let rand_ms = t.lap();
    let sk = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap();
    let pk = sk.compute_public_key().unwrap();
    let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, pk.as_ref().to_vec());
    agreement::agree_ephemeral(sk, &peer, |k| k.len()).unwrap();
    let x25519_ms = t.lap();
    signature::UnparsedPublicKey::new(&signature::RSA_PKCS1_2048_8192_SHA256, RSA_PUB)
        .verify(MSG, RSA_SIG)
        .unwrap();
    let rsa_ms = t.lap();
    let key =
        aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &buf[..16]).unwrap());
    let mut data = b"payload".to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key([0; 12]),
        aead::Aad::empty(),
        &mut data,
    )
    .unwrap();
    let aes_ms = t.lap();
    let _ = digest::digest(&digest::SHA256, MSG);
    let sha_ms = t.lap();
    format!(
        r#"{{"rand":{rand_ms},"x25519":{x25519_ms},"rsa":{rsa_ms},"aes":{aes_ms},"sha":{sha_ms}}}"#
    )
}

async fn handshake(config: Arc<rustls::ClientConfig>, host: &str) -> String {
    let mut t = Lap::new();
    let addr = tokio::net::lookup_host((host, 443))
        .await
        .unwrap()
        .next()
        .unwrap();
    let dns_ms = t.lap();
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).unwrap();
    let tcp_ms = t.lap();
    let name = rustls::pki_types::ServerName::try_from(host.to_owned()).unwrap();
    let tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, tcp)
        .await
        .unwrap();
    let tls_ms = t.lap();
    let group = tls
        .get_ref()
        .1
        .negotiated_key_exchange_group()
        .map(|g| format!("{:?}", g.name()))
        .unwrap_or_default();
    format!(r#"{{"dns":{dns_ms},"tcp":{tcp_ms},"tls":{tls_ms},"group":"{group}"}}"#)
}

async fn tls(mode: &str) -> String {
    let mut t = Lap::new();
    let provider = match mode {
        "awslc-pq" => rustls::crypto::aws_lc_rs::default_provider(),
        "awslc-x25519" => rustls::crypto::CryptoProvider {
            kx_groups: vec![rustls::crypto::aws_lc_rs::kx_group::X25519],
            ..rustls::crypto::aws_lc_rs::default_provider()
        },
        "ring" => rustls::crypto::ring::default_provider(),
        other => panic!("unknown PROBE_TLS {other}"),
    };
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let config_ms = t.lap();
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "eu-central-1".into());
    let first = handshake(config.clone(), &format!("dynamodb.{region}.amazonaws.com")).await;
    let second = handshake(config, &format!("kms.{region}.amazonaws.com")).await;
    format!(r#"{{"config":{config_ms},"first":{first},"second":{second}}}"#)
}

async fn measure() -> String {
    let mut t = Lap::new();
    let steps = std::env::var("PROBE_STEPS").expect("PROBE_STEPS");
    let out = match steps.as_str() {
        "micro" => match std::env::var("PROBE_LIB").expect("PROBE_LIB").as_str() {
            "awslc" => micro_awslc(),
            "ring" => micro_ring(),
            other => panic!("unknown PROBE_LIB {other}"),
        },
        "tls" => tls(&std::env::var("PROBE_TLS").expect("PROBE_TLS")).await,
        "dns" => dns_steps(),
        other => panic!("unknown PROBE_STEPS {other}"),
    };
    format!(
        r#"{{"total":{},"prefault":{},"madvise":{},"touch":{},"reads":{},"env":{},"steps":{out}}}"#,
        t.lap(),
        PREFAULT.get().map_or("null", String::as_str),
        MADVISE.get().map_or("null", String::as_str),
        TOUCH.get().map_or("null", String::as_str),
        READS.get().map_or("null", String::as_str),
        environment(),
    )
}

/// One blocking HTTP/1.1 request to the Lambda runtime API; returns the raw response.
fn request(api: &str, method: &str, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(api).expect("connect to runtime API");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {api}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[tokio::main]
async fn main() {
    if let Ok(mode) = std::env::var("PROBE_MADVISE") {
        let advice = match mode.as_str() {
            "random" => libc::MADV_RANDOM,
            "sequential" => libc::MADV_SEQUENTIAL,
            "normal" => libc::MADV_NORMAL,
            other => panic!("unknown PROBE_MADVISE {other}"),
        };
        let (n, kb) = madvise_exe(advice);
        let _ = MADVISE.set(format!(r#"{{"mappings":{n},"kb":{kb}}}"#));
    }
    if let Some(b) = read_bytes() {
        let _ = IO_START.set(b);
    }
    let _ = MEM_START.set(meminfo());
    if let Ok(n) = std::env::var("PROBE_TOUCH") {
        let _ = TOUCH.set(touch_blocks(n.parse().expect("PROBE_TOUCH")));
    }
    // PROBE_READ_FILE=a,b: read those files at init (page-cache accounting and
    // eviction tests); PROBE_REREAD=1 reads them all a second time.
    if let Ok(list) = std::env::var("PROBE_READ_FILE") {
        let paths: Vec<&str> = list.split(',').collect();
        let mut reads = read_files(&paths, "first");
        if std::env::var("PROBE_REREAD").as_deref() == Ok("1") {
            reads.extend(read_files(&paths, "second"));
        }
        let _ = READS.set(format!("[{}]", reads.join(",")));
    }
    // PROBE_PREFAULT=read: read the executable once so its pages sit in the
    // page cache before anything runs; the elapsed ms goes to stderr (logs).
    if std::env::var("PROBE_PREFAULT").as_deref() == Ok("read") {
        let mut t = Lap::new();
        let mut exe = std::fs::File::open("/proc/self/exe").expect("open /proc/self/exe");
        std::io::copy(&mut exe, &mut std::io::sink()).expect("read /proc/self/exe");
        let _ = PREFAULT.set(t.lap());
    }
    let in_handler = std::env::var("PROBE_PHASE").as_deref() == Ok("invoke");
    let mut result = if in_handler {
        None
    } else {
        Some(measure().await)
    };
    let api = std::env::var("AWS_LAMBDA_RUNTIME_API").unwrap();
    loop {
        let next = request(&api, "GET", "/2018-06-01/runtime/invocation/next", "");
        let request_id = next
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("lambda-runtime-aws-request-id")
                    .then(|| value.trim().to_string())
            })
            .unwrap();
        if result.is_none() {
            result = Some(measure().await);
        }
        let body = result.clone().unwrap();
        request(
            &api,
            "POST",
            &format!("/2018-06-01/runtime/invocation/{request_id}/response"),
            &body,
        );
    }
}
