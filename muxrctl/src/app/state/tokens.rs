//! Token state — the Tokens dialogs (list, create form, one-time secret,
//! revoke confirmation) and the fullscreen pairing QR layer.
//!
//! Everything here is plain data the reducer owns and the views read: no
//! drawing and no async types, per `docs/CORE_CODE_STANDARDS.md` § muxrctl
//! keeps its TEA split.

use crate::server::tokens::TokenRecord;

// ── Create-form vocabulary ────────────────────────────────────────────────────

/// Expiry choice in the token create dialog.
///
/// `Never` is the historical long-lived behaviour; the others time-box the
/// token so it can only be used to pair (call `Login`) within the window. Expiry
/// is enforced server-side by `muxrd::token_expiry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenExpiryChoice {
    /// Long-lived token — never expires (default).
    #[default]
    Never,
    /// Expires 30 minutes after creation.
    ThirtyMinutes,
    /// Expires 1 hour after creation.
    OneHour,
    /// Expires 24 hours after creation.
    OneDay,
    /// Expires 7 days after creation.
    SevenDays,
}

impl TokenExpiryChoice {
    /// Every choice in cycle order — the option list the create dialog's
    /// `Cycle` is built from, so the control's indices and
    /// [`from_index`](Self::from_index) can never disagree.
    pub const ALL: [Self; 5] = [
        Self::Never,
        Self::ThirtyMinutes,
        Self::OneHour,
        Self::OneDay,
        Self::SevenDays,
    ];

    /// Advance to the next choice (wraps).
    ///
    /// The create dialog's `Cycle` reports the index it moved to rather than
    /// asking for a successor, so nothing in the view calls this; it is kept as
    /// the one statement of cycle order that
    /// [`index`](Self::index)/[`from_index`](Self::from_index) are checked
    /// against.
    #[allow(dead_code)] // see above: the Cycle moves by index; tests pin the order.
    pub fn next(self) -> Self {
        Self::from_index(self.index() + 1)
    }

    /// Position in [`ALL`](Self::ALL) — what the `Cycle` reads back.
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|choice| *choice == self)
            .unwrap_or(0)
    }

    /// The choice at `index`, wrapping — what the `Cycle` emits.
    pub fn from_index(index: usize) -> Self {
        Self::ALL[index % Self::ALL.len()]
    }

    /// Time-to-live in seconds, or `None` for a non-expiring token.
    pub fn ttl_secs(self) -> Option<i64> {
        match self {
            Self::Never => None,
            Self::ThirtyMinutes => Some(30 * 60),
            Self::OneHour => Some(60 * 60),
            Self::OneDay => Some(24 * 60 * 60),
            Self::SevenDays => Some(7 * 24 * 60 * 60),
        }
    }

    /// Short human label for the create dialog.
    pub fn label(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::ThirtyMinutes => "30 minutes",
            Self::OneHour => "1 hour",
            Self::OneDay => "24 hours",
            Self::SevenDays => "7 days",
        }
    }
}

// ── Explanations ──────────────────────────────────────────────────────────────
//
// One sentence per control, painted muted under it. They say what the operator
// cannot see from the control itself — what the field is for, and what the
// choice costs.

/// Why the name field exists, and what an empty one means. The generator is
/// zellij's, not muxrd's (muxrd/src/grpc/token_ops.rs passes `None` through), so
/// the sentence names no component.
pub const TOKEN_EXPLAIN_NAME: &str =
    "A label for the phone or person; a name is generated when empty.";

/// What a read-only token may and may not do.
pub const TOKEN_EXPLAIN_READ_ONLY: &str = "A read-only token can navigate tabs, panes and spaces, \
     scroll and size its own view, but never types or changes the session.";

/// What happens when the expiry window passes.
///
/// Expiry gates *new* logins only: `muxrd/src/token_expiry.rs` § Semantics, whose
/// `is_expired` has a single production call site inside `Login`
/// (muxrd/src/grpc/token_ops.rs). A client that already exchanged the auth token
/// for a session token keeps that session until the session token's own TTL
/// lapses — the relay re-validates the *session* token, never the pairing token's
/// expiry sidecar. Only Revoke ends a live connection; do not claim otherwise
/// here.
pub const TOKEN_EXPLAIN_EXPIRY: &str = "After the window the token can no longer pair a new \
     device; a live session runs to its own TTL. Only Revoke disconnects a phone; 'never' lasts \
     until revoked.";

/// Why the minted secret is shown exactly once.
pub const TOKEN_EXPLAIN_SECRET: &str = "The secret is shown once and stored only as a hash. Scan \
     the QR now, or copy it; you cannot retrieve it later.";

// ── QR layer ──────────────────────────────────────────────────────────────────

/// Phase of the app-level QR layer shown for an already-created token.
///
/// This builds a QR for the **existing** plaintext token the user just minted
/// (the only one whose plaintext we still hold). No throwaway token is minted,
/// and the displayed token is never revoked on close.
#[derive(Debug, Clone)]
pub enum QrOverlayPhase {
    /// The QR URI is being built (task is in flight).
    Generating,
    /// QR is ready; displaying the code and waiting for a client to connect.
    Showing {
        uri: String,
        host: String,
        port: u16,
        fingerprint_short: String,
    },
    /// A new client connected (client_count > baseline).
    Connected,
    /// Generation failed with an error.
    Failed { err: String },
}

/// The app-level QR layer opened for a freshly minted token. While present it
/// renders fullscreen over the dashboard. Closing it never revokes the
/// underlying token — it is a real user token, not a throwaway pairing secret.
#[derive(Debug, Clone)]
pub struct QrOverlay {
    /// Current phase of the layer's state machine.
    pub phase: QrOverlayPhase,
    /// Stale-result guard: async results carry the seq they were started with;
    /// a result whose seq no longer matches the live layer's seq is ignored.
    pub seq: u64,
    /// Attached-client count captured when the QR became ready; a rise above
    /// this drives connection detection.
    pub baseline_clients: usize,
    /// Display-only name of the token (NEVER revoked on close).
    pub token_name: String,
    /// Whether the token grants read-only access (display only).
    pub read_only: bool,
    /// Tick counter used for the ~1 s status poll cadence while `Showing`.
    pub tick_counter: u32,
}

// ── Token state ───────────────────────────────────────────────────────────────

/// State for the token list, its create form, and the QR layer.
#[derive(Debug, Clone, Default)]
pub struct TokensState {
    /// All tokens currently in the DB.
    pub tokens: Vec<TokenRecord>,
    /// Name of the token the list's cursor is on, or `None` when the list is
    /// empty. Keyed by name rather than index so a reload cannot silently move
    /// the cursor onto a different token.
    pub focused: Option<String>,
    /// True while a load / create / revoke task is in flight.
    pub loading: bool,
    /// The one-time minted secret (held until the next create, revoke or
    /// reload clears it): `(name, plaintext, read_only)`.
    pub last_minted_secret: Option<(String, String, bool)>,
    /// Text typed into the create dialog's "name" field.
    pub form_name: String,
    /// Read-only toggle in the create dialog.
    pub form_read_only: bool,
    /// Expiry choice in the create dialog.
    pub form_expiry: TokenExpiryChoice,
    /// The fullscreen pairing QR layer, when one is open.
    pub qr_overlay: Option<QrOverlay>,
}

impl TokensState {
    /// The record the list cursor is on, if it still exists.
    pub fn focused_record(&self) -> Option<&TokenRecord> {
        let name = self.focused.as_deref()?;
        self.tokens.iter().find(|t| t.name == name)
    }

    /// Clear the create form back to its defaults.
    pub fn reset_form(&mut self) {
        self.form_name.clear();
        self.form_read_only = false;
        self.form_expiry = TokenExpiryChoice::default();
    }

    /// How many of the loaded tokens are read-only, and how many read-write.
    ///
    /// Returned as `(read_write, read_only)` for the dashboard's Tokens line.
    pub fn rw_ro_split(&self) -> (usize, usize) {
        let read_only = self.tokens.iter().filter(|t| t.read_only).count();
        (self.tokens.len() - read_only, read_only)
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Everything the token dialogs can ask for.
#[derive(Debug, Clone)]
pub enum TokensMsg {
    /// Close the top token dialog.
    Close,
    /// The list cursor moved onto the token with this name.
    Focused(String),
    /// Open the create dialog on a fresh form.
    CreateRequested,
    /// The create dialog's name field was edited.
    CreateNameChanged(String),
    /// The create dialog's read-only checkbox was toggled.
    CreateReadOnlyChanged(bool),
    /// The create dialog's expiry cycle moved to this index.
    CreateExpiryChanged(usize),
    /// Submit the create form.
    CreateSubmit,
    /// Abandon the create form.
    CreateCancel,
    /// Dismiss the minted-secret dialog (the secret stays available for a QR
    /// until the next create, revoke or reload).
    MintedDone,
    /// Show the pairing QR straight from the minted-secret dialog.
    MintedShowQr,
    /// Revoke the focused token — asks for confirmation first.
    RevokeRequested,
    /// The revoke confirmation was accepted.
    RevokeConfirmed,
    /// The revoke confirmation was dismissed.
    RevokeCancelled,
    /// Show the pairing QR for the held minted secret, from the token list.
    ShowQrForMinted,
    /// Close the QR layer. This never revokes the token it showed.
    QrClose,
    /// Reload the token list.
    Refresh,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, read_only: bool) -> TokenRecord {
        TokenRecord {
            name: name.to_string(),
            created_at: "2026-01-01".to_string(),
            read_only,
        }
    }

    #[test]
    fn expiry_choice_cycles_and_maps_to_ttl() {
        assert_eq!(TokenExpiryChoice::Never.ttl_secs(), None);
        assert_eq!(
            TokenExpiryChoice::Never.next(),
            TokenExpiryChoice::ThirtyMinutes
        );
        assert_eq!(TokenExpiryChoice::ThirtyMinutes.ttl_secs(), Some(30 * 60));
        assert_eq!(
            TokenExpiryChoice::SevenDays.next(),
            TokenExpiryChoice::Never
        );
        assert!(!TokenExpiryChoice::OneDay.label().is_empty());
    }

    /// The `Cycle` binding is index-based in both directions, so the two
    /// conversions must round-trip over every choice — an off-by-one here would
    /// silently create a token with the wrong expiry.
    #[test]
    fn expiry_choice_index_round_trips_and_wraps() {
        for (index, choice) in TokenExpiryChoice::ALL.iter().enumerate() {
            assert_eq!(choice.index(), index, "{choice:?}");
            assert_eq!(TokenExpiryChoice::from_index(index), *choice);
        }
        assert_eq!(
            TokenExpiryChoice::from_index(TokenExpiryChoice::ALL.len()),
            TokenExpiryChoice::Never,
            "the cycle wraps rather than panicking"
        );
    }

    #[test]
    fn rw_ro_split_counts_both_kinds() {
        let mut state = TokensState::default();
        assert_eq!(state.rw_ro_split(), (0, 0));
        state.tokens = vec![record("a", false), record("b", true), record("c", false)];
        assert_eq!(state.rw_ro_split(), (2, 1));
    }

    #[test]
    fn focused_record_follows_the_name_not_a_position() {
        let mut state = TokensState {
            tokens: vec![record("a", false), record("b", true)],
            focused: Some("b".to_string()),
            ..TokensState::default()
        };
        assert_eq!(state.focused_record().map(|t| t.read_only), Some(true));

        // A reload that drops "b" leaves the cursor naming nothing rather than
        // pointing at whatever slid into its slot.
        state.tokens = vec![record("a", false)];
        assert!(state.focused_record().is_none());
    }

    /// The expiry explanation must not promise that expiry tears down a live
    /// connection — it does not. `muxrd/src/token_expiry.rs` § Semantics: expiry
    /// gates *new* logins (its `is_expired` is called only from `Login`), while
    /// `muxrd/src/relay/inbound.rs` re-validates the *session* token and never
    /// consults the pairing token's expiry sidecar. Only Revoke disconnects a
    /// phone, so the sentence has to point at Revoke instead.
    #[test]
    fn expiry_explanation_does_not_claim_a_live_session_is_torn_down() {
        let explain = TOKEN_EXPLAIN_EXPIRY.to_lowercase();
        for forbidden in ["torn down", "tear down", "stops authenticating"] {
            assert!(
                !explain.contains(forbidden),
                "expiry explanation claims teardown-on-expiry ({forbidden:?}): \
                 {TOKEN_EXPLAIN_EXPIRY}"
            );
        }
        assert!(
            explain.contains("revoke"),
            "expiry explanation must name Revoke as what ends a live session: \
             {TOKEN_EXPLAIN_EXPIRY}"
        );
    }

    #[test]
    fn reset_form_restores_the_defaults() {
        let mut state = TokensState {
            form_name: "phone".to_string(),
            form_read_only: true,
            form_expiry: TokenExpiryChoice::OneDay,
            ..TokensState::default()
        };
        state.reset_form();
        assert!(state.form_name.is_empty());
        assert!(!state.form_read_only);
        assert_eq!(state.form_expiry, TokenExpiryChoice::Never);
    }
}
