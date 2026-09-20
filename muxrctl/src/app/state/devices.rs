//! Push-device state: the Devices dialog's model, messages, and the
//! registration-age formatter the view renders.

use crate::server::devices::DeviceRecord;

/// Explanatory lines shown under the device list.
///
/// Rendered verbatim by `tui/views/devices_dialog.rs`; kept here so the copy
/// lives beside the state it explains rather than in the view.
pub const DEVICES_EXPLAIN: &[&str] = &[
    "Phones that registered for push notifications through this server. Removing one stops its notifications until the app registers again; it does not revoke the phone's token.",
    "Only a prefix of each push handle is shown — the full handle never leaves muxrd.",
];

/// State for the Devices dialog.
#[derive(Debug, Clone, Default)]
pub struct DevicesState {
    /// All registered push devices.
    pub devices: Vec<DeviceRecord>,
    /// The device name the list cursor is on, if any.
    pub focused: Option<String>,
    /// True while a load / remove task is in flight.
    pub loading: bool,
    /// The push-notification relay URL, or `None` when disabled. Resolved
    /// from the running daemon's `StatusInfo` when up, or from the effective
    /// config as a fallback when the daemon is stopped.
    pub relay_url: Option<String>,
}

impl DevicesState {
    /// The focused device's full record, if any (it may have been removed
    /// out from under the cursor by a concurrent reload).
    pub fn focused_device(&self) -> Option<&DeviceRecord> {
        let name = self.focused.as_deref()?;
        self.devices.iter().find(|d| d.device_name == name)
    }
}

/// Everything the Devices dialog can ask for.
#[derive(Debug, Clone)]
pub enum DevicesMsg {
    /// Close the dialog.
    Close,
    /// The list cursor moved onto this device.
    Focused(String),
    /// The user asked to remove the focused device; opens the confirmation.
    RemoveRequested,
    /// The confirmation was accepted: remove the focused device.
    RemoveConfirmed,
    /// The confirmation was dismissed without removing anything.
    RemoveCancelled,
    /// Reload the device list.
    Refresh,
}

/// Format a unix-epoch-seconds registration timestamp as a short "time ago"
/// string for display.
///
/// Pure (no I/O beyond reading the wall clock) so it is cheap to call every
/// render tick. A timestamp at or after "now" (clock skew, or freshly
/// registered) renders as "just now" rather than underflowing.
pub fn humanize_registered_at(epoch_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(epoch_secs);
    let elapsed = now.saturating_sub(epoch_secs);
    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn device(name: &str) -> DeviceRecord {
        DeviceRecord {
            device_name: name.to_string(),
            platform: "android".to_string(),
            registered_at: 0,
            handle_prefix: "abcd1234".to_string(),
        }
    }

    #[test]
    fn humanize_just_now() {
        assert_eq!(humanize_registered_at(now_secs()), "just now");
    }

    #[test]
    fn humanize_minutes_ago() {
        assert_eq!(humanize_registered_at(now_secs() - 300), "5m ago");
    }

    #[test]
    fn humanize_hours_ago() {
        assert_eq!(humanize_registered_at(now_secs() - 7_200), "2h ago");
    }

    #[test]
    fn humanize_days_ago() {
        assert_eq!(humanize_registered_at(now_secs() - 172_800), "2d ago");
    }

    #[test]
    fn humanize_future_timestamp_clamped_to_just_now() {
        // Clock skew: a registration timestamp slightly ahead of "now" must not
        // underflow (`saturating_sub`) — it renders as "just now".
        assert_eq!(humanize_registered_at(now_secs() + 10), "just now");
    }

    #[test]
    fn devices_explain_is_two_nonempty_lines() {
        assert_eq!(DEVICES_EXPLAIN.len(), 2);
        assert!(DEVICES_EXPLAIN.iter().all(|line| !line.is_empty()));
    }

    #[test]
    fn focused_device_looks_up_by_name() {
        let mut state = DevicesState {
            devices: vec![device("pixel"), device("ipad")],
            ..DevicesState::default()
        };
        state.focused = Some("ipad".to_string());
        assert_eq!(
            state.focused_device().map(|d| d.device_name.as_str()),
            Some("ipad")
        );
    }

    #[test]
    fn focused_device_is_none_when_nothing_is_focused_or_the_name_is_stale() {
        let mut state = DevicesState {
            devices: vec![device("pixel")],
            ..DevicesState::default()
        };
        assert!(state.focused_device().is_none());
        state.focused = Some("gone".to_string());
        assert!(state.focused_device().is_none());
    }
}
