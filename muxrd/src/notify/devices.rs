//! devices — the on-disk push-notification device registry.
//!
//! ## Why this exists
//!
//! Once a device has exchanged its platform push token for an opaque
//! `push_handle` with the notification relay (see `RESEARCH-DELTA.md` §6), it
//! registers that handle with muxrd via `RegisterPushTarget`. muxrd needs
//! somewhere durable to keep the mapping from `device_name` → `push_handle`
//! so the (future) event-kernel notifier can look up who to notify.
//!
//! ## How it works
//!
//! A JSON sidecar next to muxrd's other data-dir state (mirrors
//! `crate::token_expiry`'s sidecar pattern):
//!
//! ```text
//! <data_dir>/push_devices.json
//!   [ { "device_name": "...", "push_handle": "...", "platform": "...", "registered_at": 1710000000 }, ... ]
//! ```
//!
//! ## Concurrency contract
//!
//! `muxrctl` may read/edit this same file while the daemon is running (a
//! future devices screen — see `RESEARCH-DELTA.md` §4/§7), and two muxrd
//! RPCs can mutate concurrently, so:
//!
//! - an advisory lock on a `<file>.lock` sidecar spans the **whole**
//!   load→mutate→store cycle of every mutator (`upsert`/`remove_by_*`) — held
//!   *exclusive* — so two concurrent read-modify-write cycles serialize
//!   instead of losing each other's updates (a lost-update race). Reads
//!   (`list`/`count`) take the *shared* lock, so they never observe a write
//!   mid-flight. The lock is `flock(2)`-style advisory (`fs4`) and works
//!   cross-process, covering the muxrctl-while-daemon-runs case too;
//! - every **write** = serialize → temp file in the same directory → atomic
//!   `rename` (never a partial file is observable);
//! - every **read** is a fresh read of the file — there is NO in-memory
//!   cache, so a concurrent external writer's changes are always visible on
//!   the next call.
//!
//! ## Failure posture
//!
//! Unlike `token_expiry` (which fails open for security reasons), a
//! read/parse/write failure here is surfaced as an `Err` — silently treating
//! a corrupt or unreadable devices file as "empty" risks an `upsert`/`remove`
//! silently discarding every other registered device on its next write.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

/// Sidecar file name inside the muxrd data dir.
const DEVICES_FILE: &str = "push_devices.json";

/// Minimum/maximum length of a valid `push_handle` (see [`validate_push_handle`]).
pub const MIN_HANDLE_LEN: usize = 32;
pub const MAX_HANDLE_LEN: usize = 128;

/// Minimum/maximum length of a valid `device_name` (see [`validate_device_name`]).
pub const MIN_NAME_LEN: usize = 1;
pub const MAX_NAME_LEN: usize = 64;

/// Number of leading hex characters of a `push_handle` that are safe to
/// surface to a client / log line — the gRPC layer (`grpc::push_ops`) uses
/// this to build both `PushTargetInfo.handle_prefix` and log breadcrumbs, so
/// a full handle (the sole bearer credential the relay accepts) never leaves
/// this process.
pub const HANDLE_PREFIX_LEN: usize = 8;

/// One registered device: an opaque relay-issued `push_handle` plus display
/// metadata. muxrd never sees the raw platform push token — only what the
/// relay handed back (see `RESEARCH-DELTA.md` §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushDevice {
    /// Human-readable, client-chosen device label. Unique — [`PushDeviceStore::upsert`]
    /// replaces any existing entry with the same name (token-refresh / re-register path).
    pub device_name: String,
    /// Opaque handle minted by the notification relay. Treat as a bearer
    /// capability: never log in full, never return in full from a list RPC.
    pub push_handle: String,
    /// Client-reported platform string (e.g. "android", "ios").
    pub platform: String,
    /// Unix epoch seconds at registration (refreshed on every re-register).
    pub registered_at: u64,
}

/// Handle to the on-disk push-device registry.
///
/// Cheap to clone (holds at most one `PathBuf`) — every method resolves the
/// file path and does its own fresh read/write, so cloning never shares stale
/// state. See the module docs for the atomic-write / no-cache concurrency
/// contract.
#[derive(Debug, Clone, Default)]
pub struct PushDeviceStore {
    /// Override path (used by tests to avoid touching the real data dir).
    /// `None` resolves to `crate::config::data_dir()?.join(DEVICES_FILE)` at
    /// call time — resolved lazily (not at construction) so that constructing
    /// a [`PushDeviceStore`] stays infallible and side-effect-free, matching
    /// the other `MuxrService` builder fields (e.g. `notify_relay_url`).
    override_path: Option<PathBuf>,
}

impl PushDeviceStore {
    /// Store rooted at the standard location (`<data_dir>/push_devices.json`).
    pub fn new() -> Self {
        Self {
            override_path: None,
        }
    }

    /// Store rooted at an arbitrary path — used by tests to avoid touching
    /// the real data dir.
    pub fn at_path(path: PathBuf) -> Self {
        Self {
            override_path: Some(path),
        }
    }

    fn resolve_path(&self) -> Result<PathBuf> {
        match &self.override_path {
            Some(p) => Ok(p.clone()),
            None => Ok(crate::config::data_dir()?.join(DEVICES_FILE)),
        }
    }

    /// List every registered device (fresh read — see module docs). Takes the
    /// *shared* advisory lock so a concurrent mutator's load→store cycle is
    /// never observed half-applied.
    pub fn list(&self) -> Result<Vec<PushDevice>> {
        let path = self.resolve_path()?;
        let _lock = lock_shared(&path)?;
        load(&path)
    }

    /// Number of registered devices (fresh read).
    pub fn count(&self) -> Result<usize> {
        Ok(self.list()?.len())
    }

    /// Insert a new device, or replace the existing entry with the same
    /// `device_name` (the token-refresh / re-register path). The *exclusive*
    /// advisory lock spans the whole load→mutate→store cycle so concurrent
    /// mutators serialize instead of losing updates.
    pub fn upsert(&self, device: PushDevice) -> Result<()> {
        let path = self.resolve_path()?;
        let _lock = lock_exclusive(&path)?;
        let mut devices = load(&path)?;
        match devices
            .iter_mut()
            .find(|d| d.device_name == device.device_name)
        {
            Some(existing) => *existing = device,
            None => devices.push(device),
        }
        store(&path, &devices)
    }

    /// Remove a device by `device_name`. Returns `true` if a device was
    /// removed, `false` if no device with that name existed. Holds the
    /// *exclusive* lock across load→mutate→store (see [`Self::upsert`]).
    pub fn remove_by_name(&self, name: &str) -> Result<bool> {
        let path = self.resolve_path()?;
        let _lock = lock_exclusive(&path)?;
        let mut devices = load(&path)?;
        let before = devices.len();
        devices.retain(|d| d.device_name != name);
        let removed = devices.len() != before;
        if removed {
            store(&path, &devices)?;
        }
        Ok(removed)
    }

    /// Remove a device by `push_handle`. Returns `true` if a device was
    /// removed, `false` if no device with that handle existed. Used by the
    /// notifier when the relay reports the handle as gone (404/410 — see
    /// `RESEARCH-DELTA.md` §6). Holds the *exclusive* lock across
    /// load→mutate→store (see [`Self::upsert`]).
    pub fn remove_by_handle(&self, handle: &str) -> Result<bool> {
        let path = self.resolve_path()?;
        let _lock = lock_exclusive(&path)?;
        let mut devices = load(&path)?;
        let before = devices.len();
        devices.retain(|d| d.push_handle != handle);
        let removed = devices.len() != before;
        if removed {
            store(&path, &devices)?;
        }
        Ok(removed)
    }
}

// ─── Advisory file lock ──────────────────────────────────────────────────────

/// Path of the advisory-lock sidecar for `path` (`<path>.lock`). A dedicated
/// sidecar (not the registry file itself) so the lock is orthogonal to the
/// atomic temp-file+`rename` write — the `rename` never disturbs the fd the
/// lock is held on.
fn lock_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

/// Open (creating if absent) the advisory-lock sidecar. The parent dir is
/// created first so the very first `upsert` on a fresh data dir can lock
/// before the registry file exists.
fn open_lock_file(path: &Path) -> Result<File> {
    let lock_path = lock_path_for(path);
    let parent = lock_path
        .parent()
        .context("push_devices: lock sidecar has no parent dir")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("push_devices: create dir {}", parent.display()))?;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("push_devices: open lock {}", lock_path.display()))
}

/// Acquire the exclusive advisory lock, returning the fd-holding guard. The
/// lock releases when the returned `File` is dropped (fd close), i.e. at the
/// end of the caller's method — after `store` has renamed the new file in.
fn lock_exclusive(path: &Path) -> Result<File> {
    let f = open_lock_file(path)?;
    FileExt::lock_exclusive(&f).context("push_devices: acquire exclusive lock")?;
    Ok(f)
}

/// Acquire the shared advisory lock (concurrent readers permitted; blocks while
/// an exclusive mutator holds it). Guard released on drop.
fn lock_shared(path: &Path) -> Result<File> {
    let f = open_lock_file(path)?;
    FileExt::lock_shared(&f).context("push_devices: acquire shared lock")?;
    Ok(f)
}

/// Load the device list. A missing file is treated as an empty registry (the
/// common "no devices registered yet" case); any other read/parse error is
/// surfaced (see module docs — fail-closed, unlike `token_expiry`).
fn load(path: &Path) -> Result<Vec<PushDevice>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Serialize `devices` to `path` with `0600` perms, written atomically
/// (temp file in the same directory, then `rename`).
fn store(path: &Path, devices: &[PushDevice]) -> Result<()> {
    let json = serde_json::to_string(devices).context("push_devices: serialize")?;

    let parent = path
        .parent()
        .context("push_devices: sidecar has no parent dir")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("push_devices: create dir {}", parent.display()))?;
    // Derive the temp name from the target file's own name (not a fixed
    // ".push_devices.json.tmp") so two `PushDeviceStore`s that share a parent
    // directory but target different files (as every test does, all rooted at
    // `std::env::temp_dir()`) never race on the same temp path.
    let file_name = path
        .file_name()
        .context("push_devices: sidecar path has no file name")?
        .to_string_lossy();
    let tmp = parent.join(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, &json)
        .with_context(|| format!("push_devices: write temp {}", tmp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("push_devices: chmod 0600 {}", tmp.display()))?;
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("push_devices: rename over {}", path.display()))?;
    Ok(())
}

// ─── Input validation ─────────────────────────────────────────────────────────

/// Validate a client-supplied `push_handle`: lowercase hex, [`MIN_HANDLE_LEN`]
/// to [`MAX_HANDLE_LEN`] chars. Returns a human-readable message on failure —
/// the gRPC layer wraps it in `Status::invalid_argument`.
pub fn validate_push_handle(handle: &str) -> Result<(), String> {
    if handle.len() < MIN_HANDLE_LEN || handle.len() > MAX_HANDLE_LEN {
        return Err(format!(
            "must be {MIN_HANDLE_LEN}-{MAX_HANDLE_LEN} characters (got {})",
            handle.len()
        ));
    }
    if !handle
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("must be lowercase hex ([0-9a-f])".to_owned());
    }
    Ok(())
}

/// Validate a client-supplied `device_name`: [`MIN_NAME_LEN`] to
/// [`MAX_NAME_LEN`] chars, `[A-Za-z0-9 _.-]` only. Returns a human-readable
/// message on failure — the gRPC layer wraps it in `Status::invalid_argument`.
pub fn validate_device_name(name: &str) -> Result<(), String> {
    if name.len() < MIN_NAME_LEN || name.len() > MAX_NAME_LEN {
        return Err(format!(
            "must be {MIN_NAME_LEN}-{MAX_NAME_LEN} characters (got {})",
            name.len()
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'_' | b'.' | b'-'))
    {
        return Err("must contain only [A-Za-z0-9 _.-]".to_owned());
    }
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique-ish temp path per test (avoids cross-test collisions when run
    /// in parallel — mirrors the pattern used by `control.rs`'s tests).
    fn temp_store_path(tag: &str) -> PathBuf {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "muxrd-push-devices-test-{tag}-{}-{secs}.json",
            std::process::id()
        ))
    }

    fn sample(name: &str, handle_suffix: char) -> PushDevice {
        PushDevice {
            device_name: name.to_owned(),
            push_handle: handle_suffix.to_string().repeat(MIN_HANDLE_LEN),
            platform: "android".to_owned(),
            registered_at: 1_700_000_000,
        }
    }

    #[test]
    fn upsert_list_round_trip() {
        let path = temp_store_path("roundtrip");
        let store = PushDeviceStore::at_path(path.clone());

        assert_eq!(store.list().unwrap(), Vec::new());

        store.upsert(sample("phone-a", 'a')).unwrap();
        store.upsert(sample("phone-b", 'b')).unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|d| d.device_name == "phone-a"));
        assert!(listed.iter().any(|d| d.device_name == "phone-b"));
        assert_eq!(store.count().unwrap(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn same_name_upsert_replaces_existing_entry() {
        let path = temp_store_path("replace");
        let store = PushDeviceStore::at_path(path.clone());

        store.upsert(sample("phone-a", 'a')).unwrap();
        let mut refreshed = sample("phone-a", 'c');
        refreshed.registered_at = 1_700_000_999;
        store.upsert(refreshed.clone()).unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1, "re-register must replace, not duplicate");
        assert_eq!(listed[0], refreshed);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_by_name_and_by_handle() {
        let path = temp_store_path("remove");
        let store = PushDeviceStore::at_path(path.clone());

        store.upsert(sample("phone-a", 'a')).unwrap();
        store.upsert(sample("phone-b", 'b')).unwrap();

        assert!(store.remove_by_name("phone-a").unwrap());
        assert!(!store.remove_by_name("phone-a").unwrap(), "already removed");
        assert_eq!(store.count().unwrap(), 1);

        let b_handle = "b".repeat(MIN_HANDLE_LEN);
        assert!(store.remove_by_handle(&b_handle).unwrap());
        assert_eq!(store.count().unwrap(), 0);
        assert!(!store.remove_by_handle(&b_handle).unwrap());

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn store_file_has_0600_perms() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_store_path("perms");
        let store = PushDeviceStore::at_path(path.clone());
        store.upsert(sample("phone-a", 'a')).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "sidecar must be 0600, got {mode:o}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn concurrent_ish_write_is_visible_to_a_second_store_instance() {
        // No in-memory cache: a second `PushDeviceStore` pointed at the same
        // path must see writes made through the first instance immediately.
        let path = temp_store_path("concurrent");
        let writer = PushDeviceStore::at_path(path.clone());
        let reader = PushDeviceStore::at_path(path.clone());

        assert_eq!(reader.list().unwrap(), Vec::new());
        writer.upsert(sample("phone-a", 'a')).unwrap();
        assert_eq!(reader.count().unwrap(), 1);

        reader.upsert(sample("phone-b", 'b')).unwrap();
        assert_eq!(writer.count().unwrap(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn concurrent_upserts_of_distinct_devices_all_persist() {
        // N threads race `upsert` of distinct-named devices at one path. With
        // the exclusive advisory lock spanning each load→mutate→store cycle,
        // every write is applied to the latest state, so all N survive.
        //
        // On the *pre-lock* code this fails: unsynchronized read-modify-write
        // cycles interleave — a thread loads an older snapshot, then its
        // `store` renames a file missing the entries other threads added in
        // between (classic lost-update), leaving fewer than N devices.
        use std::sync::Arc;
        use std::thread;

        const N: usize = 24;
        let path = temp_store_path("race");
        let store = Arc::new(PushDeviceStore::at_path(path.clone()));

        let handles: Vec<_> = (0..N)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store.upsert(sample(&format!("device-{i}"), 'a')).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().expect("upsert thread must not panic");
        }

        assert_eq!(
            store.count().unwrap(),
            N,
            "every concurrent upsert of a distinct device must persist"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(lock_path_for(&path));
    }

    #[test]
    fn missing_file_is_treated_as_empty_registry() {
        let path = temp_store_path("missing");
        let store = PushDeviceStore::at_path(path);
        assert_eq!(store.list().unwrap(), Vec::new());
        assert_eq!(store.count().unwrap(), 0);
    }

    // ── Validation ────────────────────────────────────────────────────────────

    #[test]
    fn validate_push_handle_accepts_valid_hex() {
        assert!(validate_push_handle(&"a".repeat(MIN_HANDLE_LEN)).is_ok());
        assert!(validate_push_handle(&"0123456789abcdef".repeat(4)).is_ok()); // 64 chars
        assert!(validate_push_handle(&"f".repeat(MAX_HANDLE_LEN)).is_ok());
    }

    #[test]
    fn validate_push_handle_rejects_bad_input() {
        assert!(
            validate_push_handle(&"a".repeat(MIN_HANDLE_LEN - 1)).is_err(),
            "too short"
        );
        assert!(
            validate_push_handle(&"a".repeat(MAX_HANDLE_LEN + 1)).is_err(),
            "too long"
        );
        assert!(
            validate_push_handle(&"A".repeat(MIN_HANDLE_LEN)).is_err(),
            "uppercase hex rejected"
        );
        assert!(
            validate_push_handle(&"g".repeat(MIN_HANDLE_LEN)).is_err(),
            "non-hex char rejected"
        );
    }

    #[test]
    fn validate_device_name_accepts_valid_names() {
        assert!(validate_device_name("phone").is_ok());
        assert!(validate_device_name("My Phone_2.local-01").is_ok());
        assert!(validate_device_name(&"a".repeat(MAX_NAME_LEN)).is_ok());
    }

    #[test]
    fn validate_device_name_rejects_bad_input() {
        assert!(validate_device_name("").is_err(), "empty rejected");
        assert!(
            validate_device_name(&"a".repeat(MAX_NAME_LEN + 1)).is_err(),
            "too long"
        );
        assert!(
            validate_device_name("evil\x1b[2Jname").is_err(),
            "control chars rejected"
        );
        assert!(
            validate_device_name("name/with/slash").is_err(),
            "path-ish characters rejected"
        );
    }
}
