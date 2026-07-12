//! Neutral multiplexer **event** types and the internal broadcast bus.
//!
//! The herdr event kernel ([`herdr::subscribe`](crate::multiplexer::herdr::subscribe))
//! consumes herdr's JSON-API `events.subscribe` stream and republishes each event
//! onto a process-local [`tokio::sync::broadcast`] channel of [`MuxEvent`]s. This
//! is the shared foundation for the notifications notifier (task 05) and the
//! future agents screen — both are just *consumers* of this same bus.
//!
//! ## Design notes
//!
//! - The types here are **neutral**: they carry muxrd's own numeric pane ids
//!   (registry-translated from herdr's opaque `String`s) and never leak
//!   `herdr::api` shapes above this seam — the event-bus analogue of the neutral
//!   [`types`](crate::multiplexer::types) boundary.
//! - The bus is **lossy by design**. Consumers that fall behind receive
//!   [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged) and
//!   must tolerate it (agent status is a level, not an edge — a fresh event or a
//!   post-reconnect resync re-establishes truth). Capacity is
//!   [`EVENT_BUS_CAPACITY`].
//! - Distinct from [`types::MuxEvent`](crate::multiplexer::types::MuxEvent), which
//!   is the *render-stream* event carried inside a live terminal attach. This
//!   `MuxEvent` is the *daemon-side* event bus; the two never mix, so this module
//!   is referenced module-qualified (`events::MuxEvent`) rather than re-exported
//!   flat at the `multiplexer` level.

use tokio::sync::broadcast;

/// Broadcast-bus capacity. Agent-status transitions are infrequent (human-paced
/// agent activity), so 256 buffered events is generous headroom; a consumer that
/// still lags simply observes [`Lagged`](tokio::sync::broadcast::error::RecvError::Lagged)
/// and reconciles from the next event / resync.
pub const EVENT_BUS_CAPACITY: usize = 256;

/// High-level agent activity status for a pane — muxrd's neutral mirror of
/// herdr's `AgentStatus`, kept independent so no `herdr::api` type crosses this
/// seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// No agent, or an agent at rest.
    Idle,
    /// Agent actively producing output.
    Working,
    /// Agent is waiting for user input ("your agent needs you").
    Blocked,
    /// Agent process finished and the pane has not been viewed yet.
    Done,
    /// State could not be classified.
    Unknown,
}

/// A pane's agent status changed (or, for a post-reconnect resync, its current
/// status was observed). Pane ids are muxrd's neutral `u32` (registry-translated);
/// the herdr `workspace_id` is carried raw for downstream routing/labelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatusChanged {
    /// Neutral pane id (registry-translated from herdr's opaque `pane_id`).
    pub pane: u32,
    /// herdr workspace id (raw opaque string) the pane belongs to.
    pub workspace_id: String,
    /// Human-readable workspace label, when known (populated on resync; a pushed
    /// event may omit it).
    pub workspace_name: Option<String>,
    /// The pane's agent status.
    pub status: AgentStatus,
    /// Pane / agent title, when herdr carries one.
    pub title: Option<String>,
    /// `true` when emitted by the post-reconnect resync (a re-observation of
    /// current state), `false` for a live pushed transition.
    pub synthetic: bool,
}

/// A neutral event published on the internal bus.
///
/// Currently a single variant; kept as an enum so the notifier / agents-screen
/// consumers can grow to further herdr event families
/// (`pane.exited`, `workspace.closed`, …) without a breaking bus-type change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuxEvent {
    /// A pane's agent status changed (or was re-observed on resync).
    AgentStatusChanged(AgentStatusChanged),
}

/// The producer half of the internal event bus. The event kernel holds one and
/// publishes; consumers obtain a receiver via [`broadcast::Sender::subscribe`].
pub type EventBus = broadcast::Sender<MuxEvent>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_delivers_events_to_subscribers() {
        let (tx, _rx0): (EventBus, _) = broadcast::channel(EVENT_BUS_CAPACITY);
        let mut rx = tx.subscribe();
        let ev = MuxEvent::AgentStatusChanged(AgentStatusChanged {
            pane: 3,
            workspace_id: "ws-1".into(),
            workspace_name: Some("main".into()),
            status: AgentStatus::Blocked,
            title: Some("claude".into()),
            synthetic: false,
        });
        tx.send(ev.clone()).expect("a subscriber exists");
        assert_eq!(rx.try_recv().expect("one buffered event"), ev);
    }

    #[test]
    fn send_with_no_receivers_is_a_recoverable_error() {
        // The kernel publishes even when no consumer is attached yet (task 05
        // is the first). `send` must simply report "no receivers", never panic.
        let (tx, rx): (EventBus, _) = broadcast::channel(EVENT_BUS_CAPACITY);
        drop(rx);
        let ev = MuxEvent::AgentStatusChanged(AgentStatusChanged {
            pane: 1,
            workspace_id: "ws".into(),
            workspace_name: None,
            status: AgentStatus::Done,
            title: None,
            synthetic: true,
        });
        assert!(tx.send(ev).is_err(), "no receivers → Err, not a panic");
    }
}
