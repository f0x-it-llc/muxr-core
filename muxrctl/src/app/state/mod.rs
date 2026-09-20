//! Application state (TEA model), split per feature.
//!
//! Pure data: no drawing, terminal-backend or async-runtime types. The app
//! layer may name the ratcn runtime's own state types ([`FocusState`], [`ModalState`],
//! [`ToasterState`]) because they are plain values the reducer owns and the
//! view only reads — see `docs/CORE_ARCHITECTURE.md` § muxrctl.
//!
//! [`AppState`] owns one sub-struct per feature; each lives in its own module
//! beside its message enum and reducer (`app/update/<feature>.rs`).

use std::time::Duration;

use ratcn::runtime::{FocusState, ModalState};
use ratcn::{Toast, ToasterState};

pub mod cert;
pub mod config;
pub mod devices;
pub mod server;
pub mod tokens;
pub mod wizard;

// ── App-layer infra mirrors ────────────────────────────────────────────────────
//
// The `app/` layer must stay free of `muxrd::` types (TEA purity +
// layer boundary). These plain mirrors stand in for the infra types; the
// `server/` facade is the only place that converts to/from the real
// `muxrd::tls::SanEntry` / `muxrd::control::StatusInfo`.

/// App-layer mirror of `muxrd::tls::SanEntry`.
///
/// A Subject Alternative Name carried through the TEA layer as plain strings.
/// The `server/` facade converts this into `muxrd::tls::SanEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum San {
    /// An IP-address SAN (stringified; the facade re-parses it).
    Ip(String),
    /// A DNS-name SAN.
    Dns(String),
}

impl San {
    /// Build a [`San`] from a host string: IP if it parses as one, else DNS.
    pub fn from_host(host: &str) -> Self {
        let h = host.trim();
        if h.parse::<std::net::IpAddr>().is_ok() {
            San::Ip(h.to_string())
        } else {
            San::Dns(h.to_string())
        }
    }

    /// Human-readable value (used for display and as the cert SAN list entry).
    pub fn value(&self) -> &str {
        match self {
            San::Ip(s) | San::Dns(s) => s,
        }
    }
}

/// App-layer mirror of `muxrd::control::StatusInfo`.
///
/// A plain snapshot of a running server's status with no infra types. The
/// `server/` facade converts `StatusInfo` into this on the way into the TEA
/// layer.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// The server crate version.
    pub version: String,
    /// The address the server is bound to.
    pub bind_addr: String,
    /// The server process id.
    pub pid: u32,
    /// Seconds the server has been running.
    pub uptime_secs: u64,
    /// Total number of mobile clients currently attached across all sessions.
    pub client_count: usize,
    /// The configured push-notification relay URL, or `None` when push
    /// notifications are disabled.
    pub notify_relay_url: Option<String>,
    /// Number of devices currently registered for push notifications.
    ///
    /// Not currently rendered anywhere (the Devices dialog derives its own
    /// count from the freshly loaded device list so it works whether or not
    /// the daemon is running — see `server::devices::relay_url`) — kept as a
    /// straight mirror of `StatusInfo` for parity / future use.
    #[allow(dead_code)]
    pub push_device_count: usize,
}

// ── Dialog identity ───────────────────────────────────────────────────────────

/// Every dialog the control panel can open.
///
/// The dashboard is the only screen; everything else is a ratcn modal keyed by
/// [`DialogId::id`]. Those id strings are the contract between the reducer
/// (which pushes them onto [`UiState::modals`]) and the view (which declares
/// exactly the open ids each frame) — they are stable and must not be respelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DialogId {
    /// Bind-address form.
    Config,
    /// "Stop muxrd?" confirmation.
    StopServer,
    /// Certificate overview.
    Cert,
    /// Certificate explainer.
    CertHelp,
    /// "Regenerate the certificate?" confirmation.
    CertRegenConfirm,
    /// Token list.
    Tokens,
    /// Token create form.
    TokenCreate,
    /// One-time minted-secret display.
    TokenMinted,
    /// "Revoke this token?" confirmation.
    TokenRevokeConfirm,
    /// Fullscreen pairing QR.
    Qr,
    /// Push-device list.
    Devices,
    /// "Remove this device?" confirmation.
    DeviceRemoveConfirm,
    /// First-run setup wizard.
    Wizard,
}

impl DialogId {
    /// The modal id string this dialog is opened and declared under.
    pub const fn id(self) -> &'static str {
        match self {
            DialogId::Config => "config",
            DialogId::StopServer => "stop_server",
            DialogId::Cert => "cert",
            DialogId::CertHelp => "cert_help",
            DialogId::CertRegenConfirm => "cert_regen_confirm",
            DialogId::Tokens => "tokens",
            DialogId::TokenCreate => "token_create",
            DialogId::TokenMinted => "token_minted",
            DialogId::TokenRevokeConfirm => "token_revoke_confirm",
            DialogId::Qr => "qr",
            DialogId::Devices => "devices",
            DialogId::DeviceRemoveConfirm => "device_remove_confirm",
            DialogId::Wizard => "wizard",
        }
    }

    /// The dialog an id string names, or `None` for an id this app never opened.
    pub fn parse(id: &str) -> Option<DialogId> {
        let dialog = match id {
            "config" => DialogId::Config,
            "stop_server" => DialogId::StopServer,
            "cert" => DialogId::Cert,
            "cert_help" => DialogId::CertHelp,
            "cert_regen_confirm" => DialogId::CertRegenConfirm,
            "tokens" => DialogId::Tokens,
            "token_create" => DialogId::TokenCreate,
            "token_minted" => DialogId::TokenMinted,
            "token_revoke_confirm" => DialogId::TokenRevokeConfirm,
            "qr" => DialogId::Qr,
            "devices" => DialogId::Devices,
            "device_remove_confirm" => DialogId::DeviceRemoveConfirm,
            "wizard" => DialogId::Wizard,
            _ => return None,
        };
        Some(dialog)
    }
}

// ── Shared UI state ───────────────────────────────────────────────────────────

/// The state the ratcn runtime reads back: focus, the modal stack, the toast
/// stack, and the clock the toasts age against.
#[derive(Debug, Default)]
pub struct UiState {
    /// Which component holds keyboard focus (bound with `Ratcn::focus`).
    pub focus: FocusState,
    /// The open dialogs, innermost last (bound with `Ratcn::modals`).
    pub modals: ModalState,
    /// The toast stack; entries are pushed with [`UiState::now`].
    pub toasts: ToasterState<'static>,
    /// Time since process start, written by the runner before each update
    /// cycle. Toasts are pushed and aged against it.
    pub now: Duration,
}

// ── Root app state ────────────────────────────────────────────────────────────

/// The TEA model: the entire UI is a pure function of this state.
#[derive(Debug, Default)]
pub struct AppState {
    /// Set to break the runner's event loop and restore the terminal.
    pub should_quit: bool,
    /// Focus, modal stack, toasts, clock.
    pub ui: UiState,
    /// Bind-address form state.
    pub config: config::ConfigState,
    /// Daemon status / start / stop state.
    pub server: server::ServerPanelState,
    /// Certificate state.
    pub cert: cert::CertState,
    /// Token state (list, create form, QR overlay).
    pub tokens: tokens::TokensState,
    /// Push-device state.
    pub devices: devices::DevicesState,
    /// Setup-wizard state.
    pub wizard: wizard::WizardState,
    /// Process-monotonic sequence counter for the QR overlay. Bumped each time a
    /// new overlay is opened; carried into the async build task so a result whose
    /// overlay was since closed (or superseded) is discarded.
    #[allow(dead_code)] // bumped by the tokens card when it opens the overlay.
    pub qr_seq: u64,
}

impl AppState {
    /// Construct the initial state: the dashboard with no dialog open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push `id` onto the modal stack, moving focus into the new layer.
    ///
    /// Re-opening the dialog already on top is a no-op; opening one that is
    /// already open *below* another is a caller bug the runtime reports — it is
    /// ignored here rather than propagated, because a stale re-open must never
    /// take the TUI down.
    pub fn open_dialog(&mut self, id: DialogId) {
        if let Err(e) = self.ui.modals.open(id.id(), &mut self.ui.focus) {
            log::debug!("open_dialog ignored: {e}");
        }
    }

    /// Pop the top dialog, restoring the focus it saved, and report which one
    /// closed.
    pub fn close_dialog(&mut self) -> Option<DialogId> {
        let closed = self.ui.modals.close(&mut self.ui.focus)?;
        DialogId::parse(closed.as_str())
    }

    /// The dialog currently on top of the stack, if any.
    pub fn top_dialog(&self) -> Option<DialogId> {
        DialogId::parse(self.ui.modals.top()?.as_str())
    }

    /// Raise a success toast.
    pub fn toast_ok(&mut self, msg: impl Into<String>) {
        let now = self.ui.now;
        self.ui.toasts.push(Toast::success(msg.into()), now);
    }

    /// Raise an error toast.
    pub fn toast_err(&mut self, msg: impl Into<String>) {
        let now = self.ui.now;
        self.ui.toasts.push(Toast::error(msg.into()), now);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [DialogId; 13] = [
        DialogId::Config,
        DialogId::StopServer,
        DialogId::Cert,
        DialogId::CertHelp,
        DialogId::CertRegenConfirm,
        DialogId::Tokens,
        DialogId::TokenCreate,
        DialogId::TokenMinted,
        DialogId::TokenRevokeConfirm,
        DialogId::Qr,
        DialogId::Devices,
        DialogId::DeviceRemoveConfirm,
        DialogId::Wizard,
    ];

    #[test]
    fn dialog_ids_round_trip_through_parse() {
        for id in ALL {
            assert_eq!(DialogId::parse(id.id()), Some(id), "{id:?}");
        }
    }

    #[test]
    fn dialog_ids_are_distinct_and_spelled_exactly() {
        let ids: Vec<&str> = ALL.iter().map(|d| d.id()).collect();
        assert_eq!(
            ids,
            [
                "config",
                "stop_server",
                "cert",
                "cert_help",
                "cert_regen_confirm",
                "tokens",
                "token_create",
                "token_minted",
                "token_revoke_confirm",
                "qr",
                "devices",
                "device_remove_confirm",
                "wizard",
            ]
        );
    }

    #[test]
    fn unknown_dialog_id_does_not_parse() {
        assert_eq!(DialogId::parse("dashboard"), None);
    }

    #[test]
    fn default_state_has_no_dialog_open() {
        let state = AppState::new();
        assert!(!state.should_quit);
        assert_eq!(state.top_dialog(), None);
    }

    #[test]
    fn open_and_close_dialog_round_trip() {
        let mut state = AppState::new();
        state.open_dialog(DialogId::Config);
        assert_eq!(state.top_dialog(), Some(DialogId::Config));
        assert_eq!(state.close_dialog(), Some(DialogId::Config));
        assert_eq!(state.top_dialog(), None);
        assert_eq!(state.close_dialog(), None);
    }

    #[test]
    fn toasts_are_pushed_against_the_ui_clock() {
        let mut state = AppState::new();
        state.ui.now = Duration::from_secs(3);
        state.toast_ok("saved");
        state.toast_err("boom");
        assert_eq!(state.ui.toasts.len(), 2);
    }
}
