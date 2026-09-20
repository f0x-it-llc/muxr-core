//! The Config dialog's reducer: the bind-address form.

use crate::app::action::UpdateAction;
use crate::app::message::ConfigSnapshot;
use crate::app::state::AppState;
use crate::app::state::config::ConfigMsg;

/// Apply a [`ConfigMsg`].
pub fn update(state: &mut AppState, msg: ConfigMsg) -> Vec<UpdateAction> {
    match msg {
        ConfigMsg::HostChanged(host) => {
            state.config.host = host;
            state.config.error = None;
            Vec::new()
        }
        ConfigMsg::PortChanged(port) => {
            // The field is declared `digits_only`, but the reducer is the
            // authority on what the model may hold.
            state.config.port = port.chars().filter(char::is_ascii_digit).collect();
            state.config.error = None;
            Vec::new()
        }
        ConfigMsg::IpOpenChanged(open) => {
            state.config.ip_open = open;
            Vec::new()
        }
        ConfigMsg::IpFocused(ip) => {
            state.config.ip_cursor = Some(ip);
            Vec::new()
        }
        ConfigMsg::IpPicked(ip) => {
            state.config.host = ip.to_string();
            state.config.ip_cursor = Some(ip);
            state.config.ip_open = false;
            state.config.error = None;
            Vec::new()
        }
        ConfigMsg::Save => save(state),
        ConfigMsg::Cancel => super::close_top(state),
    }
}

/// Validate the form and dispatch a `SaveBind`, or set the inline error instead
/// of persisting garbage.
///
/// The dialog does not close here: it closes on the `ActionOk` that confirms
/// the write (see `pending_save`).
fn save(state: &mut AppState) -> Vec<UpdateAction> {
    if state.config.loading {
        return Vec::new();
    }
    let host = state.config.host.trim();
    if host.is_empty() {
        state.config.error = Some("Bind host must not be empty.".to_string());
        return Vec::new();
    }
    if !matches!(state.config.port.trim().parse::<u16>(), Ok(1..=u16::MAX)) {
        state.config.error = Some("Port must be a number in 1..=65535.".to_string());
        return Vec::new();
    }
    let addr = state.config.bind_addr();
    state.config.error = None;
    state.config.loading = true;
    state.config.pending_save = true;
    vec![UpdateAction::SaveBind(addr)]
}

/// Apply a loaded config snapshot.
pub fn on_config_loaded(state: &mut AppState, snapshot: ConfigSnapshot) -> Vec<UpdateAction> {
    state.config.apply_bind_addr(&snapshot.bind_addr);
    state.config.cert_dir = snapshot.cert_dir;
    state.config.reachable_ips = snapshot.reachable_ips;
    state.config.advertise_sans = snapshot.advertise_sans;
    // Drop a cursor that names an address this refresh no longer reports.
    if let Some(ip) = state.config.ip_cursor
        && !state.config.reachable_ips.contains(&ip)
    {
        state.config.ip_cursor = None;
    }
    state.config.loading = false;
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::Message;
    use crate::app::message::UiMsg;
    use crate::app::state::DialogId;
    use crate::app::update::update;
    use std::net::Ipv4Addr;

    fn snapshot(bind_addr: &str, ips: Vec<Ipv4Addr>) -> ConfigSnapshot {
        ConfigSnapshot {
            bind_addr: bind_addr.to_string(),
            cert_dir: "/var/lib/muxrd".to_string(),
            reachable_ips: ips,
            advertise_sans: Vec::new(),
        }
    }

    fn config(state: &mut AppState, msg: ConfigMsg) -> Vec<UpdateAction> {
        update(state, Message::Ui(UiMsg::Config(msg)))
    }

    #[test]
    fn config_loaded_populates_fields() {
        let mut state = AppState::new();
        let ips = vec![Ipv4Addr::new(192, 168, 1, 10)];
        update(
            &mut state,
            Message::ConfigLoaded(snapshot("0.0.0.0:50051", ips)),
        );
        assert_eq!(state.config.host, "0.0.0.0");
        assert_eq!(state.config.port, "50051");
        assert_eq!(state.config.cert_dir, "/var/lib/muxrd");
        assert_eq!(state.config.reachable_ips.len(), 1);
        assert!(!state.config.loading);
    }

    #[test]
    fn config_loaded_drops_a_stale_ip_cursor() {
        let mut state = AppState::new();
        state.config.ip_cursor = Some(Ipv4Addr::new(10, 0, 0, 9));
        update(
            &mut state,
            Message::ConfigLoaded(snapshot("0.0.0.0:50051", vec![Ipv4Addr::new(10, 0, 0, 1)])),
        );
        assert_eq!(state.config.ip_cursor, None);
    }

    #[test]
    fn host_and_port_edits_land_in_the_model() {
        let mut state = AppState::new();
        config(
            &mut state,
            ConfigMsg::HostChanged("server.local".to_string()),
        );
        config(&mut state, ConfigMsg::PortChanged("50051".to_string()));
        assert_eq!(state.config.bind_addr(), "server.local:50051");
    }

    #[test]
    fn port_field_keeps_digits_only() {
        let mut state = AppState::new();
        config(&mut state, ConfigMsg::PortChanged("5o0o51".to_string()));
        assert_eq!(state.config.port, "5051");
    }

    #[test]
    fn save_dispatches_save_bind() {
        let mut state = AppState::new();
        state.config.host = "10.0.0.5".to_string();
        state.config.port = "50051".to_string();
        let actions = config(&mut state, ConfigMsg::Save);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::SaveBind(addr) if addr == "10.0.0.5:50051"))
        );
        assert!(state.config.pending_save);
        assert!(state.config.loading);
    }

    #[test]
    fn save_rejects_empty_host() {
        let mut state = AppState::new();
        state.config.host = "   ".to_string();
        state.config.port = "50051".to_string();
        let actions = config(&mut state, ConfigMsg::Save);
        assert!(actions.is_empty());
        assert_eq!(
            state.config.error.as_deref(),
            Some("Bind host must not be empty.")
        );
        assert!(!state.config.pending_save);
    }

    #[test]
    fn save_rejects_a_bad_port() {
        for port in ["", "0", "70000", "abc"] {
            let mut state = AppState::new();
            state.config.host = "127.0.0.1".to_string();
            state.config.port = port.to_string();
            let actions = config(&mut state, ConfigMsg::Save);
            assert!(actions.is_empty(), "port {port:?} should not save");
            assert!(
                state
                    .config
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("Port")),
                "port {port:?} should set the port error"
            );
        }
    }

    #[test]
    fn save_is_single_flight() {
        let mut state = AppState::new();
        state.config.host = "127.0.0.1".to_string();
        state.config.port = "50051".to_string();
        state.config.loading = true;
        assert!(config(&mut state, ConfigMsg::Save).is_empty());
    }

    #[test]
    fn picking_an_ip_sets_the_host_and_closes_the_panel() {
        let mut state = AppState::new();
        state.config.ip_open = true;
        let ip = Ipv4Addr::new(192, 168, 1, 10);
        config(&mut state, ConfigMsg::IpPicked(ip));
        assert_eq!(state.config.host, "192.168.1.10");
        assert_eq!(state.config.ip_cursor, Some(ip));
        assert!(!state.config.ip_open);
    }

    #[test]
    fn focusing_an_ip_moves_the_cursor_only() {
        let mut state = AppState::new();
        let ip = Ipv4Addr::new(192, 168, 1, 10);
        config(&mut state, ConfigMsg::IpFocused(ip));
        assert_eq!(state.config.ip_cursor, Some(ip));
        assert!(state.config.host.is_empty());
    }

    #[test]
    fn cancel_closes_the_dialog() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Config)));
        config(&mut state, ConfigMsg::Cancel);
        assert_eq!(state.top_dialog(), None);
    }
}
