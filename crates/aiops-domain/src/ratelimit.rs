//! A token bucket with the clock passed in, so the policy is testable without sleeping.

use crate::time::MICROS_PER_SECOND;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_micros: i64,
}

/// How long until the requested amount could be taken, rounded up to whole seconds
/// (what `Retry-After` carries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryAfter(pub u64);

impl TokenBucket {
    /// A full bucket.
    pub fn new(capacity: u32, refill_per_sec: u32, now_micros: i64) -> Self {
        Self {
            capacity: f64::from(capacity),
            refill_per_sec: f64::from(refill_per_sec.max(1)),
            tokens: f64::from(capacity),
            last_micros: now_micros,
        }
    }

    /// Takes `amount` tokens or refuses the whole amount (no partial batches).
    pub fn try_take(&mut self, amount: u32, now_micros: i64) -> Result<(), RetryAfter> {
        let elapsed = now_micros.saturating_sub(self.last_micros).max(0);
        #[expect(
            clippy::cast_precision_loss,
            reason = "elapsed microseconds fit f64 for any realistic gap"
        )]
        let refill = elapsed as f64 / MICROS_PER_SECOND as f64 * self.refill_per_sec;
        self.tokens = (self.tokens + refill).min(self.capacity);
        self.last_micros = now_micros.max(self.last_micros);
        let want = f64::from(amount);
        if want <= self.tokens {
            self.tokens -= want;
            return Ok(());
        }
        let deficit = (want.min(self.capacity) - self.tokens).max(0.0);
        let secs = (deficit / self.refill_per_sec).ceil().max(1.0);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "secs is ≥ 1 and bounded by capacity/refill"
        )]
        Err(RetryAfter(secs as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_whole_batches_and_refills_over_time() {
        let mut b = TokenBucket::new(100, 50, 0);
        assert!(b.try_take(80, 0).is_ok());
        assert_eq!(b.try_take(30, 0), Err(RetryAfter(1)));
        // Half a second refills 25 tokens: 20 + 25 = 45.
        assert!(b.try_take(45, 500_000).is_ok());
        assert!(b.try_take(1, 500_000).is_err());
    }

    #[test]
    fn retry_after_grows_with_the_deficit_and_a_backwards_clock_is_ignored() {
        let mut b = TokenBucket::new(1_000, 100, 10_000_000);
        assert!(b.try_take(1_000, 10_000_000).is_ok());
        assert_eq!(b.try_take(500, 10_000_000), Err(RetryAfter(5)));
        assert_eq!(b.try_take(500, 0), Err(RetryAfter(5)));
    }
}
