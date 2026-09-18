//! The token reducer — a stub for the tokens card, plus the QR overlay's
//! connection detection and poll, which the dashboard's status poll drives.

use crate::app::action::UpdateAction;
use crate::app::state::AppState;
use crate::app::state::tokens::{QrOverlayPhase, TokensFormPhase, TokensMsg};
use crate::server::tokens::TokenRecord;

/// Apply a [`TokensMsg`]. The tokens card extends this.
pub fn update(state: &mut AppState, msg: TokensMsg) -> Vec<UpdateAction> {
    match msg {
        TokensMsg::Close => super::close_top(state),
    }
}

/// The token list came back.
pub fn on_tokens_loaded(state: &mut AppState, records: Vec<TokenRecord>) -> Vec<UpdateAction> {
    state.tokens.tokens = records;
    // Clamp the cursor to the new list.
    if state.tokens.cursor >= state.tokens.tokens.len() {
        state.tokens.cursor = state.tokens.tokens.len().saturating_sub(1);
    }
    state.tokens.loading = false;
    Vec::new()
}

/// A token was minted: hold its one-time plaintext and refresh the list.
pub fn on_token_created(
    state: &mut AppState,
    token: String,
    name: String,
    read_only: bool,
) -> Vec<UpdateAction> {
    state.tokens.last_minted_secret = Some((name, token, read_only));
    state.tokens.loading = true;
    state.tokens.form_phase = TokensFormPhase::Browsing;
    state.tokens.form_name = String::new();
    vec![UpdateAction::LoadTokens]
}

/// A token was created or revoked; the list needs a reload.
pub fn on_tokens_changed(state: &mut AppState) -> Vec<UpdateAction> {
    state.tokens.last_minted_secret = None;
    state.tokens.loading = true;
    vec![UpdateAction::LoadTokens]
}

/// The QR URI is ready. A result whose `seq` no longer matches the live overlay
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

/// What a status poll result means for the QR overlay.
///
/// `client_count` is `None` when the daemon is stopped — there is no count to
/// compare against, so the overlay is left alone.
pub fn on_status_for_qr(state: &mut AppState, client_count: Option<usize>) -> Vec<UpdateAction> {
    if let Some(n) = client_count {
        check_overlay_connection(state, n);
    }
    Vec::new()
}

/// Promote the QR overlay to `Connected` if a new client appeared.
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

/// The overlay's own ~1 s status poll, so a connection is noticed while the QR
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::{Message, UiMsg};
    use crate::app::state::DialogId;
    use crate::app::state::tokens::QrOverlay;
    use crate::app::update::tests::server_info;
    use crate::app::update::update;

    fn record(name: &str, read_only: bool) -> TokenRecord {
        TokenRecord {
            name: name.to_string(),
            created_at: "2026-01-01 00:00:00".to_string(),
            read_only,
        }
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

    #[test]
    fn tokens_loaded_updates_the_list_and_clamps_the_cursor() {
        let mut state = AppState::new();
        state.tokens.cursor = 5;
        state.tokens.loading = true;
        update(
            &mut state,
            Message::TokensLoaded(vec![record("a", false), record("b", true)]),
        );
        assert_eq!(state.tokens.tokens.len(), 2);
        assert_eq!(state.tokens.cursor, 1);
        assert!(!state.tokens.loading);
    }

    #[test]
    fn token_created_stores_the_secret_and_reloads() {
        let mut state = AppState::new();
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
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }

    #[test]
    fn tokens_changed_drops_the_secret_and_reloads() {
        let mut state = AppState::new();
        state.tokens.last_minted_secret = Some(("n".to_string(), "p".to_string(), false));
        let actions = update(&mut state, Message::TokensChanged);
        assert!(state.tokens.last_minted_secret.is_none());
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
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

    #[test]
    fn close_closes_the_token_dialog_and_refreshes_the_list() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Tokens)));
        let actions = update(&mut state, Message::Ui(UiMsg::Tokens(TokensMsg::Close)));
        assert_eq!(state.top_dialog(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }
}
