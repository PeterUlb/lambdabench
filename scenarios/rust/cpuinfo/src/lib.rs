//! Host CPU reporting for the benchmark.
//!
//! After each cold+warm cycle the bencher sends one extra, unrecorded invoke whose
//! ClientContext carries `{"custom":{"lambdabench":"cpu"}}`. A handler passes the
//! context's custom map to [`report_if_requested`], which on that invoke logs one
//! `LAMBDABENCH_CPU <cpu>` line; the bencher reads it from the log tail and stamps
//! the cycle's rows with it. A sandbox stays on one host, so it identifies the CPU
//! the cycle's cold start ran on (Lambda mixes Graviton generations on arm64).
//!
//! Recorded invokes carry no ClientContext, so for them this is one `Option`
//! check: the `/proc/cpuinfo` read never runs on a measured invoke.

use std::collections::HashMap;

/// ClientContext custom key and value the bencher sets on the CPU invoke.
pub const KEY: &str = "lambdabench";
pub const VALUE: &str = "cpu";

/// Logs the host CPU if the invoke's ClientContext asks for it. Takes the custom
/// map rather than a `lambda_runtime` type so crates on different
/// `lambda_runtime` versions (the Smithy scenarios pin 0.8) can share it.
pub fn report_if_requested(custom: Option<&HashMap<String, String>>) {
    if requested(custom) {
        report();
    }
}

fn requested(custom: Option<&HashMap<String, String>>) -> bool {
    custom.and_then(|c| c.get(KEY)).map(String::as_str) == Some(VALUE)
}

#[cold]
#[inline(never)]
fn report() {
    let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    eprintln!("LAMBDABENCH_CPU {}", describe(&info));
}

/// One line naming the CPU: the x86 `model name`, which Firecracker (Lambda's VM
/// layer) normalizes to "Intel(R) Xeon(R) Processor" plus the host's nominal clock,
/// so the clock is the part that varies by host (family/model/stepping come from
/// Firecracker's CPU template and are fixed: model 63 in every probe); or the arm64
/// implementer, part and variant, which keep their host values (part 0xd0c is
/// Neoverse N1 / Graviton2, 0xd40 V1 / Graviton3, 0xd4f V2 / Graviton4); plus the
/// vCPU count.
fn describe(info: &str) -> String {
    let field = |name: &str| {
        info.lines()
            .find(|l| l.split(':').next().map(str::trim) == Some(name))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    };
    let vcpus = info.lines().filter(|l| l.starts_with("processor")).count();
    let cpu = match field("model name").filter(|m| !m.is_empty()) {
        Some(model) => model,
        None => format!(
            "arm implementer={} part={} variant={}",
            field("CPU implementer").unwrap_or_default(),
            field("CPU part").unwrap_or_default(),
            field("CPU variant").unwrap_or_default()
        ),
    };
    format!("{cpu}; vcpus={vcpus}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_arm64() {
        let info = "processor\t: 0\nBogoMIPS\t: 2100.00\nCPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x1\nCPU part\t: 0xd40\n\nprocessor\t: 1\nCPU implementer\t: 0x41\nCPU variant\t: 0x1\nCPU part\t: 0xd40\n";
        assert_eq!(
            describe(info),
            "arm implementer=0x41 part=0xd40 variant=0x1; vcpus=2"
        );
    }

    #[test]
    fn describes_x86() {
        let info = "processor\t: 0\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 63\nmodel name\t: Intel(R) Xeon(R) Processor @ 2.50GHz\nstepping\t: 2\n\nprocessor\t: 1\nmodel name\t: Intel(R) Xeon(R) Processor @ 2.50GHz\n";
        assert_eq!(
            describe(info),
            "Intel(R) Xeon(R) Processor @ 2.50GHz; vcpus=2"
        );
    }

    #[test]
    fn only_the_flag_reports() {
        assert!(!requested(None));
        let mut custom = HashMap::new();
        custom.insert("x".to_string(), "cpu".to_string());
        assert!(!requested(Some(&custom)));
        custom.insert(KEY.to_string(), "other".to_string());
        assert!(!requested(Some(&custom)));
        custom.insert(KEY.to_string(), VALUE.to_string());
        assert!(requested(Some(&custom)));
    }
}
