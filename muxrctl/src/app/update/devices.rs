//! The push-device reducer — a stub for the devices card.

use crate::app::action::UpdateAction;
use crate::app::state::AppState;
use crate::app::state::devices::DevicesMsg;
use crate::server::devices::DeviceRecord;

/// Apply a [`DevicesMsg`]. The devices card extends this.
pub fn update(state: &mut AppState, msg: DevicesMsg) -> Vec<UpdateAction> {
    match msg {
        DevicesMsg::Close => super::close_top(state),
    }
}

/// The device list came back.
pub fn on_devices_loaded(
    state: &mut AppState,
    devices: Vec<DeviceRecord>,
    relay_url: Option<String>,
) -> Vec<UpdateAction> {
    state.devices.devices = devices;
    if state.devices.cursor >= state.devices.devices.len() {
        state.devices.cursor = state.devices.devices.len().saturating_sub(1);
    }
    state.devices.relay_url = relay_url;
    state.devices.loading = false;
    Vec::new()
}

/// A device was removed; the list needs a reload.
pub fn on_devices_changed(state: &mut AppState) -> Vec<UpdateAction> {
    state.devices.loading = true;
    vec![UpdateAction::LoadDevices]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::{Message, UiMsg};
    use crate::app::state::DialogId;
    use crate::app::update::update;

    fn device(name: &str) -> DeviceRecord {
        DeviceRecord {
            device_name: name.to_string(),
            platform: "android".to_string(),
            registered_at: 0,
            handle_prefix: "abcd1234".to_string(),
        }
    }

    #[test]
    fn devices_loaded_updates_the_list_and_relay_url() {
        let mut state = AppState::new();
        state.devices.loading = true;
        update(
            &mut state,
            Message::DevicesLoaded {
                devices: vec![device("pixel"), device("ipad")],
                relay_url: Some("https://relay.example".to_string()),
            },
        );
        assert_eq!(state.devices.devices.len(), 2);
        assert_eq!(
            state.devices.relay_url.as_deref(),
            Some("https://relay.example")
        );
        assert!(!state.devices.loading);
        assert_eq!(state.devices.selected_name(), Some("pixel"));
    }

    #[test]
    fn devices_loaded_empty_list_clamps_the_cursor() {
        let mut state = AppState::new();
        state.devices.cursor = 3;
        update(
            &mut state,
            Message::DevicesLoaded {
                devices: Vec::new(),
                relay_url: None,
            },
        );
        assert_eq!(state.devices.cursor, 0);
        assert_eq!(state.devices.selected_name(), None);
    }

    #[test]
    fn devices_changed_triggers_a_reload() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::DevicesChanged);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadDevices))
        );
    }

    #[test]
    fn close_closes_the_devices_dialog_and_refreshes_it() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Devices)));
        let actions = update(&mut state, Message::Ui(UiMsg::Devices(DevicesMsg::Close)));
        assert_eq!(state.top_dialog(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadDevices))
        );
    }
}
