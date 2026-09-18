//! Token state — the dashboard's Tokens section, the QR overlay, and a stub for
//! the tokens card's own dialogs.

use crate::server::tokens::TokenRecord;

/// The phase of the token create mini-form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokensFormPhase {
    /// The list is shown; user browses or initiates create/revoke.
    #[default]
    Browsing,
    /// The "create" form is open; user types a name.
    #[allow(dead_code)] // entered by the tokens card's create action.
    Creating,
}

/// Expiry choice in the token create form.
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

// The create form is the tokens card's; these helpers exist so that card only
// has to add its view and reducer arms.
#[allow(dead_code)]
impl TokenExpiryChoice {
    /// Advance to the next choice (wraps), for the create-form cycle control.
    pub fn next(self) -> Self {
        match self {
            Self::Never => Self::ThirtyMinutes,
            Self::ThirtyMinutes => Self::OneHour,
            Self::OneHour => Self::OneDay,
            Self::OneDay => Self::SevenDays,
            Self::SevenDays => Self::Never,
        }
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

    /// Short human label for the create form.
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

/// Phase of the app-level QR overlay shown for an already-created token.
///
/// This builds a QR for the **existing** plaintext token the user just minted
/// (the only one whose plaintext we still hold). No throwaway token is minted,
/// and the displayed token is never revoked on close.
#[derive(Debug, Clone)]
pub enum QrOverlayPhase {
    /// The QR URI is being built (task is in flight).
    #[allow(dead_code)] // entered by the tokens card when it opens the overlay.
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

/// The app-level QR overlay opened for a freshly minted token. While present it
/// renders fullscreen over the dashboard. Closing it never revokes the
/// underlying token — it is a real user token, not a throwaway pairing secret.
#[derive(Debug, Clone)]
pub struct QrOverlay {
    /// Current phase of the overlay state machine.
    pub phase: QrOverlayPhase,
    /// Stale-result guard: async results carry the seq they were started with;
    /// a result whose seq no longer matches the live overlay's seq is ignored.
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

/// State for the token list, its create form, and the QR overlay.
#[derive(Debug, Clone, Default)]
pub struct TokensState {
    /// All tokens currently in the DB.
    pub tokens: Vec<TokenRecord>,
    /// Index of the highlighted row.
    #[allow(dead_code)] // consumed by the tokens card.
    pub cursor: usize,
    /// True while a load / create / revoke task is in flight.
    pub loading: bool,
    /// The one-time minted secret (shown after creation until next action):
    /// `(name, plaintext, read_only)`.
    pub last_minted_secret: Option<(String, String, bool)>,
    /// Current phase of the mini create-form.
    #[allow(dead_code)] // consumed by the tokens card.
    pub form_phase: TokensFormPhase,
    /// Text typed into the "name" field while creating.
    #[allow(dead_code)] // consumed by the tokens card.
    pub form_name: String,
    /// Read-only toggle in the create form.
    #[allow(dead_code)] // consumed by the tokens card.
    pub form_read_only: bool,
    /// Expiry choice in the create form.
    #[allow(dead_code)] // consumed by the tokens card.
    pub form_expiry: TokenExpiryChoice,
    /// The fullscreen pairing QR overlay, when one is open.
    pub qr_overlay: Option<QrOverlay>,
}

impl TokensState {
    /// The selected token name, if any.
    #[allow(dead_code)] // consumed by the tokens card.
    pub fn selected_name(&self) -> Option<&str> {
        self.tokens.get(self.cursor).map(|t| t.name.as_str())
    }

    /// When the selected token was created, for the list the tokens card renders.
    #[allow(dead_code)] // consumed by the tokens card.
    pub fn selected_created_at(&self) -> Option<&str> {
        self.tokens.get(self.cursor).map(|t| t.created_at.as_str())
    }

    /// How many of the loaded tokens are read-only, and how many read-write.
    ///
    /// Returned as `(read_write, read_only)` for the dashboard's Tokens line.
    pub fn rw_ro_split(&self) -> (usize, usize) {
        let read_only = self.tokens.iter().filter(|t| t.read_only).count();
        (self.tokens.len() - read_only, read_only)
    }
}

/// Everything the token dialogs can ask for. The tokens card extends this enum.
#[derive(Debug, Clone)]
pub enum TokensMsg {
    /// Close the top token dialog.
    Close,
}

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

    #[test]
    fn rw_ro_split_counts_both_kinds() {
        let mut state = TokensState::default();
        assert_eq!(state.rw_ro_split(), (0, 0));
        state.tokens = vec![record("a", false), record("b", true), record("c", false)];
        assert_eq!(state.rw_ro_split(), (2, 1));
        assert_eq!(state.selected_name(), Some("a"));
    }
}
