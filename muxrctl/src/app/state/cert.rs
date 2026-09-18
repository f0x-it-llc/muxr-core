//! Certificate state — the dashboard's Certificate section, and a stub for the
//! cert card's own dialog.

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
    /// Cycle to the next variant (for the cert dialog's toggle).
    #[allow(dead_code)] // consumed by the cert card's toggle control.
    pub fn cycle(self) -> Self {
        match self {
            AdvertiseTrust::Auto => AdvertiseTrust::Ca,
            AdvertiseTrust::Ca => AdvertiseTrust::Pin,
            AdvertiseTrust::Pin => AdvertiseTrust::Auto,
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
}

/// Everything the Cert dialog can ask for. The cert card extends this enum.
#[derive(Debug, Clone)]
pub enum CertMsg {
    /// Close the dialog.
    Close,
}

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
}
