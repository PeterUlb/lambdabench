//! The results data model: one gzipped-JSONL row per invocation, plus run
//! metadata.
//!
//! The durable record of a run: one JSON object per line, gzip-compressed on the
//! fly. It carries every parsed timing the analysis needs but NOT the raw Lambda
//! log tail. The tail is consumed live (the REPORT line is parsed out of it, and
//! it is embedded in error messages on an invariant violation), so its data is
//! already in the parsed columns; persisting the base64 tail on every one of
//! ~1.4M rows added ~40% per row for an archive nothing read back.

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

/// A single invocation's full record.
#[derive(Debug, Serialize)]
pub struct InvocationRow {
    pub run_id: String,
    pub ts_unix_ms: u128,

    // Cell identity.
    pub lang: String,
    pub scenario: String,
    pub memory_mb: i32,
    pub arch: String,
    /// Rust optimization level ("o3" / "oz"); null for Node and Java.
    pub opt: Option<String>,
    /// Java-only SnapStart dimension: true if the function had SnapStart enabled
    /// (measured via published versions). Always false for Rust and Node.
    pub snapstart: bool,
    /// Rust-only `aws-lc-rs` jitter-entropy build dimension ("on" / "off"); null
    /// for non-Rust runtimes. The standing matrix is "off" (build flag disables
    /// jitter-entropy seeding); the diagnostic A/B variant "on" is emitted only
    /// for `oneclient` and `lettercount`, which place the same one-time cost in
    /// opposing Lambda lifecycle phases (Invoke-phase cliff vs Init-phase flat
    /// bump). See `config::Jitter`.
    pub jitter: Option<String>,
    pub runtime_id: String,
    pub function_name: String,
    /// Deployed package (compressed) size and uncompressed code size.
    pub artifact_zip_bytes: u64,
    pub artifact_unzipped_bytes: u64,

    // Position in the cold/warm schedule.
    pub cycle: u32,
    pub idx_in_cycle: u32,
    pub is_cold: bool,
    pub cold_nonce: String,

    // Invocation outcome.
    pub request_id: String,
    pub invoke_status: i32,
    pub function_error: Option<String>,

    // Parsed timings.
    /// Present only on a non-SnapStart cold start.
    pub init_ms: Option<f64>,
    /// Present only on a SnapStart cold (restored) start.
    pub restore_ms: Option<f64>,
    pub duration_ms: f64,
    pub billed_ms: f64,
    pub memory_size_mb: i64,
    pub max_memory_used_mb: i64,
}

/// Thread-safe append-only gzipped-JSONL writer.
///
/// Rows are JSON lines in a single continuous gzip stream, not one member per
/// row, so the deflate sliding window dedupes the heavily repeated fields (run
/// id, function name, nonce, …) across rows. That cross-row back-referencing is
/// what makes the file ~10-17x smaller; a row compressed in isolation barely
/// shrinks.
///
/// Durability is at CELL granularity: [`write_cell`](Self::write_cell) appends a
/// completed cell's rows then issues one deflate sync-flush (`GzEncoder::flush`),
/// which advances to a byte boundary without resetting the dictionary, so
/// everything written so far is decompressible even if the process later crashes
/// while compression stays near whole-stream optimal. The flush hands the bytes
/// to the OS (through the BufWriter), it does not fsync: a process crash loses
/// nothing, but a machine/power failure can still drop recently flushed rows —
/// the meta's `total_invocations_recorded` cross-check catches that on read. The gzip footer is written
/// when the encoder is dropped on a clean finish; a crash leaves a footer-less
/// but readable stream (gunzip warns about the truncated trailer yet still emits
/// every flushed row). Per-cell (not per-row) syncing matches the run loop's own
/// atomicity and avoids per-row sync markers that would cost ~70% in compressed
/// size for no extra durability.
pub struct ResultsWriter {
    inner: Mutex<GzEncoder<BufWriter<File>>>,
    /// Count of rows successfully written, so the run can record how many
    /// invocations actually landed in the file (cross-checked against the
    /// planned count in the meta).
    rows: std::sync::atomic::AtomicU64,
}

impl ResultsWriter {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Truncate rather than append: a results file is always produced by a
        // single run, and appending would leave concatenated gzip members to
        // handle on read.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("opening results file {}", path.display()))?;
        Ok(Self {
            inner: Mutex::new(GzEncoder::new(BufWriter::new(file), Compression::default())),
            rows: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Appends a completed cell's rows as JSON lines and flushes them
    /// decompressibly to the OS (a deflate sync-flush, not an fsync; see the
    /// struct doc). The unit of durability and the only way to add rows: a
    /// process crash keeps every cell handed here, never a partial one. Rows are
    /// serialized before the lock is taken, then written and sync-flushed under a
    /// single lock acquisition, so a cell's lines land contiguously. Rows from
    /// concurrent cells may interleave at line boundaries, which JSONL readers
    /// don't care about.
    pub fn write_cell(&self, rows: &[InvocationRow]) -> Result<()> {
        let lines: Vec<String> = rows
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<_, _>>()
            .context("serializing invocation row")?;
        // Recover a poisoned lock rather than propagate the panic: already-written
        // bytes are valid JSONL (rows are line-atomic), and every later cell's rows
        // are recoverable data not to discard at the end of a multi-hour run.
        // Matches the poison recovery in run.rs.
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for line in &lines {
            guard.write_all(line.as_bytes())?;
            guard.write_all(b"\n")?;
        }
        guard.flush()?;
        self.rows
            .fetch_add(lines.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Number of rows written so far. Recorded into the run meta so a reader can
    /// verify the file holds the planned number of invocations.
    pub fn rows_written(&self) -> u64 {
        self.rows.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// One distinct resolved iteration count and how much of the run fell into it.
/// Lets the meta enumerate how every cell was sampled without listing all cells:
/// cells sharing the same `(cold, warm)` are tallied together.
#[derive(Debug, Clone, Serialize)]
pub struct IterationBucket {
    pub cold_cycles: u32,
    pub warm_per_cycle: u32,
    /// Functions that resolved to this `(cold, warm)` count.
    pub functions: usize,
    /// Invocations these functions contribute: `functions * cold * (1 + warm)`.
    pub invocations: u64,
}

/// Run-level metadata, written once alongside the JSONL.
#[derive(Debug, Serialize)]
pub struct RunMeta {
    pub run_id: String,
    pub started_at_unix_ms: u128,
    pub finished_at_unix_ms: Option<u128>,
    /// Terminal outcome of the run: `"running"` until it ends, then `"ok"` if
    /// every cell completed or `"failed"` if the run aborted. A populated
    /// `finished_at_unix_ms` does NOT imply a complete dataset: a failed run also
    /// stamps the finish time but leaves a truncated `.jsonl.gz` (see
    /// `total_invocations_recorded`).
    pub status: RunStatus,
    pub region: String,
    pub account_id: String,
    /// The iteration-count profile the run used (`full` / `smoke`). Per-cell
    /// cold/warm counts are a deterministic function of this profile and the cell
    /// (`Cell::iterations`), so profile plus matrix fully describes the sampling.
    /// The resolved counts are also enumerated in `iteration_buckets` so a reader
    /// need not re-derive them.
    pub profile: crate::config::Profile,
    /// The distinct resolved `(cold, warm)` counts actually run, each with how
    /// many functions and invocations fell into that bucket. Counts vary by
    /// scenario (CPU probes), language/memory (starved-Python thinning), and
    /// dimension (SnapStart clamp), so no single pair describes the whole run. The
    /// per-bucket invocation tallies sum to `total_invocations_planned`.
    pub iteration_buckets: Vec<IterationBucket>,
    pub pool: usize,
    /// The full memory sweep this matrix is defined over (`config::MEMORY_MB`),
    /// NOT the tiers this run exercised: a `--memory`-restricted run still records
    /// the whole sweep here. Documents the matrix definition, not the run's subset.
    pub memory_mb: Vec<i32>,
    pub total_functions: usize,
    pub total_invocations_planned: u64,
    /// Rows actually written to the `.jsonl.gz`. Equals
    /// `total_invocations_planned` on a clean run; a smaller value (with
    /// `status == "failed"`) means the run aborted with a truncated dataset.
    pub total_invocations_recorded: u64,
    pub build_manifest: serde_json::Value,
}

/// Terminal outcome of a run, persisted in the meta so consumers can distinguish
/// a complete dataset from a truncated one.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    /// The run is in progress (initial meta, before the run loop returns).
    Running,
    /// Every selected cell completed and its rows were written.
    Ok,
    /// The run aborted; the `.jsonl.gz` holds fewer rows than planned.
    Failed,
}

impl RunMeta {
    pub fn write(&self, path: impl AsRef<Path>) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("serializing run meta")?;
        std::fs::write(path.as_ref(), json)
            .with_context(|| format!("writing meta {}", path.as_ref().display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;

    fn sample_row() -> InvocationRow {
        InvocationRow {
            run_id: "run-test".into(),
            ts_unix_ms: 1,
            lang: "rust".into(),
            scenario: "hello".into(),
            memory_mb: 128,
            arch: "arm64".into(),
            opt: Some("o3".into()),
            snapstart: false,
            jitter: Some("off".into()),
            runtime_id: "provided.al2023".into(),
            function_name: "fn".into(),
            artifact_zip_bytes: 10,
            artifact_unzipped_bytes: 20,
            cycle: 0,
            idx_in_cycle: 0,
            is_cold: true,
            cold_nonce: "n".into(),
            request_id: "r".into(),
            invoke_status: 200,
            function_error: None,
            init_ms: Some(30.0),
            restore_ms: None,
            duration_ms: 1.5,
            billed_ms: 2.0,
            memory_size_mb: 128,
            max_memory_used_mb: 40,
        }
    }

    /// A written gzip stream decompresses to exactly the rows written, one JSON
    /// object per line, and carries no `log_tail_b64` field.
    #[test]
    fn writes_gzipped_jsonl_without_log_tail() {
        let dir = std::env::temp_dir().join(format!("lambdabench-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl.gz");

        {
            let w = ResultsWriter::create(&path).unwrap();
            // Two "cells" of one row each.
            w.write_cell(&[sample_row()]).unwrap();
            w.write_cell(&[sample_row()]).unwrap();
            // Drop closes the encoder and writes the gzip footer.
        }

        let bytes = std::fs::read(&path).unwrap();
        let mut decoded = String::new();
        GzDecoder::new(&bytes[..])
            .read_to_string(&mut decoded)
            .expect("valid gzip stream");

        let lines: Vec<&str> = decoded.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "two rows written");
        for line in &lines {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("each line is one JSON object");
            assert!(
                v.get("log_tail_b64").is_none(),
                "log tail must not be persisted"
            );
            assert_eq!(v["duration_ms"], 1.5);
            assert_eq!(v["lang"], "rust");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A non-Rust row carries `jitter: None`. The serialized JSON must encode
    /// that as a JSON `null` (not omit the field), so downstream readers see
    /// the column on every row and can distinguish "absent" from `"off"`.
    #[test]
    fn non_rust_row_serializes_jitter_as_null() {
        let row = InvocationRow {
            lang: "node".into(),
            opt: None,
            jitter: None,
            ..sample_row()
        };
        let json = serde_json::to_string(&row).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v.get("jitter"),
            Some(&serde_json::Value::Null),
            "jitter must be present and null on non-Rust rows"
        );
        assert_eq!(v.get("opt"), Some(&serde_json::Value::Null));
    }

    /// Simulates a crash: rows are flushed per cell but the encoder is never
    /// cleanly finished (no gzip footer). The flushed rows must still be
    /// recoverable from the truncated stream, the durability guarantee per-cell
    /// flushing provides.
    #[test]
    fn flushed_rows_survive_a_missing_footer() {
        let dir =
            std::env::temp_dir().join(format!("lambdabench-rec-crash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.jsonl.gz");

        // Write + sync three cells of one row each, then std::mem::forget the
        // writer so Drop never runs: the footer is never written, mimicking a
        // hard crash.
        let w = ResultsWriter::create(&path).unwrap();
        for _ in 0..3 {
            w.write_cell(&[sample_row()]).unwrap();
        }
        std::mem::forget(w);

        // Decode what reached disk. GzDecoder errors on the absent trailer, so we
        // read incrementally and keep whatever decoded before that error.
        let bytes = std::fs::read(&path).unwrap();
        let mut dec = GzDecoder::new(&bytes[..]);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match dec.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break, // truncated trailer; flushed bytes already decoded
            }
        }
        let decoded = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = decoded.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            3,
            "all flushed rows recovered despite no footer"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
