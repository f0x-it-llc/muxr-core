//! Daemon status state: what the dashboard's Daemon section renders and what
//! the start / stop controls drive.

use super::ServerInfo;

/// State for the daemon panel.
#[derive(Debug, Clone, Default)]
pub struct ServerPanelState {
    /// Last-known server status; `None` means not yet fetched.
    pub status: Option<ServerInfo>,
    /// True if we know the server is not running (Stopped).
    pub stopped: bool,
    /// True while a start/stop/refresh task is in flight.
    pub loading: bool,
    /// Tick counter used to drive the ~1 s live poll cadence.
    pub tick_counter: u32,
}

impl ServerPanelState {
    /// Whether the daemon is known to be up right now.
    pub fn is_running(&self) -> bool {
        self.status.is_some() && !self.stopped
    }
}

/// Everything the daemon controls can ask for.
#[derive(Debug, Clone)]
pub enum ServerMsg {
    /// Launch the daemon.
    Start,
    /// Ask for confirmation before stopping (opens the stop dialog).
    StopRequested,
    /// Confirmed in the stop dialog: close it and stop the daemon.
    StopConfirmed,
    /// Cancelled in the stop dialog: just close it.
    StopCancelled,
    /// Re-query the daemon status now. (Emitted by the cert and wizard cards,
    /// which wait on the daemon coming up.)
    #[allow(dead_code)]
    Refresh,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ServerInfo {
        ServerInfo {
            version: "0.1.0".to_string(),
            bind_addr: "127.0.0.1:50051".to_string(),
            pid: 4242,
            uptime_secs: 12,
            client_count: 0,
            notify_relay_url: None,
            push_device_count: 0,
        }
    }

    #[test]
    fn is_running_needs_status_and_not_stopped() {
        let mut state = ServerPanelState::default();
        assert!(!state.is_running());
        state.status = Some(info());
        assert!(state.is_running());
        state.stopped = true;
        assert!(!state.is_running());
    }
}
