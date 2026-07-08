//! Application state (TEA model).
//!
//! Pure data: no ratatui imports. Mutated only by [`super::update::update`].
//! Each screen gets its own sub-struct so the update function can delegate
//! clearly. The `AppState` owns all of them.

use std::net::Ipv4Addr;

use crate::server::tokens::TokenRecord;

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
}

// ── Screen navigation ─────────────────────────────────────────────────────────

/// The navigable screens of the control panel.
///
/// Ordering matters: `Tab` / arrow navigation cycles through them in this
/// declaration order (see [`Screen::next`] / [`Screen::prev`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Config,
    Cert,
    Tokens,
    Server,
}

impl Screen {
    /// All screens in navigation order. Drives the dashboard tab list.
    pub const ALL: [Screen; 5] = [
        Screen::Dashboard,
        Screen::Config,
        Screen::Cert,
        Screen::Tokens,
        Screen::Server,
    ];

    /// Human-readable label for tab lists and panel headings.
    pub fn label(self) -> &'static str {
        match self {
            Screen::Dashboard => "Dashboard",
            Screen::Config => "Config",
            Screen::Cert => "Cert",
            Screen::Tokens => "Tokens",
            Screen::Server => "Server",
        }
    }

    /// Index of this screen within [`Screen::ALL`].
    pub fn index(self) -> usize {
        Screen::ALL
            .iter()
            .position(|s| *s == self)
            .expect("Screen must be present in Screen::ALL")
    }

    /// The next screen in navigation order, wrapping at the end.
    pub fn next(self) -> Screen {
        let i = self.index();
        Screen::ALL[(i + 1) % Screen::ALL.len()]
    }

    /// The previous screen in navigation order, wrapping at the start.
    pub fn prev(self) -> Screen {
        let i = self.index();
        Screen::ALL[(i + Screen::ALL.len() - 1) % Screen::ALL.len()]
    }
}

// ── Per-screen substates ─────────────────────────────────────────────────────

/// Which field in the Config form is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigField {
    #[default]
    Host,
    Port,
    IpPicker,
}

/// State for the Config screen (bind address form + IP picker).
#[derive(Debug, Clone, Default)]
pub struct ConfigState {
    /// Editable host part of the bind address.
    pub host: String,
    /// Editable port part of the bind address.
    pub port: String,
    /// Non-loopback reachable IPv4 addresses discovered from interfaces.
    pub reachable_ips: Vec<Ipv4Addr>,
    /// Index of the currently highlighted IP in the picker list.
    pub ip_cursor: usize,
    /// The currently focused form field.
    pub focused: ConfigField,
    /// Directory where TLS certs are stored (display only).
    pub cert_dir: String,
    /// Extra advertise SANs from the `ZELLIMSERVER_SAN` env var, merged into the
    /// cert SANs alongside the reachable IPs (e.g. a tailnet IP not visible as a
    /// local interface inside a container). Loaded via `ConfigLoaded`.
    pub advertise_sans: Vec<String>,
    /// Transient status line shown after save / load.
    pub status: String,
    /// True while a LoadConfig or SaveBind task is in flight.
    pub loading: bool,
}

impl ConfigState {
    /// Return the current bind address as `"host:port"`.
    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Populate form fields from a resolved bind address string.
    pub fn apply_bind_addr(&mut self, addr: &str) {
        if let Some(colon) = addr.rfind(':') {
            self.host = addr[..colon].to_string();
            self.port = addr[colon + 1..].to_string();
        } else {
            self.host = addr.to_string();
            self.port = "50051".to_string();
        }
    }
}

/// Operator-declared advertised trust mode.
///
/// Controls how the pairing QR encodes the trust model — overrides the automatic
/// detection from the server's `cert_mode` when set to `Ca` or `Pin`.
///
/// Cycled on the Cert screen with the `t` key.  Default is `Auto`.
///
/// ## Resolution (see PLAN.md § "How ctl decides `tm`")
///
/// | `AdvertiseTrust` | `cert_mode`             | Resolved `PairingTrust`  |
/// |------------------|-------------------------|--------------------------|
/// | `Auto`           | `External` / `H2c`      | `Ca` (no fp)             |
/// | `Auto`           | `SelfSigned` / unknown  | `Pin` (fp required)      |
/// | `Auto`           | any, DNS advertise host | nudge toward `Ca`        |
/// | `Ca`             | (any)                   | `Ca` (no fp)             |
/// | `Pin`            | (any)                   | `Pin` (fp required)      |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdvertiseTrust {
    /// Automatically determine trust from the running server's cert_mode.
    #[default]
    Auto,
    /// Force system-CA trust (no fingerprint in the QR).
    Ca,
    /// Force fingerprint-pin trust (requires an on-disk self-signed cert).
    Pin,
}

impl AdvertiseTrust {
    /// Cycle to the next variant (for the `t` key toggle).
    pub fn cycle(self) -> Self {
        match self {
            AdvertiseTrust::Auto => AdvertiseTrust::Ca,
            AdvertiseTrust::Ca => AdvertiseTrust::Pin,
            AdvertiseTrust::Pin => AdvertiseTrust::Auto,
        }
    }

    /// Human-readable label for the Cert screen display.
    pub fn label(self) -> &'static str {
        match self {
            AdvertiseTrust::Auto => "Auto",
            AdvertiseTrust::Ca => "CA (force)",
            AdvertiseTrust::Pin => "Pin (force)",
        }
    }

    /// Serialise to the canonical persistence string (`"auto"`, `"ca"`, `"pin"`).
    pub fn persist_str(self) -> &'static str {
        match self {
            AdvertiseTrust::Auto => "auto",
            AdvertiseTrust::Ca => "ca",
            AdvertiseTrust::Pin => "pin",
        }
    }

    /// Deserialise from a persistence string.  Returns `Auto` for any
    /// unrecognised value so forward compatibility is safe.
    pub fn from_persist_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "ca" => AdvertiseTrust::Ca,
            "pin" => AdvertiseTrust::Pin,
            _ => AdvertiseTrust::Auto,
        }
    }
}

/// State for the Cert screen.
#[derive(Debug, Clone, Default)]
pub struct CertState {
    /// SHA-256 fingerprint of the current cert (hex, lowercase), or empty if unknown.
    pub fingerprint: String,
    /// SANs currently in the certificate.
    pub sans: Vec<String>,
    /// Transient status line.
    pub status: String,
    /// True while EnsureCert is in flight.
    pub loading: bool,
    /// Operator-declared advertised trust override for the pairing QR.
    ///
    /// `Auto` resolves the trust from the server's reported `cert_mode`; `Ca`
    /// and `Pin` force the respective mode regardless of what the server reports.
    /// Cycled with the `t` key on the Cert screen.
    pub advertise_trust: AdvertiseTrust,
}

// ── Tokens screen state ───────────────────────────────────────────────────────

/// The phase of the token create mini-form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokensFormPhase {
    /// The list is shown; user browses or initiates create/revoke.
    #[default]
    Browsing,
    /// The "create" form is open; user types a name.
    Creating,
}

/// Expiry choice in the token create form, cycled with `Ctrl-E`.
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
    /// Advance to the next choice (wraps), for the create-form cycle key.
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

/// State for the Tokens screen.
#[derive(Debug, Clone, Default)]
pub struct TokensState {
    /// All tokens currently in the DB.
    pub tokens: Vec<TokenRecord>,
    /// Index of the highlighted row.
    pub cursor: usize,
    /// True while a load / create / revoke task is in flight.
    pub loading: bool,
    /// Transient status / error line.
    pub status: String,
    /// The one-time minted secret (shown after creation until next action).
    pub last_minted_secret: Option<(String, String, bool)>, // (name, plaintext, read_only)
    /// Current phase of the mini create-form.
    pub form_phase: TokensFormPhase,
    /// Text typed into the "name" field while creating.
    pub form_name: String,
    /// Read-only toggle in the create form.
    pub form_read_only: bool,
    /// Expiry choice in the create form (cycled with Ctrl-E).
    pub form_expiry: TokenExpiryChoice,
}

impl TokensState {
    /// The selected token name, if any.
    pub fn selected_name(&self) -> Option<&str> {
        self.tokens.get(self.cursor).map(|t| t.name.as_str())
    }
}

// ── Token QR overlay state ────────────────────────────────────────────────────

/// Phase of the app-level QR overlay shown for an already-created token.
///
/// Unlike the old pairing flow, this builds a QR for the **existing** plaintext
/// token the user just minted (the only one whose plaintext we still hold). No
/// throwaway token is minted, and the displayed token is never revoked on close.
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

/// The app-level QR overlay opened from the Tokens screen for a freshly minted
/// token. While present, it intercepts all input and renders fullscreen over the
/// active screen. Closing it (`Esc`/`q`) never revokes the underlying token —
/// it is a real user token, not a throwaway pairing secret.
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

/// State for the Server panel screen.
#[derive(Debug, Clone, Default)]
pub struct ServerPanelState {
    /// Last-known server status; `None` means not yet fetched.
    pub status: Option<ServerInfo>,
    /// True if we know the server is not running (Stopped).
    pub stopped: bool,
    /// True while a start/stop/refresh task is in flight.
    pub loading: bool,
    /// Transient message shown after start/stop actions.
    pub action_msg: String,
    /// Tick counter used to drive the ~1 s live poll cadence.
    pub tick_counter: u32,
}

// ── Root app state ────────────────────────────────────────────────────────────

/// The TEA model: the entire UI is a pure function of this state.
#[derive(Debug)]
pub struct AppState {
    /// The currently visible screen.
    pub screen: Screen,
    /// Set to break the runner's event loop and restore the terminal.
    pub should_quit: bool,
    /// Config screen state.
    pub config: ConfigState,
    /// Cert screen state.
    pub cert: CertState,
    /// Server panel screen state.
    pub server: ServerPanelState,
    /// Tokens screen state.
    pub tokens: TokensState,
    /// The app-level token QR overlay, when one is open (`None` otherwise).
    pub qr_overlay: Option<QrOverlay>,
    /// Process-monotonic sequence counter for the QR overlay. Bumped each time a
    /// new overlay is opened; carried into the async build task so a result whose
    /// overlay was since closed (or superseded) is discarded.
    pub qr_seq: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            screen: Screen::Dashboard,
            should_quit: false,
            config: ConfigState::default(),
            cert: CertState::default(),
            server: ServerPanelState::default(),
            tokens: TokensState::default(),
            qr_overlay: None,
            qr_seq: 0,
        }
    }
}

impl AppState {
    /// Construct the initial state (lands on the Dashboard).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a printable key should be routed to a focused text field rather
    /// than triggering global / screen-level single-key shortcuts.
    ///
    /// True when:
    /// - the Config screen has the Host or Port field focused, or
    /// - the Tokens screen is in the `Creating` (name-entry) form phase.
    ///
    /// While editing, only `Ctrl-C` quits; save/reload move to `Ctrl-S` /
    /// `Ctrl-R` so common hostnames containing `q`/`s`/`r` can be typed.
    pub fn is_text_editing(&self) -> bool {
        match self.screen {
            Screen::Config => matches!(self.config.focused, ConfigField::Host | ConfigField::Port),
            Screen::Tokens => self.tokens.form_phase == TokensFormPhase::Creating,
            _ => false,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_screens_have_distinct_labels() {
        for s in Screen::ALL {
            assert!(!s.label().is_empty());
        }
    }

    #[test]
    fn next_wraps_around() {
        assert_eq!(Screen::Server.next(), Screen::Dashboard);
        assert_eq!(Screen::Dashboard.next(), Screen::Config);
    }

    #[test]
    fn prev_wraps_around() {
        assert_eq!(Screen::Dashboard.prev(), Screen::Server);
        assert_eq!(Screen::Config.prev(), Screen::Dashboard);
    }

    #[test]
    fn index_matches_all_order() {
        for (i, s) in Screen::ALL.iter().enumerate() {
            assert_eq!(s.index(), i);
        }
    }

    #[test]
    fn default_state_lands_on_dashboard() {
        let state = AppState::new();
        assert_eq!(state.screen, Screen::Dashboard);
        assert!(!state.should_quit);
    }

    #[test]
    fn config_state_apply_bind_addr() {
        let mut cs = ConfigState::default();
        cs.apply_bind_addr("0.0.0.0:50051");
        assert_eq!(cs.host, "0.0.0.0");
        assert_eq!(cs.port, "50051");
    }

    #[test]
    fn config_state_apply_bind_addr_ipv6() {
        let mut cs = ConfigState::default();
        cs.apply_bind_addr("127.0.0.1:9090");
        assert_eq!(cs.host, "127.0.0.1");
        assert_eq!(cs.port, "9090");
    }

    #[test]
    fn config_state_bind_addr_roundtrip() {
        let mut cs = ConfigState::default();
        cs.apply_bind_addr("192.168.1.5:50051");
        assert_eq!(cs.bind_addr(), "192.168.1.5:50051");
    }
}
