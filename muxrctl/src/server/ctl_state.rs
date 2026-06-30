//! ctl-local persistent state — `advertise_trust` setting.
//!
//! Stores a single value (`advertise_trust`) in a plain-text file under the
//! muxrd data dir:
//!
//! ```text
//! $XDG_DATA_HOME/zellij/muxrd/muxrctl_state
//! ```
//!
//! File format: one line containing `auto`, `ca`, or `pin` (case-insensitive).
//! A missing, empty, or unrecognised file defaults to `auto`.  No external crate
//! dependency is needed.
//!
//! ## Why plain text?
//!
//! - No extra crate needed (serde_json is a dev-time convenience, not bundled in
//!   the ctl Cargo.toml).
//! - The file is tiny (≤4 bytes + newline) and human-readable/editable.
//! - Extending to more fields in the future is a straightforward TOML migration;
//!   the file name is "muxrctl_state" (not "muxrctl_state.txt") so it can be
//!   repurposed without a rename.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// File name for the ctl state file inside the muxrd data dir.
const CTL_STATE_FILE: &str = "muxrctl_state";

/// The serialised string for each `AdvertiseTrust` variant.
///
/// Using lower-case values that round-trip through [`parse_advertise_trust`].
const TRUST_AUTO: &str = "auto";
const TRUST_CA: &str = "ca";
const TRUST_PIN: &str = "pin";

/// Return the path to the ctl state file.
///
/// Does NOT create the parent directory — that is the caller's responsibility
/// (or the responsibility of [`save_advertise_trust`], which calls
/// `data_dir()` which already creates it).
fn ctl_state_path() -> Result<PathBuf> {
    let dir = muxrd::config::data_dir()?;
    Ok(dir.join(CTL_STATE_FILE))
}

/// Parse an `advertise_trust` string value back to one of the three canonical
/// strings.  Returns `TRUST_AUTO` for any unrecognised input so forward
/// compatibility is safe.
fn parse_advertise_trust(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        TRUST_CA => TRUST_CA,
        TRUST_PIN => TRUST_PIN,
        _ => TRUST_AUTO,
    }
}

/// Load the persisted `advertise_trust` value from disk.
///
/// Returns one of `"auto"`, `"ca"`, or `"pin"` — always a valid value.  If
/// the file does not exist, or is unreadable/corrupted, returns `"auto"` (the
/// safe default) without returning an error.
pub fn load_advertise_trust() -> &'static str {
    let path = match ctl_state_path() {
        Ok(p) => p,
        Err(e) => {
            log::debug!(
                "ctl_state: cannot resolve data dir ({e}); defaulting advertise_trust=auto"
            );
            return TRUST_AUTO;
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_advertise_trust(&content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TRUST_AUTO,
        Err(e) => {
            log::warn!(
                "ctl_state: failed to read {}: {e}; defaulting advertise_trust=auto",
                path.display()
            );
            TRUST_AUTO
        }
    }
}

/// Persist the `advertise_trust` value to disk.
///
/// Writes a single line containing one of `"auto"`, `"ca"`, or `"pin"` to the
/// state file.  Errors are logged at `warn` level and silently swallowed — a
/// persistence failure must never crash the TUI.
pub fn save_advertise_trust(value: &str) {
    let path = match ctl_state_path() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("ctl_state: cannot resolve data dir for save ({e})");
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, value).with_context(|| {
        format!(
            "ctl_state: write advertise_trust={value} to {}",
            path.display()
        )
    }) {
        log::warn!("{e}");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_values() {
        assert_eq!(parse_advertise_trust("auto"), TRUST_AUTO);
        assert_eq!(parse_advertise_trust("ca"), TRUST_CA);
        assert_eq!(parse_advertise_trust("pin"), TRUST_PIN);
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(parse_advertise_trust("CA"), TRUST_CA);
        assert_eq!(parse_advertise_trust("Pin"), TRUST_PIN);
        assert_eq!(parse_advertise_trust("AUTO"), TRUST_AUTO);
    }

    #[test]
    fn parse_unknown_defaults_to_auto() {
        assert_eq!(parse_advertise_trust(""), TRUST_AUTO);
        assert_eq!(parse_advertise_trust("garbage"), TRUST_AUTO);
        assert_eq!(parse_advertise_trust("force"), TRUST_AUTO);
    }

    #[test]
    fn parse_strips_whitespace() {
        assert_eq!(parse_advertise_trust("  ca\n"), TRUST_CA);
        assert_eq!(parse_advertise_trust("\tpin "), TRUST_PIN);
    }

    #[test]
    fn roundtrip_via_temp_file() {
        // Write a known value to a temp file and confirm parse_advertise_trust
        // reads the correct canonical string back.  This tests the serialise →
        // deserialise round-trip without touching the real data dir or needing
        // external crates.
        let dir = std::env::temp_dir();
        let path = dir.join("muxrctl_test_advertise_trust");
        std::fs::write(&path, "pin").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_advertise_trust(&raw);
        assert_eq!(parsed, TRUST_PIN);
        // Cleanup.
        let _ = std::fs::remove_file(&path);
    }
}
