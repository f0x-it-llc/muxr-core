//! The daemon controls' reducer: start, confirm-and-stop, refresh, and the
//! status result every poll posts back.

use crate::app::action::UpdateAction;
use crate::app::state::server::ServerMsg;
use crate::app::state::{AppState, DialogId, ServerInfo};

/// Apply a [`ServerMsg`].
pub fn update(state: &mut AppState, msg: ServerMsg) -> Vec<UpdateAction> {
    match msg {
        ServerMsg::Start => {
            if state.server.loading {
                return Vec::new();
            }
            state.server.loading = true;
            // Clear `stopped` now so the dashboard does not hold a stale
            // "Stopped" line for the cycle before the first StatusLoaded.
            state.server.stopped = false;
            vec![UpdateAction::StartServer]
        }
        ServerMsg::StopRequested => super::open_hook(state, DialogId::StopServer),
        ServerMsg::StopConfirmed => {
            let mut actions = super::close_top(state);
            if !state.server.loading {
                state.server.loading = true;
                actions.push(UpdateAction::StopServer);
            }
            actions
        }
        ServerMsg::StopCancelled => super::close_top(state),
        ServerMsg::Refresh => {
            if state.server.loading {
                return Vec::new();
            }
            state.server.loading = true;
            vec![UpdateAction::RefreshStatus]
        }
    }
}

/// Apply a status poll result.
pub fn on_status_loaded(state: &mut AppState, info: Option<ServerInfo>) -> Vec<UpdateAction> {
    let client_count = info.as_ref().map(|i| i.client_count);
    match info {
        Some(info) => {
            state.server.status = Some(info);
            state.server.stopped = false;
        }
        None => {
            state.server.status = None;
            state.server.stopped = true;
        }
    }
    state.server.loading = false;
    // The QR overlay's connection detection rides on the same poll.
    super::tokens::on_status_for_qr(state, client_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::{Message, UiMsg};
    use crate::app::update::tests::server_info;
    use crate::app::update::update;

    fn server(state: &mut AppState, msg: ServerMsg) -> Vec<UpdateAction> {
        update(state, Message::Ui(UiMsg::Server(msg)))
    }

    #[test]
    fn start_dispatches_start_server_and_clears_stopped() {
        let mut state = AppState::new();
        state.server.stopped = true;
        let actions = server(&mut state, ServerMsg::Start);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::StartServer))
        );
        assert!(!state.server.stopped);
        assert!(state.server.loading);
    }

    #[test]
    fn start_is_single_flight() {
        let mut state = AppState::new();
        state.server.loading = true;
        assert!(server(&mut state, ServerMsg::Start).is_empty());
    }

    #[test]
    fn stop_asks_for_confirmation_first() {
        let mut state = AppState::new();
        let actions = server(&mut state, ServerMsg::StopRequested);
        assert!(actions.is_empty());
        assert_eq!(state.top_dialog(), Some(DialogId::StopServer));

        let actions = server(&mut state, ServerMsg::StopConfirmed);
        assert_eq!(state.top_dialog(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::StopServer))
        );
    }

    #[test]
    fn stop_cancelled_closes_without_stopping() {
        let mut state = AppState::new();
        server(&mut state, ServerMsg::StopRequested);
        let actions = server(&mut state, ServerMsg::StopCancelled);
        assert_eq!(state.top_dialog(), None);
        assert!(actions.is_empty());
    }

    #[test]
    fn refresh_dispatches_refresh_status() {
        let mut state = AppState::new();
        let actions = server(&mut state, ServerMsg::Refresh);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RefreshStatus))
        );
    }

    #[test]
    fn status_loaded_running_updates_state() {
        let mut state = AppState::new();
        state.server.loading = true;
        update(&mut state, Message::StatusLoaded(Some(server_info(2))));
        assert!(state.server.is_running());
        assert_eq!(state.server.status.as_ref().unwrap().client_count, 2);
        assert!(!state.server.loading);
    }

    #[test]
    fn status_loaded_none_marks_stopped() {
        let mut state = AppState::new();
        state.server.status = Some(server_info(0));
        update(&mut state, Message::StatusLoaded(None));
        assert!(state.server.stopped);
        assert!(state.server.status.is_none());
        assert!(!state.server.is_running());
    }
}
