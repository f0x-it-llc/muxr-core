//! Certificate state — the dashboard's Certificate section, the Certificate
//! dialog's model, and the words both of them show.
//!
//! The explanation constants at the bottom are operator-facing prose about what
//! muxrd actually does with TLS. They are part of the contract documented in
//! `docs/CORE_CONFIGURATION.md` § TLS modes: change the daemon's behaviour and
//! these strings change with it.

use super::AppState;

/// App-layer mirror of `muxrd::config::CertMode`.
///
/// The transport identity the running daemon reported over the control socket.
/// The runner converts `muxrd::config::CertMode` into this on the way in, so
/// `app/` stays free of `muxrd::` types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// A self-signed cert muxrd generated: the pairing QR pins its fingerprint.
    SelfSigned,
    /// An operator-supplied PEM pair: clients trust it through the system CAs.
    External,
    /// Plaintext h2c behind a terminating proxy: no local cert at all.
    H2c,
}

impl TlsMode {
    /// One-line label for the dashboard's Certificate section.
    pub const fn label(self) -> &'static str {
        match self {
            TlsMode::SelfSigned => "self-signed · fingerprint pinned",
            TlsMode::External => "external certificate · CA trust",
            TlsMode::H2c => "h2c behind a proxy · CA trust",
        }
    }
}

/// Operator-declared advertised trust mode.
///
/// Controls how the pairing QR encodes the trust model — overrides the automatic
/// detection from the server's `cert_mode` when set to `Ca` or `Pin`.
///
/// Default is `Auto`.
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
    /// Cycle to the next variant.
    ///
    /// The dialog itself drives the choice by index through [`from_index`], so
    /// this is the ordering those indices follow, kept as the one statement of
    /// the cycle order.
    ///
    /// [`from_index`]: AdvertiseTrust::from_index
    #[allow(dead_code)] // the ordering contract; exercised by the tests below.
    pub fn cycle(self) -> Self {
        match self {
            AdvertiseTrust::Auto => AdvertiseTrust::Ca,
            AdvertiseTrust::Ca => AdvertiseTrust::Pin,
            AdvertiseTrust::Pin => AdvertiseTrust::Auto,
        }
    }

    /// The `Cycle` option index this trust mode is shown at: 0 Auto, 1 CA, 2 Pin.
    pub const fn index(self) -> usize {
        match self {
            AdvertiseTrust::Auto => 0,
            AdvertiseTrust::Ca => 1,
            AdvertiseTrust::Pin => 2,
        }
    }

    /// The trust mode a `Cycle` option index names.
    ///
    /// Anything outside `0..=2` is `Auto`, so an out-of-range index from a
    /// future option list can never panic the reducer.
    pub const fn from_index(index: usize) -> Self {
        match index {
            1 => AdvertiseTrust::Ca,
            2 => AdvertiseTrust::Pin,
            _ => AdvertiseTrust::Auto,
        }
    }

    /// Human-readable label for the advertised-trust display.
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

/// State for the certificate section and its dialog.
#[derive(Debug, Clone, Default)]
pub struct CertState {
    /// SHA-256 fingerprint of the current cert (hex, lowercase), or `None` when
    /// no certificate exists yet.
    pub fingerprint: Option<String>,
    /// SANs currently in the certificate.
    pub sans: Vec<String>,
    /// The daemon's reported transport identity, or `None` while the daemon is
    /// stopped (the mode is only knowable from a running daemon).
    pub tls_mode: Option<TlsMode>,
    /// True while an EnsureCert / LoadCertInfo task is in flight.
    pub loading: bool,
    /// Operator-declared advertised trust override for the pairing QR.
    ///
    /// `Auto` resolves the trust from the server's reported `cert_mode`; `Ca`
    /// and `Pin` force the respective mode regardless of what the server reports.
    pub advertise_trust: AdvertiseTrust,
    /// First visible row of the Help dialog's scroll area.
    pub help_scroll: usize,
}

/// Everything the Cert dialogs can ask for.
#[derive(Debug, Clone)]
pub enum CertMsg {
    /// Close the dialog.
    Close,
    /// The advertised-trust cycle moved to this option index (0/1/2).
    TrustChanged(usize),
    /// Open the explainer.
    OpenHelp,
    /// Close the explainer.
    CloseHelp,
    /// The explainer scrolled to this first visible row.
    HelpScrolled(usize),
    /// "Regenerate…" was pressed: ask for confirmation first.
    RegenerateRequested,
    /// The confirmation was accepted: regenerate now.
    RegenerateConfirmed,
    /// The confirmation was dismissed.
    RegenerateCancelled,
    /// Re-read the on-disk cert and the daemon's live TLS mode.
    Refresh,
}

// ── Labels ────────────────────────────────────────────────────────────────────

/// What the daemon is actually serving, in one sentence.
///
/// `has_cert` only matters while the mode is unknown (the daemon is stopped):
/// a local certificate on disk is the difference between "nothing to pair with
/// yet" and "the file is there, the daemon just is not running".
pub fn tls_mode_label(mode: Option<TlsMode>, has_cert: bool) -> &'static str {
    match mode {
        Some(TlsMode::SelfSigned) => "Self-signed — the phone pins this certificate's fingerprint",
        Some(TlsMode::External) => {
            "External certificate — the phone trusts it through the system CA store"
        }
        Some(TlsMode::H2c) => {
            "Plaintext h2c behind a TLS-terminating proxy — the proxy's certificate is trusted \
             through the system CA store"
        }
        None if has_cert => {
            "Unknown until the daemon starts (a local self-signed certificate exists)"
        }
        None => "Unknown until the daemon starts (no local certificate yet)",
    }
}

/// What the pairing QR will carry, given the operator's choice and the live mode.
///
/// The `Auto` arm follows exactly the mapping `tui::runner::resolve_auto_trust`
/// applies when the QR is actually built — external and h2c resolve to CA, a
/// self-signed or unknown mode resolves to a pin — so this line can never
/// promise something the QR does not do.
pub fn resolved_trust_label(
    trust: AdvertiseTrust,
    mode: Option<TlsMode>,
    has_cert: bool,
) -> String {
    /// The pin wording, split by whether there is anything to pin.
    fn pin(has_cert: bool) -> String {
        if has_cert {
            "tm=pin: the QR carries the SHA-256 fingerprint".to_string()
        } else {
            "tm=pin — but no certificate exists yet: generate one first".to_string()
        }
    }

    match trust {
        AdvertiseTrust::Ca => "tm=ca: no fingerprint in the QR".to_string(),
        AdvertiseTrust::Pin => pin(has_cert),
        AdvertiseTrust::Auto => match mode {
            // resolve_auto_trust: External / H2c → CA, no fingerprint needed.
            Some(TlsMode::External) | Some(TlsMode::H2c) => {
                "tm=ca: no fingerprint in the QR — the phone uses its system CA store".to_string()
            }
            // resolve_auto_trust: SelfSigned and the unknown (daemon stopped)
            // case both pin.
            Some(TlsMode::SelfSigned) | None => pin(has_cert),
        },
    }
}

// ── The DNS-host advisory ─────────────────────────────────────────────────────

/// Return true when `host` looks like a DNS name that could be behind a
/// CA-terminating proxy — i.e. it is a non-IP, non-empty, non-loopback string.
///
/// Used for the advisory hint: if the operator's advertise host is a DNS name
/// and the cert is self-signed (Auto mode), we surface a reminder that
/// connections will be PINNED and that they can override the trust to CA.
///
/// Loopback names (`localhost`, `localhost.localdomain`) are excluded: they are
/// never behind a real CA proxy, so no advisory is needed.
fn host_looks_like_dns(host: &str) -> bool {
    let h = host.trim();
    if h.is_empty() {
        return false;
    }
    // An IP address is never a DNS name for this purpose.
    if h.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    // Loopback names don't need the CA-proxy advisory.
    let lower = h.to_ascii_lowercase();
    if lower == "localhost" || lower.starts_with("localhost.") {
        return false;
    }
    true
}

/// The advisory line, shown only when the operator is one setting away from a
/// pairing that will not validate: Auto trust, a self-signed cert on disk, and
/// a DNS-shaped bind host — the shape of a deployment fronted by a
/// CA-terminating proxy, which the daemon still reports as self-signed.
pub fn dns_advisory(state: &AppState) -> Option<&'static str> {
    let cert = &state.cert;
    (cert.advertise_trust == AdvertiseTrust::Auto
        && cert.fingerprint.is_some()
        && host_looks_like_dns(&state.config.host))
    .then_some(
        "DNS host + self-signed cert — connections will be PINNED; choose CA if this server \
         is behind a CA-terminating proxy",
    )
}

// ── Explanation constants ─────────────────────────────────────────────────────
//
// Operator-facing prose. Every claim here is checkable against
// `docs/CORE_CONFIGURATION.md` § TLS modes and § On-disk locations; the first
// line of `CERT_EXPLAIN` doubles as the one-line summary on the Certificate
// dialog itself, so it is kept short enough to fit one row.

/// What TLS muxrd serves and why the QR sometimes carries a fingerprint.
pub const CERT_EXPLAIN: &[&str] = &[
    "muxrd serves gRPC over TLS; by default with a self-signed certificate.",
    "It generates that certificate itself and stores it as server.crt and server.key in the \
     certificate directory (cert_dir, which is the data dir unless it is overridden).",
    "A phone cannot verify a self-signed certificate through a certificate authority, so the \
     pairing QR carries the certificate's SHA-256 fingerprint and the Muxr app pins it: the app \
     checks that exact fingerprint instead of the CA chain and the name.",
    "With an external certificate (--tls-cert / --tls-key), or plaintext h2c behind a \
     TLS-terminating proxy, the QR carries no fingerprint and the phone trusts the certificate \
     through its system CA store.",
    "Precedence when several are configured: h2c > external > self-signed.",
];

/// What a SAN is, who puts one in the certificate, and when it matters.
pub const SAN_EXPLAIN: &[&str] = &[
    "Subject Alternative Names are the addresses a certificate claims to be valid for.",
    "muxrd always includes 127.0.0.1 and localhost, adds a non-loopback bind address \
     automatically, and adds any --san / MUXRD_SAN entries — those are additive, never a \
     replacement for the built-in pair.",
    "muxrctl also folds in every reachable interface IP and the configured bind host: that is the \
     'Planned after regenerate' column on the Certificate dialog.",
    "A pinned pairing does not check the SAN list — the fingerprint is the whole check — but SANs \
     matter for CA-trusted clients, for standard TLS tooling, and because muxrd regenerates the \
     certificate whenever the stored SAN list lacks a requested address.",
];

/// What regenerating costs. Shown again, in full, on the confirmation.
pub const REGEN_EXPLAIN: &[&str] = &[
    "Regenerating produces a new key and certificate with a NEW fingerprint.",
    "Every phone that pinned the old fingerprint stops validating this server and must scan a \
     fresh pairing QR.",
    "The running daemon keeps serving the old certificate until it is restarted.",
];

/// What the advertised-trust choice actually decides.
pub const TRUST_EXPLAIN: &[&str] = &[
    "Advertised trust decides what the pairing QR tells the phone.",
    "Auto reads the daemon's live TLS mode: self-signed pins the fingerprint, external or h2c \
     uses the system CA store; with no daemon running it pins.",
    "Choose CA yourself when this daemon sits behind a TLS-terminating proxy that presents its \
     own certificate — the daemon still reports self-signed.",
    "Choose Pin to force fingerprint pinning.",
    "The choice persists in muxrctl_state, beside the daemon's own files.",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertise_trust_cycles() {
        assert_eq!(AdvertiseTrust::Auto.cycle(), AdvertiseTrust::Ca);
        assert_eq!(AdvertiseTrust::Ca.cycle(), AdvertiseTrust::Pin);
        assert_eq!(AdvertiseTrust::Pin.cycle(), AdvertiseTrust::Auto);
    }

    #[test]
    fn advertise_trust_persist_str_round_trips() {
        for trust in [
            AdvertiseTrust::Auto,
            AdvertiseTrust::Ca,
            AdvertiseTrust::Pin,
        ] {
            assert_eq!(
                AdvertiseTrust::from_persist_str(trust.persist_str()),
                trust,
                "round trip failed for {trust:?}"
            );
        }
    }

    #[test]
    fn advertise_trust_from_persist_str_unknown_defaults_to_auto() {
        assert_eq!(AdvertiseTrust::from_persist_str(""), AdvertiseTrust::Auto);
        assert_eq!(
            AdvertiseTrust::from_persist_str("nonsense"),
            AdvertiseTrust::Auto
        );
        // Case and surrounding whitespace are tolerated.
        assert_eq!(AdvertiseTrust::from_persist_str(" CA "), AdvertiseTrust::Ca);
        assert_eq!(AdvertiseTrust::from_persist_str("Pin"), AdvertiseTrust::Pin);
    }

    #[test]
    fn advertise_trust_labels_are_distinct() {
        assert_ne!(AdvertiseTrust::Auto.label(), AdvertiseTrust::Ca.label());
        assert_ne!(AdvertiseTrust::Ca.label(), AdvertiseTrust::Pin.label());
    }

    #[test]
    fn tls_mode_labels_are_distinct() {
        assert_ne!(TlsMode::SelfSigned.label(), TlsMode::External.label());
        assert_ne!(TlsMode::External.label(), TlsMode::H2c.label());
    }

    #[test]
    fn trust_indices_round_trip_and_follow_the_cycle_order() {
        for trust in [
            AdvertiseTrust::Auto,
            AdvertiseTrust::Ca,
            AdvertiseTrust::Pin,
        ] {
            assert_eq!(
                AdvertiseTrust::from_index(trust.index()),
                trust,
                "round trip failed for {trust:?}"
            );
            // The Cycle advances by one index; that must be `cycle()`.
            assert_eq!(
                AdvertiseTrust::from_index((trust.index() + 1) % 3),
                trust.cycle(),
                "index order disagrees with cycle() for {trust:?}"
            );
        }
    }

    #[test]
    fn trust_from_out_of_range_index_is_auto() {
        assert_eq!(AdvertiseTrust::from_index(3), AdvertiseTrust::Auto);
        assert_eq!(AdvertiseTrust::from_index(usize::MAX), AdvertiseTrust::Auto);
    }

    #[test]
    fn tls_mode_label_names_the_trust_each_mode_implies() {
        assert!(tls_mode_label(Some(TlsMode::SelfSigned), true).contains("pins"));
        assert!(tls_mode_label(Some(TlsMode::External), true).contains("system CA store"));
        assert!(tls_mode_label(Some(TlsMode::H2c), true).contains("h2c"));
        assert!(tls_mode_label(Some(TlsMode::H2c), true).contains("system CA store"));
    }

    #[test]
    fn tls_mode_label_unknown_reports_whether_a_cert_exists() {
        let with = tls_mode_label(None, true);
        let without = tls_mode_label(None, false);
        assert!(with.starts_with("Unknown until the daemon starts"));
        assert!(without.starts_with("Unknown until the daemon starts"));
        assert!(with.contains("a local self-signed certificate exists"));
        assert!(without.contains("no local certificate yet"));
    }

    #[test]
    fn resolved_trust_label_follows_resolve_auto_trust() {
        // Auto + a CA-trusted transport → tm=ca, whatever is on disk.
        for mode in [TlsMode::External, TlsMode::H2c] {
            let label = resolved_trust_label(AdvertiseTrust::Auto, Some(mode), true);
            assert!(label.starts_with("tm=ca"), "{mode:?} → {label}");
        }
        // Auto + self-signed / unknown → tm=pin.
        for mode in [Some(TlsMode::SelfSigned), None] {
            let label = resolved_trust_label(AdvertiseTrust::Auto, mode, true);
            assert!(label.starts_with("tm=pin"), "{mode:?} → {label}");
            assert!(label.contains("SHA-256 fingerprint"));
        }
    }

    #[test]
    fn resolved_trust_label_says_so_when_there_is_nothing_to_pin() {
        for trust in [AdvertiseTrust::Auto, AdvertiseTrust::Pin] {
            let label = resolved_trust_label(trust, Some(TlsMode::SelfSigned), false);
            assert!(label.contains("no certificate exists yet"), "{label}");
        }
    }

    #[test]
    fn forced_trust_ignores_the_daemons_mode() {
        for mode in [
            Some(TlsMode::SelfSigned),
            Some(TlsMode::External),
            Some(TlsMode::H2c),
            None,
        ] {
            assert_eq!(
                resolved_trust_label(AdvertiseTrust::Ca, mode, true),
                "tm=ca: no fingerprint in the QR",
                "{mode:?}"
            );
            assert!(
                resolved_trust_label(AdvertiseTrust::Pin, mode, true).starts_with("tm=pin"),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn dns_host_triggers_hint() {
        // Non-IP, non-loopback names are DNS names that could be CA-proxy-fronted.
        assert!(host_looks_like_dns("server.local"));
        assert!(host_looks_like_dns("myserver.example.com"));
        assert!(host_looks_like_dns("zelli.example.com"));
    }

    #[test]
    fn ip_host_does_not_trigger_hint() {
        // IP addresses are never DNS names for the advisory purpose.
        assert!(!host_looks_like_dns("192.168.1.1"));
        assert!(!host_looks_like_dns("10.0.0.1"));
        assert!(!host_looks_like_dns("127.0.0.1"));
        assert!(!host_looks_like_dns("::1"));
        assert!(!host_looks_like_dns("0.0.0.0"));
    }

    #[test]
    fn localhost_does_not_trigger_hint() {
        // "localhost" is never behind a real CA proxy — exclude it from the hint.
        assert!(!host_looks_like_dns("localhost"));
        assert!(!host_looks_like_dns("LOCALHOST"));
        assert!(!host_looks_like_dns("localhost.localdomain"));
    }

    #[test]
    fn empty_host_does_not_trigger_hint() {
        assert!(!host_looks_like_dns(""));
        assert!(!host_looks_like_dns("  "));
    }

    /// A state with a self-signed cert on disk and a DNS-shaped bind host.
    fn advisory_state() -> AppState {
        let mut state = AppState::new();
        state.cert.fingerprint = Some("ab".repeat(32));
        state.config.host = "muxr.example.com".to_string();
        state
    }

    #[test]
    fn dns_advisory_fires_only_on_auto_with_a_cert_and_a_dns_host() {
        let state = advisory_state();
        let advisory = dns_advisory(&state).expect("advisory expected");
        assert!(advisory.contains("PINNED"));

        // Forcing the trust answers the question the advisory asks.
        let mut forced = advisory_state();
        forced.cert.advertise_trust = AdvertiseTrust::Ca;
        assert_eq!(dns_advisory(&forced), None);
        forced.cert.advertise_trust = AdvertiseTrust::Pin;
        assert_eq!(dns_advisory(&forced), None);

        // No certificate: nothing is being pinned, so nothing to advise about.
        let mut no_cert = advisory_state();
        no_cert.cert.fingerprint = None;
        assert_eq!(dns_advisory(&no_cert), None);

        // An IP host is the ordinary LAN case the advisory must stay out of.
        let mut ip_host = advisory_state();
        ip_host.config.host = "192.168.1.10".to_string();
        assert_eq!(dns_advisory(&ip_host), None);
    }

    #[test]
    fn explanations_state_the_facts_the_operator_is_deciding_on() {
        let cert = CERT_EXPLAIN.join(" ");
        assert!(cert.contains("self-signed"));
        assert!(cert.contains("server.crt"));
        assert!(cert.contains("server.key"));
        assert!(cert.contains("SHA-256 fingerprint"));
        assert!(cert.contains("--tls-cert"));
        assert!(cert.contains("system CA store"));
        assert!(cert.contains("h2c > external > self-signed"));

        let san = SAN_EXPLAIN.join(" ");
        assert!(san.contains("127.0.0.1"));
        assert!(san.contains("localhost"));
        assert!(san.contains("MUXRD_SAN"));
        assert!(san.contains("regenerates"));

        let regen = REGEN_EXPLAIN.join(" ");
        assert!(regen.contains("NEW fingerprint"));
        assert!(regen.contains("must scan a fresh pairing QR"));
        assert!(regen.contains("until it is restarted"));

        let trust = TRUST_EXPLAIN.join(" ");
        assert!(trust.contains("Auto"));
        assert!(trust.contains("muxrctl_state"));
    }

    #[test]
    fn the_cert_summary_line_fits_the_dialog() {
        // CERT_EXPLAIN[0] is painted as a single muted row inside a 78-column
        // dialog (74 columns of content).
        assert!(
            CERT_EXPLAIN[0].chars().count() <= 74,
            "summary line is {} columns",
            CERT_EXPLAIN[0].chars().count()
        );
    }
}
