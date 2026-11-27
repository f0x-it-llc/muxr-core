//! Event kernel: tmux control-mode notifications fed into the event bus.

use std::sync::Arc;

use super::agent_state::AgentState;
use super::backend::TmuxBackend;
use crate::multiplexer::events::EventBus;

/// Start the tmux event kernel and return its task handle.
///
/// The task currently ends at once; the watcher replaces the body without
/// changing this signature.
pub fn spawn_tmux_event_kernel(
    backend: &TmuxBackend,
    state: Arc<AgentState>,
    bus: EventBus,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let _ = (backend, state, bus, shutdown);
    tokio::spawn(async {})
}
