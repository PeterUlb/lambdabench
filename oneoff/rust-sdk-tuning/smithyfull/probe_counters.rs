//! Tuning-probe counters: process CPU, page faults, bytes read from storage
//! and resident executable size, snapshotted at the end of Init and printed
//! per request as one `PROBE {json}` line on stderr (read from the log tail).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone, Copy)]
pub struct Snap {
    cpu_ms: f64,
    majflt: i64,
    minflt: i64,
    read_kb: f64,
    exe_rss_kb: u64,
}

static INIT: OnceLock<Snap> = OnceLock::new();
static INIT_PAGES: OnceLock<String> = OnceLock::new();

/// File page indexes (4 KiB) of the executable that are mapped into this
/// process right now, as compact `a-b,c,...` ranges. Reads the present bit
/// of /proc/self/pagemap, which unprivileged processes can see.
fn exe_pages() -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(exe) = std::fs::read_link("/proc/self/exe") else {
        return String::new();
    };
    let exe = exe.to_string_lossy().into_owned();
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return String::new();
    };
    let Ok(mut pagemap) = std::fs::File::open("/proc/self/pagemap") else {
        return String::new();
    };
    let mut pages: Vec<u64> = Vec::new();
    for line in maps.lines().filter(|l| l.ends_with(&exe)) {
        let mut f = line.split_whitespace();
        let (Some(range), Some(_perms), Some(offset)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let Some((start, end)) = range.split_once('-') else { continue };
        let (Ok(start), Ok(end), Ok(offset)) = (
            u64::from_str_radix(start, 16),
            u64::from_str_radix(end, 16),
            u64::from_str_radix(offset, 16),
        ) else {
            continue;
        };
        let n = ((end - start) / 4096) as usize;
        let mut buf = vec![0u8; n * 8];
        if pagemap.seek(SeekFrom::Start(start / 4096 * 8)).is_err()
            || pagemap.read_exact(&mut buf).is_err()
        {
            continue;
        }
        for (i, e) in buf.chunks_exact(8).enumerate() {
            let entry = u64::from_le_bytes(e.try_into().unwrap());
            if entry & (1 << 63) != 0 {
                pages.push(offset / 4096 + i as u64);
            }
        }
    }
    pages.sort_unstable();
    pages.dedup();
    let mut out = String::new();
    let mut i = 0;
    while i < pages.len() {
        let mut j = i;
        while j + 1 < pages.len() && pages[j + 1] == pages[j] + 1 {
            j += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if i == j {
            out.push_str(&pages[i].to_string());
        } else {
            out.push_str(&format!("{}-{}", pages[i], pages[j]));
        }
        i = j + 1;
    }
    out
}
static N: AtomicU32 = AtomicU32::new(0);

fn snap() -> Snap {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: both calls only write into the structs we pass.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts);
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
    }
    let read_kb = std::fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|io| {
            io.lines()
                .find_map(|l| l.strip_prefix("read_bytes: "))
                .and_then(|v| v.trim().parse::<f64>().ok())
        })
        .unwrap_or(0.0)
        / 1024.0;
    Snap {
        cpu_ms: ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6,
        majflt: ru.ru_majflt,
        minflt: ru.ru_minflt,
        read_kb,
        exe_rss_kb: exe_rss_kb(),
    }
}

/// KB of the executable's own mappings resident in memory.
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
        if line.split_whitespace().next().is_some_and(|f| f.contains('-')) {
            in_exe = line.ends_with(&exe);
        } else if in_exe && let Some(v) = line.strip_prefix("Rss:") {
            kb += v.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0);
        }
    }
    kb
}

/// Selected /proc/meminfo fields (KB) and, where the sandbox exposes them,
/// the memory cgroup's usage and limit, as a JSON object.
fn memory() -> String {
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |key: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map_or("null".to_string(), |v| v.to_string())
    };
    let cgroup = |paths: &[&str]| {
        paths
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .map_or("null".to_string(), |v| format!("\"{}\"", v.trim()))
    };
    format!(
        r#"{{"total":{},"free":{},"available":{},"cached":{},"anon":{},"active_file":{},"inactive_file":{},"cg_current":{},"cg_max":{}}}"#,
        field("MemTotal"),
        field("MemFree"),
        field("MemAvailable"),
        field("Cached"),
        field("AnonPages"),
        field("Active(file)"),
        field("Inactive(file)"),
        cgroup(&["/sys/fs/cgroup/memory.current", "/sys/fs/cgroup/memory/memory.usage_in_bytes"]),
        cgroup(&["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"]),
    )
}

static INIT_MEM: OnceLock<String> = OnceLock::new();

/// The host CPU: the arm64 part number from /proc/cpuinfo (0xd0c Neoverse N1 for
/// Graviton2, 0xd40 V1 for Graviton3, 0xd4f V2 for Graviton4) or the x86 model name.
fn cpu_model() -> String {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    info.lines()
        .find(|l| l.starts_with("CPU part") || l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map_or(String::new(), |v| v.trim().replace('"', ""))
}

/// Call once at the end of Init.
pub fn mark_init() {
    let _ = INIT.set(snap());
    let _ = INIT_MEM.set(memory());
    if std::env::var("SF_PAGES").as_deref() == Ok("1") {
        let _ = INIT_PAGES.set(exe_pages());
    }
}

/// Call at the end of each request: prints the request's deltas since Init
/// (or since the previous request's line for later requests, via the
/// absolute values) plus the Init snapshot.
pub fn report() {
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n == 1 && let Some(init) = INIT_PAGES.get() {
        // Full CloudWatch lines (the invoke's 4 KB log tail would cut them).
        eprintln!("PAGES init {init}");
        eprintln!("PAGES req1 {}", exe_pages());
    }
    let (Some(i), now) = (INIT.get().copied(), snap()) else {
        return;
    };
    eprintln!(
        "PROBE {{\"n\":{n},\"cpu\":\"{}\",\"mem_init\":{},\"mem_now\":{},\"init_cpu_ms\":{:.3},\"init_majflt\":{},\"init_minflt\":{},\"init_read_kb\":{:.0},\"init_exe_rss_kb\":{},\"cpu_ms\":{:.3},\"majflt\":{},\"minflt\":{},\"read_kb\":{:.0},\"exe_rss_kb\":{}}}",
        cpu_model(),
        INIT_MEM.get().map_or("null", String::as_str),
        memory(),
        i.cpu_ms,
        i.majflt,
        i.minflt,
        i.read_kb,
        i.exe_rss_kb,
        now.cpu_ms - i.cpu_ms,
        now.majflt - i.majflt,
        now.minflt - i.minflt,
        now.read_kb - i.read_kb,
        now.exe_rss_kb,
    );
}
