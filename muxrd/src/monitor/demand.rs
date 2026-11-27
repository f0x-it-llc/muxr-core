//! demand — who is watching, and therefore how often the sampler runs.
//!
//! Each live subscription registers its tier; the effective rate is published on a watch channel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;

use super::model::Tier;

/// Interval while monitoring is enabled and nobody is subscribed; also the coarse history step.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(15);

/// Interval a [`Tier::Summary`] subscriber asks for.
pub const SUMMARY_INTERVAL: Duration = Duration::from_secs(5);

/// Floor of a [`Tier::Full`] request; the spike measured host totals at ~1.5 ms cpu per refresh.
pub const MIN_FULL_INTERVAL: Duration = Duration::from_secs(2);

/// Ceiling of a [`Tier::Full`] request.
pub const MAX_FULL_INTERVAL: Duration = Duration::from_secs(10);

/// Interval of [`Tier::full_default`].
pub const DEFAULT_FULL_INTERVAL: Duration = Duration::from_secs(3);

impl Tier {
    /// A `Full` tier at [`DEFAULT_FULL_INTERVAL`].
    pub fn full_default() -> Self {
        Self::Full {
            interval: DEFAULT_FULL_INTERVAL,
        }
    }

    /// The interval this tier asks for, after clamping.
    pub fn interval(self) -> Duration {
        match self {
            Self::Summary => SUMMARY_INTERVAL,
            Self::Full { interval } => interval.clamp(MIN_FULL_INTERVAL, MAX_FULL_INTERVAL),
        }
    }

    fn is_full(self) -> bool {
        matches!(self, Self::Full { .. })
    }
}

/// What the sampler must deliver right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DemandLevel {
    /// The smallest interval any live subscriber asks for, or [`IDLE_INTERVAL`].
    pub interval: Duration,
    /// At least one `Full` subscriber exists: feed the fine history ring and read temperatures.
    pub full: bool,
}

impl DemandLevel {
    /// Nobody is subscribed.
    pub const IDLE: Self = Self {
        interval: IDLE_INTERVAL,
        full: false,
    };
}

#[derive(Debug)]
struct Entries {
    next_id: u64,
    tiers: HashMap<u64, Tier>,
}

#[derive(Debug)]
struct Inner {
    entries: Mutex<Entries>,
    level: watch::Sender<DemandLevel>,
}

/// The live subscribers' tiers. Cheap to clone; every clone shares one registry.
#[derive(Debug, Clone)]
pub(crate) struct Demand {
    inner: Arc<Inner>,
}

impl Default for Demand {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(Entries {
                    next_id: 0,
                    tiers: HashMap::new(),
                }),
                level: watch::Sender::new(DemandLevel::IDLE),
            }),
        }
    }
}

impl Demand {
    fn entries(&self) -> MutexGuard<'_, Entries> {
        // A poisoned lock still holds a valid map: every write is a single insert or remove.
        self.inner
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register a subscriber; its demand lasts until the guard drops or [`Demand::clear`].
    pub(crate) fn register(&self, tier: Tier) -> DemandGuard {
        let mut entries = self.entries();
        let id = entries.next_id;
        entries.next_id += 1;
        entries.tiers.insert(id, tier);
        self.publish(&entries);
        DemandGuard {
            demand: self.clone(),
            id,
        }
    }

    /// Forget every subscriber (monitoring was disabled). Guards dropped later are no-ops.
    pub(crate) fn clear(&self) {
        let mut entries = self.entries();
        entries.tiers.clear();
        self.publish(&entries);
    }

    /// The current level.
    pub(crate) fn level(&self) -> DemandLevel {
        *self.inner.level.borrow()
    }

    /// A receiver that observes every level change.
    pub(crate) fn watch(&self) -> watch::Receiver<DemandLevel> {
        self.inner.level.subscribe()
    }

    fn release(&self, id: u64) {
        let mut entries = self.entries();
        if entries.tiers.remove(&id).is_some() {
            self.publish(&entries);
        }
    }

    fn publish(&self, entries: &Entries) {
        let level = level_of(entries.tiers.values().copied());
        self.inner.level.send_if_modified(|current| {
            let changed = *current != level;
            *current = level;
            changed
        });
    }
}

/// The level a set of tiers asks for.
fn level_of(tiers: impl Iterator<Item = Tier>) -> DemandLevel {
    tiers.fold(DemandLevel::IDLE, |level, tier| DemandLevel {
        interval: level.interval.min(tier.interval()),
        full: level.full || tier.is_full(),
    })
}

/// One subscriber's registered demand; dropping it releases the demand.
#[derive(Debug)]
pub(crate) struct DemandGuard {
    demand: Demand,
    id: u64,
}

impl Drop for DemandGuard {
    fn drop(&mut self) {
        self.demand.release(self.id);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn full(secs_ms: u64) -> Tier {
        Tier::Full {
            interval: Duration::from_millis(secs_ms),
        }
    }

    #[test]
    fn full_requests_are_clamped_to_the_floor_and_ceiling() {
        assert_eq!(full(500).interval(), MIN_FULL_INTERVAL);
        assert_eq!(full(3_000).interval(), Duration::from_secs(3));
        assert_eq!(full(60_000).interval(), MAX_FULL_INTERVAL);
        assert_eq!(Tier::full_default().interval(), DEFAULT_FULL_INTERVAL);
        assert_eq!(Tier::Summary.interval(), SUMMARY_INTERVAL);
    }

    #[test]
    fn the_smallest_live_request_wins_and_drops_walk_back_up() {
        let demand = Demand::default();
        assert_eq!(demand.level(), DemandLevel::IDLE);

        let summary = demand.register(Tier::Summary);
        assert_eq!(
            demand.level(),
            DemandLevel {
                interval: SUMMARY_INTERVAL,
                full: false
            }
        );
        let fast = demand.register(full(3_000));
        assert_eq!(
            demand.level(),
            DemandLevel {
                interval: Duration::from_secs(3),
                full: true
            }
        );
        drop(fast);
        assert_eq!(demand.level().interval, SUMMARY_INTERVAL);
        assert!(!demand.level().full);
        drop(summary);
        assert_eq!(demand.level(), DemandLevel::IDLE);
    }

    #[test]
    fn clear_forgets_everyone_and_later_drops_are_harmless() {
        let demand = Demand::default();
        let guard = demand.register(full(2_000));
        demand.clear();
        assert_eq!(demand.level(), DemandLevel::IDLE);
        let summary = demand.register(Tier::Summary);
        drop(guard);
        assert_eq!(
            demand.level().interval,
            SUMMARY_INTERVAL,
            "stale guard kept out"
        );
        drop(summary);
    }

    #[test]
    fn the_watch_sees_level_changes() {
        let demand = Demand::default();
        let mut rx = demand.watch();
        rx.mark_unchanged();
        let guard = demand.register(Tier::Summary);
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        let second = demand.register(Tier::Summary);
        assert!(!rx.has_changed().unwrap(), "same level is not a change");
        drop((guard, second));
        assert!(rx.has_changed().unwrap());
    }
}
