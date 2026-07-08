//! config — server configuration with precedence resolution.
//!
//! ## Precedence (highest → lowest)
//!
//! 1. CLI flags (`--bind`)
//! 2. Environment variables (`MUXRD_BIND`)
//! 3. Config file (`$DATA_DIR/muxrd/config.toml`)
//! 4. Hard-coded defaults (`127.0.0.1:50051`)
//!
//! ## Config file format (TOML)
//!
//! ```toml
//! # muxrd config — edit to override defaults.
//! bind_addr = "127.0.0.1:50051"
//! cert_dir  = "/home/user/.local/share/zellij/muxrd"
//! log_path  = "/home/user/.local/share/zellij/muxrd/muxrd.log"
//! ```
//!
//! All fields are optional; missing fields fall back to defaults.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default bind address used when nothing else overrides it.
pub const DEFAULT_BIND: &str = "127.0.0.1:50051";

/// The raw on-disk config (all fields optional; missing → use defaults).
#[derive(Debug, Default, Deserialize, Serialize)]
struct FileConfig {
    bind_addr: Option<String>,
    cert_dir: Option<String>,
    log_path: Option<String>,
}

/// The fully-resolved effective configuration.
///
/// Every field is guaranteed to be populated (via the precedence chain).
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// The address + port the server will bind to.
    pub bind_addr: String,
    /// Directory containing `server.crt` and `server.key`.
    pub cert_dir: PathBuf,
    /// Path where the server writes its log output (used in daemon mode).
    pub log_path: PathBuf,
    /// Path to the config file that was read (or would be written).
    pub config_file: PathBuf,
}

impl EffectiveConfig {
    /// Human-readable summary, one field per line.
    pub fn display(&self) -> String {
        format!(
            "bind_addr  = {}\n\
             cert_dir   = {}\n\
             log_path   = {}\n\
             config_file= {}",
            self.bind_addr,
            self.cert_dir.display(),
            self.log_path.display(),
            self.config_file.display(),
        )
    }
}

/// Returns the path to the config file inside the muxrd data dir.
pub fn config_file_path() -> Result<PathBuf> {
    let data_dir = data_dir()?;
    Ok(data_dir.join("config.toml"))
}

/// Returns (and creates) the data directory: `$XDG_DATA_HOME/zellij/muxrd/`.
///
/// On Unix the directory is restricted to mode `0700` (owner-only).  This is the
/// primary access control for the control socket (`control.sock`) and the
/// session pidfile that live inside it: there is no per-message auth on the
/// control socket, so a `0700` data dir is what prevents other local users from
/// connecting to it and issuing `Shutdown` (review Major D).
pub fn data_dir() -> Result<PathBuf> {
    let base = zellij_utils::consts::ZELLIJ_PROJ_DIR.data_dir();
    let dir = base.join("muxrd");
    std::fs::create_dir_all(&dir).with_context(|| format!("create data dir {}", dir.display()))?;

    // Restrict directory permissions on Unix (matches `tls.rs::cert_dir`).  The
    // 0700 dir is the PRIMARY access control for the un-authenticated control
    // socket (see the doc comment), so a failure here is a real security
    // regression and must NOT be silently discarded (review round-2 minor):
    // surface it loudly (warn) and propagate the error so `init`/`start` fail
    // rather than running with a world-accessible control socket.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&dir, perms).map_err(|e| {
            log::warn!(
                "config: FAILED to chmod 0700 the data dir {} ({e}); the control \
                 socket access guarantee is NOT in place — refusing to continue",
                dir.display()
            );
            anyhow::anyhow!(
                "failed to set 0700 permissions on data dir {}: {e}",
                dir.display()
            )
        })?;
    }

    Ok(dir)
}

/// The bundled bar-less default layout used for app-created sessions, baked into
/// the binary so the server is self-contained (no external file to deploy).
const DEFAULT_SESSION_LAYOUT_KDL: &str = include_str!("../assets/muxr-default.kdl");

/// File name for the materialised default layout under `<data_dir>/layouts/`.
const DEFAULT_SESSION_LAYOUT_FILE: &str = "muxr-default.kdl";

/// Materialise the bundled bar-less default layout to disk and return its
/// absolute path, for `CreateSession` to pass to `zellij --layout`.
///
/// The mobile client renders tab/pane controls itself, so sessions it creates
/// should not show zellij's tab-bar/status-bar. The built-in zellij default
/// layout declares those bar plugins; this layout (an empty
/// `default_tab_template`) does not, so every tab — including ones created at
/// runtime — opens bar-less.
///
/// Idempotent: the file is (over)written every call so a server upgrade always
/// ships the current layout. The returned path is a fixed, server-controlled
/// location derived from [`data_dir`] — never client input — so passing it to
/// `--layout` as an absolute path is safe (it is exempt from the client-layout
/// name allowlist enforced in `grpc::session_ops`).
pub fn ensure_default_session_layout() -> Result<PathBuf> {
    let dir = data_dir()?.join("layouts");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create layouts dir {}", dir.display()))?;
    let path = dir.join(DEFAULT_SESSION_LAYOUT_FILE);
    std::fs::write(&path, DEFAULT_SESSION_LAYOUT_KDL)
        .with_context(|| format!("write default layout {}", path.display()))?;
    Ok(path)
}

// ─── Cert source resolution ───────────────────────────────────────────────────

/// The resolved TLS / transport mode for the server.
///
/// Precedence (highest → lowest):
/// 1. h2c (`--insecure-h2c` / `MUXRD_H2C`)
/// 2. External cert (`--tls-cert` + `--tls-key` / `MUXRD_TLS_CERT` + `MUXRD_TLS_KEY`)
/// 3. Self-signed (default — auto-generated in the data dir)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertSource {
    /// Auto-generate (or reuse) the self-signed cert in the data dir.
    SelfSigned,
    /// Serve TLS using a caller-supplied cert + key PEM pair.
    External {
        /// Absolute path to the certificate PEM file.
        cert: PathBuf,
        /// Absolute path to the private key PEM file.
        key: PathBuf,
    },
    /// Serve plaintext HTTP/2 (h2c) — MUST sit behind a TLS-terminating proxy.
    H2c,
}

impl CertSource {
    /// Return the lightweight [`CertMode`] tag for this source (used in status
    /// reporting and serialisation; see S4/control.rs).
    pub fn mode(&self) -> CertMode {
        match self {
            CertSource::SelfSigned => CertMode::SelfSigned,
            CertSource::External { .. } => CertMode::External,
            CertSource::H2c => CertMode::H2c,
        }
    }
}

/// Lightweight serialisable tag mirroring [`CertSource`].
///
/// Used in `StatusInfo` (S4) and any other place that needs to log or
/// serialise the cert mode without carrying the full file paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertMode {
    SelfSigned,
    External,
    H2c,
}

impl From<&CertSource> for CertMode {
    fn from(src: &CertSource) -> Self {
        src.mode()
    }
}

impl From<CertSource> for CertMode {
    fn from(src: CertSource) -> Self {
        src.mode()
    }
}

/// Resolve the cert source from CLI arguments and environment variables,
/// applying the project-standard precedence chain (CLI > env > default).
///
/// ## Precedence
/// h2c  >  external (cert + key)  >  self-signed
///
/// ## Env var fallbacks
/// - `MUXRD_TLS_CERT` — path to the external cert PEM
/// - `MUXRD_TLS_KEY`  — path to the external key PEM
/// - `MUXRD_H2C`      — truthy (non-empty and not "0") → h2c mode
///
/// ## Validation errors
/// - Exactly one of `--tls-cert` / `--tls-key` given → error (both required).
/// - `--insecure-h2c` combined with `--tls-cert` / `--tls-key` → error.
pub fn resolve_cert_source(
    cli_cert: Option<PathBuf>,
    cli_key: Option<PathBuf>,
    cli_h2c: bool,
) -> anyhow::Result<CertSource> {
    // ── Apply env fallbacks (CLI > env) ──────────────────────────────────────
    let cert = cli_cert.or_else(|| {
        std::env::var("MUXRD_TLS_CERT")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });
    let key = cli_key.or_else(|| {
        std::env::var("MUXRD_TLS_KEY")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });
    let h2c = cli_h2c || {
        std::env::var("MUXRD_H2C")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0")
    };

    // ── Mutual-exclusion guard ────────────────────────────────────────────────
    if h2c && (cert.is_some() || key.is_some()) {
        anyhow::bail!(
            "--insecure-h2c serves no TLS; remove --tls-cert/--tls-key \
             (or MUXRD_TLS_CERT/MUXRD_TLS_KEY) when using h2c mode"
        );
    }

    // ── h2c wins ─────────────────────────────────────────────────────────────
    if h2c {
        return Ok(CertSource::H2c);
    }

    // ── External cert ─────────────────────────────────────────────────────────
    match (cert, key) {
        (Some(cert_path), Some(key_path)) => Ok(CertSource::External {
            cert: cert_path,
            key: key_path,
        }),
        (Some(_), None) => anyhow::bail!(
            "--tls-cert requires --tls-key (or MUXRD_TLS_KEY); \
             both paths must be provided together"
        ),
        (None, Some(_)) => anyhow::bail!(
            "--tls-key requires --tls-cert (or MUXRD_TLS_CERT); \
             both paths must be provided together"
        ),
        // ── Default: self-signed ─────────────────────────────────────────────
        (None, None) => Ok(CertSource::SelfSigned),
    }
}

// ─── H2c bind-safety guard ───────────────────────────────────────────────────

/// Enforce that h2c (plaintext HTTP/2) is not bound on a publicly-reachable
/// address without an explicit operator acknowledgement.
///
/// # Rules
/// - Non-h2c modes: always allowed (returns `Ok(())`).
/// - H2c on a **loopback** address (`127.0.0.1` / `[::1]`): always allowed.
/// - H2c on a **non-loopback** address:
///   - `allow_public = true` (set via `--i-know-this-is-behind-a-proxy` or
///     `MUXRD_H2C_ALLOW_PUBLIC`): allowed (emits a `warn!`).
///   - `allow_public = false`: **hard-fail** with a clear error.
///
/// This is a pure function with no I/O side-effects, extracted for unit testability.
pub fn check_h2c_bind_safety(
    cert_source: &CertSource,
    addr: std::net::SocketAddr,
    allow_public: bool,
) -> anyhow::Result<()> {
    if *cert_source != CertSource::H2c {
        return Ok(());
    }
    if addr.ip().is_loopback() {
        // Loopback h2c is always safe — nothing on the network can reach it.
        return Ok(());
    }
    // Non-loopback h2c.
    if allow_public {
        // Operator has explicitly acknowledged the risk.
        log::warn!(
            "h2c: non-loopback bind on {} acknowledged via \
             --i-know-this-is-behind-a-proxy / MUXRD_H2C_ALLOW_PUBLIC — \
             ensure a TLS-terminating proxy is in front of this port",
            addr
        );
        Ok(())
    } else {
        anyhow::bail!(
            "refusing to bind h2c (plaintext gRPC) on non-loopback address {addr}. \
             Plaintext gRPC on a public/LAN address exposes API tokens and terminal \
             output in the clear. If you are running behind a TLS-terminating reverse \
             proxy (e.g. Traefik + Let's Encrypt, Cloudflare), re-run with \
             --i-know-this-is-behind-a-proxy (or set \
             MUXRD_H2C_ALLOW_PUBLIC=1) to acknowledge the risk. \
             To serve TLS directly, omit --insecure-h2c."
        )
    }
}

/// Load or create the config file, then apply env + CLI overrides.
///
/// `bind_override` — set when the user passes `--bind` on the CLI.
pub fn resolve(bind_override: Option<&str>) -> Result<EffectiveConfig> {
    let config_file = config_file_path()?;
    let data_dir = data_dir()?;

    // ── Read file (tolerant — missing file → defaults; parse error → warn) ──
    let file_cfg: FileConfig = if config_file.exists() {
        let raw = std::fs::read_to_string(&config_file)
            .with_context(|| format!("read {}", config_file.display()))?;
        toml::from_str(&raw).unwrap_or_else(|e| {
            log::warn!(
                "config: failed to parse {}: {e} — using defaults",
                config_file.display()
            );
            FileConfig::default()
        })
    } else {
        FileConfig::default()
    };

    // ── Precedence chain ─────────────────────────────────────────────────────

    // bind_addr: CLI flag > MUXRD_BIND env > config file > default
    let bind_addr = bind_override
        .map(|s| s.to_owned())
        .or_else(|| std::env::var("MUXRD_BIND").ok())
        .or(file_cfg.bind_addr)
        .unwrap_or_else(|| DEFAULT_BIND.to_owned());

    // cert_dir: config file > default (data_dir)
    let cert_dir = file_cfg
        .cert_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.clone());

    // log_path: config file > default (data_dir/muxrd.log)
    let log_path = file_cfg
        .log_path
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("muxrd.log"));

    Ok(EffectiveConfig {
        bind_addr,
        cert_dir,
        log_path,
        config_file,
    })
}

/// Persist `bind_addr` into the config file (creating it from the template if
/// absent), preserving all other fields.
///
/// Used by `muxrctl`'s Config screen so users can update the bind address
/// without manually editing TOML.  The write is done atomically: read the
/// current config → update `bind_addr` → serialize → write to a temp file in
/// the same directory → `chmod 0600` the temp file (Unix) → `rename` the temp
/// file over the real config (POSIX-atomic on the same filesystem). This ensures
/// no crash mid-write leaves a partial or world-readable file, and no other
/// fields are silently lost.
pub fn set_bind_addr(bind_addr: &str) -> Result<()> {
    // Ensure the file exists (idempotent; writes a template when absent).
    let path = ensure_config_file()?;

    // Read the current content, falling back to defaults on parse errors so a
    // corrupted file doesn't block the update.
    let file_cfg: FileConfig = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("set_bind_addr: read {}", path.display()))?;
        toml::from_str(&raw).unwrap_or_else(|e| {
            log::warn!(
                "config: set_bind_addr: failed to parse {}: {e} — overwriting with defaults + new bind_addr",
                path.display()
            );
            FileConfig::default()
        })
    } else {
        FileConfig::default()
    };

    // Update the bind_addr field and serialise.
    let updated = FileConfig {
        bind_addr: Some(bind_addr.to_owned()),
        ..file_cfg
    };
    let toml_str = toml::to_string_pretty(&updated).context("set_bind_addr: serialize config")?;

    // Write to a temporary file in the same directory as the target.
    let parent = path
        .parent()
        .context("config file has no parent directory")?;
    let temp_path = parent.join(".config.toml.tmp");
    std::fs::write(&temp_path, &toml_str)
        .with_context(|| format!("set_bind_addr: write temp file {}", temp_path.display()))?;

    // Restrict permissions on the temp file before renaming (Unix).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&temp_path, perms).with_context(|| {
            format!(
                "set_bind_addr: chmod 0600 temp file {}",
                temp_path.display()
            )
        })?;
    }

    // Atomically rename temp file over the real config (POSIX-atomic on same filesystem).
    std::fs::rename(&temp_path, &path).with_context(|| {
        format!(
            "set_bind_addr: rename {} to {}",
            temp_path.display(),
            path.display()
        )
    })?;

    log::info!(
        "config: set bind_addr = {bind_addr:?} in {}",
        path.display()
    );
    Ok(())
}

/// Ensure the config file exists.  If it doesn't, write a commented template
/// with the current defaults so the user can inspect and edit it.
pub fn ensure_config_file() -> Result<PathBuf> {
    let path = config_file_path()?;
    if !path.exists() {
        let data_dir = data_dir()?;
        let template = format!(
            "# muxrd configuration file\n\
             # All fields are optional; missing fields use built-in defaults.\n\
             #\n\
             # bind_addr = \"{DEFAULT_BIND}\"\n\
             # cert_dir  = \"{}\"\n\
             # log_path  = \"{}\"\n",
            data_dir.display(),
            data_dir.join("muxrd.log").display(),
        );
        std::fs::write(&path, template)
            .with_context(|| format!("write config file {}", path.display()))?;
        log::info!("config: created template at {}", path.display());
    }
    Ok(path)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise all config-file tests so they don't race on the shared file.
    static CONFIG_FILE_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that restores the original config file content on drop.
    /// Ensures restoration runs even if the test panics.
    struct ConfigRestoreGuard {
        path: PathBuf,
        original: Option<String>,
    }

    impl Drop for ConfigRestoreGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(content) => {
                    let _ = std::fs::write(&self.path, content);
                }
                None => {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }
    }

    /// Round-trip `set_bind_addr` → `resolve(None)`.
    ///
    /// This test writes to the real data-dir config file.  A mutex serialises it
    /// with other config tests, and a RAII guard restores the original content
    /// even if an assertion panics.
    #[test]
    fn set_bind_addr_round_trips() {
        let _guard = CONFIG_FILE_LOCK.lock().unwrap();

        // Save whatever is currently on disk (may not exist yet) and construct
        // the restore guard to ensure cleanup runs even on panic.
        let path = config_file_path().expect("config_file_path");
        let original = std::fs::read_to_string(&path).ok();
        let _restore = ConfigRestoreGuard {
            path: path.clone(),
            original,
        };

        // Act: write a distinctive bind address.
        let test_addr = "0.0.0.0:9999";
        set_bind_addr(test_addr).expect("set_bind_addr");

        // Assert: resolve(None) should return the value we wrote.
        // (No env override, no CLI override — file wins over default.)
        let cfg = resolve(None).expect("resolve");
        assert_eq!(
            cfg.bind_addr, test_addr,
            "resolved bind_addr should match what was written"
        );
        // Restore guard runs automatically when this scope exits.
    }

    /// The bundled default layout must be materialised on disk, live under the
    /// data dir's `layouts/`, and be BAR-LESS (no tab-bar/status-bar plugins).
    #[test]
    fn default_session_layout_is_written_and_barless() {
        let path = ensure_default_session_layout().expect("ensure default layout");
        assert!(path.exists(), "layout file should exist at {path:?}");
        assert!(
            path.ends_with("layouts/muxr-default.kdl"),
            "unexpected layout path {path:?}"
        );
        let body = std::fs::read_to_string(&path).expect("read layout");
        assert!(
            body.contains("default_tab_template"),
            "layout should define a default_tab_template"
        );
        // The bars are `plugin location=...` panes; a bar-less layout declares no
        // plugin panes at all. Check non-comment lines so the explanatory header
        // (which mentions tab-bar/status-bar) doesn't trip the assertion.
        let has_plugin = body
            .lines()
            .map(|l| l.trim_start())
            .filter(|l| !l.starts_with("//"))
            .any(|l| l.contains("plugin"));
        assert!(
            !has_plugin,
            "bar-less layout must not declare any plugin (tab-bar/status-bar) panes"
        );
    }

    // ── resolve_cert_source tests ─────────────────────────────────────────────

    /// Serialise cert-source env-var tests so they don't race on shared env state.
    static CERT_SOURCE_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Helper: temporarily set env vars and restore them on drop.
    struct EnvGuard {
        vars: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn set(pairs: &[(&str, &str)]) -> Self {
            let mut vars = Vec::new();
            for (k, v) in pairs {
                let old = std::env::var(k).ok();
                // SAFETY: tests are serialised via CERT_SOURCE_ENV_LOCK so no
                // concurrent threads are reading the environment while we mutate it.
                unsafe { std::env::set_var(k, v) };
                vars.push((k.to_string(), old));
            }
            EnvGuard { vars }
        }
        fn remove(keys: &[&str]) -> Self {
            let mut vars = Vec::new();
            for k in keys {
                let old = std::env::var(k).ok();
                // SAFETY: same serialisation guarantee as set().
                unsafe { std::env::remove_var(k) };
                vars.push((k.to_string(), old));
            }
            EnvGuard { vars }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.vars {
                match v {
                    // SAFETY: the mutex held by the test body is still held during
                    // drop (Rust drops at end of scope before mutex is released).
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    #[test]
    fn cert_source_default_is_self_signed() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::remove(&["MUXRD_TLS_CERT", "MUXRD_TLS_KEY", "MUXRD_H2C"]);

        let src = resolve_cert_source(None, None, false).expect("should succeed");
        assert_eq!(src, CertSource::SelfSigned);
        assert_eq!(src.mode(), CertMode::SelfSigned);
    }

    #[test]
    fn cert_source_h2c_flag_wins_over_all() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::remove(&["MUXRD_TLS_CERT", "MUXRD_TLS_KEY", "MUXRD_H2C"]);

        // h2c flag alone → H2c
        let src = resolve_cert_source(None, None, true).expect("should succeed");
        assert_eq!(src, CertSource::H2c);
        assert_eq!(src.mode(), CertMode::H2c);
    }

    #[test]
    fn cert_source_h2c_env_truthy() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set(&[("MUXRD_H2C", "1")]);
        let _key_guard = EnvGuard::remove(&["MUXRD_TLS_CERT", "MUXRD_TLS_KEY"]);

        let src = resolve_cert_source(None, None, false).expect("should succeed");
        assert_eq!(src, CertSource::H2c);
    }

    #[test]
    fn cert_source_h2c_env_zero_is_falsy() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set(&[("MUXRD_H2C", "0")]);
        let _key_guard = EnvGuard::remove(&["MUXRD_TLS_CERT", "MUXRD_TLS_KEY"]);

        let src = resolve_cert_source(None, None, false).expect("should succeed");
        assert_eq!(src, CertSource::SelfSigned, "H2C=0 should not activate h2c");
    }

    #[test]
    fn cert_source_h2c_env_empty_is_falsy() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set(&[("MUXRD_H2C", "")]);
        let _key_guard = EnvGuard::remove(&["MUXRD_TLS_CERT", "MUXRD_TLS_KEY"]);

        let src = resolve_cert_source(None, None, false).expect("should succeed");
        assert_eq!(
            src,
            CertSource::SelfSigned,
            "H2C=<empty> should not activate h2c"
        );
    }

    #[test]
    fn cert_source_external_requires_both_cert_and_key() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::remove(&["MUXRD_TLS_CERT", "MUXRD_TLS_KEY", "MUXRD_H2C"]);

        // Only cert → error
        let err = resolve_cert_source(Some("/tmp/cert.pem".into()), None, false)
            .expect_err("should fail with only cert");
        assert!(
            err.to_string().contains("--tls-cert requires --tls-key"),
            "unexpected error: {err}"
        );

        // Only key → error
        let err = resolve_cert_source(None, Some("/tmp/key.pem".into()), false)
            .expect_err("should fail with only key");
        assert!(
            err.to_string().contains("--tls-key requires --tls-cert"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn cert_source_external_both_paths_succeeds() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::remove(&["MUXRD_TLS_CERT", "MUXRD_TLS_KEY", "MUXRD_H2C"]);

        let cert: PathBuf = "/etc/ssl/cert.pem".into();
        let key: PathBuf = "/etc/ssl/key.pem".into();
        let src = resolve_cert_source(Some(cert.clone()), Some(key.clone()), false)
            .expect("should succeed");
        assert_eq!(
            src,
            CertSource::External {
                cert: cert.clone(),
                key: key.clone()
            }
        );
        assert_eq!(src.mode(), CertMode::External);
    }

    #[test]
    fn cert_source_h2c_with_cert_or_key_is_error() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::remove(&["MUXRD_TLS_CERT", "MUXRD_TLS_KEY", "MUXRD_H2C"]);

        // h2c + cert → error
        let err =
            resolve_cert_source(Some("/tmp/cert.pem".into()), None, true).expect_err("should fail");
        assert!(
            err.to_string().contains("--insecure-h2c serves no TLS"),
            "unexpected error: {err}"
        );

        // h2c + key → error
        let err =
            resolve_cert_source(None, Some("/tmp/key.pem".into()), true).expect_err("should fail");
        assert!(
            err.to_string().contains("--insecure-h2c serves no TLS"),
            "unexpected error: {err}"
        );

        // h2c + cert + key → error
        let err = resolve_cert_source(
            Some("/tmp/cert.pem".into()),
            Some("/tmp/key.pem".into()),
            true,
        )
        .expect_err("should fail");
        assert!(
            err.to_string().contains("--insecure-h2c serves no TLS"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn cert_source_env_fallback_external() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set(&[
            ("MUXRD_TLS_CERT", "/env/cert.pem"),
            ("MUXRD_TLS_KEY", "/env/key.pem"),
        ]);
        let _h2c_guard = EnvGuard::remove(&["MUXRD_H2C"]);

        // No CLI args → should pick up from env
        let src = resolve_cert_source(None, None, false).expect("should succeed");
        assert_eq!(
            src,
            CertSource::External {
                cert: "/env/cert.pem".into(),
                key: "/env/key.pem".into(),
            },
            "env fallback should produce External cert source"
        );
    }

    #[test]
    fn cert_source_cli_overrides_env() {
        let _lock = CERT_SOURCE_ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set(&[
            ("MUXRD_TLS_CERT", "/env/cert.pem"),
            ("MUXRD_TLS_KEY", "/env/key.pem"),
        ]);
        let _h2c_guard = EnvGuard::remove(&["MUXRD_H2C"]);

        // CLI takes precedence over env
        let src = resolve_cert_source(
            Some("/cli/cert.pem".into()),
            Some("/cli/key.pem".into()),
            false,
        )
        .expect("should succeed");
        assert_eq!(
            src,
            CertSource::External {
                cert: "/cli/cert.pem".into(),
                key: "/cli/key.pem".into(),
            },
            "CLI values should override env"
        );
    }

    #[test]
    fn cert_mode_from_cert_source() {
        assert_eq!(CertMode::from(CertSource::SelfSigned), CertMode::SelfSigned);
        assert_eq!(CertMode::from(CertSource::H2c), CertMode::H2c);
        assert_eq!(
            CertMode::from(CertSource::External {
                cert: "/c.pem".into(),
                key: "/k.pem".into()
            }),
            CertMode::External
        );
    }

    // ── check_h2c_bind_safety tests ───────────────────────────────────────────

    /// Non-h2c cert sources are always allowed regardless of address or ack.
    #[test]
    fn h2c_safety_non_h2c_always_ok() {
        let loopback: std::net::SocketAddr = "127.0.0.1:50051".parse().unwrap();
        let public: std::net::SocketAddr = "0.0.0.0:50051".parse().unwrap();
        let lan: std::net::SocketAddr = "192.168.1.100:50051".parse().unwrap();

        for addr in [loopback, public, lan] {
            assert!(
                check_h2c_bind_safety(&CertSource::SelfSigned, addr, false).is_ok(),
                "SelfSigned on {addr} should always be ok"
            );
            assert!(
                check_h2c_bind_safety(
                    &CertSource::External {
                        cert: "/c.pem".into(),
                        key: "/k.pem".into()
                    },
                    addr,
                    false
                )
                .is_ok(),
                "External on {addr} should always be ok"
            );
        }
    }

    /// H2c on loopback is always allowed, even without the ack flag.
    #[test]
    fn h2c_safety_loopback_always_allowed() {
        let lo4: std::net::SocketAddr = "127.0.0.1:50051".parse().unwrap();
        let lo6: std::net::SocketAddr = "[::1]:50051".parse().unwrap();

        assert!(
            check_h2c_bind_safety(&CertSource::H2c, lo4, false).is_ok(),
            "h2c on 127.0.0.1 should be allowed without ack"
        );
        assert!(
            check_h2c_bind_safety(&CertSource::H2c, lo4, true).is_ok(),
            "h2c on 127.0.0.1 should be allowed with ack"
        );
        assert!(
            check_h2c_bind_safety(&CertSource::H2c, lo6, false).is_ok(),
            "h2c on [::1] should be allowed without ack"
        );
    }

    /// H2c on a non-loopback address is denied when the ack flag is not set.
    #[test]
    fn h2c_safety_non_loopback_denied_without_ack() {
        let addrs: Vec<std::net::SocketAddr> = vec![
            "0.0.0.0:50051".parse().unwrap(),
            "192.168.1.5:50051".parse().unwrap(),
            "10.0.0.1:50051".parse().unwrap(),
            "[::]:50051".parse().unwrap(),
        ];
        for addr in addrs {
            let err = check_h2c_bind_safety(&CertSource::H2c, addr, false)
                .expect_err(&format!("h2c on {addr} without ack should fail"));
            assert!(
                err.to_string().contains("--i-know-this-is-behind-a-proxy"),
                "error message should mention the ack flag, got: {err}"
            );
        }
    }

    /// H2c on a non-loopback address is allowed when the ack flag is set.
    #[test]
    fn h2c_safety_non_loopback_allowed_with_ack() {
        let addrs: Vec<std::net::SocketAddr> = vec![
            "0.0.0.0:50051".parse().unwrap(),
            "192.168.1.5:50051".parse().unwrap(),
            "[::]:50051".parse().unwrap(),
        ];
        for addr in addrs {
            assert!(
                check_h2c_bind_safety(&CertSource::H2c, addr, true).is_ok(),
                "h2c on {addr} with ack should be allowed"
            );
        }
    }
}
