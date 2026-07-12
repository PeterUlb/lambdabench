//! Parsing of the Lambda `REPORT` line emitted in the `LogType=Tail` output.
//!
//! A cold invocation's REPORT line includes an `Init Duration` field; a warm
//! one does not. A SnapStart cold (restored) invocation instead reports
//! `Restore Duration` (and no `Init Duration`), since the init work ran once at
//! snapshot time, not per cold start. Either field marks a cold start (see
//! [`Report::is_cold`]). The benchmark treats any deviation (a cold marker on an
//! expected-warm invoke, or neither on an expected-cold invoke) as a hard error
//! rather than falling back.

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use std::sync::OnceLock;

/// Timings extracted from a single REPORT line.
#[derive(Debug, Clone)]
pub struct Report {
    pub request_id: String,
    pub duration_ms: f64,
    pub billed_ms: f64,
    pub memory_size_mb: i64,
    pub max_memory_used_mb: i64,
    /// Present only on a non-SnapStart cold start.
    pub init_ms: Option<f64>,
    /// Present only on a SnapStart cold (restored) start. Mutually exclusive
    /// with `init_ms` in practice.
    pub restore_ms: Option<f64>,
}

impl Report {
    /// Whether this invocation was a cold start. A non-SnapStart cold start
    /// reports `Init Duration`; a SnapStart cold (restored) start reports
    /// `Restore Duration` instead. Either marks a cold start; a warm start
    /// reports neither.
    pub fn is_cold(&self) -> bool {
        self.init_ms.is_some() || self.restore_ms.is_some()
    }
}

/// Matches the always-present REPORT fields (RequestId, Duration, Billed
/// Duration, Memory Size, Max Memory Used). The cold markers are matched
/// separately by [`init_regex`] / [`restore_regex`], not as positional trailing
/// groups, so a trailing field the platform appends (X-Ray `TraceId` /
/// `SegmentId`, a future status field) cannot displace a cold marker and make a
/// genuine cold start parse as warm.
fn report_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Fields are tab/space separated; capture each by name. No end anchor:
        // any additional trailing fields are ignored, and the cold markers are
        // located independently below.
        Regex::new(
            r"REPORT RequestId:\s*(?P<rid>[0-9a-fA-F-]+)\s+.*?Duration:\s*(?P<dur>[0-9.]+)\s*ms\s+Billed Duration:\s*(?P<billed>[0-9.]+)\s*ms\s+Memory Size:\s*(?P<memsize>[0-9]+)\s*MB\s+Max Memory Used:\s*(?P<maxmem>[0-9]+)\s*MB",
        )
        .expect("valid REPORT regex")
    })
}

/// Locates the `Init Duration` field (non-SnapStart cold) anywhere in the line.
///
/// AWS emits no `Billed Init Duration` today, but it did add a `Billed Restore
/// Duration` sibling to `Restore Duration` (see [`restore_regex`]), and the
/// substring `Init Duration:` would match inside any such `Billed Init Duration:`
/// prefix. The optional `billed` group captures that prefix so the caller can
/// skip a billed match and keep the real `Init Duration` regardless of field
/// order, mirroring the restore path.
fn init_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?P<billed>Billed )?Init Duration:\s*(?P<init>[0-9.]+)\s*ms")
            .expect("valid Init Duration regex")
    })
}

/// Locates the `Restore Duration` field (SnapStart cold) anywhere in the line.
///
/// A SnapStart REPORT line carries BOTH `Restore Duration` and `Billed Restore
/// Duration` (verified against live eu-central-1 output), and `Restore Duration:`
/// matches inside the billed field too. The optional `billed` group captures that
/// prefix so the caller can skip the billed match and keep the real `Restore
/// Duration` regardless of which field the platform emits first.
fn restore_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?P<billed>Billed )?Restore Duration:\s*(?P<restore>[0-9.]+)\s*ms")
            .expect("valid Restore Duration regex")
    })
}

/// Parses the REPORT line out of a decoded log tail. Errors if no REPORT line
/// is present or it cannot be parsed (never silently returns partial data).
pub fn parse_report(log_tail: &str) -> Result<Report> {
    // Scan from the end: the platform's REPORT line is always the last line of a
    // LogType=Tail tail, so a handler that happened to log a line containing the
    // substring "REPORT RequestId:" earlier cannot shadow it.
    let line = log_tail
        .lines()
        .rev()
        .find(|l| l.contains("REPORT RequestId:"))
        .ok_or_else(|| anyhow!("no REPORT line in log tail:\n{log_tail}"))?;

    let caps = report_regex()
        .captures(line)
        .ok_or_else(|| anyhow!("REPORT line did not match expected format: {line}"))?;

    let f = |name: &str| -> Result<f64> {
        caps.name(name)
            .ok_or_else(|| anyhow!("missing field {name} in REPORT: {line}"))?
            .as_str()
            .parse::<f64>()
            .with_context(|| format!("parsing {name} as f64 in REPORT: {line}"))
    };
    let i = |name: &str| -> Result<i64> {
        caps.name(name)
            .ok_or_else(|| anyhow!("missing field {name} in REPORT: {line}"))?
            .as_str()
            .parse::<i64>()
            .with_context(|| format!("parsing {name} as i64 in REPORT: {line}"))
    };

    // Cold markers are located independently of the mandatory-field match, so a
    // trailing platform field after them does not hide them. Each is optional and
    // the two are mutually exclusive in practice (a cold start reports one, never
    // both). Skipping billed-prefixed matches is explained on `init_regex` /
    // `restore_regex`.
    let init_ms = match init_regex()
        .captures_iter(line)
        .find(|c| c.name("billed").is_none())
        .and_then(|c| c.name("init").map(|m| m.as_str().to_string()))
    {
        Some(s) => Some(
            s.parse::<f64>()
                .with_context(|| format!("parsing Init Duration: {line}"))?,
        ),
        None => None,
    };
    let restore_ms = match restore_regex()
        .captures_iter(line)
        .find(|c| c.name("billed").is_none())
        .and_then(|c| c.name("restore").map(|m| m.as_str().to_string()))
    {
        Some(s) => Some(
            s.parse::<f64>()
                .with_context(|| format!("parsing Restore Duration: {line}"))?,
        ),
        None => None,
    };

    Ok(Report {
        request_id: caps
            .name("rid")
            .ok_or_else(|| anyhow!("missing RequestId in REPORT: {line}"))?
            .as_str()
            .to_string(),
        duration_ms: f("dur")?,
        billed_ms: f("billed")?,
        memory_size_mb: i("memsize")?,
        max_memory_used_mb: i("maxmem")?,
        init_ms,
        restore_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cold_report() {
        let log = "START RequestId: abc Version: $LATEST\n\
            END RequestId: abc\n\
            REPORT RequestId: 8f7c-1\tDuration: 12.34 ms\tBilled Duration: 13 ms\tMemory Size: 512 MB\tMax Memory Used: 90 MB\tInit Duration: 187.65 ms\n";
        let r = parse_report(log).unwrap();
        assert_eq!(r.request_id, "8f7c-1");
        assert!((r.duration_ms - 12.34).abs() < 1e-9);
        assert_eq!(r.billed_ms as i64, 13);
        assert_eq!(r.memory_size_mb, 512);
        assert_eq!(r.max_memory_used_mb, 90);
        assert_eq!(r.init_ms.map(|x| x as i64), Some(187));
        assert!(r.restore_ms.is_none());
        assert!(r.is_cold());
    }

    #[test]
    fn parses_warm_report_without_init() {
        let log = "REPORT RequestId: 9a\tDuration: 1.01 ms\tBilled Duration: 2 ms\tMemory Size: 128 MB\tMax Memory Used: 40 MB\n";
        let r = parse_report(log).unwrap();
        assert!(r.init_ms.is_none());
        assert!(r.restore_ms.is_none());
        assert!(!r.is_cold());
        assert_eq!(r.memory_size_mb, 128);
    }

    /// A SnapStart cold (restored) invocation reports `Restore Duration` instead
    /// of `Init Duration`. It must be detected as cold via `restore_ms`, with
    /// `init_ms` absent.
    #[test]
    fn parses_snapstart_restore_report() {
        let log = "REPORT RequestId: 7b\tDuration: 22.5 ms\tBilled Duration: 90 ms\tMemory Size: 512 MB\tMax Memory Used: 180 MB\tRestore Duration: 245.10 ms\n";
        let r = parse_report(log).unwrap();
        assert!(r.init_ms.is_none());
        assert_eq!(r.restore_ms.map(|x| x as i64), Some(245));
        assert!(r.is_cold());
        assert_eq!(r.memory_size_mb, 512);
    }

    /// A cold REPORT line with trailing fields after `Init Duration` (e.g. the
    /// X-Ray segment a traced function appends) must still be detected as cold:
    /// the cold marker is located independently of its position in the line.
    #[test]
    fn parses_cold_report_with_trailing_xray_field() {
        let log = "REPORT RequestId: 8f7c-1\tDuration: 12.34 ms\tBilled Duration: 13 ms\tMemory Size: 512 MB\tMax Memory Used: 90 MB\tInit Duration: 187.65 ms\tXRAY TraceId: 1-abc\tSegmentId: def\tSampled: true\n";
        let r = parse_report(log).unwrap();
        assert_eq!(r.init_ms.map(|x| x as i64), Some(187));
        assert!(r.restore_ms.is_none());
        assert!(r.is_cold());
        assert_eq!(r.memory_size_mb, 512);
        assert_eq!(r.max_memory_used_mb, 90);
    }

    /// Likewise for a SnapStart restore line with trailing fields after
    /// `Restore Duration`.
    #[test]
    fn parses_snapstart_restore_with_trailing_field() {
        let log = "REPORT RequestId: 7b\tDuration: 22.5 ms\tBilled Duration: 90 ms\tMemory Size: 512 MB\tMax Memory Used: 180 MB\tRestore Duration: 245.10 ms\tXRAY TraceId: 1-abc\n";
        let r = parse_report(log).unwrap();
        assert!(r.init_ms.is_none());
        assert_eq!(r.restore_ms.map(|x| x as i64), Some(245));
        assert!(r.is_cold());
    }

    /// The live SnapStart REPORT line carries BOTH `Restore Duration` and
    /// `Billed Restore Duration` (verified against eu-central-1 output). The
    /// parser must extract the real restore time, never the billed one, and must
    /// not depend on which field the platform prints first.
    #[test]
    fn parses_snapstart_restore_ignoring_billed_restore_duration() {
        // Real field order: Restore Duration before Billed Restore Duration.
        let log = "REPORT RequestId: 0e5c8424\tDuration: 159.87 ms\tBilled Duration: 278 ms\tMemory Size: 1024 MB\tMax Memory Used: 88 MB\tRestore Duration: 562.43 ms\tBilled Restore Duration: 118 ms\n";
        let r = parse_report(log).unwrap();
        assert_eq!(r.restore_ms.map(|x| x as i64), Some(562));
        assert!(r.init_ms.is_none());
        assert!(r.is_cold());

        // Same fields, billed printed FIRST: the result must be identical, since
        // selection is by the `billed` prefix, not match position.
        let swapped = "REPORT RequestId: 0e5c8424\tDuration: 159.87 ms\tBilled Duration: 278 ms\tMemory Size: 1024 MB\tMax Memory Used: 88 MB\tBilled Restore Duration: 118 ms\tRestore Duration: 562.43 ms\n";
        let r2 = parse_report(swapped).unwrap();
        assert_eq!(r2.restore_ms.map(|x| x as i64), Some(562));
    }

    /// AWS emits no `Billed Init Duration` field today, but if it ever adds one
    /// (as it did for Restore Duration), the parser must extract the real init
    /// time, never the billed one, independent of field order, matching the
    /// restore path. Guards against a future platform change silently recording
    /// the billed value as the init cost.
    #[test]
    fn parses_init_ignoring_hypothetical_billed_init_duration() {
        let log = "REPORT RequestId: abc\tDuration: 12.34 ms\tBilled Duration: 13 ms\tMemory Size: 512 MB\tMax Memory Used: 90 MB\tInit Duration: 187.65 ms\tBilled Init Duration: 999 ms\n";
        let r = parse_report(log).unwrap();
        assert_eq!(r.init_ms.map(|x| x as i64), Some(187));
        assert!(r.restore_ms.is_none());
        assert!(r.is_cold());

        // Same fields, billed printed FIRST: the result must be identical, since
        // selection is by the `billed` prefix, not match position.
        let swapped = "REPORT RequestId: abc\tDuration: 12.34 ms\tBilled Duration: 13 ms\tMemory Size: 512 MB\tMax Memory Used: 90 MB\tBilled Init Duration: 999 ms\tInit Duration: 187.65 ms\n";
        let r2 = parse_report(swapped).unwrap();
        assert_eq!(r2.init_ms.map(|x| x as i64), Some(187));
    }

    #[test]
    fn errors_without_report_line() {
        assert!(parse_report("START\nEND\n").is_err());
    }
}
