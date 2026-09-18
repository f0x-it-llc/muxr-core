//! The setup-wizard reducer — a stub, plus the pass-through hooks
//! `app/update/mod.rs` calls unconditionally.
//!
//! Every hook here is already wired into the routing, so the wizard card
//! implements this file (and `app/state/wizard.rs`, `tui/views/wizard.rs`) and
//! never has to edit `app/update/mod.rs`. Each returns the actions the wizard
//! wants dispatched, concatenated onto the arm's own actions by the caller.

use crate::app::action::UpdateAction;
use crate::app::state::AppState;
use crate::app::state::wizard::WizardMsg;

/// Apply a [`WizardMsg`]. The wizard card extends this.
pub fn update(state: &mut AppState, msg: WizardMsg) -> Vec<UpdateAction> {
    match msg {
        WizardMsg::Close => super::close_top(state),
    }
}

/// Open the wizard by itself the first time muxrctl runs against a machine with
/// no cert and no tokens. Called after every `CertInfoLoaded` and every
/// `TokensLoaded`, which is when both facts are knowable.
///
/// Stub: first-run detection is the wizard card's.
pub fn maybe_auto_open(state: &mut AppState) -> Vec<UpdateAction> {
    let _ = state.wizard.first_run_checked;
    Vec::new()
}

/// Called after the `ActionOk` toast is raised.
pub fn on_action_ok(_state: &mut AppState, _msg: &str) -> Vec<UpdateAction> {
    Vec::new()
}

/// Called after the `ActionFailed` toast is raised.
pub fn on_action_failed(_state: &mut AppState, _msg: &str) -> Vec<UpdateAction> {
    Vec::new()
}

/// Called after the tick's poll logic.
pub fn on_tick(_state: &mut AppState) -> Vec<UpdateAction> {
    Vec::new()
}

/// Called after `cert::on_cert_ensured`.
pub fn on_cert_ensured(_state: &mut AppState) -> Vec<UpdateAction> {
    Vec::new()
}

/// Called after `tokens::on_token_created`.
pub fn on_token_created(_state: &mut AppState) -> Vec<UpdateAction> {
    Vec::new()
}

/// Called after `server::on_status_loaded`.
pub fn on_status_loaded(_state: &mut AppState) -> Vec<UpdateAction> {
    Vec::new()
}

/// Called after `tokens::on_tokens_loaded`.
pub fn on_tokens_loaded(_state: &mut AppState) -> Vec<UpdateAction> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::{Message, UiMsg};
    use crate::app::state::DialogId;
    use crate::app::update::update;

    #[test]
    fn close_closes_the_wizard() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Wizard)));
        let actions = update(&mut state, Message::Ui(UiMsg::Wizard(WizardMsg::Close)));
        assert_eq!(state.top_dialog(), None);
        assert!(actions.is_empty());
    }

    #[test]
    fn hooks_are_inert_until_the_wizard_card_lands() {
        let mut state = AppState::new();
        assert!(maybe_auto_open(&mut state).is_empty());
        assert!(on_action_ok(&mut state, "ok").is_empty());
        assert!(on_action_failed(&mut state, "err").is_empty());
        assert!(on_tick(&mut state).is_empty());
        assert!(on_cert_ensured(&mut state).is_empty());
        assert!(on_token_created(&mut state).is_empty());
        assert!(on_status_loaded(&mut state).is_empty());
        assert!(on_tokens_loaded(&mut state).is_empty());
    }
}
