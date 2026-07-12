//! In-memory token-bucket rate limiting.
//!
//! State is process-local and resets on restart — acceptable for v1 (the
//! persisted daily cap in the store is the durable control; these buckets only
//! smooth bursts). Both limiters are keyed by an opaque string (push-handle or
//! client IP).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A keyed collection of token buckets with a shared capacity and refill rate.
struct TokenBuckets {
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl TokenBuckets {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Try to consume one token for `key`. Returns `true` if allowed.
    fn try_acquire(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.buckets.lock().expect("ratelimit mutex poisoned");
        let bucket = map.entry(key.to_string()).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The relay's two rate limiters.
pub struct RateLimiter {
    /// Per-handle notify burst: 10 tokens, refilling 10/minute.
    per_handle_burst: TokenBuckets,
    /// Per-IP register: 10 tokens, refilling 10/hour.
    per_ip_register: TokenBuckets,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            per_handle_burst: TokenBuckets::new(10.0, 10.0 / 60.0),
            per_ip_register: TokenBuckets::new(10.0, 10.0 / 3600.0),
        }
    }

    /// Consume one notify-burst token for `handle`.
    pub fn allow_notify(&self, handle: &str) -> bool {
        self.per_handle_burst.try_acquire(handle)
    }

    /// Consume one register token for `ip`.
    pub fn allow_register(&self, ip: &str) -> bool {
        self.per_ip_register.try_acquire(ip)
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_allows_ten_then_blocks() {
        let rl = RateLimiter::new();
        for i in 0..10 {
            assert!(rl.allow_notify("h"), "call {i} should pass");
        }
        assert!(!rl.allow_notify("h"), "11th rapid call must be blocked");
    }

    #[test]
    fn keys_are_independent() {
        let rl = RateLimiter::new();
        for _ in 0..10 {
            assert!(rl.allow_notify("a"));
        }
        assert!(!rl.allow_notify("a"));
        assert!(rl.allow_notify("b"), "a separate key has its own bucket");
    }
}
