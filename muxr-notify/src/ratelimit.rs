//! In-memory token-bucket rate limiting.
//!
//! State is process-local and resets on restart — acceptable for v1 (the
//! persisted daily cap in the store is the durable control; these buckets only
//! smooth bursts). Limiters are keyed by an opaque string (push-handle or
//! client IP).
//!
//! # Bounded memory
//! On a public relay the keys (IPs, and — via a spoofable `X-Forwarded-For` —
//! attacker-mintable ones) could grow the bucket maps without bound. Each
//! `try_acquire` therefore performs opportunistic maintenance: entries that have
//! been idle long enough to be provably back at full capacity are dropped, and a
//! hard cap on distinct keys is enforced by evicting the oldest-touched entries.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Hard cap on distinct keys per bucket map. Keys are attacker-influenced on a
/// public relay, so this bounds memory; when exceeded the oldest-touched entries
/// are evicted.
const MAX_KEYS: usize = 10_000;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A keyed collection of token buckets with a shared capacity and refill rate.
struct TokenBuckets {
    capacity: f64,
    refill_per_sec: f64,
    /// Once a bucket has been untouched for this long it is provably refilled to
    /// `capacity` (empty→full takes `capacity / refill_per_sec` seconds), so it
    /// carries no state worth keeping and is dropped. `Duration::MAX` when
    /// `refill_per_sec == 0` (idle sweep disabled).
    idle_horizon: Duration,
    hard_cap: usize,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl TokenBuckets {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self::with_limits(capacity, refill_per_sec, MAX_KEYS)
    }

    fn with_limits(capacity: f64, refill_per_sec: f64, hard_cap: usize) -> Self {
        let idle_horizon = if refill_per_sec > 0.0 {
            Duration::from_secs_f64(capacity / refill_per_sec)
        } else {
            Duration::MAX
        };
        Self {
            capacity,
            refill_per_sec,
            idle_horizon,
            hard_cap,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Try to consume one token for `key`. Returns `true` if allowed.
    fn try_acquire(&self, key: &str) -> bool {
        self.try_acquire_at(key, Instant::now())
    }

    /// `try_acquire` with an explicit clock, for deterministic tests.
    fn try_acquire_at(&self, key: &str, now: Instant) -> bool {
        let mut map = self.buckets.lock().expect("ratelimit mutex poisoned");
        self.evict(&mut map, key, now);

        let bucket = map.entry(key.to_string()).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Opportunistic maintenance: drop idle (fully-refilled) entries, then evict
    /// the oldest-touched keys until inserting `keep` will keep us within cap.
    fn evict(&self, map: &mut HashMap<String, Bucket>, keep: &str, now: Instant) {
        if self.idle_horizon != Duration::MAX {
            let horizon = self.idle_horizon;
            map.retain(|_, b| now.saturating_duration_since(b.last) < horizon);
        }
        while !map.contains_key(keep) && map.len() >= self.hard_cap {
            match map
                .iter()
                .min_by_key(|(_, b)| b.last)
                .map(|(k, _)| k.clone())
            {
                Some(oldest) => {
                    map.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// The relay's rate limiters.
pub struct RateLimiter {
    /// Per-handle notify burst: 10 tokens, refilling 10/minute.
    per_handle_burst: TokenBuckets,
    /// Per-IP register: 10 tokens, refilling 10/hour.
    per_ip_register: TokenBuckets,
    /// Per-IP request throttle for /v1/notify + DELETE /v1/registrations:
    /// 60 tokens, refilling 1/second (≈60/minute). Applied before any store or
    /// mutex work to bound the single-mutex DoS surface.
    per_ip_request: TokenBuckets,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            per_handle_burst: TokenBuckets::new(10.0, 10.0 / 60.0),
            per_ip_register: TokenBuckets::new(10.0, 10.0 / 3600.0),
            per_ip_request: TokenBuckets::new(60.0, 1.0),
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

    /// Consume one per-IP request token (notify / delete). Call before any
    /// store or mutex lookup.
    pub fn allow_request(&self, ip: &str) -> bool {
        self.per_ip_request.try_acquire(ip)
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

    #[test]
    fn idle_entries_are_evicted() {
        // refill 10/s ⇒ idle horizon = capacity/refill = 1s.
        let tb = TokenBuckets::with_limits(10.0, 10.0, MAX_KEYS);
        let t0 = Instant::now();
        assert!(tb.try_acquire_at("stale", t0));
        // Touch a different key well past the horizon → the sweep drops "stale".
        assert!(tb.try_acquire_at("fresh", t0 + Duration::from_secs(5)));
        let map = tb.buckets.lock().unwrap();
        assert!(!map.contains_key("stale"), "idle bucket should be evicted");
        assert!(map.contains_key("fresh"));
    }

    #[test]
    fn active_entries_survive() {
        let tb = TokenBuckets::with_limits(10.0, 10.0, MAX_KEYS);
        let t0 = Instant::now();
        assert!(tb.try_acquire_at("a", t0));
        assert!(tb.try_acquire_at("a", t0 + Duration::from_millis(100)));
        assert!(tb.try_acquire_at("b", t0 + Duration::from_millis(200)));
        let map = tb.buckets.lock().unwrap();
        assert!(map.contains_key("a"), "recently-touched key survives");
        assert!(map.contains_key("b"));
    }

    #[test]
    fn hard_cap_evicts_oldest() {
        // Long horizon (600s) so the idle sweep never fires — isolate the cap.
        let tb = TokenBuckets::with_limits(10.0, 10.0 / 60.0, 3);
        let t0 = Instant::now();
        for (i, k) in ["a", "b", "c"].iter().enumerate() {
            assert!(tb.try_acquire_at(k, t0 + Duration::from_millis(i as u64)));
        }
        // A 4th distinct key must evict the oldest-touched ("a"), staying at cap.
        assert!(tb.try_acquire_at("d", t0 + Duration::from_millis(3)));
        let map = tb.buckets.lock().unwrap();
        assert!(map.len() <= 3, "must not exceed the hard cap");
        assert!(!map.contains_key("a"), "oldest-touched key evicted");
        assert!(map.contains_key("d"), "newest key inserted");
    }
}
