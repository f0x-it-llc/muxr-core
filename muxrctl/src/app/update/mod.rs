//! The TEA update function: `(state, message) -> actions`.
//!
//! Pure with respect to I/O: it mutates [`AppState`] in place and returns the
//! side effects ([`UpdateAction`]s) the runner should perform. No drawing,
//! terminal, or async code here — that keeps the `app/` layer unit-testable
//! without a terminal.
//!
//! This module is routing only. Every feature owns its own reducer beside it
//! (`config.rs`, `server.rs`, `cert.rs`, `tokens.rs`, `devices.rs`,
//! `wizard.rs`), so a feature card edits one file and never this one. The
//! wizard's pass-through hooks are called unconditionally for the same reason.

use ratcn::runtime::{KeyCode, KeyEvent};

use super::action::UpdateAction;
use super::message::{Message, UiMsg};
use super::state::server::ServerMsg;
use super::state::{AppState, DialogId};

pub mod cert;
pub mod config;
pub mod devices;
pub mod server;
pub mod tokens;
pub mod wizard;

/// Apply a [`Message`] to the [`AppState`], returning any side effects.
pub fn update(state: &mut AppState, message: Message) -> Vec<UpdateAction> {
    match message {
        Message::Key(key) => handle_key(state, key),
        Message::Tick => handle_tick(state),
        Message::Quit => {
            state.should_quit = true;
            vec![UpdateAction::Quit]
        }
        Message::Ui(msg) => handle_ui(state, msg),

        // ── Async task results ────────────────────────────────────────────────
        Message::StatusLoaded(info) => {
            let mut actions = server::on_status_loaded(state, info);
            actions.extend(wizard::on_status_loaded(state));
            actions
        }
        Message::ConfigLoaded(snapshot) => config::on_config_loaded(state, snapshot),
        Message::CertEnsured { fingerprint, sans } => {
            let mut actions = cert::on_cert_ensured(state, fingerprint, sans);
            actions.extend(wizard::on_cert_ensured(state));
            actions
        }
        Message::CertInfoLoaded { fingerprint, sans } => {
            let mut actions = cert::on_cert_info_loaded(state, fingerprint, sans);
            actions.extend(wizard::maybe_auto_open(state));
            actions
        }
        Message::CertModeLoaded(mode) => cert::on_cert_mode_loaded(state, mode),
        Message::ActionOk(msg) => {
            clear_loading(state);
            state.toast_ok(msg.clone());
            // A bind save is only done once the write came back: close the
            // Config dialog now rather than when Save was pressed.
            let mut actions = Vec::new();
            if state.config.pending_save {
                state.config.pending_save = false;
                if state.top_dialog() == Some(DialogId::Config) {
                    actions.extend(close_top(state));
                }
            }
            actions.extend(wizard::on_action_ok(state, &msg));
            actions
        }
        Message::ActionFailed(msg) => {
            clear_loading(state);
            state.config.pending_save = false;
            state.toast_err(msg.clone());
            // The Config dialog shows the reason inline as well, beside the
            // field that produced it.
            if state.ui.modals.is_open(DialogId::Config.id()) {
                state.config.error = Some(msg.clone());
            }
            wizard::on_action_failed(state, &msg)
        }

        // ── Token messages ────────────────────────────────────────────────────
        Message::TokensLoaded(records) => {
            let mut actions = tokens::on_tokens_loaded(state, records);
            actions.extend(wizard::on_tokens_loaded(state));
            actions.extend(wizard::maybe_auto_open(state));
            actions
        }
        Message::TokenCreated {
            token,
            name,
            read_only,
        } => {
            let mut actions = tokens::on_token_created(state, token, name, read_only);
            actions.extend(wizard::on_token_created(state));
            actions
        }
        Message::TokensChanged => tokens::on_tokens_changed(state),

        // ── Devices messages ──────────────────────────────────────────────────
        Message::DevicesLoaded { devices, relay_url } => {
            devices::on_devices_loaded(state, devices, relay_url)
        }
        Message::DevicesChanged => devices::on_devices_changed(state),

        // ── Token QR overlay messages ─────────────────────────────────────────
        Message::TokenQrReady {
            uri,
            host,
            port,
            fingerprint_short,
            baseline_clients,
            seq,
        } => tokens::on_qr_ready(
            state,
            uri,
            host,
            port,
            fingerprint_short,
            baseline_clients,
            seq,
        ),
        Message::TokenQrFailed { err, seq } => tokens::on_qr_failed(state, err, seq),
    }
}

/// Route a message a declared component emitted.
fn handle_ui(state: &mut AppState, msg: UiMsg) -> Vec<UpdateAction> {
    match msg {
        UiMsg::Focus(focus) => {
            state.ui.focus = focus;
            Vec::new()
        }
        UiMsg::Open(id) => open_hook(state, id),
        UiMsg::Close => close_top(state),
        UiMsg::Config(msg) => config::update(state, msg),
        UiMsg::Server(msg) => server::update(state, msg),
        UiMsg::Cert(msg) => cert::update(state, msg),
        UiMsg::Tokens(msg) => tokens::update(state, msg),
        UiMsg::Devices(msg) => devices::update(state, msg),
        UiMsg::Wizard(msg) => wizard::update(state, msg),
    }
}

/// Open `id` and return the loads that dialog needs to show real data.
///
/// Every path that opens a dialog goes through here, so a dialog can never be
/// shown with stale contents because one caller forgot its load.
pub(crate) fn open_hook(state: &mut AppState, id: DialogId) -> Vec<UpdateAction> {
    state.open_dialog(id);
    match id {
        DialogId::Config => {
            state.config.loading = true;
            state.config.error = None;
            vec![UpdateAction::LoadConfig]
        }
        DialogId::Cert => {
            // LoadConfig populates `reachable_ips`, which `build_sans_from_config`
            // needs; LoadCertInfo reads the on-disk cert without regenerating it;
            // LoadCertMode asks the running daemon what transport it is serving.
            state.config.loading = true;
            state.cert.loading = true;
            vec![
                UpdateAction::LoadConfig,
                UpdateAction::LoadCertInfo,
                UpdateAction::LoadCertMode,
            ]
        }
        DialogId::Tokens => {
            state.tokens.loading = true;
            vec![UpdateAction::LoadTokens]
        }
        DialogId::Devices => {
            state.devices.loading = true;
            vec![UpdateAction::LoadDevices]
        }
        _ => Vec::new(),
    }
}

/// Close the top dialog and refresh whatever it may have changed.
pub(crate) fn close_top(state: &mut AppState) -> Vec<UpdateAction> {
    match state.close_dialog() {
        Some(id) => refresh_after_close(state, id),
        None => Vec::new(),
    }
}

/// The reload a just-closed dialog owes the dashboard.
pub(crate) fn refresh_after_close(state: &mut AppState, id: DialogId) -> Vec<UpdateAction> {
    match id {
        DialogId::Tokens | DialogId::TokenCreate | DialogId::TokenMinted => {
            state.tokens.loading = true;
            vec![UpdateAction::LoadTokens]
        }
        DialogId::Devices => {
            state.devices.loading = true;
            vec![UpdateAction::LoadDevices]
        }
        DialogId::Cert | DialogId::CertRegenConfirm => {
            state.cert.loading = true;
            vec![UpdateAction::LoadCertInfo]
        }
        DialogId::Config => {
            state.config.loading = true;
            vec![UpdateAction::LoadConfig]
        }
        _ => Vec::new(),
    }
}

/// Clear every feature's in-flight flag — what an `ActionOk` / `ActionFailed`
/// means whichever task posted it.
fn clear_loading(state: &mut AppState) {
    state.config.loading = false;
    state.cert.loading = false;
    state.server.loading = false;
    state.tokens.loading = false;
    state.devices.loading = false;
}

/// Translate a key the ratcn runtime left unhandled.
///
/// With a dialog open the modal layer owns the keyboard (Esc is the dialog's
/// own `on_dismiss`), so only `Ctrl-C` is honoured here.
fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<UpdateAction> {
    // Ctrl-C always quits.
    if key.modifiers.ctrl && key.code == KeyCode::Char('c') {
        state.should_quit = true;
        return vec![UpdateAction::Quit];
    }
    if state.top_dialog().is_some() {
        return Vec::new();
    }

    match key.code {
        KeyCode::Char('q') => {
            state.should_quit = true;
            vec![UpdateAction::Quit]
        }
        KeyCode::Char('c') => open_hook(state, DialogId::Config),
        KeyCode::Char('e') => open_hook(state, DialogId::Cert),
        KeyCode::Char('t') => open_hook(state, DialogId::Tokens),
        KeyCode::Char('d') => open_hook(state, DialogId::Devices),
        KeyCode::Char('w') => open_hook(state, DialogId::Wizard),
        KeyCode::Char('s') => {
            let msg = if state.server.is_running() {
                ServerMsg::StopRequested
            } else {
                ServerMsg::Start
            };
            server::update(state, msg)
        }
        KeyCode::Char('r') => vec![
            UpdateAction::RefreshStatus,
            UpdateAction::LoadConfig,
            UpdateAction::LoadTokens,
            UpdateAction::LoadCertInfo,
            UpdateAction::LoadDevices,
        ],
        _ => Vec::new(),
    }
}

/// Tick handler — the ~1 s daemon status poll plus the QR overlay's own poll.
///
/// Both share the single `server.loading` in-flight guard, so at most one
/// `RefreshStatus` is dispatched per ~1 s window even with the overlay up.
/// The dashboard is always visible, so the status poll always runs.
fn handle_tick(state: &mut AppState) -> Vec<UpdateAction> {
    let mut actions = Vec::new();

    if !state.server.loading {
        state.server.tick_counter = state.server.tick_counter.wrapping_add(1);
        if state.server.tick_counter.is_multiple_of(20) {
            state.server.loading = true;
            actions.push(UpdateAction::RefreshStatus);
        }
    }

    actions.extend(tokens::on_tick(state));
    actions.extend(wizard::on_tick(state));
    actions
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::ConfigSnapshot;
    use crate::app::state::ServerInfo;
    use crate::app::state::config::ConfigMsg;
    use ratcn::runtime::Modifiers;

    pub(crate) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        }
    }

    pub(crate) fn server_info(client_count: usize) -> ServerInfo {
        ServerInfo {
            version: "0.1.0".to_string(),
            bind_addr: "127.0.0.1:50051".to_string(),
            pid: 4242,
            uptime_secs: 120,
            client_count,
            notify_relay_url: None,
            push_device_count: 0,
        }
    }

    #[test]
    fn q_sets_should_quit() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('q'))));
        assert!(state.should_quit);
        assert!(matches!(actions.as_slice(), [UpdateAction::Quit]));
    }

    #[test]
    fn ctrl_c_quits() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Key(ctrl(KeyCode::Char('c'))));
        assert!(state.should_quit);
        assert!(matches!(actions.as_slice(), [UpdateAction::Quit]));
    }

    #[test]
    fn ctrl_c_quits_even_with_a_dialog_open() {
        let mut state = AppState::new();
        state.open_dialog(DialogId::Config);
        let actions = update(&mut state, Message::Key(ctrl(KeyCode::Char('c'))));
        assert!(state.should_quit);
        assert!(matches!(actions.as_slice(), [UpdateAction::Quit]));
    }

    #[test]
    fn quit_message_sets_flag() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Quit);
        assert!(state.should_quit);
        assert!(matches!(actions.as_slice(), [UpdateAction::Quit]));
    }

    #[test]
    fn dashboard_keys_open_their_dialogs() {
        for (code, expected) in [
            ('c', DialogId::Config),
            ('e', DialogId::Cert),
            ('t', DialogId::Tokens),
            ('d', DialogId::Devices),
            ('w', DialogId::Wizard),
        ] {
            let mut state = AppState::new();
            update(&mut state, Message::Key(key(KeyCode::Char(code))));
            assert_eq!(state.top_dialog(), Some(expected), "key {code}");
        }
    }

    #[test]
    fn open_config_dispatches_load_config() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Ui(UiMsg::Open(DialogId::Config)));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadConfig))
        );
        assert!(state.config.loading);
    }

    #[test]
    fn open_cert_dispatches_all_three_cert_loads() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Ui(UiMsg::Open(DialogId::Cert)));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadConfig))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadCertInfo))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadCertMode))
        );
    }

    #[test]
    fn close_refreshes_what_the_dialog_may_have_changed() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Tokens)));
        let actions = update(&mut state, Message::Ui(UiMsg::Close));
        assert_eq!(state.top_dialog(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }

    #[test]
    fn keys_are_ignored_while_a_dialog_is_open() {
        let mut state = AppState::new();
        state.open_dialog(DialogId::Config);
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('q'))));
        assert!(!state.should_quit);
        assert!(actions.is_empty());
    }

    #[test]
    fn r_reloads_every_dashboard_section() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('r'))));
        assert_eq!(actions.len(), 5);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RefreshStatus))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadDevices))
        );
    }

    #[test]
    fn s_starts_a_stopped_daemon_and_confirms_a_running_one() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('s'))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::StartServer))
        );

        let mut state = AppState::new();
        state.server.status = Some(server_info(0));
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('s'))));
        assert!(actions.is_empty());
        assert_eq!(state.top_dialog(), Some(DialogId::StopServer));
    }

    #[test]
    fn tick_triggers_refresh_after_20() {
        let mut state = AppState::new();
        for _ in 0..19 {
            let actions = update(&mut state, Message::Tick);
            assert!(actions.is_empty(), "expected no action before tick 20");
        }
        let actions = update(&mut state, Message::Tick);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RefreshStatus))
        );
    }

    #[test]
    fn tick_is_single_flight_while_a_status_query_is_in_flight() {
        let mut state = AppState::new();
        state.server.loading = true;
        for _ in 0..40 {
            assert!(update(&mut state, Message::Tick).is_empty());
        }
    }

    #[test]
    fn action_ok_clears_loading_and_toasts() {
        let mut state = AppState::new();
        state.config.loading = true;
        state.tokens.loading = true;
        let actions = update(&mut state, Message::ActionOk("Saved.".to_string()));
        assert!(actions.is_empty());
        assert!(!state.config.loading);
        assert!(!state.tokens.loading);
        assert_eq!(state.ui.toasts.len(), 1);
    }

    #[test]
    fn action_ok_closes_the_config_dialog_after_a_save() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Config)));
        update(
            &mut state,
            Message::ConfigLoaded(ConfigSnapshot {
                bind_addr: "127.0.0.1:50051".to_string(),
                cert_dir: "/tmp".to_string(),
                reachable_ips: Vec::new(),
                advertise_sans: Vec::new(),
            }),
        );
        let actions = update(&mut state, Message::Ui(UiMsg::Config(ConfigMsg::Save)));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::SaveBind(addr) if addr == "127.0.0.1:50051"))
        );
        assert!(state.config.pending_save);

        let actions = update(&mut state, Message::ActionOk("Saved.".to_string()));
        assert_eq!(state.top_dialog(), None, "save must close the dialog");
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadConfig))
        );
    }

    #[test]
    fn action_failed_sets_the_config_error_while_the_dialog_is_open() {
        let mut state = AppState::new();
        state.open_dialog(DialogId::Config);
        update(&mut state, Message::ActionFailed("boom".to_string()));
        assert_eq!(state.config.error.as_deref(), Some("boom"));
        assert_eq!(state.ui.toasts.len(), 1);
        assert_eq!(
            state.top_dialog(),
            Some(DialogId::Config),
            "a failure leaves the dialog open so it can be corrected"
        );
    }

    #[test]
    fn focus_message_stores_the_new_snapshot() {
        use ratcn::runtime::FocusState;
        let mut state = AppState::new();
        let focus = FocusState::intent(["btn_config"]);
        update(&mut state, Message::Ui(UiMsg::Focus(focus.clone())));
        assert_eq!(state.ui.focus, focus);
    }
}
