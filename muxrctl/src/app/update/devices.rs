//! The Devices dialog's reducer: cursor, remove-behind-a-confirmation, and
//! the async load/remove results.

use crate::app::action::UpdateAction;
use crate::app::state::devices::DevicesMsg;
use crate::app::state::{AppState, DialogId};
use crate::server::devices::DeviceRecord;

/// Apply a [`DevicesMsg`].
pub fn update(state: &mut AppState, msg: DevicesMsg) -> Vec<UpdateAction> {
    match msg {
        DevicesMsg::Close => super::close_top(state),
        DevicesMsg::Focused(name) => {
            state.devices.focused = Some(name);
            Vec::new()
        }
        DevicesMsg::RemoveRequested => {
            if state.devices.focused.is_some() {
                super::open_hook(state, DialogId::DeviceRemoveConfirm)
            } else {
                Vec::new()
            }
        }
        DevicesMsg::RemoveConfirmed => {
            let mut actions = super::close_top(state);
            // Look the name up through the still-registered device rather than
            // trusting the stored name alone, so a confirm that outlives a
            // concurrent reload cannot fire `RemoveDevice` for a device that is
            // already gone.
            if let Some(name) = state
                .devices
                .focused_device()
                .map(|d| d.device_name.clone())
            {
                state.devices.loading = true;
                actions.push(UpdateAction::RemoveDevice(name));
            }
            actions
        }
        DevicesMsg::RemoveCancelled => super::close_top(state),
        DevicesMsg::Refresh => {
            state.devices.loading = true;
            vec![UpdateAction::LoadDevices]
        }
    }
}

/// The device list came back.
pub fn on_devices_loaded(
    state: &mut AppState,
    devices: Vec<DeviceRecord>,
    relay_url: Option<String>,
) -> Vec<UpdateAction> {
    state.devices.devices = devices;
    if let Some(name) = state.devices.focused.as_deref()
        && !state.devices.devices.iter().any(|d| d.device_name == name)
    {
        // The focused device is gone (removed, or dropped off a reload) —
        // clamp rather than point the cursor at nothing that exists.
        state.devices.focused = None;
    }
    state.devices.relay_url = relay_url;
    state.devices.loading = false;
    Vec::new()
}

/// A device was removed; the list needs a reload.
pub fn on_devices_changed(state: &mut AppState) -> Vec<UpdateAction> {
    state.devices.loading = false;
    state.toast_ok("Device removed");
    vec![UpdateAction::LoadDevices]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::{Message, UiMsg};
    use crate::app::update::update;

    fn devices(state: &mut AppState, msg: DevicesMsg) -> Vec<UpdateAction> {
        update(state, Message::Ui(UiMsg::Devices(msg)))
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
    fn close_closes_the_devices_dialog_and_refreshes_it() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Devices)));
        let actions = devices(&mut state, DevicesMsg::Close);
        assert_eq!(state.top_dialog(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadDevices))
        );
    }

    #[test]
    fn focused_stores_the_device_name() {
        let mut state = AppState::new();
        let actions = devices(&mut state, DevicesMsg::Focused("pixel".to_string()));
        assert_eq!(state.devices.focused.as_deref(), Some("pixel"));
        assert!(actions.is_empty());
    }

    #[test]
    fn remove_requested_does_nothing_without_a_focused_device() {
        let mut state = AppState::new();
        let actions = devices(&mut state, DevicesMsg::RemoveRequested);
        assert_eq!(state.top_dialog(), None);
        assert!(actions.is_empty());
    }

    #[test]
    fn remove_requested_opens_the_confirm_dialog_when_focused() {
        let mut state = AppState::new();
        state.devices.focused = Some("pixel".to_string());
        devices(&mut state, DevicesMsg::RemoveRequested);
        assert_eq!(state.top_dialog(), Some(DialogId::DeviceRemoveConfirm));
    }

    #[test]
    fn remove_confirmed_closes_the_confirm_and_dispatches_remove_device() {
        let mut state = AppState::new();
        state.devices.devices = vec![device("pixel")];
        state.devices.focused = Some("pixel".to_string());
        devices(&mut state, DevicesMsg::RemoveRequested);

        let actions = devices(&mut state, DevicesMsg::RemoveConfirmed);
        assert_eq!(state.top_dialog(), None);
        assert!(state.devices.loading);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RemoveDevice(name) if name == "pixel"))
        );
    }

    #[test]
    fn remove_confirmed_without_a_focused_device_only_closes() {
        let mut state = AppState::new();
        state.open_dialog(DialogId::DeviceRemoveConfirm);
        let actions = devices(&mut state, DevicesMsg::RemoveConfirmed);
        assert_eq!(state.top_dialog(), None);
        assert!(actions.is_empty());
    }

    #[test]
    fn remove_cancelled_closes_without_removing() {
        let mut state = AppState::new();
        state.devices.focused = Some("pixel".to_string());
        devices(&mut state, DevicesMsg::RemoveRequested);

        let actions = devices(&mut state, DevicesMsg::RemoveCancelled);
        assert_eq!(state.top_dialog(), None);
        assert!(actions.is_empty());
    }

    #[test]
    fn refresh_dispatches_load_devices() {
        let mut state = AppState::new();
        let actions = devices(&mut state, DevicesMsg::Refresh);
        assert!(state.devices.loading);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadDevices))
        );
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
    }

    #[test]
    fn devices_loaded_clears_a_focus_that_no_longer_exists() {
        let mut state = AppState::new();
        state.devices.focused = Some("gone".to_string());
        update(
            &mut state,
            Message::DevicesLoaded {
                devices: vec![device("pixel")],
                relay_url: None,
            },
        );
        assert_eq!(state.devices.focused, None);
    }

    #[test]
    fn devices_loaded_keeps_a_focus_still_present() {
        let mut state = AppState::new();
        state.devices.focused = Some("pixel".to_string());
        update(
            &mut state,
            Message::DevicesLoaded {
                devices: vec![device("pixel"), device("ipad")],
                relay_url: None,
            },
        );
        assert_eq!(state.devices.focused.as_deref(), Some("pixel"));
    }

    #[test]
    fn devices_changed_clears_loading_toasts_and_triggers_a_reload() {
        let mut state = AppState::new();
        state.devices.loading = true;
        let actions = update(&mut state, Message::DevicesChanged);
        assert!(!state.devices.loading);
        assert_eq!(state.ui.toasts.len(), 1);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadDevices))
        );
    }
}
