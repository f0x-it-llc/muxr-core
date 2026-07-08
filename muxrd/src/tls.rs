//! tls — self-signed TLS certificate management.
//!
//! Generates a self-signed certificate + key pair for `127.0.0.1` and
//! `localhost` using `rcgen`, persists them as PEM files under the zellij
//! data dir (`ZELLIJ_PROJ_DIR.data_dir()/muxrd/`), and reloads them
//! on restart so the client can pin the same cert across server restarts.
//!
//! ## Cert dir layout
//!
//! ```text
//! $XDG_DATA_HOME/zellij/muxrd/
//!     server.crt      — X.509 certificate (PEM)
//!     server.key      — private key (PEM)
//!     server.san.json — JSON list of extra SANs included when cert was generated
//! ```
//!
//! The directory is created with `0700` permissions; the files themselves
//! inherit `0600` from the umask (or are `chmod`-ed explicitly on Unix).
//!
//! ## SAN support
//!
//! The cert always covers `127.0.0.1` and `localhost` (built-in).  Additional
//! SANs can be requested via [`SanEntry`] (parsed from `--san` CLI flags or the
//! `MUXRD_SAN` environment variable, comma-separated).  If the on-disk
//! cert does not cover all requested extra SANs it is regenerated.
//!
//! ## Usage
//!
//! ```no_run
//! use muxrd::tls::{load_or_generate_identity, SanEntry};
//! use tonic::transport::{ServerTlsConfig, Identity};
//!
//! let extra: Vec<SanEntry> = vec![SanEntry::parse("100.64.0.5")];
//! let (identity, cert_pem) = load_or_generate_identity(&extra).unwrap();
//! let tls = ServerTlsConfig::new().identity(identity);
//! ```

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair, SanType};
use sha2::Digest;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use tonic::transport::Identity;

// ─── SAN entry ────────────────────────────────────────────────────────────────

/// A Subject Alternative Name entry for the self-signed certificate.
///
/// Parsed from CLI `--san` values or the `MUXRD_SAN` environment
/// variable (comma-separated).  Each value is tried as an IP address first;
/// on parse failure it is treated as a DNS name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum SanEntry {
    /// An IP-address SAN (IPv4 or IPv6).
    Ip(std::net::IpAddr),
    /// A DNS-name SAN.
    Dns(String),
}

impl SanEntry {
    /// Parse a single SAN value: IP if it parses as one, otherwise DNS.
    pub fn parse(s: &str) -> Self {
        if let Ok(ip) = s.trim().parse::<IpAddr>() {
            SanEntry::Ip(ip)
        } else {
            SanEntry::Dns(s.trim().to_owned())
        }
    }

    /// Parse the `MUXRD_SAN` environment variable (comma-separated list).
    ///
    /// Returns an empty vec when the env var is absent or empty.
    pub fn from_env() -> Vec<Self> {
        std::env::var("MUXRD_SAN")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Self::parse)
            .collect()
    }

    /// Human-readable display string (used in log output).
    fn display(&self) -> String {
        match self {
            SanEntry::Ip(ip) => ip.to_string(),
            SanEntry::Dns(d) => d.clone(),
        }
    }
}

// ─── Cert dir ─────────────────────────────────────────────────────────────────

/// Returns the directory where the server cert/key are persisted.
fn cert_dir() -> Result<PathBuf> {
    let base = zellij_utils::consts::ZELLIJ_PROJ_DIR.data_dir();
    let dir = base.join("muxrd");
    std::fs::create_dir_all(&dir).with_context(|| format!("create cert dir {}", dir.display()))?;

    // Restrict directory permissions on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        let _ = std::fs::set_permissions(&dir, perms);
    }

    Ok(dir)
}

// ─── SAN sidecar ──────────────────────────────────────────────────────────────

/// Read the persisted extra-SAN list from `server.san.json`.
///
/// Returns an empty vec if the file is absent or unparseable (that means the
/// cert was generated without extras → no extra SANs covered).
///
/// Exposed as `pub` so the `muxrctl` server facade can reuse this parser
/// instead of duplicating it with its own `serde_json` call.
pub fn read_san_sidecar(dir: &Path) -> Vec<SanEntry> {
    let path = dir.join("server.san.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        log::warn!("tls: failed to parse SAN sidecar {}: {e}", path.display());
        vec![]
    })
}

/// Persist the extra-SAN list to `server.san.json`.
fn write_san_sidecar(dir: &Path, sans: &[SanEntry]) -> Result<()> {
    let path = dir.join("server.san.json");
    let json = serde_json::to_string(sans).context("tls: serialize SAN list")?;
    write_restricted(&path, &json)
        .with_context(|| format!("tls: write SAN sidecar {}", path.display()))
}

/// Returns `true` if `stored_sans` covers every entry in `requested`.
///
/// The built-in `127.0.0.1` / `localhost` are always present in the cert, so
/// only the *extra* SANs beyond those need to be tracked in the sidecar.
fn sidecar_covers(stored_sans: &[SanEntry], requested: &[SanEntry]) -> bool {
    requested.iter().all(|r| stored_sans.contains(r))
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Load the persisted cert+key, or generate a fresh self-signed pair and
/// persist it for future restarts.
///
/// `extra_sans` — additional Subject Alternative Names beyond the built-in
/// `127.0.0.1` / `localhost`.  If the on-disk cert does NOT cover all requested
/// SANs the cert+key are regenerated (and the new SANs persisted to a sidecar).
///
/// Returns `(Identity, cert_pem_string)`:
/// - `Identity` — passed directly to `ServerTlsConfig::identity()`.
/// - `cert_pem_string` — the public certificate in PEM format; hand this to
///   clients so they can trust the self-signed cert without a CA.
pub fn load_or_generate_identity(extra_sans: &[SanEntry]) -> Result<(Identity, String)> {
    let dir = cert_dir()?;
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");

    if cert_path.exists() && key_path.exists() {
        let stored_sans = read_san_sidecar(&dir);
        if sidecar_covers(&stored_sans, extra_sans) {
            let cert_pem = std::fs::read_to_string(&cert_path)
                .with_context(|| format!("read {}", cert_path.display()))?;
            let key_pem = std::fs::read_to_string(&key_path)
                .with_context(|| format!("read {}", key_path.display()))?;
            log::info!(
                "tls: loading existing cert/key from {} (covers all requested SANs)",
                dir.display()
            );
            let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
            return Ok((identity, cert_pem));
        }
        log::info!(
            "tls: existing cert at {} does not cover all requested SANs — regenerating",
            cert_path.display()
        );
    }

    let san_desc: Vec<String> = extra_sans.iter().map(SanEntry::display).collect();
    log::info!(
        "tls: generating self-signed cert for 127.0.0.1 + localhost{}{} → {}",
        if san_desc.is_empty() { "" } else { " + " },
        san_desc.join(", "),
        dir.display()
    );

    let (cert_pem, key_pem) = generate_self_signed_pem(extra_sans)?;

    // Persist cert, key, and the SAN sidecar.
    write_restricted(&cert_path, &cert_pem)
        .with_context(|| format!("write {}", cert_path.display()))?;
    write_restricted(&key_path, &key_pem)
        .with_context(|| format!("write {}", key_path.display()))?;
    write_san_sidecar(&dir, extra_sans)?;

    log::info!("tls: persisted cert to {}", cert_path.display());

    let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
    Ok((identity, cert_pem))
}

/// Generate a fresh self-signed cert+key for `127.0.0.1`, `localhost`, and any
/// `extra_sans`.
///
/// Returns `(cert_pem, key_pem)`.
///
/// This function is `pub` primarily to enable unit testing.
pub fn generate_self_signed_pem(extra_sans: &[SanEntry]) -> Result<(String, String)> {
    let signing_key = KeyPair::generate().context("rcgen: generate key pair")?;

    let mut params = CertificateParams::new(vec!["localhost".to_owned()])
        .context("rcgen: CertificateParams::new")?;

    // Always include 127.0.0.1.
    params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::V4(std::net::Ipv4Addr::new(
            127, 0, 0, 1,
        ))));

    // Add any extra SANs.
    for san in extra_sans {
        match san {
            SanEntry::Ip(ip) => {
                params.subject_alt_names.push(SanType::IpAddress(*ip));
            }
            SanEntry::Dns(name) => {
                params.subject_alt_names.push(SanType::DnsName(
                    name.clone()
                        .try_into()
                        .with_context(|| format!("rcgen: invalid DNS SAN {:?}", name))?,
                ));
            }
        }
    }

    let cert = params
        .self_signed(&signing_key)
        .context("rcgen: self_signed")?;

    Ok((cert.pem(), signing_key.serialize_pem()))
}

/// Load a TLS identity from user-provided PEM files.
///
/// Reads `cert_path` and `key_path` from disk, validates that the cert file
/// contains at least one `CERTIFICATE` PEM block and that the key file is
/// non-empty, then builds a [`tonic::transport::Identity`].
///
/// Returns `(Identity, cert_pem_string)` — the same shape as
/// [`load_or_generate_identity`] so callers can compute a fingerprint or log
/// the cert.
///
/// ## Constraints
///
/// - Does **not** write, chmod, rename, or otherwise mutate the user's files —
///   they are operator-managed.
/// - Both files must be readable; returns a clear `anyhow` error naming the
///   missing/unreadable path if either cannot be read.
/// - Returns an error if the cert file contains no `CERTIFICATE` block.
/// - Returns an error if the key file contains no recognisable private key PEM block.
///
/// ## Example
///
/// ```no_run
/// use std::path::Path;
/// use muxrd::tls::load_external_identity;
///
/// let (identity, cert_pem) = load_external_identity(
///     Path::new("/etc/ssl/certs/server.crt"),
///     Path::new("/etc/ssl/private/server.key"),
/// ).unwrap();
/// ```
pub fn load_external_identity(cert_path: &Path, key_path: &Path) -> Result<(Identity, String)> {
    // Read cert PEM.
    let cert_pem = std::fs::read_to_string(cert_path)
        .with_context(|| format!("tls: cannot read cert file '{}'", cert_path.display()))?;

    // Read key PEM.
    let key_pem = std::fs::read_to_string(key_path)
        .with_context(|| format!("tls: cannot read key file '{}'", key_path.display()))?;

    // Validate: cert must contain at least one CERTIFICATE block.
    {
        use std::io::BufReader;
        let mut reader = BufReader::new(cert_pem.as_bytes());
        let first = rustls_pemfile::certs(&mut reader).next();
        match first {
            None => {
                anyhow::bail!(
                    "tls: cert file '{}' contains no CERTIFICATE PEM block",
                    cert_path.display()
                );
            }
            Some(Err(e)) => {
                return Err(e).with_context(|| {
                    format!(
                        "tls: failed to decode CERTIFICATE block in '{}'",
                        cert_path.display()
                    )
                });
            }
            Some(Ok(_)) => {} // at least one valid DER block present
        }
    }

    // Validate: key file must contain at least one parseable private key.
    //
    // `Identity::from_pem` defers key parsing to serve-time, so a malformed or
    // mismatched key would pass here and then crash after daemonization with an
    // opaque error.  We parse eagerly to surface the problem at `init` time.
    {
        use std::io::BufReader;
        let mut reader = BufReader::new(key_pem.as_bytes());
        match rustls_pemfile::private_key(&mut reader) {
            Ok(Some(_)) => {} // found a valid private key — all good
            Ok(None) => {
                anyhow::bail!(
                    "tls: key file '{}' contains no recognisable private key PEM block \
                     (expected PRIVATE KEY, RSA PRIVATE KEY, or EC PRIVATE KEY)",
                    key_path.display()
                );
            }
            Err(e) => {
                return Err(anyhow::anyhow!(e)).with_context(|| {
                    format!(
                        "tls: failed to parse private key from '{}'",
                        key_path.display()
                    )
                });
            }
        }
    }

    let identity = Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes());
    Ok((identity, cert_pem))
}

/// Write `content` to `path` and restrict its permissions to `0600` on Unix.
fn write_restricted(path: &std::path::Path, content: &str) -> Result<()> {
    std::fs::write(path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }

    Ok(())
}

/// Lowercase hex SHA-256 of the leaf certificate's DER bytes (no colons).
///
/// Used by `muxrctl` to embed a pin in the pairing QR code; the mobile client
/// recomputes it over the presented certificate and compares the fingerprint before
/// trusting the connection.
///
/// `cert_pem` must contain at least one `CERTIFICATE` PEM block.  Only the
/// **first** block (the leaf certificate) is hashed.
///
/// Returns a 64-character lowercase hex string.
pub fn cert_sha256_fingerprint(cert_pem: &str) -> Result<String> {
    use std::io::BufReader;

    let mut reader = BufReader::new(cert_pem.as_bytes());
    let der = rustls_pemfile::certs(&mut reader)
        .next()
        .ok_or_else(|| anyhow::anyhow!("tls: no CERTIFICATE block found in PEM input"))?
        .context("tls: failed to decode certificate DER from PEM")?;

    let digest = sha2::Sha256::digest(der.as_ref());
    Ok(format!("{:064x}", digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_64_hex_chars_and_deterministic() {
        let (cert_pem, _key_pem) = generate_self_signed_pem(&[]).expect("generate cert");

        let fp1 = cert_sha256_fingerprint(&cert_pem).expect("fingerprint 1");
        let fp2 = cert_sha256_fingerprint(&cert_pem).expect("fingerprint 2");

        // Must be exactly 64 lowercase hex characters.
        assert_eq!(fp1.len(), 64, "fingerprint length should be 64 chars");
        assert!(
            fp1.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "fingerprint should be lowercase hex: {fp1}"
        );

        // Must be deterministic for the same input.
        assert_eq!(fp1, fp2, "fingerprint should be deterministic");
    }

    #[test]
    fn load_external_identity_roundtrip() {
        // Generate a self-signed pair to use as "user-provided" PEM files.
        let (cert_pem, key_pem) = generate_self_signed_pem(&[]).expect("generate cert");

        // Write to temp dir with unique names.
        let tmp = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let cert_path = tmp.join(format!("muxrd_test_cert_{unique}.pem"));
        let key_path = tmp.join(format!("muxrd_test_key_{unique}.pem"));

        std::fs::write(&cert_path, &cert_pem).expect("write temp cert");
        std::fs::write(&key_path, &key_pem).expect("write temp key");

        // load_external_identity must succeed.
        let result = load_external_identity(&cert_path, &key_path);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());

        let (_identity, returned_cert_pem) = result.unwrap();

        // Returned cert PEM must equal what was written.
        assert_eq!(
            returned_cert_pem, cert_pem,
            "returned cert_pem must match written cert PEM"
        );

        // cert_sha256_fingerprint of the returned pem must be 64 hex chars.
        let fp = cert_sha256_fingerprint(&returned_cert_pem).expect("fingerprint");
        assert_eq!(fp.len(), 64, "fingerprint must be 64 chars");
        assert!(
            fp.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "fingerprint must be lowercase hex: {fp}"
        );

        // Clean up temp files.
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
    }

    #[test]
    fn load_external_identity_missing_cert_returns_err() {
        let tmp = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let cert_path = tmp.join(format!("muxrd_test_missing_cert_{unique}.pem"));
        let key_path = tmp.join(format!("muxrd_test_missing_key_{unique}.pem"));

        // Neither file exists — should fail with a clear error naming the cert path.
        let err = load_external_identity(&cert_path, &key_path)
            .expect_err("expected Err for missing cert");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&cert_path.display().to_string()),
            "error should name the missing cert path; got: {msg}"
        );
    }

    #[test]
    fn load_external_identity_missing_key_returns_err() {
        let (cert_pem, _key_pem) = generate_self_signed_pem(&[]).expect("generate cert");

        let tmp = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let cert_path = tmp.join(format!("muxrd_test_cert2_{unique}.pem"));
        let key_path = tmp.join(format!("muxrd_test_missing_key2_{unique}.pem"));

        std::fs::write(&cert_path, &cert_pem).expect("write temp cert");
        // key_path intentionally not written — must be missing.

        let err = load_external_identity(&cert_path, &key_path)
            .expect_err("expected Err for missing key");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&key_path.display().to_string()),
            "error should name the missing key path; got: {msg}"
        );

        let _ = std::fs::remove_file(&cert_path);
    }

    #[test]
    fn load_external_identity_invalid_cert_pem_returns_err() {
        let (_cert_pem, key_pem) = generate_self_signed_pem(&[]).expect("generate cert");

        let tmp = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let cert_path = tmp.join(format!("muxrd_test_invalid_cert_{unique}.pem"));
        let key_path = tmp.join(format!("muxrd_test_valid_key_{unique}.pem"));

        // Write garbage instead of a valid CERTIFICATE block.
        std::fs::write(&cert_path, "this is not a certificate\n").expect("write invalid cert");
        std::fs::write(&key_path, &key_pem).expect("write temp key");

        let err = load_external_identity(&cert_path, &key_path)
            .expect_err("expected Err for invalid cert PEM");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no CERTIFICATE PEM block") || msg.contains("CERTIFICATE"),
            "error should mention missing CERTIFICATE block; got: {msg}"
        );

        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
    }

    /// A cert file paired with a completely empty key file must return Err naming
    /// the key path (covers the `Ok(None)` branch of `private_key`).
    #[test]
    fn load_external_identity_empty_key_returns_err() {
        let (cert_pem, _key_pem) = generate_self_signed_pem(&[]).expect("generate cert");

        let tmp = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let cert_path = tmp.join(format!("muxrd_test_cert_emptykey_{unique}.pem"));
        let key_path = tmp.join(format!("muxrd_test_empty_key_{unique}.pem"));

        std::fs::write(&cert_path, &cert_pem).expect("write temp cert");
        std::fs::write(&key_path, "").expect("write empty key file");

        let err = load_external_identity(&cert_path, &key_path)
            .expect_err("expected Err for empty key file");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&key_path.display().to_string()),
            "error should name the key path; got: {msg}"
        );
        assert!(
            msg.contains("private key") || msg.contains("PRIVATE KEY"),
            "error should mention private key; got: {msg}"
        );

        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
    }

    /// A cert file paired with a garbage (non-PEM) key file must return Err naming
    /// the key path (covers the `Ok(None)` branch of `private_key` on junk input).
    #[test]
    fn load_external_identity_garbage_key_returns_err() {
        let (cert_pem, _key_pem) = generate_self_signed_pem(&[]).expect("generate cert");

        let tmp = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let cert_path = tmp.join(format!("muxrd_test_cert_garbagekey_{unique}.pem"));
        let key_path = tmp.join(format!("muxrd_test_garbage_key_{unique}.pem"));

        std::fs::write(&cert_path, &cert_pem).expect("write temp cert");
        // Write content that looks nothing like a PEM key block.
        std::fs::write(&key_path, "not a key at all\nthis is garbage\n")
            .expect("write garbage key file");

        let err = load_external_identity(&cert_path, &key_path)
            .expect_err("expected Err for garbage key file");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&key_path.display().to_string()),
            "error should name the key path; got: {msg}"
        );

        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
    }
}
