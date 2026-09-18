//! The token reducer: the list, the create form, the one-time secret, the
//! revoke confirmation, and the pairing QR layer's own state machine.
//!
//! The QR layer is opened for the **freshly minted** token — the only one whose
//! plaintext muxrctl still holds. It is a real user token, so closing the layer
//! never revokes anything; see [`update`]'s `QrClose` arm.

use crate::app::action::UpdateAction;
use crate::app::state::tokens::{QrOverlay, QrOverlayPhase, TokenExpiryChoice, TokensMsg};
use crate::app::state::{AppState, DialogId};
use crate::server::tokens::TokenRecord;

/// Apply a [`TokensMsg`].
pub fn update(state: &mut AppState, msg: TokensMsg) -> Vec<UpdateAction> {
    match msg {
        TokensMsg::Close => super::close_top(state),

        TokensMsg::Focused(name) => {
            state.tokens.focused = Some(name);
            Vec::new()
        }

        // ── Create ────────────────────────────────────────────────────────────
        TokensMsg::CreateRequested => {
            state.tokens.reset_form();
            super::open_hook(state, DialogId::TokenCreate)
        }
        TokensMsg::CreateNameChanged(name) => {
            state.tokens.form_name = name;
            Vec::new()
        }
        TokensMsg::CreateReadOnlyChanged(read_only) => {
            state.tokens.form_read_only = read_only;
            Vec::new()
        }
        TokensMsg::CreateExpiryChanged(index) => {
            state.tokens.form_expiry = TokenExpiryChoice::from_index(index);
            Vec::new()
        }
        TokensMsg::CreateSubmit => {
            if state.tokens.loading {
                return Vec::new();
            }
            let name = state.tokens.form_name.trim();
            let name = (!name.is_empty()).then(|| name.to_string());
            let read_only = state.tokens.form_read_only;
            let expiry_secs = state.tokens.form_expiry.ttl_secs();
            state.tokens.loading = true;
            vec![UpdateAction::CreateToken {
                name,
                read_only,
                expiry_secs,
            }]
        }
        TokensMsg::CreateCancel => super::close_top(state),

        // ── The one-time secret ───────────────────────────────────────────────
        //
        // `last_minted_secret` deliberately survives this dialog: the token list
        // offers "Show QR" for as long as the plaintext is held, so dismissing
        // the secret is not the same as throwing it away. A create, a revoke or
        // a reload is what clears it.
        TokensMsg::MintedDone => super::close_top(state),
        TokensMsg::MintedShowQr | TokensMsg::ShowQrForMinted => open_qr(state),

        // ── Revoke ────────────────────────────────────────────────────────────
        TokensMsg::RevokeRequested => {
            if state.tokens.loading || state.tokens.focused_record().is_none() {
                return Vec::new();
            }
            super::open_hook(state, DialogId::TokenRevokeConfirm)
        }
        TokensMsg::RevokeConfirmed => {
            let mut actions = super::close_top(state);
            let Some(name) = state.tokens.focused.clone() else {
                return actions;
            };
            state.tokens.loading = true;
            // The revoked token may be the one whose plaintext we still hold.
            state.tokens.last_minted_secret = None;
            actions.push(UpdateAction::RevokeToken(name));
            actions
        }
        TokensMsg::RevokeCancelled => super::close_top(state),

        // ── QR layer ──────────────────────────────────────────────────────────
        TokensMsg::QrClose => {
            // NEVER revokes: the QR encodes a real user token the operator
            // asked to pair with, not a throwaway pairing secret.
            state.tokens.qr_overlay = None;
            if state.top_dialog() == Some(DialogId::Qr) {
                return super::close_top(state);
            }
            Vec::new()
        }

        TokensMsg::Refresh => {
            state.tokens.loading = true;
            vec![UpdateAction::LoadTokens]
        }
    }
}

/// Open the pairing QR layer for the held minted secret.
///
/// Exactly what the pre-dialog Tokens screen did on Enter: bump the process
/// sequence, open the layer in `Generating`, and dispatch the build with the
/// `advertise_trust` that was active at this moment — not whatever it is when
/// the async task gets around to reading it.
fn open_qr(state: &mut AppState) -> Vec<UpdateAction> {
    let Some((name, secret, read_only)) = state.tokens.last_minted_secret.clone() else {
        state
            .toast_err("Create a token first — only a freshly minted secret can be shown as a QR.");
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
    let mut actions = super::open_hook(state, DialogId::Qr);
    actions.push(UpdateAction::ShowTokenQr {
        token: secret,
        read_only,
        seq,
        advertise_trust,
    });
    actions
}

// ── Async results ─────────────────────────────────────────────────────────────

/// The token list came back.
pub fn on_tokens_loaded(state: &mut AppState, records: Vec<TokenRecord>) -> Vec<UpdateAction> {
    state.tokens.tokens = records;
    // Keep the cursor on the same token when it survived the reload; otherwise
    // fall to the first row, or to nothing at all on an empty list.
    let still_present = state
        .tokens
        .focused
        .as_deref()
        .is_some_and(|name| state.tokens.tokens.iter().any(|t| t.name == name));
    if !still_present {
        state.tokens.focused = state.tokens.tokens.first().map(|t| t.name.clone());
    }
    state.tokens.loading = false;
    Vec::new()
}

/// A token was minted: hold its one-time plaintext, swap the create dialog for
/// the secret dialog, and refresh the list behind them.
pub fn on_token_created(
    state: &mut AppState,
    token: String,
    name: String,
    read_only: bool,
) -> Vec<UpdateAction> {
    state.tokens.last_minted_secret = Some((name, token, read_only));
    state.tokens.loading = false;
    state.tokens.reset_form();
    if state.top_dialog() == Some(DialogId::TokenCreate) {
        state.close_dialog();
    }
    let mut actions = super::open_hook(state, DialogId::TokenMinted);
    actions.push(UpdateAction::LoadTokens);
    actions
}

/// A token was created or revoked; the list needs a reload.
pub fn on_tokens_changed(state: &mut AppState) -> Vec<UpdateAction> {
    state.tokens.last_minted_secret = None;
    state.tokens.loading = true;
    vec![UpdateAction::LoadTokens]
}

/// The QR URI is ready. A result whose `seq` no longer matches the live layer
/// is discarded — nothing was minted, so there is no token to revoke.
pub fn on_qr_ready(
    state: &mut AppState,
    uri: String,
    host: String,
    port: u16,
    fingerprint_short: String,
    baseline_clients: usize,
    seq: u64,
) -> Vec<UpdateAction> {
    if let Some(overlay) = state.tokens.qr_overlay.as_mut()
        && overlay.seq == seq
    {
        overlay.baseline_clients = baseline_clients;
        overlay.phase = QrOverlayPhase::Showing {
            uri,
            host,
            port,
            fingerprint_short,
        };
    }
    Vec::new()
}

/// QR generation failed; same seq guard as [`on_qr_ready`].
pub fn on_qr_failed(state: &mut AppState, err: String, seq: u64) -> Vec<UpdateAction> {
    if let Some(overlay) = state.tokens.qr_overlay.as_mut()
        && overlay.seq == seq
    {
        overlay.phase = QrOverlayPhase::Failed { err };
    }
    Vec::new()
}

/// What a status poll result means for the QR layer.
///
/// `client_count` is `None` when the daemon is stopped — there is no count to
/// compare against, so the layer is left alone.
pub fn on_status_for_qr(state: &mut AppState, client_count: Option<usize>) -> Vec<UpdateAction> {
    if let Some(n) = client_count {
        check_overlay_connection(state, n);
    }
    Vec::new()
}

/// Promote the QR layer to `Connected` if a new client appeared.
///
/// The rise is a heuristic (the attached-client count went up), not verified
/// per-token auth.
pub(crate) fn check_overlay_connection(state: &mut AppState, current_clients: usize) {
    if let Some(overlay) = state.tokens.qr_overlay.as_mut()
        && let QrOverlayPhase::Showing { .. } = &overlay.phase
        && current_clients > overlay.baseline_clients
    {
        overlay.phase = QrOverlayPhase::Connected;
    }
}

/// The QR layer's own ~1 s status poll, so a connection is noticed while the QR
/// is up. It shares `server.loading` with the dashboard poll, so the two never
/// spawn concurrent status reads.
pub fn on_tick(state: &mut AppState) -> Vec<UpdateAction> {
    if state.server.loading {
        return Vec::new();
    }
    let Some(overlay) = state.tokens.qr_overlay.as_mut() else {
        return Vec::new();
    };
    if !matches!(overlay.phase, QrOverlayPhase::Showing { .. }) {
        return Vec::new();
    }
    overlay.tick_counter = overlay.tick_counter.wrapping_add(1);
    if overlay.tick_counter.is_multiple_of(20) {
        state.server.loading = true;
        return vec![UpdateAction::RefreshStatus];
    }
    Vec::new()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::{Message, UiMsg};
    use crate::app::state::cert::AdvertiseTrust;
    use crate::app::update::tests::server_info;
    use crate::app::update::update;

    fn record(name: &str, read_only: bool) -> TokenRecord {
        TokenRecord {
            name: name.to_string(),
            created_at: "2026-01-01 00:00:00".to_string(),
            read_only,
        }
    }

    fn tokens_msg(state: &mut AppState, msg: TokensMsg) -> Vec<UpdateAction> {
        update(state, Message::Ui(UiMsg::Tokens(msg)))
    }

    fn open_tokens(state: &mut AppState) {
        update(state, Message::Ui(UiMsg::Open(DialogId::Tokens)));
    }

    fn open_overlay(state: &mut AppState, seq: u64, phase: QrOverlayPhase) {
        state.qr_seq = seq;
        state.tokens.qr_overlay = Some(QrOverlay {
            phase,
            seq,
            baseline_clients: 0,
            token_name: "phone".to_string(),
            read_only: false,
            tick_counter: 0,
        });
    }

    fn showing() -> QrOverlayPhase {
        QrOverlayPhase::Showing {
            uri: "muxr://pair?v=2".to_string(),
            host: "10.0.0.1".to_string(),
            port: 50051,
            fingerprint_short: "abc…".to_string(),
        }
    }

    /// Seed the state a minted secret leaves behind, without the async round trip.
    fn with_minted_secret(state: &mut AppState) {
        state.tokens.last_minted_secret =
            Some(("my-tok".to_string(), "plaintext-secret".to_string(), false));
    }

    // ── List ──────────────────────────────────────────────────────────────────

    #[test]
    fn tokens_loaded_keeps_the_focus_on_the_same_token() {
        let mut state = AppState::new();
        state.tokens.focused = Some("b".to_string());
        state.tokens.loading = true;
        update(
            &mut state,
            Message::TokensLoaded(vec![record("a", false), record("b", true)]),
        );
        assert_eq!(state.tokens.tokens.len(), 2);
        assert_eq!(state.tokens.focused.as_deref(), Some("b"));
        assert!(!state.tokens.loading);
    }

    #[test]
    fn tokens_loaded_clamps_focus_onto_the_first_surviving_token() {
        let mut state = AppState::new();
        state.tokens.focused = Some("gone".to_string());
        update(&mut state, Message::TokensLoaded(vec![record("a", false)]));
        assert_eq!(state.tokens.focused.as_deref(), Some("a"));

        update(&mut state, Message::TokensLoaded(Vec::new()));
        assert_eq!(state.tokens.focused, None, "an empty list focuses nothing");
    }

    #[test]
    fn focused_records_the_name_the_list_moved_to() {
        let mut state = AppState::new();
        tokens_msg(&mut state, TokensMsg::Focused("b".to_string()));
        assert_eq!(state.tokens.focused.as_deref(), Some("b"));
    }

    #[test]
    fn close_closes_the_token_dialog_and_refreshes_the_list() {
        let mut state = AppState::new();
        open_tokens(&mut state);
        let actions = tokens_msg(&mut state, TokensMsg::Close);
        assert_eq!(state.top_dialog(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }

    #[test]
    fn refresh_reloads_the_list() {
        let mut state = AppState::new();
        let actions = tokens_msg(&mut state, TokensMsg::Refresh);
        assert!(state.tokens.loading);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }

    // ── Create ────────────────────────────────────────────────────────────────

    #[test]
    fn create_requested_opens_a_fresh_form() {
        let mut state = AppState::new();
        state.tokens.form_name = "stale".to_string();
        state.tokens.form_read_only = true;
        state.tokens.form_expiry = TokenExpiryChoice::OneDay;
        tokens_msg(&mut state, TokensMsg::CreateRequested);
        assert_eq!(state.top_dialog(), Some(DialogId::TokenCreate));
        assert!(state.tokens.form_name.is_empty());
        assert!(!state.tokens.form_read_only);
        assert_eq!(state.tokens.form_expiry, TokenExpiryChoice::Never);
    }

    #[test]
    fn create_form_fields_are_bound_through_their_messages() {
        let mut state = AppState::new();
        tokens_msg(
            &mut state,
            TokensMsg::CreateNameChanged("phone".to_string()),
        );
        tokens_msg(&mut state, TokensMsg::CreateReadOnlyChanged(true));
        tokens_msg(
            &mut state,
            TokensMsg::CreateExpiryChanged(TokenExpiryChoice::OneHour.index()),
        );
        assert_eq!(state.tokens.form_name, "phone");
        assert!(state.tokens.form_read_only);
        assert_eq!(state.tokens.form_expiry, TokenExpiryChoice::OneHour);
    }

    #[test]
    fn create_submit_trims_the_name_and_carries_the_expiry() {
        let mut state = AppState::new();
        state.tokens.form_name = "  phone  ".to_string();
        state.tokens.form_read_only = true;
        state.tokens.form_expiry = TokenExpiryChoice::OneDay;
        let actions = tokens_msg(&mut state, TokensMsg::CreateSubmit);
        assert!(state.tokens.loading);
        assert!(actions.iter().any(|a| matches!(
            a,
            UpdateAction::CreateToken { name, read_only, expiry_secs }
                if name.as_deref() == Some("phone")
                    && *read_only
                    && *expiry_secs == Some(24 * 60 * 60)
        )));
    }

    #[test]
    fn create_submit_with_a_blank_name_asks_muxrd_to_generate_one() {
        let mut state = AppState::new();
        state.tokens.form_name = "   ".to_string();
        let actions = tokens_msg(&mut state, TokensMsg::CreateSubmit);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::CreateToken { name, .. } if name.is_none()))
        );
    }

    #[test]
    fn create_submit_is_single_flight() {
        let mut state = AppState::new();
        state.tokens.loading = true;
        assert!(tokens_msg(&mut state, TokensMsg::CreateSubmit).is_empty());
    }

    #[test]
    fn create_cancel_closes_the_form() {
        let mut state = AppState::new();
        tokens_msg(&mut state, TokensMsg::CreateRequested);
        tokens_msg(&mut state, TokensMsg::CreateCancel);
        assert_eq!(state.top_dialog(), None);
    }

    #[test]
    fn token_created_swaps_the_create_dialog_for_the_secret_and_reloads() {
        let mut state = AppState::new();
        tokens_msg(&mut state, TokensMsg::CreateRequested);
        state.tokens.form_name = "phone".to_string();
        let actions = update(
            &mut state,
            Message::TokenCreated {
                token: "plaintext".to_string(),
                name: "phone".to_string(),
                read_only: true,
            },
        );
        assert_eq!(
            state.tokens.last_minted_secret,
            Some(("phone".to_string(), "plaintext".to_string(), true))
        );
        assert_eq!(state.top_dialog(), Some(DialogId::TokenMinted));
        assert!(!state.ui.modals.is_open(DialogId::TokenCreate.id()));
        assert!(state.tokens.form_name.is_empty());
        assert!(!state.tokens.loading);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }

    #[test]
    fn minted_done_closes_the_dialog_but_keeps_the_secret() {
        let mut state = AppState::new();
        with_minted_secret(&mut state);
        state.open_dialog(DialogId::TokenMinted);
        tokens_msg(&mut state, TokensMsg::MintedDone);
        assert_eq!(state.top_dialog(), None);
        assert!(
            state.tokens.last_minted_secret.is_some(),
            "'Show QR' on the list still needs the plaintext"
        );
    }

    #[test]
    fn tokens_changed_drops_the_secret_and_reloads() {
        let mut state = AppState::new();
        with_minted_secret(&mut state);
        let actions = update(&mut state, Message::TokensChanged);
        assert!(state.tokens.last_minted_secret.is_none());
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }

    // ── Revoke ────────────────────────────────────────────────────────────────

    #[test]
    fn revoke_requested_needs_a_focused_token() {
        let mut state = AppState::new();
        assert!(tokens_msg(&mut state, TokensMsg::RevokeRequested).is_empty());
        assert_eq!(state.top_dialog(), None);

        state.tokens.tokens = vec![record("phone", false)];
        state.tokens.focused = Some("phone".to_string());
        tokens_msg(&mut state, TokensMsg::RevokeRequested);
        assert_eq!(state.top_dialog(), Some(DialogId::TokenRevokeConfirm));
    }

    #[test]
    fn revoke_confirmed_revokes_the_focused_token_and_drops_the_secret() {
        let mut state = AppState::new();
        with_minted_secret(&mut state);
        state.tokens.tokens = vec![record("phone", false)];
        state.tokens.focused = Some("phone".to_string());
        tokens_msg(&mut state, TokensMsg::RevokeRequested);
        let actions = tokens_msg(&mut state, TokensMsg::RevokeConfirmed);
        assert_eq!(state.top_dialog(), None);
        assert!(state.tokens.loading);
        assert!(state.tokens.last_minted_secret.is_none());
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RevokeToken(name) if name == "phone"))
        );
    }

    #[test]
    fn revoke_cancelled_revokes_nothing() {
        let mut state = AppState::new();
        state.tokens.tokens = vec![record("phone", false)];
        state.tokens.focused = Some("phone".to_string());
        tokens_msg(&mut state, TokensMsg::RevokeRequested);
        let actions = tokens_msg(&mut state, TokensMsg::RevokeCancelled);
        assert_eq!(state.top_dialog(), None);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RevokeToken(_)))
        );
    }

    // ── QR layer ──────────────────────────────────────────────────────────────

    #[test]
    fn show_qr_opens_the_layer_and_dispatches_the_build() {
        let mut state = AppState::new();
        with_minted_secret(&mut state);
        state.cert.advertise_trust = AdvertiseTrust::Pin;
        let before_seq = state.qr_seq;
        let actions = tokens_msg(&mut state, TokensMsg::ShowQrForMinted);

        let overlay = state.tokens.qr_overlay.as_ref().expect("overlay");
        assert!(matches!(overlay.phase, QrOverlayPhase::Generating));
        assert_eq!(overlay.token_name, "my-tok");
        assert_eq!(overlay.seq, before_seq + 1);
        assert_eq!(state.qr_seq, before_seq + 1);
        assert_eq!(state.top_dialog(), Some(DialogId::Qr));
        assert!(actions.iter().any(|a| matches!(
            a,
            UpdateAction::ShowTokenQr { token, seq, advertise_trust, .. }
                if token == "plaintext-secret"
                    && *seq == overlay.seq
                    && *advertise_trust == AdvertiseTrust::Pin
        )));
    }

    #[test]
    fn show_qr_from_the_minted_dialog_takes_the_same_path() {
        let mut state = AppState::new();
        with_minted_secret(&mut state);
        state.open_dialog(DialogId::TokenMinted);
        let actions = tokens_msg(&mut state, TokensMsg::MintedShowQr);
        assert_eq!(state.top_dialog(), Some(DialogId::Qr));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::ShowTokenQr { .. }))
        );
    }

    #[test]
    fn show_qr_without_a_secret_explains_itself_instead() {
        let mut state = AppState::new();
        let actions = tokens_msg(&mut state, TokensMsg::ShowQrForMinted);
        assert!(actions.is_empty());
        assert!(state.tokens.qr_overlay.is_none());
        assert_eq!(state.ui.toasts.len(), 1);
    }

    /// The whole point of the layer: it shows a real user token, so closing it
    /// must never revoke anything.
    #[test]
    fn qr_close_clears_the_layer_and_never_revokes() {
        let mut state = AppState::new();
        with_minted_secret(&mut state);
        tokens_msg(&mut state, TokensMsg::ShowQrForMinted);
        let actions = tokens_msg(&mut state, TokensMsg::QrClose);
        assert!(state.tokens.qr_overlay.is_none());
        assert_eq!(state.top_dialog(), None);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RevokeToken(_))),
            "closing the QR must never revoke the token it showed"
        );
        assert!(
            state.tokens.last_minted_secret.is_some(),
            "the secret outlives the layer"
        );
    }

    #[test]
    fn qr_ready_with_a_matching_seq_shows_the_code() {
        let mut state = AppState::new();
        open_overlay(&mut state, 7, QrOverlayPhase::Generating);
        update(
            &mut state,
            Message::TokenQrReady {
                uri: "muxr://pair?v=2".to_string(),
                host: "10.0.0.1".to_string(),
                port: 50051,
                fingerprint_short: "abc…".to_string(),
                baseline_clients: 3,
                seq: 7,
            },
        );
        let overlay = state.tokens.qr_overlay.as_ref().unwrap();
        assert!(matches!(overlay.phase, QrOverlayPhase::Showing { .. }));
        assert_eq!(overlay.baseline_clients, 3);
    }

    #[test]
    fn qr_ready_with_a_stale_seq_is_ignored() {
        let mut state = AppState::new();
        open_overlay(&mut state, 7, QrOverlayPhase::Generating);
        update(
            &mut state,
            Message::TokenQrReady {
                uri: "muxr://pair?v=2".to_string(),
                host: "10.0.0.1".to_string(),
                port: 50051,
                fingerprint_short: "abc…".to_string(),
                baseline_clients: 3,
                seq: 6,
            },
        );
        let overlay = state.tokens.qr_overlay.as_ref().unwrap();
        assert!(matches!(overlay.phase, QrOverlayPhase::Generating));
    }

    #[test]
    fn qr_failed_with_a_matching_seq_sets_the_failed_phase() {
        let mut state = AppState::new();
        open_overlay(&mut state, 1, QrOverlayPhase::Generating);
        update(
            &mut state,
            Message::TokenQrFailed {
                err: "no cert".to_string(),
                seq: 1,
            },
        );
        assert!(matches!(
            state.tokens.qr_overlay.as_ref().unwrap().phase,
            QrOverlayPhase::Failed { .. }
        ));
    }

    #[test]
    fn overlay_connection_detected_when_the_client_count_rises() {
        let mut state = AppState::new();
        open_overlay(&mut state, 1, showing());
        update(&mut state, Message::StatusLoaded(Some(server_info(1))));
        assert!(matches!(
            state.tokens.qr_overlay.as_ref().unwrap().phase,
            QrOverlayPhase::Connected
        ));
    }

    #[test]
    fn overlay_connection_not_detected_without_a_rise() {
        let mut state = AppState::new();
        open_overlay(&mut state, 1, showing());
        update(&mut state, Message::StatusLoaded(Some(server_info(0))));
        assert!(matches!(
            state.tokens.qr_overlay.as_ref().unwrap().phase,
            QrOverlayPhase::Showing { .. }
        ));
    }

    #[test]
    fn overlay_and_dashboard_poll_coalesce_to_a_single_refresh() {
        // The overlay poll and the dashboard poll share the `server.loading`
        // single-flight guard, so at most ONE RefreshStatus is dispatched per
        // ~1 s window (no redundant concurrent status() reads).
        let mut state = AppState::new();
        open_overlay(&mut state, 1, showing());

        let mut total_refreshes = 0;
        for _ in 0..20 {
            let actions = update(&mut state, Message::Tick);
            total_refreshes += actions
                .iter()
                .filter(|a| matches!(a, UpdateAction::RefreshStatus))
                .count();
            // Clear the in-flight flag the way StatusLoaded would.
            state.server.loading = false;
        }
        assert_eq!(
            total_refreshes, 1,
            "exactly one RefreshStatus expected across the shared 20-tick window; got {total_refreshes}"
        );
    }
}
