//! Push-device state — a stub for the devices card.

use crate::server::devices::DeviceRecord;

/// State for the Devices dialog.
#[derive(Debug, Clone, Default)]
pub struct DevicesState {
    /// All registered push devices.
    pub devices: Vec<DeviceRecord>,
    /// Index of the highlighted row.
    #[allow(dead_code)] // consumed by the devices card.
    pub cursor: usize,
    /// True while a load / remove task is in flight.
    pub loading: bool,
    /// The push-notification relay URL, or `None` when disabled. Resolved
    /// from the running daemon's `StatusInfo` when up, or from the effective
    /// config as a fallback when the daemon is stopped.
    pub relay_url: Option<String>,
}

impl DevicesState {
    /// The selected device's display name, if any.
    #[allow(dead_code)] // consumed by the devices card.
    pub fn selected_name(&self) -> Option<&str> {
        self.devices
            .get(self.cursor)
            .map(|d| d.device_name.as_str())
    }
}

/// Everything the Devices dialog can ask for.
#[derive(Debug, Clone)]
pub enum DevicesMsg {
    /// Close the dialog.
    Close,
}
