//! The shared measurement core: one cold sample's decomposition, its
//! aggregation to p50s + residual spread, and the median. Used identically by
//! all three modes (`download-start`, the zip `download-scaling`, and the
//! container-image family), so the residual is computed the same way everywhere.

use crate::aws::Aws;
use anyhow::{Context, Result, bail};
use std::time::Duration;

/// One cold sample's decomposition (all in ms).
pub(super) struct Sample {
    pub(super) w_cold: f64,
    pub(super) init: f64,
    pub(super) cold_duration: f64,
    /// Warm network + invoke-API overhead: a warm invoke's wall-clock minus its
    /// own handler `Duration`, so handler processing does not leak into the
    /// residual (see `take_sample`).
    pub(super) warm_rtt: f64,
    /// A warm invoke's FULL wall-clock (median over this sample's warm invokes,
    /// handler `Duration` included): the end-to-end wait a warm caller sees from
    /// this vantage. `warm_rtt` above is this minus the handler's own Duration,
    /// per invoke; keeping both lets the site publish the full caller wait
    /// without un-subtracting.
    pub(super) w_warm: f64,
    pub(super) residual: f64,
}

/// The p50s + spreads of a set of samples, shared by the zip-family
/// (`SyntheticSample`), image-family (`ImageSample`), and matrix-cell
/// (`CellResult`) aggregation so every family reduces identically.
///
/// Spread (min-max) is carried for the three published-as-headline quantities:
/// the residual (the decomposition's payoff) and the two full caller waits
/// (`w_cold`, `w_warm`), so the page can always show a range next to a p50 it
/// asks the reader to rely on. The pure subtraction terms (init, cold_dur,
/// warm_rtt) stay p50-only.
pub(super) struct Aggregated {
    pub(super) n_samples: u32,
    pub(super) w_cold_p50: f64,
    pub(super) w_cold_min: f64,
    pub(super) w_cold_max: f64,
    pub(super) init_p50: f64,
    pub(super) cold_dur_p50: f64,
    pub(super) warm_rtt_p50: f64,
    pub(super) w_warm_p50: f64,
    pub(super) w_warm_min: f64,
    pub(super) w_warm_max: f64,
    pub(super) residual_p50: f64,
    pub(super) residual_min: f64,
    pub(super) residual_max: f64,
}

/// Min-max of a sample slice (assumes non-empty, like `median`).
fn spread(v: &[f64]) -> (f64, f64) {
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min, max)
}

/// Reduces a set of samples to the p50 of each term plus the residual and
/// caller-wait spreads.
pub(super) fn aggregate(samples: &[Sample]) -> Aggregated {
    let mut w_cold: Vec<f64> = samples.iter().map(|s| s.w_cold).collect();
    let mut init: Vec<f64> = samples.iter().map(|s| s.init).collect();
    let mut cold_dur: Vec<f64> = samples.iter().map(|s| s.cold_duration).collect();
    let mut warm: Vec<f64> = samples.iter().map(|s| s.warm_rtt).collect();
    let mut w_warm: Vec<f64> = samples.iter().map(|s| s.w_warm).collect();
    let mut resid: Vec<f64> = samples.iter().map(|s| s.residual).collect();
    let (w_cold_min, w_cold_max) = spread(&w_cold);
    let (w_warm_min, w_warm_max) = spread(&w_warm);
    let (residual_min, residual_max) = spread(&resid);
    Aggregated {
        n_samples: samples.len() as u32,
        w_cold_p50: median(&mut w_cold),
        w_cold_min,
        w_cold_max,
        init_p50: median(&mut init),
        cold_dur_p50: median(&mut cold_dur),
        warm_rtt_p50: median(&mut warm),
        w_warm_p50: median(&mut w_warm),
        w_warm_min,
        w_warm_max,
        residual_p50: median(&mut resid),
        residual_min,
        residual_max,
    }
}

/// Takes one cold sample: force the target cold, prewarm the data-plane
/// connection immediately before the measured call, take the timed cold invoke
/// (retrying if it lands warm), then N timed warm invokes.
///
/// Order matters. `force_cold` is a control-plane operation
/// (`UpdateFunctionConfiguration` + readiness polling) taking SECONDS, and the
/// invoke it precedes is data-plane. Warming the connection before `force_cold`
/// would leave it idle across the multi-second control-plane window, where it
/// could be evicted and make `W_cold` pay a fresh TLS handshake, inflating the
/// residual. So the prewarm fires right after `force_cold` and right before the
/// timed cold invoke: both are data-plane invokes to the same endpoint on the
/// same client, so the second reuses the connection the first opened, ~ms apart.
/// The warm invokes that follow need no separate prewarm (back-to-back on that
/// same connection). `warm_rtt` nets out each warm invoke's own handler
/// `Duration`, so it is network + overhead only (see below).
pub(super) async fn take_sample(
    aws: &Aws,
    timed: &aws_sdk_lambda::Client,
    name: &str,
    payload: &str,
    http_fronted: bool,
    warm_per_sample: u32,
) -> Result<Sample> {
    // Force cold, prewarm, then take the timed cold invoke, retrying the force if
    // the invoke lands on a warm sandbox (data-plane propagation lag). Mirrors
    // run.rs::force_cold_invoke's fail-loud discipline: require init_ms.
    const MAX_COLD_FORCE_ATTEMPTS: u32 = 6;
    let mut cold: Option<(f64, f64, f64)> = None; // (w_cold, init, cold_duration)
    for attempt in 1..=MAX_COLD_FORCE_ATTEMPTS {
        let nonce = uuid::Uuid::new_v4().to_string();
        aws.force_cold_by_name(name, &nonce)
            .await
            .with_context(|| format!("forcing cold for {name}"))?;

        // Prewarm the shared data-plane HTTPS connection with a discarded
        // throwaway invoke of a NON-EXISTENT function. Runs after the slow
        // force_cold and immediately before the measured invoke, so the connection
        // cannot be evicted in between (see the function doc).
        super::prewarm_connection(timed).await?;

        let (res, w_cold) = aws
            .invoke_tail_timed_by_name(timed, name, payload, None)
            .await?;
        check_ok(name, http_fronted, &res)?;
        let report = crate::parse::parse_report(&res.log_tail)
            .with_context(|| format!("parsing REPORT for {name}"))?;

        // Require a non-SnapStart cold start specifically (init_ms present), not
        // the looser is_cold(): these are plain functions, so a restore is wrong.
        if let Some(init) = report.init_ms {
            cold = Some((w_cold.as_secs_f64() * 1000.0, init, report.duration_ms));
            break;
        }
        eprintln!(
            "   cold-force retry {attempt}/{MAX_COLD_FORCE_ATTEMPTS} for {name}: invoke landed warm (req {}), re-forcing",
            report.request_id,
        );
        tokio::time::sleep(Duration::from_millis(500) * attempt).await;
    }
    let (w_cold, init, cold_duration) = cold.ok_or_else(|| {
        anyhow::anyhow!(
            "{name}: failed to force a cold start in {MAX_COLD_FORCE_ATTEMPTS} attempts"
        )
    })?;

    // Warm round-trip: N timed warm invokes on the now-warm sandbox, each must be
    // warm. Subtract each warm invoke's OWN REPORT `Duration` from its wall-clock,
    // leaving only network + invoke-API overhead (`warm_rtt`), NOT handler
    // processing. The residual below also subtracts the cold invoke's handler work
    // (`cold_duration`): if `warm_rtt` still carried the warm handler's processing,
    // the residual would be understated by that amount, negligible for a trivial
    // handler but tens of ms for an SDK-heavy one. Netting out both handler terms
    // makes `residual` the download + environment-start cost regardless of handler.
    let mut warm = Vec::with_capacity(warm_per_sample as usize);
    let mut warm_wall = Vec::with_capacity(warm_per_sample as usize);
    for i in 1..=warm_per_sample {
        let (res, w) = aws
            .invoke_tail_timed_by_name(timed, name, payload, None)
            .await?;
        check_ok(name, http_fronted, &res)?;
        let report = crate::parse::parse_report(&res.log_tail)
            .with_context(|| format!("parsing warm REPORT for {name}"))?;
        if report.is_cold() {
            bail!("{name}: warm invoke {i} unexpectedly cold-started (sandbox retired mid-sample)");
        }
        let wall = w.as_secs_f64() * 1000.0;
        // Wall-clock minus the handler's own Duration = network + overhead only.
        // Clamp at 0 in the rare case clock skew makes Duration exceed wall-clock.
        let net = (wall - report.duration_ms).max(0.0);
        warm.push(net);
        warm_wall.push(wall);
    }
    let warm_rtt = median(&mut warm);
    let w_warm = median(&mut warm_wall);
    let residual = w_cold - init - cold_duration - warm_rtt;

    Ok(Sample {
        w_cold,
        init,
        cold_duration,
        warm_rtt,
        w_warm,
        residual,
    })
}

/// Takes `cold_samples` cold samples for `name`, wrapped in `retry_transient` so
/// a transient hard error re-runs the whole batch (buffer-then-commit) rather
/// than leaving a partially-sampled result.
pub(super) async fn sample_cold_series(
    aws: &Aws,
    timed: &aws_sdk_lambda::Client,
    name: &str,
    payload: &str,
    http_fronted: bool,
    cold_samples: u32,
    warm_per_sample: u32,
) -> Result<Vec<Sample>> {
    super::retry_transient(name, || async {
        let mut samples = Vec::with_capacity(cold_samples as usize);
        for s in 1..=cold_samples {
            let sm = take_sample(aws, timed, name, payload, http_fronted, warm_per_sample)
                .await
                .with_context(|| format!("cold sample {s}/{cold_samples} of {name}"))?;
            println!(
                "   sample {s}/{cold_samples}: W_cold={:.1} init={:.1} cold_dur={:.1} warm_rtt={:.1} W_warm={:.1} -> residual={:.1} ms",
                sm.w_cold, sm.init, sm.cold_duration, sm.warm_rtt, sm.w_warm, sm.residual,
            );
            samples.push(sm);
        }
        Ok(samples)
    })
    .await
}

/// Fails loud on a platform-level invoke problem (non-200 status or a
/// FunctionError), and for an HTTP-fronted target also validates the Smithy
/// response envelope's `statusCode`. That last check matters for the matrix
/// `smithyfull` targets: their server framework can serialize a failed AWS write
/// as a 500 INSIDE the response body while the outer invoke still returns 200 / no
/// FunctionError, and timing that degraded error path as a clean invoke would
/// corrupt the cell's residual. Shares `run::check_http_envelope_ok`. The
/// synthetic targets are trivial handlers (never HTTP-fronted), so they pass
/// `http_fronted = false`.
pub(super) fn check_ok(
    name: &str,
    http_fronted: bool,
    res: &crate::aws::lambda::InvokeResult,
) -> Result<()> {
    if res.status_code != 200 {
        bail!(
            "{name} returned status {} (expected 200); log:\n{}",
            res.status_code,
            res.log_tail
        );
    }
    if let Some(err) = &res.function_error {
        bail!(
            "{name} reported FunctionError={err}; log:\n{}",
            res.log_tail
        );
    }
    if http_fronted {
        crate::run::check_http_envelope_ok(name, res)?;
    }
    Ok(())
}

/// Median of a sample slice (sorts in place). No interpolation: for an even
/// count, the mean of the two central values. Panics on an empty slice, which
/// callers never pass (sample counts are >= 1).
pub(super) fn median(v: &mut [f64]) -> f64 {
    assert!(!v.is_empty(), "median of empty sample set");
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in timing samples"));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Sample` carrying only a residual; the other terms are irrelevant to the
    /// spread-focused cases and carry filler.
    fn sample_with(residual: f64) -> Sample {
        Sample {
            w_cold: 0.0,
            init: 0.0,
            cold_duration: 0.0,
            warm_rtt: 0.0,
            w_warm: 0.0,
            residual,
        }
    }

    /// Odd counts return the true middle element; even counts return the mean of
    /// the two central values; a single element is itself. Input order does not
    /// matter (median sorts in place).
    #[test]
    fn median_odd_even_and_single() {
        assert_eq!(median(&mut [5.0]), 5.0);
        // Odd: middle of the sorted values.
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
        // Even: mean of the two central sorted values (2.0 and 3.0).
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
        // Unsorted input with a duplicate straddling the midpoint.
        assert_eq!(median(&mut [10.0, 2.0, 2.0, 8.0, 6.0]), 6.0);
    }

    /// `aggregate` reduces each per-sample term to its own median and reports the
    /// residual and caller-wait spreads (min/max) across the raw samples.
    #[test]
    fn aggregate_reduces_each_term_and_reports_spread() {
        let samples = [
            Sample {
                w_cold: 300.0,
                init: 100.0,
                cold_duration: 20.0,
                warm_rtt: 5.0,
                w_warm: 7.0,
                residual: 175.0,
            },
            Sample {
                w_cold: 320.0,
                init: 110.0,
                cold_duration: 22.0,
                warm_rtt: 6.0,
                w_warm: 8.5,
                residual: 182.0,
            },
            Sample {
                w_cold: 310.0,
                init: 105.0,
                cold_duration: 21.0,
                warm_rtt: 4.0,
                w_warm: 6.0,
                residual: 180.0,
            },
        ];
        let a = aggregate(&samples);
        assert_eq!(a.n_samples, 3);
        // Odd count: each p50 is the middle sorted value of THAT term.
        assert_eq!(a.w_cold_p50, 310.0);
        assert_eq!(a.init_p50, 105.0);
        assert_eq!(a.cold_dur_p50, 21.0);
        assert_eq!(a.warm_rtt_p50, 5.0);
        assert_eq!(a.w_warm_p50, 7.0);
        assert_eq!(a.residual_p50, 180.0);
        // Spreads are over the raw samples, not derived from the p50s.
        assert_eq!(a.w_cold_min, 300.0);
        assert_eq!(a.w_cold_max, 320.0);
        assert_eq!(a.w_warm_min, 6.0);
        assert_eq!(a.w_warm_max, 8.5);
        assert_eq!(a.residual_min, 175.0);
        assert_eq!(a.residual_max, 182.0);
    }

    /// The residual spread widens to the true extremes even when they sit far from
    /// the median, and a single sample collapses min == p50 == max.
    #[test]
    fn aggregate_spread_tracks_extremes_and_collapses_for_one_sample() {
        let spread = [
            sample_with(200.0),
            sample_with(50.0),
            sample_with(180.0),
            sample_with(500.0),
            sample_with(190.0),
        ];
        let a = aggregate(&spread);
        assert_eq!(a.residual_p50, 190.0);
        assert_eq!(a.residual_min, 50.0);
        assert_eq!(a.residual_max, 500.0);

        let single = [sample_with(123.0)];
        let a = aggregate(&single);
        assert_eq!(a.residual_p50, 123.0);
        assert_eq!(a.residual_min, 123.0);
        assert_eq!(a.residual_max, 123.0);
    }
}
