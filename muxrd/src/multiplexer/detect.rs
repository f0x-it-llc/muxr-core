//! Backend auto-detection — probes which terminal-multiplexer backends are
//! usable at startup.
//!
//! All probe functions are **synchronous / blocking**: they run before (or
//! outside) the Tokio runtime and must never be called from an async context.
//!
//! [`detect_backends`] is the primary entry point consumed by `bin/muxrd.rs`
//! (T03 wires it into `cmd_start`). [`probe_zellij`] is also called directly
//! by the startup version check in `bin/muxrd.rs` so the two code paths share
//! the same algorithm without duplication.

use anyhow::{Result, bail};

use crate::cli::BackendKind;

// ── Constants ──────────────────────────────────────────────────────────────────

/// Environment variable that bypasses the zellij version-mismatch check.
/// The same variable is referenced in `bin/muxrd.rs` for startup-warning
/// logging; the actual skip logic lives here in the shared probe.
const SKIP_VERSION_CHECK_ENV: &str = "MUXRD_SKIP_VERSION_CHECK";

/// Timeout for the `zellij --version` subprocess.
const VERSION_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ── Zellij probe ────────────────────────────────────────────────────────────────

/// Probe whether the zellij backend is usable.
///
/// Returns `true` when:
/// - `MUXRD_SKIP_VERSION_CHECK` is set (truthy, non-"0") **and** the
///   `zellij` binary is present on PATH — version is not checked.
/// - Otherwise: the `zellij` binary is found on PATH, `zellij --version`
///   completes within 5 s, and its version matches the linked
///   `zellij_utils::consts::VERSION` ("0.44.3").
///
/// All failures are logged at `debug` level and reduce to `false`. Never
/// panics or propagates errors — callers receive a boolean verdict only.
pub fn probe_zellij() -> bool {
    let skip = std::env::var(SKIP_VERSION_CHECK_ENV).is_ok_and(|v| !v.is_empty() && v != "0");

    if skip {
        log::debug!(
            "probe_zellij: {SKIP_VERSION_CHECK_ENV} set — skip version check; \
             probe passes if the zellij binary is present"
        );
        return crate::actions::which_zellij().is_ok();
    }

    let bin = match crate::actions::which_zellij() {
        Ok(b) => b,
        Err(e) => {
            log::debug!("probe_zellij: binary not found: {e:#}");
            return false;
        }
    };

    let linked = zellij_utils::consts::VERSION;
    let bin_clone = bin.clone();
    let (tx, rx) = std::sync::mpsc::channel::<anyhow::Result<std::process::Output>>();

    if std::thread::Builder::new()
        .name("zellij-probe-version".into())
        .spawn(move || {
            let result = std::process::Command::new(&bin_clone)
                .arg("--version")
                .output()
                .map_err(|e| anyhow::anyhow!("run '{} --version': {e}", bin_clone.display()));
            let _ = tx.send(result);
        })
        .is_err()
    {
        log::debug!("probe_zellij: failed to spawn version-check thread");
        return false;
    }

    let output = match rx.recv_timeout(VERSION_CHECK_TIMEOUT) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            log::debug!("probe_zellij: '{} --version' failed: {e:#}", bin.display());
            return false;
        }
        Err(_) => {
            log::debug!(
                "probe_zellij: '{} --version' timed out or thread exited unexpectedly",
                bin.display()
            );
            return false;
        }
    };

    if !output.status.success() {
        log::debug!(
            "probe_zellij: '{} --version' exited with {}",
            bin.display(),
            output.status
        );
        return false;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let installed = parse_zellij_version(&stdout);

    if installed.is_empty() {
        log::debug!(
            "probe_zellij: could not parse a version string from output: {:?}",
            stdout.trim()
        );
        return false;
    }

    if installed != linked {
        log::debug!("probe_zellij: version mismatch — installed {installed}, linked {linked}");
        return false;
    }

    log::debug!(
        "probe_zellij: OK — installed {installed} matches linked {linked} ({})",
        bin.display()
    );
    true
}

/// Parse the version string out of `zellij --version` output.
///
/// `"zellij 0.44.3\n"` → `"0.44.3"`. Returns `""` when unparseable.
///
/// Split out as a private pure function so the unit test exercises it directly
/// without needing a live process.
fn parse_zellij_version(output: &str) -> String {
    output
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim()
        .to_string()
}

// ── Herdr probe ─────────────────────────────────────────────────────────────────

/// Probe whether the herdr backend is usable.
///
/// Checks:
/// 1. `HerdrSocketPaths::resolve().api` (the JSON-API socket) exists on disk.
/// 2. A `workspace.list` call over that socket completes successfully.
///
/// **Blocking** — must not be called from an async context; this runs at
/// server startup before the Tokio runtime is constructed.
///
/// Returns `true` if herdr is reachable, `false` (with `debug` log) otherwise.
/// Temporary registries are created for the probe and discarded immediately.
pub fn probe_herdr() -> bool {
    use std::sync::Arc;

    use super::herdr::control::HerdrControl;
    use super::herdr::paths::HerdrSocketPaths;
    use super::herdr::registry::{HerdrPaneRegistry, HerdrTabRegistry};

    let paths = HerdrSocketPaths::resolve();

    if !paths.api.exists() {
        log::debug!(
            "probe_herdr: JSON-API socket {} not present — herdr not running",
            paths.api.display()
        );
        return false;
    }

    // Build a throw-away control client with ephemeral registries (the probe
    // only needs one round-trip; it discards registries immediately after).
    let control = HerdrControl::new(
        paths.api.clone(),
        Arc::new(HerdrPaneRegistry::new()),
        Arc::new(HerdrTabRegistry::new()),
    );

    match control.list_workspaces() {
        Ok(_) => {
            log::debug!("probe_herdr: OK ({})", paths.api.display());
            true
        }
        Err(e) => {
            log::debug!("probe_herdr: workspace.list failed: {e:#}");
            false
        }
    }
}

// ── Backend selection ─────────────────────────────────────────────────────────

/// Determine which backends are usable at startup.
///
/// ### Behaviour
///
/// - `Some(kind)` — probe only the requested backend; return `vec![kind]` if
///   its probe passes, or `Err("requested backend {kind} not available: …")`.
/// - `None` — probe all backends in declaration order (zellij first, then
///   herdr); return the subset that passes.  If **none** pass, returns
///   `Err("No usable backend: install zellij 0.44.3 or start herdr")`.
///
/// ### Thread safety
///
/// All probes are blocking and self-contained. `detect_backends` may be called
/// from any thread; it spawns at most one short-lived helper thread (for the
/// `zellij --version` subprocess) which terminates before returning.
pub fn detect_backends(override_kind: Option<BackendKind>) -> Result<Vec<BackendKind>> {
    match override_kind {
        Some(kind) => {
            let ok = match kind {
                BackendKind::Zellij => probe_zellij(),
                BackendKind::Herdr => probe_herdr(),
            };
            if ok {
                Ok(vec![kind])
            } else {
                bail!("requested backend {kind} not available: probe failed");
            }
        }
        None => {
            let mut available = Vec::new();
            if probe_zellij() {
                available.push(BackendKind::Zellij);
            }
            if probe_herdr() {
                available.push(BackendKind::Herdr);
            }
            if available.is_empty() {
                bail!("No usable backend: install zellij 0.44.3 or start herdr");
            }
            Ok(available)
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_zellij_version ─────────────────────────────────────────────────
    // (previously in bin/muxrd.rs, co-located here with the implementation)

    #[test]
    fn parse_version_extracts_semver_from_typical_output() {
        assert_eq!(parse_zellij_version("zellij 0.44.3\n"), "0.44.3");
        assert_eq!(parse_zellij_version("zellij 0.44.3"), "0.44.3");
    }

    #[test]
    fn parse_version_returns_empty_for_unparseable_output() {
        assert_eq!(parse_zellij_version(""), "");
        assert_eq!(parse_zellij_version("   "), "");
    }

    // ── detect_backends policy ───────────────────────────────────────────────
    // Tests that do not require a live socket.

    /// When a requested backend is unavailable, detect_backends returns Err
    /// whose message names the failing backend.
    ///
    /// Uses herdr since we can reliably ensure it is absent by pointing
    /// HERDR_SOCKET_PATH at a path that does not exist. Because env-var
    /// mutation is not safe in parallel-threaded tests, this test runs with
    /// the default herdr socket path; in any standard CI or dev environment
    /// herdr is not running so the socket is absent and the probe fails.
    ///
    /// If herdr IS running in the test environment the probe will succeed and
    /// the test will verify the Ok branch instead — both outcomes are correct.
    #[test]
    fn detect_some_herdr_error_message_names_backend() {
        let result = detect_backends(Some(BackendKind::Herdr));
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("herdr"),
                    "error message must name the failing backend; got: {msg}"
                );
                assert!(
                    msg.contains("not available"),
                    "error message must say 'not available'; got: {msg}"
                );
            }
            Ok(kinds) => {
                // herdr is actually running — probe passed; result must be [Herdr].
                assert_eq!(
                    kinds,
                    vec![BackendKind::Herdr],
                    "Ok result must be exactly [Herdr]"
                );
            }
        }
    }

    /// When a probe passes, detect_backends(Some(kind)) returns Ok([kind]).
    /// Only asserts if zellij is actually installed at the linked version.
    #[test]
    fn detect_some_zellij_returns_single_elem_vec_on_pass() {
        if probe_zellij() {
            let result = detect_backends(Some(BackendKind::Zellij));
            assert!(result.is_ok(), "probe passed but detect returned Err");
            assert_eq!(result.unwrap(), vec![BackendKind::Zellij]);
        }
        // If zellij is not at the right version/absent: nothing to assert.
    }
}
