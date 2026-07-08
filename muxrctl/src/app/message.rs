//! Messages: the only way state changes.
//!
//! A `Message` is produced by the runner (terminal input, ticks) or by async
//! tasks spawned from [`super::action::UpdateAction`], then fed to
//! [`super::update::update`]. This is the sole input to the TEA update cycle.

use crossterm::event::KeyEvent;

use crate::server::tokens::TokenRecord;

use super::state::{Screen, ServerInfo};

/// A lightweight snapshot of the effective server configuration.
///
/// Plain struct (no ratatui, no proto types) that the Config screen renders.
/// Populated from `server::effective_config()` + `pairing::net::reachable_ipv4()`.
#[derive(Debug, Clone)]
pub struct ConfigSnapshot {
    /// The resolved bind address (e.g. `"127.0.0.1:50051"`).
    pub bind_addr: String,
    /// Directory where the TLS cert files are stored.
    pub cert_dir: String,
    /// Non-loopback IPv4 addresses the mobile client could reach.
    pub reachable_ips: Vec<std::net::Ipv4Addr>,
    /// Extra advertise SANs from the `MUXRD_SAN` env var (comma-separated).
    ///
    /// Needed because an externally-advertised address (e.g. a tailnet IP that
    /// is a host-side NAT publish, not a local interface inside a container) is
    /// not discoverable via interface enumeration. Merged into the cert SANs so
    /// the TUI-generated cert matches what the daemon's `collect_sans` produces.
    pub advertise_sans: Vec<String>,
}

/// Everything that can drive a state change.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Some variants emitted only by async tasks or later waves.
pub enum Message {
    /// A key was pressed (delivered by the runner's input poll).
    Key(KeyEvent),
    /// The ~50 ms wall-clock tick (poll timeout path). Drives animations + live poll counter.
    Tick,
    /// Request a clean shutdown; the runner restores the terminal and exits.
    Quit,
    /// Navigate to a specific screen.
    NavTo(Screen),

    // ── Async task results ──────────────────────────────────────────────────
    /// Server status result, posted by a `RefreshStatus` task.
    ///
    /// `Some(info)` when the server is running; `None` when it is stopped /
    /// unreachable. The `server/` facade converts the infra `StatusInfo` into
    /// the app-layer [`ServerInfo`] mirror before this message is posted.
    StatusLoaded(Option<ServerInfo>),
    /// Config + reachable-IP snapshot, posted by a `LoadConfig` task.
    ConfigLoaded(ConfigSnapshot),
    /// Cert was ensured; fingerprint + active SANs returned.
    CertEnsured {
        fingerprint: String,
        sans: Vec<String>,
    },
    /// A background action failed with a human-readable error message.
    ActionFailed(String),
    /// A background action completed successfully with an optional message.
    ActionOk(String),

    // ── Token screen messages ─────────────────────────────────────────────────
    /// Tokens list loaded from the token DB.
    TokensLoaded(Vec<TokenRecord>),

    /// A fresh token was just created; the plaintext secret is available once.
    TokenCreated {
        /// The one-time plaintext token secret.
        token: String,
        /// The display name assigned to the new token.
        name: String,
        /// Whether the new token grants read-only access.
        read_only: bool,
    },

    /// A token operation (create/revoke) completed; the list needs a refresh.
    TokensChanged,

    // ── Token QR overlay messages ─────────────────────────────────────────────
    /// The token QR overlay URI is ready: URI to encode + client baseline.
    ///
    /// Only accepted if the carried `seq` matches the current overlay's seq.
    TokenQrReady {
        /// The `muxr://pair?...` URI to encode into a QR.
        uri: String,
        /// The advertise host that was embedded in the URI.
        host: String,
        /// The port embedded in the URI.
        port: u16,
        /// Short fingerprint excerpt for display below the QR.
        fingerprint_short: String,
        /// Number of mobile clients attached when the QR was generated.
        baseline_clients: usize,
        /// Sequence number (must match current overlay seq to be accepted).
        seq: u64,
    },

    /// Token QR generation failed; carries human-readable error + seq.
    TokenQrFailed {
        /// Error message to display.
        err: String,
        /// Sequence number (must match current overlay seq to be accepted).
        seq: u64,
    },

    // ── Dashboard screen messages ─────────────────────────────────────────────
    /// Read-only cert info for the Dashboard overview.
    ///
    /// Posted by a [`super::action::UpdateAction::LoadCertInfo`] task via the
    /// read-only facade — never regenerates the cert.
    CertInfoLoaded {
        /// SHA-256 fingerprint of the on-disk cert, or `None` if no cert exists.
        fingerprint: Option<String>,
        /// SANs read from the persisted SAN sidecar (`server.san.json`).
        sans: Vec<String>,
    },
}
