//! The setup-wizard reducer: the step machine, plus the pass-through hooks
//! `app/update/mod.rs` calls unconditionally.
//!
//! Every hook here is already wired into the routing, so the wizard lives in
//! this file (and `app/state/wizard.rs`, `tui/views/wizard.rs`) and never edits
//! `app/update/mod.rs`. Each returns the actions the wizard wants dispatched,
//! concatenated onto the arm's own actions by the caller.
//!
//! ## How a step advances
//!
//! Every step that has work to do dispatches its action, sets
//! [`WizardState::busy`](crate::app::state::wizard::WizardState::busy), and
//! advances from the hook that reports the result — never optimistically from
//! `Next`:
//!
//! | Step        | Dispatched            | Advances from        |
//! |-------------|-----------------------|----------------------|
//! | Welcome     | —                     | `Next` itself        |
//! | Network     | `SaveBind`            | [`on_action_ok`]     |
//! | Certificate | `EnsureCert`          | [`on_cert_ensured`]  |
//! | Token       | `CreateToken`         | [`on_token_created`] |
//! | Start       | `StartServer`         | [`on_status_loaded`] |
//! | Pair        | `ShowTokenQr`         | — (the last step)    |
//!
//! [`on_action_failed`] is the single failure path: it clears `busy` and shows
//! the reason on the step that asked, so a failed step can be corrected and
//! retried rather than stranding the wizard.
//!
//! The wizard reuses the feature reducers rather than restating them:
//! `config::update(ConfigMsg::Save)` validates and persists the bind address,
//! `server::update(ServerMsg::Start)` keeps the daemon start single-flight, and
//! the tokens reducer's own `on_qr_ready` / `on_qr_failed` / `on_status_for_qr`
//! / `on_tick` drive the QR the Pair step paints.

use ratcn::runtime::FocusState;

use crate::app::action::UpdateAction;
use crate::app::state::config::ConfigMsg;
use crate::app::state::server::ServerMsg;
use crate::app::state::tokens::{QrOverlay, QrOverlayPhase};
use crate::app::state::wizard::{WizardMsg, WizardStep};
use crate::app::state::{AppState, DialogId};

/// The button Enter should land on for `step`: the last step has no Next.
const fn primary_button(step: WizardStep) -> &'static str {
    if step.is_last() {
        "wiz_done"
    } else {
        "wiz_next"
    }
}

/// Park focus on the current step's primary button, so Enter alone walks the
/// wizard however the step laid its own controls out.
fn park_focus(state: &mut AppState) {
    state.ui.focus = FocusState::intent([DialogId::Wizard.id(), primary_button(state.wizard.step)]);
}

/// Whether the wizard layer is open (it need not be the top dialog — the tokens
/// reducer briefly opens the minted-secret dialog above it).
fn is_open(state: &AppState) -> bool {
    state.ui.modals.is_open(DialogId::Wizard.id())
}

/// Whether the wizard is open and waiting on the result `step` dispatched.
fn waiting_on(state: &AppState, step: WizardStep) -> bool {
    is_open(state) && state.wizard.busy && state.wizard.step == step
}

/// Apply a [`WizardMsg`].
pub fn update(state: &mut AppState, msg: WizardMsg) -> Vec<UpdateAction> {
    match msg {
        WizardMsg::Close => close(state),
        WizardMsg::Next => next(state),
        WizardMsg::Back => back(state),
        WizardMsg::RegenAckChanged(ack) => {
            state.wizard.cert_regen_ack = ack;
            Vec::new()
        }
        WizardMsg::Finish => finish(state),
    }
}

/// Close the wizard from any step, dropping the QR it opened.
///
/// The minted-secret dialog can sit on top of the wizard for one update cycle
/// ([`on_token_created`] pops it), so this closes layers until the wizard
/// itself is gone rather than assuming it is on top. The QR overlay is cleared,
/// never revoked: it shows a real user token.
fn close(state: &mut AppState) -> Vec<UpdateAction> {
    state.wizard.busy = false;
    state.wizard.error = None;
    state.tokens.qr_overlay = None;
    let mut actions = Vec::new();
    while is_open(state) {
        actions.extend(super::close_top(state));
    }
    actions
}

/// Done on the last step: close, then reload everything the wizard changed.
fn finish(state: &mut AppState) -> Vec<UpdateAction> {
    let mut actions = close(state);
    state.tokens.loading = true;
    state.cert.loading = true;
    state.server.loading = true;
    actions.extend([
        UpdateAction::LoadTokens,
        UpdateAction::LoadCertInfo,
        UpdateAction::RefreshStatus,
    ]);
    actions
}

/// Commit the current step and move on. Ignored while a step is in flight.
fn next(state: &mut AppState) -> Vec<UpdateAction> {
    if state.wizard.busy {
        return Vec::new();
    }
    match state.wizard.step {
        WizardStep::Welcome => {
            state.wizard.cert_existed = state.cert.fingerprint.is_some();
            advance(state)
        }
        WizardStep::Network => save_bind(state),
        WizardStep::Certificate => ensure_cert(state),
        WizardStep::Token => create_token(state),
        WizardStep::Start => start_daemon(state),
        // The last step: Done finishes, Next does not exist.
        WizardStep::Pair => Vec::new(),
    }
}

/// Step back one, with no side effects. Ignored while busy and on the first
/// step.
fn back(state: &mut AppState) -> Vec<UpdateAction> {
    if state.wizard.busy || state.wizard.step.is_first() {
        return Vec::new();
    }
    if state.wizard.step == WizardStep::Pair {
        // Leaving Pair drops the QR that step opened; the token itself is
        // untouched (the overlay never revokes).
        state.tokens.qr_overlay = None;
    }
    state.wizard.step = state.wizard.step.previous();
    state.wizard.error = None;
    park_focus(state);
    Vec::new()
}

/// Move on to the step after the current one — every forward transition the
/// wizard makes, so [`WizardStep::next`]'s clamp at the last step is the one
/// statement of where the flow ends.
fn advance(state: &mut AppState) -> Vec<UpdateAction> {
    goto(state, state.wizard.step.next())
}

/// Move to `step`: clear the error, park focus on its primary button, and run
/// whatever entering it starts.
fn goto(state: &mut AppState, step: WizardStep) -> Vec<UpdateAction> {
    state.wizard.step = step;
    state.wizard.error = None;
    park_focus(state);
    if step == WizardStep::Pair {
        enter_pair(state)
    } else {
        Vec::new()
    }
}

// ── Steps with work to do ─────────────────────────────────────────────────────

/// Network: validate and persist the bind address through the Config reducer.
///
/// The Config dialog is not open here, and the `ActionOk` arm only closes a
/// dialog when the Config dialog is the top one, so reusing `ConfigMsg::Save`
/// cannot close the wizard out from under itself.
fn save_bind(state: &mut AppState) -> Vec<UpdateAction> {
    state.config.error = None;
    let actions = super::config::update(state, ConfigMsg::Save);
    if let Some(error) = state.config.error.clone() {
        // Validation refused the form: stay on the step with the reason.
        state.wizard.error = Some(error);
        return Vec::new();
    }
    if actions.is_empty() {
        // The Config reducer is single-flight: something else is writing.
        state.wizard.error =
            Some("A configuration read or write is already in flight — try again.".to_string());
        return Vec::new();
    }
    state.wizard.busy = true;
    state.wizard.error = None;
    actions
}

/// Certificate: keep the one on disk, or generate the planned SANs into a new
/// one when there is none — or when the operator acknowledged the re-pair cost.
fn ensure_cert(state: &mut AppState) -> Vec<UpdateAction> {
    if state.cert.fingerprint.is_some() && !state.wizard.cert_regen_ack {
        return advance(state);
    }
    state.wizard.busy = true;
    state.wizard.error = None;
    state.cert.loading = true;
    vec![UpdateAction::EnsureCert(
        super::cert::build_sans_from_config(state),
    )]
}

/// Token: mint one from the same form fields the create dialog binds.
fn create_token(state: &mut AppState) -> Vec<UpdateAction> {
    let name = state.tokens.form_name.trim();
    let name = (!name.is_empty()).then(|| name.to_string());
    let read_only = state.tokens.form_read_only;
    let expiry_secs = state.tokens.form_expiry.ttl_secs();
    state.wizard.busy = true;
    state.wizard.error = None;
    state.tokens.loading = true;
    vec![UpdateAction::CreateToken {
        name,
        read_only,
        expiry_secs,
    }]
}

/// Start: launch the daemon, or walk straight on when it is already up.
fn start_daemon(state: &mut AppState) -> Vec<UpdateAction> {
    if state.server.status.is_some() {
        return advance(state);
    }
    let actions = super::server::update(state, ServerMsg::Start);
    if actions.is_empty() {
        // `ServerMsg::Start` is single-flight; a poll or start is already out.
        state.wizard.error =
            Some("A daemon operation is already in flight — try again.".to_string());
        return Vec::new();
    }
    state.wizard.busy = true;
    state.wizard.error = None;
    actions
}

/// Entering Pair: build the QR for the token this wizard just minted.
///
/// Exactly what `tokens::update`'s QR path does, minus the extra `Qr` modal —
/// the wizard's own Pair step paints `state.tokens.qr_overlay`, and the tokens
/// reducer's `on_qr_ready` / `on_qr_failed` / `on_status_for_qr` / `on_tick`
/// drive it from there. Nothing is minted and nothing is revoked here.
fn enter_pair(state: &mut AppState) -> Vec<UpdateAction> {
    let Some((name, secret, read_only)) = state.tokens.last_minted_secret.clone() else {
        // The Pair step paints the "go Back to the Token step" prompt.
        state.tokens.qr_overlay = None;
        return Vec::new();
    };
    state.qr_seq = state.qr_seq.wrapping_add(1);
    let seq = state.qr_seq;
    let advertise_trust = state.cert.advertise_trust;
    state.tokens.qr_overlay = Some(QrOverlay {
        phase: QrOverlayPhase::Generating,
        seq,
        baseline_clients: 0,
        token_name: name,
        read_only,
        tick_counter: 0,
    });
    vec![UpdateAction::ShowTokenQr {
        token: secret,
        read_only,
        seq,
        advertise_trust,
    }]
}

// ── Hooks `app/update/mod.rs` calls ───────────────────────────────────────────

/// Open the wizard by itself the first time muxrctl runs against a machine with
/// no cert and no tokens. Called after every `CertInfoLoaded` and every
/// `TokensLoaded`, which is when both facts are knowable.
///
/// Both loads have to have landed before the decision is made — a tokens result
/// alone says nothing about the certificate — so the two are counted with
/// `cert_info_seen` / `tokens_seen`, and
/// [`WizardState::tokens_mark_pending`](crate::app::state::wizard::WizardState::tokens_mark_pending)
/// is what tells this one hook which of the two loads it is trailing. The
/// decision is taken once per session whichever way it goes: `first_run_checked`
/// is set even when the wizard stays shut, so a later reload cannot pop it open
/// over an operator who is already working.
pub fn maybe_auto_open(state: &mut AppState) -> Vec<UpdateAction> {
    if std::mem::take(&mut state.wizard.tokens_mark_pending) {
        state.wizard.tokens_seen = true;
    } else {
        state.wizard.cert_info_seen = true;
    }
    if state.wizard.first_run_checked || !(state.wizard.cert_info_seen && state.wizard.tokens_seen)
    {
        return Vec::new();
    }
    state.wizard.first_run_checked = true;

    let first_run = state.cert.fingerprint.is_none() && state.tokens.tokens.is_empty();
    if !first_run || state.top_dialog().is_some() {
        return Vec::new();
    }

    state.wizard.step = WizardStep::Welcome;
    state.wizard.busy = false;
    state.wizard.error = None;
    let mut actions = super::open_hook(state, DialogId::Wizard);
    park_focus(state);
    // The Network step binds the Config form, which nothing has loaded yet.
    actions.push(UpdateAction::LoadConfig);
    actions
}

/// Called after the `ActionOk` toast is raised.
///
/// The only step that waits on an `ActionOk` is Network: its `SaveBind` is the
/// action in flight, and the `ActionOk` arm has already cleared
/// `config.pending_save` without closing anything (the wizard, not the Config
/// dialog, is on top).
pub fn on_action_ok(state: &mut AppState, _msg: &str) -> Vec<UpdateAction> {
    if !waiting_on(state, WizardStep::Network) {
        return Vec::new();
    }
    state.wizard.busy = false;
    advance(state)
}

/// Called after the `ActionFailed` toast is raised.
///
/// Whatever the step was waiting on failed: stop waiting, show the reason, and
/// leave the step where it is so it can be corrected and retried.
pub fn on_action_failed(state: &mut AppState, msg: &str) -> Vec<UpdateAction> {
    if !is_open(state) || !state.wizard.busy {
        return Vec::new();
    }
    state.wizard.busy = false;
    state.wizard.error = Some(msg.to_string());
    park_focus(state);
    Vec::new()
}

/// Called after the tick's poll logic.
///
/// Nothing to do: the QR the Pair step paints is polled by `tokens::on_tick`,
/// which runs from the same tick and does not care which layer is on top, and
/// no wizard step times out.
pub fn on_tick(_state: &mut AppState) -> Vec<UpdateAction> {
    Vec::new()
}

/// Called after `cert::on_cert_ensured`.
pub fn on_cert_ensured(state: &mut AppState) -> Vec<UpdateAction> {
    if !waiting_on(state, WizardStep::Certificate) {
        return Vec::new();
    }
    state.wizard.busy = false;
    advance(state)
}

/// Called after `tokens::on_token_created`.
///
/// That reducer opens `DialogId::TokenMinted` unconditionally — it is the only
/// place the plaintext is shown, and it cannot know the wizard asked for the
/// mint. So the wizard pops it straight back off, defensively (only while the
/// wizard is the flow that asked, and only when that dialog really is the top
/// one), which keeps the wizard on top and lets the Pair step paint the QR for
/// the secret that dialog was about to show. `last_minted_secret` survives the
/// close, so nothing is lost by it.
pub fn on_token_created(state: &mut AppState) -> Vec<UpdateAction> {
    if !waiting_on(state, WizardStep::Token) {
        return Vec::new();
    }
    state.wizard.busy = false;
    if state.top_dialog() == Some(DialogId::TokenMinted) {
        // Closed directly rather than through `close_top`: that path would add
        // a second `LoadTokens` on top of the one `on_token_created` already
        // returned.
        state.close_dialog();
    }
    advance(state)
}

/// Called after `server::on_status_loaded`.
///
/// A status result with no server in it means the daemon has not come up yet;
/// the poll keeps running, so the step waits rather than failing.
pub fn on_status_loaded(state: &mut AppState) -> Vec<UpdateAction> {
    if !waiting_on(state, WizardStep::Start) || state.server.status.is_none() {
        return Vec::new();
    }
    state.wizard.busy = false;
    advance(state)
}

/// Called after `tokens::on_tokens_loaded`.
///
/// Records that the tokens half of first-run detection has landed, and marks the
/// `maybe_auto_open` call `app/update/mod.rs` makes immediately after this one
/// as the tokens one.
pub fn on_tokens_loaded(state: &mut AppState) -> Vec<UpdateAction> {
    state.wizard.tokens_seen = true;
    state.wizard.tokens_mark_pending = true;
    Vec::new()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::{Message, UiMsg};
    use crate::app::state::cert::AdvertiseTrust;
    use crate::app::state::tokens::TokenExpiryChoice;
    use crate::app::update::tests::server_info;
    use crate::app::update::update;
    use crate::server::tokens::TokenRecord;

    fn wizard(state: &mut AppState, msg: WizardMsg) -> Vec<UpdateAction> {
        update(state, Message::Ui(UiMsg::Wizard(msg)))
    }

    fn record(name: &str) -> TokenRecord {
        TokenRecord {
            name: name.to_string(),
            created_at: "2026-01-01 00:00:00".to_string(),
            read_only: false,
        }
    }

    /// The two loads first-run detection waits on, in the order the runner's
    /// seed dispatches them.
    fn both_loads(state: &mut AppState, fingerprint: Option<&str>, tokens: Vec<TokenRecord>) {
        update(
            state,
            Message::CertInfoLoaded {
                fingerprint: fingerprint.map(ToString::to_string),
                sans: Vec::new(),
            },
        );
        update(state, Message::TokensLoaded(tokens));
    }

    /// Open the wizard the way the dashboard's `w` does, then walk it to `step`
    /// with the step's own side effects already settled.
    fn open_at(state: &mut AppState, step: WizardStep) {
        update(state, Message::Ui(UiMsg::Open(DialogId::Wizard)));
        state.wizard.step = step;
        park_focus(state);
    }

    fn focus_path(state: &AppState) -> Vec<String> {
        state
            .ui
            .focus
            .path()
            .iter()
            .map(|id| id.as_str().to_string())
            .collect()
    }

    // ── Auto-open ─────────────────────────────────────────────────────────────

    #[test]
    fn a_machine_with_no_cert_and_no_tokens_opens_the_wizard_on_welcome() {
        let mut state = AppState::new();
        both_loads(&mut state, None, Vec::new());
        assert_eq!(state.top_dialog(), Some(DialogId::Wizard));
        assert_eq!(state.wizard.step, WizardStep::Welcome);
        assert_eq!(focus_path(&state), vec!["wizard", "wiz_next"]);
    }

    #[test]
    fn auto_open_dispatches_the_config_load_the_network_step_needs() {
        let mut state = AppState::new();
        update(
            &mut state,
            Message::CertInfoLoaded {
                fingerprint: None,
                sans: Vec::new(),
            },
        );
        let actions = update(&mut state, Message::TokensLoaded(Vec::new()));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadConfig))
        );
    }

    #[test]
    fn a_machine_with_a_certificate_does_not_auto_open() {
        let mut state = AppState::new();
        both_loads(&mut state, Some("ab12"), Vec::new());
        assert_eq!(state.top_dialog(), None);
        assert!(state.wizard.first_run_checked);
    }

    #[test]
    fn a_machine_with_tokens_does_not_auto_open() {
        let mut state = AppState::new();
        both_loads(&mut state, None, vec![record("phone")]);
        assert_eq!(state.top_dialog(), None);
    }

    #[test]
    fn one_load_alone_is_not_enough_to_decide() {
        let mut state = AppState::new();
        update(&mut state, Message::TokensLoaded(Vec::new()));
        assert_eq!(state.top_dialog(), None, "the cert is still unknown");
        assert!(!state.wizard.first_run_checked);
        // Two tokens loads must not be mistaken for one of each.
        update(&mut state, Message::TokensLoaded(Vec::new()));
        assert_eq!(state.top_dialog(), None);
        assert!(!state.wizard.first_run_checked);
    }

    #[test]
    fn auto_open_happens_at_most_once() {
        let mut state = AppState::new();
        both_loads(&mut state, None, Vec::new());
        assert_eq!(state.top_dialog(), Some(DialogId::Wizard));
        wizard(&mut state, WizardMsg::Close);
        assert_eq!(state.top_dialog(), None);

        both_loads(&mut state, None, Vec::new());
        assert_eq!(
            state.top_dialog(),
            None,
            "a later reload must not re-open the wizard"
        );
    }

    #[test]
    fn auto_open_never_covers_a_dialog_the_operator_opened() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Tokens)));
        both_loads(&mut state, None, Vec::new());
        assert_eq!(state.top_dialog(), Some(DialogId::Tokens));
    }

    // ── Walking the steps ─────────────────────────────────────────────────────

    #[test]
    fn welcome_advances_and_records_whether_a_cert_existed() {
        let mut state = AppState::new();
        state.cert.fingerprint = Some("ab12".to_string());
        open_at(&mut state, WizardStep::Welcome);
        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(actions.is_empty());
        assert_eq!(state.wizard.step, WizardStep::Network);
        assert!(state.wizard.cert_existed);
        assert_eq!(focus_path(&state), vec!["wizard", "wiz_next"]);
    }

    #[test]
    fn network_saves_the_bind_address_and_advances_on_the_action_ok() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Network);
        state.config.host = "10.0.0.5".to_string();
        state.config.port = "50051".to_string();

        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::SaveBind(addr) if addr == "10.0.0.5:50051"))
        );
        assert!(state.wizard.busy);
        assert_eq!(state.wizard.step, WizardStep::Network);

        update(&mut state, Message::ActionOk("Saved.".to_string()));
        assert_eq!(state.wizard.step, WizardStep::Certificate);
        assert!(!state.wizard.busy);
        assert_eq!(
            state.top_dialog(),
            Some(DialogId::Wizard),
            "the config save must not close the wizard"
        );
    }

    #[test]
    fn network_shows_a_validation_failure_instead_of_advancing() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Network);
        state.config.host = "   ".to_string();
        state.config.port = "50051".to_string();

        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(actions.is_empty());
        assert!(!state.wizard.busy);
        assert_eq!(state.wizard.step, WizardStep::Network);
        assert_eq!(
            state.wizard.error.as_deref(),
            Some("Bind host must not be empty.")
        );
    }

    #[test]
    fn certificate_without_one_on_disk_generates_the_planned_sans() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Certificate);
        state.config.host = "10.0.0.5".to_string();

        let actions = wizard(&mut state, WizardMsg::Next);
        let sans = actions
            .iter()
            .find_map(|a| match a {
                UpdateAction::EnsureCert(sans) => Some(sans.clone()),
                _ => None,
            })
            .expect("EnsureCert");
        assert_eq!(
            sans,
            super::super::cert::build_sans_from_config(&state),
            "the wizard must ask for exactly the planned SANs"
        );
        assert!(state.wizard.busy);

        update(
            &mut state,
            Message::CertEnsured {
                fingerprint: "ab12".to_string(),
                sans: vec!["10.0.0.5".to_string()],
            },
        );
        assert_eq!(state.wizard.step, WizardStep::Token);
        assert!(!state.wizard.busy);
    }

    #[test]
    fn certificate_with_one_on_disk_and_no_ack_keeps_it() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Certificate);
        state.cert.fingerprint = Some("ab12".to_string());

        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(actions.is_empty(), "keeping a cert generates nothing");
        assert!(!state.wizard.busy);
        assert_eq!(state.wizard.step, WizardStep::Token);
    }

    #[test]
    fn certificate_with_the_ack_regenerates() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Certificate);
        state.cert.fingerprint = Some("ab12".to_string());
        wizard(&mut state, WizardMsg::RegenAckChanged(true));
        assert!(state.wizard.cert_regen_ack);

        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::EnsureCert(_)))
        );
        assert_eq!(state.wizard.step, WizardStep::Certificate);
        assert!(state.wizard.busy);
    }

    #[test]
    fn token_creates_one_from_the_form_and_advances_without_the_minted_dialog() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Token);
        state.tokens.form_name = "  my phone  ".to_string();
        state.tokens.form_read_only = true;
        state.tokens.form_expiry = TokenExpiryChoice::OneHour;

        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(actions.iter().any(|a| matches!(
            a,
            UpdateAction::CreateToken { name, read_only, expiry_secs }
                if name.as_deref() == Some("my phone")
                    && *read_only
                    && *expiry_secs == Some(3600)
        )));
        assert!(state.wizard.busy);

        update(
            &mut state,
            Message::TokenCreated {
                token: "plaintext".to_string(),
                name: "my phone".to_string(),
                read_only: true,
            },
        );
        assert_eq!(state.wizard.step, WizardStep::Start);
        assert_eq!(
            state.top_dialog(),
            Some(DialogId::Wizard),
            "the minted dialog must not be left on top of the wizard"
        );
        assert!(state.tokens.last_minted_secret.is_some());
    }

    #[test]
    fn an_empty_token_name_is_left_to_the_generator() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Token);
        state.tokens.form_name = "   ".to_string();
        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::CreateToken { name: None, .. }))
        );
    }

    #[test]
    fn start_launches_a_stopped_daemon_and_advances_when_it_reports_in() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Start);
        state.tokens.last_minted_secret =
            Some(("my phone".to_string(), "plaintext".to_string(), false));

        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::StartServer))
        );
        assert!(state.wizard.busy);

        // A status result with no daemon in it is not arrival.
        update(&mut state, Message::StatusLoaded(None));
        assert_eq!(state.wizard.step, WizardStep::Start);
        assert!(state.wizard.busy);

        let actions = update(&mut state, Message::StatusLoaded(Some(server_info(0))));
        assert_eq!(state.wizard.step, WizardStep::Pair);
        assert!(!state.wizard.busy);
        assert!(actions.iter().any(|a| matches!(
            a,
            UpdateAction::ShowTokenQr { token, .. } if token == "plaintext"
        )));
    }

    #[test]
    fn start_walks_straight_on_when_the_daemon_is_already_up() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Start);
        state.server.status = Some(server_info(0));
        state.cert.advertise_trust = AdvertiseTrust::Pin;
        state.tokens.last_minted_secret =
            Some(("my phone".to_string(), "plaintext".to_string(), true));

        let actions = wizard(&mut state, WizardMsg::Next);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, UpdateAction::StartServer))
        );
        assert_eq!(state.wizard.step, WizardStep::Pair);
        assert!(actions.iter().any(|a| matches!(
            a,
            UpdateAction::ShowTokenQr {
                read_only: true,
                advertise_trust: AdvertiseTrust::Pin,
                ..
            }
        )));
    }

    #[test]
    fn entering_pair_opens_the_overlay_for_the_minted_token_only() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Start);
        state.server.status = Some(server_info(0));
        state.tokens.last_minted_secret =
            Some(("my phone".to_string(), "plaintext".to_string(), false));

        wizard(&mut state, WizardMsg::Next);
        let overlay = state.tokens.qr_overlay.as_ref().expect("overlay");
        assert!(matches!(overlay.phase, QrOverlayPhase::Generating));
        assert_eq!(overlay.token_name, "my phone");
        assert_eq!(overlay.seq, state.qr_seq);
        assert_eq!(
            state.top_dialog(),
            Some(DialogId::Wizard),
            "the Pair step paints the overlay itself; no Qr layer is opened"
        );
        assert_eq!(focus_path(&state), vec!["wizard", "wiz_done"]);
    }

    #[test]
    fn entering_pair_without_a_secret_opens_no_overlay() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Start);
        state.server.status = Some(server_info(0));
        let actions = wizard(&mut state, WizardMsg::Next);
        assert_eq!(state.wizard.step, WizardStep::Pair);
        assert!(actions.is_empty());
        assert!(state.tokens.qr_overlay.is_none());
    }

    // ── Back, the ends, and the guards ────────────────────────────────────────

    #[test]
    fn back_from_certificate_returns_to_network_with_no_side_effects() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Certificate);
        let actions = wizard(&mut state, WizardMsg::Back);
        assert!(actions.is_empty());
        assert_eq!(state.wizard.step, WizardStep::Network);
        assert!(!state.cert.loading && !state.tokens.loading);
        assert_eq!(focus_path(&state), vec!["wizard", "wiz_next"]);
    }

    #[test]
    fn back_from_pair_drops_the_qr_it_opened() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Start);
        state.server.status = Some(server_info(0));
        state.tokens.last_minted_secret =
            Some(("my phone".to_string(), "plaintext".to_string(), false));
        wizard(&mut state, WizardMsg::Next);
        assert!(state.tokens.qr_overlay.is_some());

        wizard(&mut state, WizardMsg::Back);
        assert_eq!(state.wizard.step, WizardStep::Start);
        assert!(state.tokens.qr_overlay.is_none());
        assert!(
            state.tokens.last_minted_secret.is_some(),
            "leaving the QR never touches the token"
        );
    }

    #[test]
    fn the_ends_clamp() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Welcome);
        assert!(wizard(&mut state, WizardMsg::Back).is_empty());
        assert_eq!(state.wizard.step, WizardStep::Welcome);

        open_at(&mut state, WizardStep::Pair);
        assert!(wizard(&mut state, WizardMsg::Next).is_empty());
        assert_eq!(state.wizard.step, WizardStep::Pair);
    }

    #[test]
    fn busy_blocks_both_directions() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Certificate);
        state.wizard.busy = true;
        assert!(wizard(&mut state, WizardMsg::Next).is_empty());
        assert!(wizard(&mut state, WizardMsg::Back).is_empty());
        assert_eq!(state.wizard.step, WizardStep::Certificate);
    }

    #[test]
    fn a_failure_clears_busy_and_shows_the_reason_on_the_step() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Certificate);
        state.wizard.busy = true;
        update(&mut state, Message::ActionFailed("disk full".to_string()));
        assert!(!state.wizard.busy);
        assert_eq!(state.wizard.error.as_deref(), Some("disk full"));
        assert_eq!(state.wizard.step, WizardStep::Certificate);
        assert_eq!(state.top_dialog(), Some(DialogId::Wizard));
    }

    #[test]
    fn a_failure_with_nothing_in_flight_is_ignored() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Certificate);
        update(&mut state, Message::ActionFailed("unrelated".to_string()));
        assert!(state.wizard.error.is_none());
    }

    #[test]
    fn the_hooks_are_inert_while_the_wizard_is_shut() {
        let mut state = AppState::new();
        state.wizard.busy = true;
        state.wizard.step = WizardStep::Network;
        assert!(on_action_ok(&mut state, "ok").is_empty());
        assert!(on_action_failed(&mut state, "err").is_empty());
        assert!(on_tick(&mut state).is_empty());
        assert!(on_cert_ensured(&mut state).is_empty());
        assert!(on_token_created(&mut state).is_empty());
        assert!(on_status_loaded(&mut state).is_empty());
        assert_eq!(state.wizard.step, WizardStep::Network);
        assert!(state.wizard.busy);
    }

    // ── Close and finish ──────────────────────────────────────────────────────

    #[test]
    fn close_drops_the_qr_and_the_in_flight_flags() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Pair);
        state.wizard.busy = true;
        state.wizard.error = Some("boom".to_string());
        state.tokens.last_minted_secret =
            Some(("my phone".to_string(), "plaintext".to_string(), false));
        state.tokens.qr_overlay = Some(QrOverlay {
            phase: QrOverlayPhase::Generating,
            seq: 1,
            baseline_clients: 0,
            token_name: "my phone".to_string(),
            read_only: false,
            tick_counter: 0,
        });

        wizard(&mut state, WizardMsg::Close);
        assert_eq!(state.top_dialog(), None);
        assert!(state.tokens.qr_overlay.is_none());
        assert!(!state.wizard.busy);
        assert!(state.wizard.error.is_none());
        assert!(
            state.tokens.last_minted_secret.is_some(),
            "closing the wizard never revokes or forgets the token"
        );
    }

    #[test]
    fn close_pops_a_dialog_that_landed_on_top_of_the_wizard() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Token);
        super::super::open_hook(&mut state, DialogId::TokenMinted);
        assert_eq!(state.top_dialog(), Some(DialogId::TokenMinted));

        wizard(&mut state, WizardMsg::Close);
        assert_eq!(state.top_dialog(), None);
    }

    #[test]
    fn finish_closes_and_reloads_the_dashboard() {
        let mut state = AppState::new();
        open_at(&mut state, WizardStep::Pair);
        let actions = wizard(&mut state, WizardMsg::Finish);
        assert_eq!(state.top_dialog(), None);
        assert!(state.tokens.qr_overlay.is_none());
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadCertInfo))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RefreshStatus))
        );
    }

    #[test]
    fn the_dashboard_key_opens_the_wizard() {
        use ratcn::runtime::{KeyCode, KeyEvent};
        let mut state = AppState::new();
        update(&mut state, Message::Key(KeyEvent::new(KeyCode::Char('w'))));
        assert_eq!(state.top_dialog(), Some(DialogId::Wizard));
    }
}
