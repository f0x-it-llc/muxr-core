//! token_expiry — optional, muxr-side expiry for auth / pairing tokens.
//!
//! ## Why this exists
//!
//! zellij's `tokens` table (the auth tokens embedded in a pairing QR) has **no**
//! expiry column — only *session* tokens expire. A pairing QR therefore carries a
//! credential that is valid forever unless manually revoked. This module adds an
//! **opt-in** expiry overlay that muxr controls, without modifying the shared
//! zellij token DB.
//!
//! ## How it works
//!
//! A small JSON sidecar next to the token DB maps a token's SHA-256 hash to an
//! absolute expiry (Unix epoch seconds):
//!
//! ```text
//! <data_dir>/muxrd/token_expiry.json
//!   { "<sha256-hex>": 1794067200, ... }
//! ```
//!
//! - `muxrctl` / `muxrd create-token --expires-in …` call [`set_expiry`] with the
//!   freshly-minted plaintext when the operator chooses an expiring token.
//! - The `Login` RPC calls [`is_expired`] on the presented `auth_token` and
//!   refuses to mint a session token once the deadline passes.
//!
//! Tokens with no sidecar entry are **long-lived** (the historical behaviour) —
//! this feature only constrains tokens the operator explicitly time-boxes.
//!
//! ## Semantics
//!
//! Expiry gates *new* logins. A client that already exchanged the auth token for
//! a session token (via `Login`) keeps that session until the session token's own
//! TTL lapses (5 min, or 28 days with `remember_me`). This is the correct model
//! for a *pairing* token: it bounds the window to pair a new device, not the life
//! of an established session.
//!
//! ## Failure posture
//!
//! Reads are best-effort and fail **open** (treat as not-expired) so a missing or
//! corrupt sidecar never bricks every login — the feature is opt-in hardening, not
//! a mandatory gate. The file is written `0600` and updated atomically.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use sha2::Digest;

/// Sidecar file name inside the muxrd data dir.
const EXPIRY_FILE: &str = "token_expiry.json";

/// SHA-256 hex digest of a token's plaintext.
///
/// The sidecar keys on this so the plaintext is never written to disk. muxrctl
/// (on create) and muxrd (on `Login`) hash the same plaintext and agree.
pub fn token_hash(plaintext: &str) -> String {
    let digest = sha2::Sha256::digest(plaintext.as_bytes());
    format!("{digest:064x}")
}

/// Absolute path to the expiry sidecar (`<data_dir>/muxrd/token_expiry.json`).
fn expiry_path() -> Result<PathBuf> {
    Ok(crate::config::data_dir()?.join(EXPIRY_FILE))
}

/// Current Unix time in seconds.
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Load the hash→expiry map, dropping already-expired entries.
///
/// Returns an empty map on any read/parse error (fail-open).
fn load_pruned() -> HashMap<String, i64> {
    let Ok(path) = expiry_path() else {
        return HashMap::new();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return HashMap::new(), // absent → nothing expires
    };
    let mut map: HashMap<String, i64> = serde_json::from_str(&raw).unwrap_or_else(|e| {
        log::warn!("token_expiry: failed to parse {}: {e}", path.display());
        HashMap::new()
    });
    let now = now_epoch();
    map.retain(|_, &mut exp| exp > now);
    map
}

/// Serialize `map` to the sidecar with `0600` perms, written atomically.
fn store(map: &HashMap<String, i64>) -> Result<()> {
    let path = expiry_path()?;
    let json = serde_json::to_string(map).context("token_expiry: serialize map")?;

    let parent = path
        .parent()
        .context("token_expiry: sidecar has no parent dir")?;
    let tmp = parent.join(".token_expiry.json.tmp");
    std::fs::write(&tmp, &json)
        .with_context(|| format!("token_expiry: write temp {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("token_expiry: chmod 0600 {}", tmp.display()))?;
    }

    std::fs::rename(&tmp, &path)
        .with_context(|| format!("token_expiry: rename over {}", path.display()))?;
    Ok(())
}

/// Record that `plaintext` expires at `expires_at` (Unix epoch seconds).
///
/// Merges into the existing sidecar (pruning already-expired entries) so multiple
/// expiring tokens coexist. Call with the freshly-minted plaintext right after
/// creating the token.
pub fn set_expiry(plaintext: &str, expires_at: i64) -> Result<()> {
    let mut map = load_pruned();
    map.insert(token_hash(plaintext), expires_at);
    store(&map)
}

/// Record that `plaintext` expires `ttl_secs` seconds from now.
///
/// Convenience over [`set_expiry`] for callers that hold a relative TTL (e.g.
/// muxrctl's create form) rather than an absolute deadline.
pub fn set_expiry_in(plaintext: &str, ttl_secs: i64) -> Result<()> {
    set_expiry(plaintext, now_epoch() + ttl_secs)
}

/// True if `plaintext` has a recorded expiry that has already passed.
///
/// Fail-open: any error, or no recorded expiry, yields `false` (not expired).
pub fn is_expired(plaintext: &str) -> bool {
    let Ok(path) = expiry_path() else {
        return false;
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let map: HashMap<String, i64> = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => return false,
    };
    match map.get(&token_hash(plaintext)) {
        Some(&exp) => exp <= now_epoch(),
        None => false,
    }
}

/// Best-effort removal of a token's expiry entry (e.g. on revoke). Never errors
/// out the caller — a stale entry is harmless (it just prunes itself once past).
pub fn forget(plaintext: &str) {
    let mut map = load_pruned();
    if map.remove(&token_hash(plaintext)).is_some() {
        let _ = store(&map);
    }
}

/// Parse an `--expires-in` duration into an absolute expiry (Unix epoch seconds),
/// or `None` for a non-expiring ("never") token.
///
/// Accepts `<n><unit>` with unit `s`/`m`/`h`/`d` (e.g. `30m`, `24h`, `7d`), a bare
/// number of seconds, or the words `never`/`none`/empty → `None`.
pub fn parse_expires_in(spec: &str) -> Result<Option<i64>> {
    let s = spec.trim().to_ascii_lowercase();
    if s.is_empty() || s == "never" || s == "none" || s == "0" {
        return Ok(None);
    }
    let (num_str, mult): (&str, i64) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some(c) if c.is_ascii_digit() => (s.as_str(), 1), // bare seconds
        _ => anyhow::bail!("invalid duration '{spec}': use e.g. 30m, 24h, 7d, or 'never'"),
    };
    let n: i64 = num_str
        .parse()
        .with_context(|| format!("invalid duration '{spec}': '{num_str}' is not a number"))?;
    if n <= 0 {
        return Ok(None);
    }
    let secs = n
        .checked_mul(mult)
        .with_context(|| format!("duration '{spec}' overflows"))?;
    Ok(Some(now_epoch() + secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_64_hex_and_stable() {
        let h = token_hash("hunter2");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, token_hash("hunter2"));
        assert_ne!(h, token_hash("hunter3"));
    }

    #[test]
    fn parse_expires_in_units() {
        assert_eq!(parse_expires_in("never").unwrap(), None);
        assert_eq!(parse_expires_in("").unwrap(), None);
        assert_eq!(parse_expires_in("0").unwrap(), None);

        let now = now_epoch();
        let in_30m = parse_expires_in("30m").unwrap().unwrap();
        assert!((in_30m - (now + 1800)).abs() <= 2);
        let in_1d = parse_expires_in("1d").unwrap().unwrap();
        assert!((in_1d - (now + 86_400)).abs() <= 2);
        let bare = parse_expires_in("90").unwrap().unwrap();
        assert!((bare - (now + 90)).abs() <= 2);

        assert!(parse_expires_in("banana").is_err());
        assert!(parse_expires_in("12x").is_err());
    }
}
