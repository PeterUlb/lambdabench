//! Shared issue-rate limiter for AWS control-plane calls.

use std::time::Duration;

/// Rate limiter enforcing a minimum interval between successive request STARTS,
/// shareable across `Aws` clones behind an `Arc`.
///
/// Distinct from a concurrency semaphore: a semaphore bounds requests in flight
/// and only bounds the RATE as a side effect of latency (fast calls slip through
/// at high TPS), whereas this bounds the issue rate directly. Each
/// `acquire().await` reserves the next free time slot and advances it by the
/// interval, so concurrent acquirers are spaced not bunched, and a slow call
/// never lets the next acquire fire early (the slot is clamped to "now", never the
/// past, so there is no catch-up burst).
pub struct RateLimiter {
    interval: Duration,
    /// The earliest instant the next request may start. Reserved-and-advanced
    /// under the lock; the lock is never held across the sleep.
    next_slot: std::sync::Mutex<tokio::time::Instant>,
}

impl RateLimiter {
    /// A limiter spacing request starts to at most `max_tps` per second.
    pub fn per_second(max_tps: u32) -> Self {
        assert!(max_tps > 0, "rate limit must be positive");
        Self {
            interval: Duration::from_secs(1) / max_tps,
            next_slot: std::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }

    /// Waits until this caller's reserved slot is due, then returns. Reserves the
    /// slot synchronously (so concurrent callers get distinct, spaced slots) and
    /// sleeps outside the lock.
    pub async fn acquire(&self) {
        let at = {
            let mut slot = self.next_slot.lock().expect("rate-limiter mutex poisoned");
            let at = (*slot).max(tokio::time::Instant::now());
            *slot = at + self.interval;
            at
        };
        tokio::time::sleep_until(at).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N back-to-back acquires against a K-per-second limiter span at least
    /// (N-1)/K seconds: the first is immediate, each subsequent one waits a full
    /// interval. Uses tokio's paused clock so the spacing is exact and instant.
    #[tokio::test(start_paused = true)]
    async fn acquire_spaces_requests_by_interval() {
        let limiter = RateLimiter::per_second(10); // 100 ms interval
        let start = tokio::time::Instant::now();
        for _ in 0..5 {
            limiter.acquire().await;
        }
        // 5 acquires => 4 intervals => 400 ms.
        assert_eq!(start.elapsed(), Duration::from_millis(400));
    }

    /// A stall longer than the interval does not bank credit: the slot is clamped
    /// to "now", so the next acquire fires immediately (no catch-up burst) rather
    /// than firing early against a slot left in the past.
    #[tokio::test(start_paused = true)]
    async fn acquire_does_not_burst_after_a_stall() {
        let limiter = RateLimiter::per_second(10); // 100 ms interval
        limiter.acquire().await; // reserves slot at t0, advances to t0+100ms

        // Simulate a slow call: sleep well past the reserved slot.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The next acquire is due immediately (slot is in the past, clamped to
        // now), and it must not let a following one fire early: that one waits a
        // full interval from now.
        let mark = tokio::time::Instant::now();
        limiter.acquire().await;
        assert_eq!(mark.elapsed(), Duration::from_millis(0));
        limiter.acquire().await;
        assert_eq!(mark.elapsed(), Duration::from_millis(100));
    }
}
