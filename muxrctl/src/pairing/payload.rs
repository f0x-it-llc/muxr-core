//! payload — QR pairing URI construction.
//!
//! Builds the `muxr://pair?...` URI that is encoded into the pairing QR
//! code.  The mobile client parses this URI to extract the server address, trust
//! mode, auth token, and label.
//!
//! ## URI format — v2 (emitted by this module)
//!
//! ```text
//! muxr://pair?v=2&h=<host>&p=<port>&t=<base64url>&ro=<0|1>&n=<pct-encoded>&tm=<pin|ca>[&fp=<hex>]
//! ```
//!
//! - `v=2` — URI version.  The emitter now writes v=2; v=1 URIs are still
//!   accepted by the mobile parser for backwards compatibility.
//! - `h` — server host or IP.
//! - `p` — server port.
//! - `t` — base64url (no padding) of the plaintext auth token bytes.
//! - `ro` — `1` for read-only tokens, `0` for read-write.
//! - `n` — percent-encoded human label for the server (shown in the app).
//! - `tm` — trust mode: `pin` (fingerprint-pinned / self-signed) or `ca`
//!   (system-CA trusted).
//! - `fp` — lowercase hex SHA-256 fingerprint of the server TLS cert DER;
//!   **present only when `tm=pin`**, absent for `tm=ca`.
//!
//! ### Trust modes
//!
//! | `tm` | `fp` | Client behaviour |
//! |------|------|-----------------|
//! | `pin` | present (64 lowercase hex chars) | Exclusive fingerprint-pin — system CA roots disabled |
//! | `ca`  | absent | System CA trust — survives cert renewals |
//!
//! ### Back-compat note
//!
//! v=1 URIs (`fp` always present, no `tm`) continue to work in the mobile
//! parser as `selfSignedPinned`.  This module no longer emits v=1.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

// ── Trust mode ────────────────────────────────────────────────────────────────

/// The trust model to encode in the pairing URI.
///
/// The caller (C2) is responsible for supplying the correct variant based on
/// the server's cert source:
/// - `Pin` — self-signed / direct TLS; the 64-char lowercase hex SHA-256
///   fingerprint of the server's DER cert **must** be supplied.
///   Invariant: `fingerprint` is exactly 64 lowercase hex characters.
/// - `Ca` — CA-signed cert (Let's Encrypt, Cloudflare edge, etc.) or h2c
///   (TLS terminated by an upstream proxy).  No fingerprint is
///   included; the mobile client uses the system CA store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingTrust {
    /// Fingerprint-pinned trust (self-signed / LAN).
    ///
    /// `fingerprint` must be exactly 64 lowercase hexadecimal characters
    /// representing the SHA-256 digest of the server's TLS certificate DER.
    Pin { fingerprint: String },
    /// System-CA trust (public domain, CA-signed cert, or h2c behind proxy).
    Ca,
}

// ── Params struct ─────────────────────────────────────────────────────────────

/// Parameters needed to generate a pairing QR code URI.
#[allow(dead_code)]
pub struct PairingParams {
    /// Server hostname or IP address.
    pub host: String,
    /// Server gRPC port.
    pub port: u16,
    /// Trust mode — determines whether a fingerprint is embedded in the URI.
    pub trust: PairingTrust,
    /// Freshly-minted plaintext auth token (will be base64url-encoded in the URI).
    pub token: String,
    /// Whether this token grants read-only access.
    pub read_only: bool,
    /// Human-readable label for the server (e.g. "Home desktop").
    pub label: String,
}

impl PairingParams {
    /// Build the `muxr://pair?...` URI (v=2).
    ///
    /// The token is base64url-encoded (no padding) so it is safe to embed in a
    /// URL without further escaping.  The label is percent-encoded.
    ///
    /// - `PairingTrust::Pin { fingerprint }` → appends `tm=pin&fp=<fingerprint>`.
    /// - `PairingTrust::Ca`                  → appends `tm=ca` (no `fp` param).
    #[allow(dead_code)]
    pub fn to_uri(&self) -> String {
        let token_b64 = URL_SAFE_NO_PAD.encode(self.token.as_bytes());
        let label_pct = percent_encode(&self.label);
        let ro = if self.read_only { "1" } else { "0" };

        let trust_suffix = match &self.trust {
            PairingTrust::Pin { fingerprint } => format!("tm=pin&fp={fingerprint}"),
            PairingTrust::Ca => "tm=ca".to_string(),
        };

        format!(
            "muxr://pair?v=2&h={h}&p={p}&t={t}&ro={ro}&n={n}&{trust}",
            h = self.host,
            p = self.port,
            t = token_b64,
            ro = ro,
            n = label_pct,
            trust = trust_suffix,
        )
    }
}

/// Percent-encode a string for use in a URI query-parameter value.
///
/// Encodes all bytes that are not unreserved URI characters (ALPHA / DIGIT /
/// `-` / `_` / `.` / `~`) as defined in RFC 3986 §2.3.  This is intentionally
/// conservative so the result is parseable by a simple `split('&')` on the
/// mobile side.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 64-char fake lowercase hex fingerprint for use in tests.
    fn fake_fp() -> String {
        "a1b2c3d4e5f6".repeat(5) + "a1b2c3d4"
    }

    fn pin_params(read_only: bool) -> PairingParams {
        PairingParams {
            host: "192.168.1.10".to_string(),
            port: 50051,
            trust: PairingTrust::Pin {
                fingerprint: fake_fp(),
            },
            token: "mysecrettoken".to_string(),
            read_only,
            label: "Home desktop".to_string(),
        }
    }

    fn ca_params(read_only: bool) -> PairingParams {
        PairingParams {
            host: "zelli.example.com".to_string(),
            port: 443,
            trust: PairingTrust::Ca,
            token: "mysecrettoken".to_string(),
            read_only,
            label: "Cloud server".to_string(),
        }
    }

    // ── Scheme / structure ────────────────────────────────────────────────────

    #[test]
    fn uri_has_correct_scheme_and_host() {
        let uri = pin_params(false).to_uri();
        assert!(
            uri.starts_with("muxr://pair?"),
            "URI should start with muxr://pair? but got: {uri}"
        );
    }

    #[test]
    fn uri_contains_version_2() {
        let uri = pin_params(false).to_uri();
        assert!(uri.contains("v=2"), "URI should contain v=2: {uri}");
    }

    #[test]
    fn uri_does_not_contain_version_1() {
        let uri = pin_params(false).to_uri();
        assert!(
            !uri.contains("v=1"),
            "Emitter should no longer write v=1: {uri}"
        );
    }

    // ── Pin trust mode ────────────────────────────────────────────────────────

    #[test]
    fn pin_uri_contains_tm_pin() {
        let uri = pin_params(false).to_uri();
        assert!(
            uri.contains("tm=pin"),
            "Pin URI should contain tm=pin: {uri}"
        );
    }

    #[test]
    fn pin_uri_contains_fingerprint() {
        let params = pin_params(false);
        let uri = params.to_uri();
        let fp = match &params.trust {
            PairingTrust::Pin { fingerprint } => fingerprint.clone(),
            PairingTrust::Ca => panic!("expected Pin"),
        };
        assert!(
            uri.contains(&format!("fp={fp}")),
            "Pin URI should contain fp=<fingerprint>: {uri}"
        );
    }

    #[test]
    fn pin_uri_round_trip_v2_with_fp() {
        let params = pin_params(false);
        let uri = params.to_uri();
        assert!(uri.contains("v=2"), "v=2: {uri}");
        assert!(uri.contains("tm=pin"), "tm=pin: {uri}");
        assert!(
            uri.contains(&format!(
                "fp={}",
                match &params.trust {
                    PairingTrust::Pin { fingerprint } => fingerprint,
                    _ => panic!("expected Pin"),
                }
            )),
            "fp present: {uri}"
        );
    }

    // ── CA trust mode ─────────────────────────────────────────────────────────

    #[test]
    fn ca_uri_contains_tm_ca() {
        let uri = ca_params(false).to_uri();
        assert!(uri.contains("tm=ca"), "CA URI should contain tm=ca: {uri}");
    }

    #[test]
    fn ca_uri_has_no_fp_param() {
        let uri = ca_params(false).to_uri();
        assert!(
            !uri.contains("fp="),
            "CA URI must NOT contain fp= param: {uri}"
        );
    }

    #[test]
    fn ca_uri_round_trip_v2_no_fp() {
        let uri = ca_params(false).to_uri();
        assert!(uri.contains("v=2"), "v=2: {uri}");
        assert!(uri.contains("tm=ca"), "tm=ca: {uri}");
        assert!(!uri.contains("fp="), "no fp: {uri}");
    }

    // ── Shared params (host, port, ro, token, label) ──────────────────────────

    #[test]
    fn uri_contains_host_and_port() {
        let uri = pin_params(false).to_uri();
        assert!(
            uri.contains("h=192.168.1.10"),
            "URI should contain host: {uri}"
        );
        assert!(uri.contains("p=50051"), "URI should contain port: {uri}");
    }

    #[test]
    fn uri_token_is_base64url_encoded() {
        let params = pin_params(false);
        let uri = params.to_uri();

        // Extract the `t=` value.
        let t_val = uri
            .split('&')
            .find(|s| s.starts_with("t="))
            .and_then(|s| s.strip_prefix("t="))
            .expect("URI should contain t= parameter");

        // Decode it and verify it round-trips to the original token.
        let decoded = URL_SAFE_NO_PAD
            .decode(t_val)
            .expect("t= value should be valid base64url");
        assert_eq!(
            std::str::from_utf8(&decoded).unwrap(),
            params.token,
            "base64url-decoded token should match original"
        );
    }

    #[test]
    fn ca_uri_token_is_base64url_encoded() {
        let params = ca_params(false);
        let uri = params.to_uri();

        let t_val = uri
            .split('&')
            .find(|s| s.starts_with("t="))
            .and_then(|s| s.strip_prefix("t="))
            .expect("URI should contain t= parameter");

        let decoded = URL_SAFE_NO_PAD
            .decode(t_val)
            .expect("t= value should be valid base64url");
        assert_eq!(
            std::str::from_utf8(&decoded).unwrap(),
            params.token,
            "base64url-decoded token should match original"
        );
    }

    #[test]
    fn uri_ro_flag_read_only() {
        let uri = pin_params(true).to_uri();
        assert!(
            uri.contains("ro=1"),
            "read-only URI should have ro=1: {uri}"
        );
    }

    #[test]
    fn uri_ro_flag_read_write() {
        let uri = pin_params(false).to_uri();
        assert!(
            uri.contains("ro=0"),
            "read-write URI should have ro=0: {uri}"
        );
    }

    #[test]
    fn uri_label_is_percent_encoded() {
        let params = pin_params(false);
        let uri = params.to_uri();
        // "Home desktop" — the space should be percent-encoded as %20.
        assert!(
            uri.contains("n=Home%20desktop"),
            "label should be percent-encoded in URI: {uri}"
        );
    }

    #[test]
    fn ca_uri_label_is_percent_encoded() {
        let params = ca_params(false);
        let uri = params.to_uri();
        // "Cloud server" — the space should be percent-encoded.
        assert!(
            uri.contains("n=Cloud%20server"),
            "label should be percent-encoded in CA URI: {uri}"
        );
    }

    #[test]
    fn pin_uri_all_required_params_present() {
        let uri = pin_params(false).to_uri();
        for param in &["v=", "h=", "p=", "t=", "ro=", "n=", "tm=", "fp="] {
            assert!(
                uri.contains(param),
                "Pin URI should contain param {param}: {uri}"
            );
        }
    }

    #[test]
    fn ca_uri_all_required_params_present_no_fp() {
        let uri = ca_params(false).to_uri();
        for param in &["v=", "h=", "p=", "t=", "ro=", "n=", "tm="] {
            assert!(
                uri.contains(param),
                "CA URI should contain param {param}: {uri}"
            );
        }
        assert!(!uri.contains("fp="), "CA URI must not have fp=: {uri}");
    }

    // ── percent_encode helpers ────────────────────────────────────────────────

    #[test]
    fn percent_encode_unreserved_chars_unchanged() {
        let s = "abcABC012-_.~";
        assert_eq!(
            percent_encode(s),
            s,
            "unreserved chars should not be encoded"
        );
    }

    #[test]
    fn percent_encode_spaces_and_special() {
        assert_eq!(percent_encode(" "), "%20");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a/b"), "a%2Fb");
        assert_eq!(percent_encode("a+b"), "a%2Bb");
    }
}
