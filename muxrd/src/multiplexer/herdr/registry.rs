//! Stable id registries bridging herdr's opaque `String` ids and muxrd's numeric
//! ids.
//!
//! The rest of muxrd (proto wire, relay, gRPC handlers) speaks **numeric** ids:
//! panes are addressed by `PaneRef { id: u32 }` and tabs by `TabSnapshot.tab_id:
//! u64`. herdr instead identifies panes and tabs by opaque `String`s (`pane_id`,
//! `tab_id`) and addresses the terminal relay by a separate `terminal_id`
//! `String`.
//!
//! These registries assign a **stable, monotonic** numeric id to each herdr
//! `String` id the first time it is seen, and keep the mapping for the lifetime
//! of the backend instance. Stability matters: a layout snapshot hands the client
//! a `PaneRef { id }`, and a later client action (`focus`, `close`, …) round-trips
//! that same `id` back — so the same herdr `pane_id` must always resolve to the
//! same `u32` across repeated layout polls.
//!
//! ## Lifecycle / pruning
//! Entries are **never pruned**. A closed pane's id is simply never looked up
//! again; leaving it in place guarantees ids stay stable and monotonic and avoids
//! any chance of id reuse aliasing a stale client `PaneRef`. Memory grows with the
//! total number of *distinct* panes/tabs seen over the backend's lifetime, which
//! is bounded in practice; the whole registry is dropped when the backend
//! (P2.04) tears down.
//!
//! ## Id 0
//! Numeric ids start at **1**. proto3 scalar fields default to `0` when unset, so
//! reserving `0` keeps a real pane/tab id distinguishable from an absent one and
//! sidesteps any accidental sentinel collision at the gRPC boundary.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// First numeric id handed out by either registry (see module docs on why not 0).
const FIRST_ID: u32 = 1;

/// One pane's herdr identity, keyed by its assigned `u32`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneEntry {
    /// herdr opaque `pane_id` — used for control-plane addressing.
    herdr_pane_id: String,
    /// herdr opaque `terminal_id` — the wire-relay attach key
    /// (`wire::ClientMessage::AttachTerminal { terminal_id }`, P2.03).
    terminal_id: String,
}

#[derive(Debug, Default)]
struct PaneInner {
    next_id: u32,
    by_herdr: HashMap<String, u32>,
    by_id: HashMap<u32, PaneEntry>,
}

/// Bidirectional `u32 ↔ herdr pane_id` map that also stores each pane's
/// `terminal_id` for the wire relay. Thread-safe; cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct HerdrPaneRegistry {
    inner: Mutex<PaneInner>,
}

impl Default for HerdrPaneRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HerdrPaneRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PaneInner {
                next_id: FIRST_ID,
                by_herdr: HashMap::new(),
                by_id: HashMap::new(),
            }),
        }
    }

    /// Return the stable `u32` for a herdr `pane_id`, assigning a fresh one on
    /// first sight. The pane's `terminal_id` is recorded (and refreshed if herdr
    /// reports a new one for an existing pane).
    pub fn assign_or_get(&self, herdr_pane_id: &str, terminal_id: &str) -> u32 {
        let mut inner = lock(&self.inner);
        if let Some(&id) = inner.by_herdr.get(herdr_pane_id) {
            if let Some(entry) = inner.by_id.get_mut(&id)
                && entry.terminal_id != terminal_id
            {
                entry.terminal_id = terminal_id.to_string();
            }
            return id;
        }
        let id = inner.next_id;
        match inner.next_id.checked_add(1) {
            Some(next) => inner.next_id = next,
            // u32 pane-id space is astronomically large in practice; if it is ever
            // exhausted, log and leave `next_id` pinned at u32::MAX rather than
            // panic in the relay/gRPC path. Subsequent assigns reuse that id.
            None => log::error!("herdr pane registry id space exhausted (u32)"),
        }
        inner.by_herdr.insert(herdr_pane_id.to_string(), id);
        inner.by_id.insert(
            id,
            PaneEntry {
                herdr_pane_id: herdr_pane_id.to_string(),
                terminal_id: terminal_id.to_string(),
            },
        );
        id
    }

    /// The stable `u32` already assigned to a herdr `pane_id`, if any — a
    /// **read-only** lookup that never assigns a new id nor touches the stored
    /// `terminal_id`.
    ///
    /// The event kernel ([`super::subscribe`]) uses this to translate a pushed
    /// `pane_agent_status_changed` event — which may omit `terminal_id` — to a
    /// neutral id without risking a clobber of a live pane's relay `terminal_id`
    /// via [`Self::assign_or_get`] with an empty string.
    pub fn id_for_herdr_pane(&self, herdr_pane_id: &str) -> Option<u32> {
        lock(&self.inner).by_herdr.get(herdr_pane_id).copied()
    }

    /// herdr `pane_id` for an assigned `u32`, if known.
    pub fn herdr_pane_id(&self, id: u32) -> Option<String> {
        lock(&self.inner)
            .by_id
            .get(&id)
            .map(|e| e.herdr_pane_id.clone())
    }

    /// herdr `terminal_id` (wire-relay attach key) for an assigned `u32`, if known.
    pub fn terminal_id(&self, id: u32) -> Option<String> {
        lock(&self.inner)
            .by_id
            .get(&id)
            .map(|e| e.terminal_id.clone())
    }
}

#[derive(Debug, Default)]
struct TabInner {
    next_id: u64,
    by_herdr: HashMap<String, u64>,
    by_id: HashMap<u64, String>,
}

/// Bidirectional `u64 ↔ herdr tab_id` map. Parallel to [`HerdrPaneRegistry`];
/// tabs round-trip through the neutral `TabSnapshot.tab_id: u64` the same way
/// panes round-trip through `PaneRef.id: u32`.
#[derive(Debug)]
pub struct HerdrTabRegistry {
    inner: Mutex<TabInner>,
}

impl Default for HerdrTabRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HerdrTabRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(TabInner {
                next_id: FIRST_ID as u64,
                by_herdr: HashMap::new(),
                by_id: HashMap::new(),
            }),
        }
    }

    /// Return the stable `u64` for a herdr `tab_id`, assigning a fresh one on
    /// first sight.
    pub fn assign_or_get(&self, herdr_tab_id: &str) -> u64 {
        let mut inner = lock(&self.inner);
        if let Some(&id) = inner.by_herdr.get(herdr_tab_id) {
            return id;
        }
        let id = inner.next_id;
        match inner.next_id.checked_add(1) {
            Some(next) => inner.next_id = next,
            // See the pane registry: u64 tab-id space is unreachable in practice;
            // log and pin rather than panic.
            None => log::error!("herdr tab registry id space exhausted (u64)"),
        }
        inner.by_herdr.insert(herdr_tab_id.to_string(), id);
        inner.by_id.insert(id, herdr_tab_id.to_string());
        id
    }

    /// herdr `tab_id` for an assigned `u64`, if known.
    pub fn herdr_tab_id(&self, id: u64) -> Option<String> {
        lock(&self.inner).by_id.get(&id).cloned()
    }
}

/// Lock a mutex, recovering the guard if a previous holder panicked. The
/// registries hold only plain maps, so a poisoned lock leaves consistent data —
/// recovering is preferable to propagating a panic into the relay/gRPC path.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_ids_start_at_one_and_increment() {
        let reg = HerdrPaneRegistry::new();
        assert_eq!(reg.assign_or_get("pane-a", "term-a"), 1);
        assert_eq!(reg.assign_or_get("pane-b", "term-b"), 2);
    }

    #[test]
    fn pane_id_is_stable_across_repeated_assigns() {
        let reg = HerdrPaneRegistry::new();
        let first = reg.assign_or_get("pane-a", "term-a");
        // Insert another pane in between, then re-poll the original.
        reg.assign_or_get("pane-b", "term-b");
        let again = reg.assign_or_get("pane-a", "term-a");
        assert_eq!(first, again, "same pane_id must map to the same u32");
    }

    #[test]
    fn pane_round_trips_id_to_herdr_id_and_terminal_id() {
        let reg = HerdrPaneRegistry::new();
        let id = reg.assign_or_get("pane-xyz", "term-123");
        assert_eq!(reg.herdr_pane_id(id).as_deref(), Some("pane-xyz"));
        assert_eq!(reg.terminal_id(id).as_deref(), Some("term-123"));
    }

    #[test]
    fn pane_terminal_id_refreshes_on_reassign() {
        let reg = HerdrPaneRegistry::new();
        let id = reg.assign_or_get("pane-a", "term-old");
        let same = reg.assign_or_get("pane-a", "term-new");
        assert_eq!(id, same);
        assert_eq!(reg.terminal_id(id).as_deref(), Some("term-new"));
    }

    #[test]
    fn unknown_pane_id_returns_none() {
        let reg = HerdrPaneRegistry::new();
        assert!(reg.herdr_pane_id(999).is_none());
        assert!(reg.terminal_id(999).is_none());
    }

    #[test]
    fn id_for_herdr_pane_is_read_only_lookup() {
        let reg = HerdrPaneRegistry::new();
        assert!(reg.id_for_herdr_pane("pane-a").is_none());
        let id = reg.assign_or_get("pane-a", "term-a");
        assert_eq!(reg.id_for_herdr_pane("pane-a"), Some(id));
        // A pure lookup must not have assigned a new id nor altered terminal_id.
        assert_eq!(reg.terminal_id(id).as_deref(), Some("term-a"));
        assert!(reg.id_for_herdr_pane("pane-unknown").is_none());
    }

    #[test]
    fn tab_ids_start_at_one_and_round_trip() {
        let reg = HerdrTabRegistry::new();
        assert_eq!(reg.assign_or_get("tab-a"), 1);
        assert_eq!(reg.assign_or_get("tab-b"), 2);
        // Stable on re-poll.
        assert_eq!(reg.assign_or_get("tab-a"), 1);
        assert_eq!(reg.herdr_tab_id(1).as_deref(), Some("tab-a"));
        assert_eq!(reg.herdr_tab_id(2).as_deref(), Some("tab-b"));
        assert!(reg.herdr_tab_id(99).is_none());
    }
}
