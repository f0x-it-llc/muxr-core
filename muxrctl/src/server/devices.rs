//! devices — thin wrappers over `muxrd::notify::devices::PushDeviceStore`.
//!
//! Devices are **muxrd-owned** state (unlike tokens, which are zellij-owned —
//! see `tokens.rs`), so this module goes through the muxrd library's
//! [`PushDeviceStore`] rather than reimplementing sidecar I/O. See
//! `muxrd::notify::devices` module docs for the atomic-write / no-cache
//! concurrency contract that makes it safe for `muxrctl` to read/write this
//! file while the daemon is running.

use anyhow::{Context, Result};
use muxrd::notify::devices::{HANDLE_PREFIX_LEN, PushDevice, PushDeviceStore};

// ── Local clone-able device record ───────────────────────────────────────────

/// A local, `Clone`-capable record of a registered push device, safe to
/// display.
///
/// Mirrors `muxrd::notify::devices::PushDevice` but replaces the full
/// `push_handle` (a bearer capability — never surfaced in full outside
/// muxrd, matching the gRPC layer's `PushTargetInfo.handle_prefix` policy)
/// with a short display-only prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    /// The display name the device registered under.
    pub device_name: String,
    /// Client-reported platform string (e.g. `"android"`, `"ios"`).
    pub platform: String,
    /// Unix epoch seconds at registration (refreshed on every re-register).
    pub registered_at: u64,
    /// Leading [`HANDLE_PREFIX_LEN`] hex chars of the push handle — safe to
    /// display; the full handle never leaves muxrd.
    pub handle_prefix: String,
}

impl From<PushDevice> for DeviceRecord {
    fn from(d: PushDevice) -> Self {
        let handle_prefix = d.push_handle.chars().take(HANDLE_PREFIX_LEN).collect();
        Self {
            device_name: d.device_name,
            platform: d.platform,
            registered_at: d.registered_at,
            handle_prefix,
        }
    }
}

// ── Device registry operations ───────────────────────────────────────────────

/// List every registered push device (fresh read).
#[allow(dead_code)]
pub fn list() -> Result<Vec<DeviceRecord>> {
    list_from(&PushDeviceStore::new())
}

/// Remove a registered device by display name.
///
/// Returns `true` if a device was found and removed, `false` if no device
/// with that name existed. Safe whether or not the daemon is currently
/// running: the underlying store is a plain atomic-rename file write, and
/// the daemon re-reads the registry fresh on every send (see
/// `muxrd::notify::devices` module docs).
#[allow(dead_code)]
pub fn remove(name: &str) -> Result<bool> {
    remove_from(&PushDeviceStore::new(), name)
}

/// Resolve the push-notification relay URL to display.
///
/// Prefers the running daemon's advertised `notify_relay_url` (read from
/// `StatusInfo` via [`super::status`]); falls back to resolving the
/// effective config directly when the daemon is stopped/unreachable, so the
/// Devices screen still shows a meaningful value with the daemon down.
#[allow(dead_code)]
pub fn relay_url() -> Option<String> {
    if let Some(info) = super::status() {
        return info.notify_relay_url;
    }
    super::effective_config()
        .ok()
        .and_then(|c| c.notify_relay_url)
}

// ── Store-parameterised helpers (test seam) ──────────────────────────────────

fn list_from(store: &PushDeviceStore) -> Result<Vec<DeviceRecord>> {
    let raw = store
        .list()
        .context("devices::list: failed to list push devices")?;
    Ok(raw.into_iter().map(DeviceRecord::from).collect())
}

fn remove_from(store: &PushDeviceStore, name: &str) -> Result<bool> {
    store
        .remove_by_name(name)
        .context("devices::remove: failed to remove push device")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique-ish temp path per test (avoids cross-test collisions when run
    /// in parallel — mirrors the pattern used by `muxrd::notify::devices` and
    /// `control.rs`'s own tests).
    fn temp_store_path(tag: &str) -> std::path::PathBuf {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "muxrctl-devices-test-{tag}-{}-{secs}.json",
            std::process::id()
        ))
    }

    fn sample(name: &str, handle_char: char) -> PushDevice {
        PushDevice {
            device_name: name.to_owned(),
            push_handle: handle_char
                .to_string()
                .repeat(muxrd::notify::devices::MIN_HANDLE_LEN),
            platform: "android".to_owned(),
            registered_at: 1_700_000_000,
        }
    }

    #[test]
    fn device_record_from_push_device_truncates_handle() {
        let device = sample("phone-a", 'a');
        let record = DeviceRecord::from(device);
        assert_eq!(record.device_name, "phone-a");
        assert_eq!(record.platform, "android");
        assert_eq!(record.registered_at, 1_700_000_000);
        assert_eq!(record.handle_prefix.len(), HANDLE_PREFIX_LEN);
        assert_eq!(record.handle_prefix, "a".repeat(HANDLE_PREFIX_LEN));
    }

    #[test]
    fn list_from_empty_store_is_empty() {
        let path = temp_store_path("list-empty");
        let store = PushDeviceStore::at_path(path.clone());
        assert_eq!(list_from(&store).unwrap(), Vec::new());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn list_from_reflects_upserted_devices() {
        let path = temp_store_path("list-nonempty");
        let store = PushDeviceStore::at_path(path.clone());
        store.upsert(sample("phone-a", 'a')).unwrap();
        store.upsert(sample("phone-b", 'b')).unwrap();

        let listed = list_from(&store).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|d| d.device_name == "phone-a"));
        assert!(listed.iter().any(|d| d.device_name == "phone-b"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_from_round_trip() {
        let path = temp_store_path("remove-roundtrip");
        let store = PushDeviceStore::at_path(path.clone());
        store.upsert(sample("phone-a", 'a')).unwrap();
        store.upsert(sample("phone-b", 'b')).unwrap();

        assert!(remove_from(&store, "phone-a").unwrap());
        let listed = list_from(&store).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device_name, "phone-b");

        // Removing again is a no-op that reports `false`.
        assert!(!remove_from(&store, "phone-a").unwrap());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_from_missing_device_returns_false() {
        let path = temp_store_path("remove-missing");
        let store = PushDeviceStore::at_path(path.clone());
        store.upsert(sample("phone-a", 'a')).unwrap();

        assert!(!remove_from(&store, "not-there").unwrap());
        assert_eq!(list_from(&store).unwrap().len(), 1);

        let _ = std::fs::remove_file(&path);
    }
}
