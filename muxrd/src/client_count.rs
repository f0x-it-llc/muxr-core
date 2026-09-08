//! client_count — per-session count of mobile clients attached **through
//! muxrd** (Phase F).
//!
//! zellij 0.45.1 exposes no per-session connected-client count (its
//! `get_sessions()` returns only name + age), so we count what muxrd
//! itself knows: the number of live `AttachTerminal` relays per session.
//!
//! Lifecycle: [`SessionClients::attach`] increments the session's counter and
//! returns a [`ClientGuard`] that decrements on drop. The relay moves that guard
//! into its inbound task, so the count drops on **every** stream-end path
//! (clean close, error, token revocation, session exit) via `Drop`.
//!
//! The counter is a plain statistic (no data is synchronised through it), so
//! `Ordering::Relaxed` is sufficient.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;

/// Cloneable handle to the per-session attached-client registry.
///
/// Clones share the same underlying map (the `Arc`), so the gRPC service and
/// every relay observe one consistent set of counters.
#[derive(Clone, Default, Debug)]
pub struct SessionClients {
    inner: Arc<DashMap<String, Arc<AtomicUsize>>>,
}

impl SessionClients {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly-attached client for `session`; returns a guard that
    /// decrements the count when dropped.
    pub fn attach(&self, session: &str) -> ClientGuard {
        let counter = self
            .inner
            .entry(session.to_owned())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        counter.fetch_add(1, Ordering::Relaxed);
        ClientGuard { counter }
    }

    /// Current number of clients attached to `session` (0 if none).
    pub fn count(&self, session: &str) -> u32 {
        self.inner
            .get(session)
            .map(|c| c.load(Ordering::Relaxed) as u32)
            .unwrap_or(0)
    }

    /// Total number of clients attached across **all** sessions.
    ///
    /// Reads each counter with `Relaxed` ordering — suitable for reporting /
    /// diagnostics where a momentarily stale count is acceptable.
    pub fn total_count(&self) -> usize {
        self.inner
            .iter()
            .map(|entry| entry.value().load(Ordering::Relaxed))
            .sum()
    }
}

/// Decrements its session's attached-client counter on drop.
///
/// Holds the `Arc<AtomicUsize>` directly (not a map key) so the decrement is a
/// single atomic op with no map lookup and is robust regardless of map churn.
pub struct ClientGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_increments_and_drop_decrements() {
        let clients = SessionClients::new();
        assert_eq!(clients.count("s"), 0);

        let g1 = clients.attach("s");
        assert_eq!(clients.count("s"), 1);

        let g2 = clients.attach("s");
        assert_eq!(clients.count("s"), 2);

        drop(g1);
        assert_eq!(clients.count("s"), 1);

        drop(g2);
        assert_eq!(clients.count("s"), 0);
    }

    #[test]
    fn counts_are_per_session_and_unknown_is_zero() {
        let clients = SessionClients::new();
        let _a = clients.attach("alpha");
        assert_eq!(clients.count("alpha"), 1);
        assert_eq!(clients.count("beta"), 0);
    }

    #[test]
    fn clones_share_the_same_counters() {
        let clients = SessionClients::new();
        let other = clients.clone();
        let _g = clients.attach("shared");
        // The clone observes the increment made through the original handle.
        assert_eq!(other.count("shared"), 1);
    }

    #[test]
    fn total_count_sums_across_sessions() {
        let clients = SessionClients::new();
        assert_eq!(clients.total_count(), 0, "empty registry → 0");

        let _a1 = clients.attach("alpha");
        let _a2 = clients.attach("alpha");
        let _b1 = clients.attach("beta");
        assert_eq!(clients.total_count(), 3, "2 on alpha + 1 on beta");

        drop(_a1);
        assert_eq!(clients.total_count(), 2, "after dropping one alpha guard");

        drop(_b1);
        assert_eq!(clients.total_count(), 1, "only one alpha guard remaining");

        drop(_a2);
        assert_eq!(clients.total_count(), 0, "all guards dropped");
    }
}
