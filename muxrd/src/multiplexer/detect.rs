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
//!
//! **Containment note:** `multiplexer::zellij` describes itself as the only
//! file in this module importing `zellij_utils`. This file is the one
//! deliberate exception: the supported-version table lives here and a test
//! ties its newest row to `zellij_utils::consts::VERSION`, the linked crate.
//! Which installed `zellij` binaries this server accepts is a startup concern
//! the zellij backend itself has no need to see.

use std::sync::OnceLock;

use anyhow::{Result, bail};

use crate::cli::BackendKind;

// ── Constants ──────────────────────────────────────────────────────────────────

/// Environment variable that bypasses the supported-zellij-version check.
/// The same variable is referenced in `bin/muxrd.rs` for startup-warning
/// logging; the actual skip logic lives here in the shared probe.
const SKIP_VERSION_CHECK_ENV: &str = "MUXRD_SKIP_VERSION_CHECK";

/// Timeout for the `zellij --version` subprocess.
const VERSION_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// ── Supported zellij versions ───────────────────────────────────────────────────

/// One supported zellij release line: every `major.minor.patch` release with
/// `patch >= min_patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedZellij {
    pub major: u32,
    pub minor: u32,
    /// Lowest patch release of the line that was tested.
    pub min_patch: u32,
}

/// The zellij versions this server drives, oldest line first. This table is
/// the only place that names them.
///
/// Policy: muxrd supports the current zellij release line and the newest one.
/// The linked `zellij-utils` crate is always the newest row (a test enforces
/// it); the older line is reached through compatibility code. A newer patch
/// release of a listed line is accepted without a change here. Adding a
/// version means bumping the `zellij-utils` pin, adding a row, and running the
/// two-version live check.
pub const SUPPORTED_ZELLIJ: &[SupportedZellij] = &[
    SupportedZellij {
        major: 0,
        minor: 45,
        min_patch: 1,
    },
    SupportedZellij {
        major: 0,
        minor: 46,
        min_patch: 0,
    },
];

/// Whether a zellij version is one this server drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZellijSupport {
    /// On a listed release line, at or above its lowest tested patch.
    Supported,
    /// A readable version that is not in [`SUPPORTED_ZELLIJ`].
    Unsupported,
    /// Not a `MAJOR.MINOR.PATCH` version.
    Unparseable,
}

/// Split `MAJOR.MINOR.PATCH` into numbers. Each part is decimal digits only.
/// A pre-release or build tag after the patch number (`-rc1`, `+build`) is
/// ignored; anything else after it (`0.46.0.1`, `0.46.0x`) is not a version.
fn parse_version_triple(version: &str) -> Option<(u32, u32, u32)> {
    fn number(digits: &str) -> Option<u32> {
        // `str::parse` alone would accept a leading `+`.
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }
    let mut parts = version.trim().splitn(3, '.');
    let major = number(parts.next()?)?;
    let minor = number(parts.next()?)?;
    let rest = parts.next()?;
    let (patch, tag) = match rest.find(|c: char| !c.is_ascii_digit()) {
        Some(end) => rest.split_at(end),
        None => (rest, ""),
    };
    if !(tag.is_empty() || tag.starts_with('-') || tag.starts_with('+')) {
        return None;
    }
    Some((major, minor, number(patch)?))
}

/// Decide whether `version` (as printed by `zellij --version`, without the
/// program name) is supported. Pure, so it is testable without a binary.
pub fn zellij_support(version: &str) -> ZellijSupport {
    let Some((major, minor, patch)) = parse_version_triple(version) else {
        return ZellijSupport::Unparseable;
    };
    let listed = SUPPORTED_ZELLIJ
        .iter()
        .any(|row| row.major == major && row.minor == minor && patch >= row.min_patch);
    if listed {
        ZellijSupport::Supported
    } else {
        ZellijSupport::Unsupported
    }
}

/// The supported set in words, built from [`SUPPORTED_ZELLIJ`], for log lines
/// and error messages: one entry per line, the lowest tested patch named when
/// it is not the line's first release.
pub fn supported_zellij_description() -> String {
    SUPPORTED_ZELLIJ
        .iter()
        .map(|row| {
            let SupportedZellij {
                major,
                minor,
                min_patch,
            } = *row;
            if min_patch == 0 {
                format!("{major}.{minor}.x")
            } else {
                format!("{major}.{minor}.{min_patch} or newer {major}.{minor}.x")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The version of the zellij binary the probe accepted, for the lifetime of
/// the process.
static DETECTED_ZELLIJ_VERSION: OnceLock<String> = OnceLock::new();

/// The installed zellij binary's version as recorded by [`probe_zellij`], or
/// `None` when no probe has read one. It describes the binary found at
/// startup, not each running zellij server.
pub fn detected_zellij_version() -> Option<&'static str> {
    DETECTED_ZELLIJ_VERSION.get().map(String::as_str)
}

// ── Zellij probe ────────────────────────────────────────────────────────────────

/// Probe whether the zellij backend is usable.
///
/// Returns `true` when the `zellij` binary is found on PATH and either:
/// - `zellij --version` completes within 5 s and reports a version in
///   [`SUPPORTED_ZELLIJ`]; or
/// - `MUXRD_SKIP_VERSION_CHECK` is set (truthy, non-"0"): the version is still
///   read when the binary answers, but it never fails the probe.
///
/// A version the probe accepts is recorded for [`detected_zellij_version`].
///
/// All failures are logged at `debug` level and reduce to `false`. Never
/// panics or propagates errors — callers receive a boolean verdict only.
pub fn probe_zellij() -> bool {
    let skip = std::env::var(SKIP_VERSION_CHECK_ENV).is_ok_and(|v| !v.is_empty() && v != "0");

    let bin = match crate::actions::which_zellij() {
        Ok(b) => b,
        Err(e) => {
            log::debug!("probe_zellij: binary not found: {e:#}");
            return false;
        }
    };

    let installed = read_zellij_version(&bin);
    let supported = supported_zellij_description();

    if skip {
        log::debug!(
            "probe_zellij: {SKIP_VERSION_CHECK_ENV} set — version not checked (installed: {}; \
             supported: {supported})",
            installed.as_deref().unwrap_or("unknown")
        );
        if let Some(version) = installed
            && zellij_support(&version) != ZellijSupport::Unparseable
        {
            let _ = DETECTED_ZELLIJ_VERSION.set(version);
        }
        return true;
    }

    let Some(installed) = installed else {
        return false;
    };
    match zellij_support(&installed) {
        ZellijSupport::Supported => {
            log::debug!(
                "probe_zellij: OK — installed {installed} is supported ({supported}) ({})",
                bin.display()
            );
            let _ = DETECTED_ZELLIJ_VERSION.set(installed);
            true
        }
        ZellijSupport::Unsupported => {
            log::debug!(
                "probe_zellij: installed {installed} is not supported (supported: {supported})"
            );
            false
        }
        ZellijSupport::Unparseable => {
            log::debug!(
                "probe_zellij: installed version {installed:?} is not a version number \
                 (supported: {supported})"
            );
            false
        }
    }
}

/// The version the installed zellij binary reports, when one is found on PATH
/// and answers. For error messages; the probe is the gate.
fn installed_zellij_version() -> Option<String> {
    let bin = crate::actions::which_zellij().ok()?;
    read_zellij_version(&bin)
}

/// Why the zellij backend is unusable, for a startup error: what was found,
/// the supported versions from [`SUPPORTED_ZELLIJ`], and the way to bypass
/// the check. Call it after [`probe_zellij`] returned `false`.
pub fn zellij_unavailable_reason() -> String {
    unavailable_reason(installed_zellij_version().as_deref())
}

fn unavailable_reason(installed: Option<&str>) -> String {
    let found = match installed {
        Some(version) if zellij_support(version) == ZellijSupport::Unparseable => {
            format!("the zellij binary reported {version:?}, which is not a version number")
        }
        Some(version) => format!("found zellij {version}, which this muxrd does not support"),
        None => "the zellij binary was not found on PATH or did not report a version".to_owned(),
    };
    format!(
        "{found} (supported: {}). Install a supported zellij, or set \
         {SKIP_VERSION_CHECK_ENV}=1 to proceed anyway.",
        supported_zellij_description()
    )
}

/// Run `<bin> --version` (5 s timeout) and return the version it prints.
/// Every failure is logged at `debug` level and reduces to `None`.
fn read_zellij_version(bin: &std::path::Path) -> Option<String> {
    let bin_clone = bin.to_path_buf();
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
        return None;
    }

    let output = match rx.recv_timeout(VERSION_CHECK_TIMEOUT) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            log::debug!("probe_zellij: '{} --version' failed: {e:#}", bin.display());
            return None;
        }
        Err(_) => {
            log::debug!(
                "probe_zellij: '{} --version' timed out or thread exited unexpectedly",
                bin.display()
            );
            return None;
        }
    };

    if !output.status.success() {
        log::debug!(
            "probe_zellij: '{} --version' exited with {}",
            bin.display(),
            output.status
        );
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let installed = parse_zellij_version(&stdout);

    if installed.is_empty() {
        log::debug!(
            "probe_zellij: could not parse a version string from output: {:?}",
            stdout.trim()
        );
        return None;
    }
    Some(installed)
}

/// Parse the version string out of `zellij --version` output.
///
/// The last whitespace-separated token: `"zellij 0.46.0\n"` → `"0.46.0"`.
/// Returns `""` for empty output.
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
///   its probe passes, or `Err("requested backend {kind} not available: …")`;
///   for zellij the reason says what was found and names the supported
///   versions.
/// - `None` — probe all backends in declaration order (zellij first, then
///   herdr); return the subset that passes.  If **none** pass, returns
///   `Err("No usable backend: install a supported zellij (…) or start herdr")`,
///   the parentheses naming the supported versions.
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
                let reason = match kind {
                    BackendKind::Zellij => zellij_unavailable_reason(),
                    BackendKind::Herdr => "probe failed".to_owned(),
                };
                bail!("requested backend {kind} not available: {reason}");
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
                bail!(
                    "No usable backend: install a supported zellij ({}) or start herdr",
                    supported_zellij_description()
                );
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
        assert_eq!(parse_zellij_version("zellij 0.45.1\n"), "0.45.1");
        assert_eq!(parse_zellij_version("zellij 0.45.1"), "0.45.1");
        assert_eq!(parse_zellij_version("zellij 0.46.0\n"), "0.46.0");
    }

    #[test]
    fn parse_version_returns_empty_for_unparseable_output() {
        assert_eq!(parse_zellij_version(""), "");
        assert_eq!(parse_zellij_version("   "), "");
    }

    // ── supported-version set ────────────────────────────────────────────────

    #[test]
    fn listed_lines_at_or_above_their_lowest_tested_patch_are_supported() {
        for version in ["0.45.1", "0.45.2", "0.46.0", "0.46.3"] {
            assert_eq!(
                zellij_support(version),
                ZellijSupport::Supported,
                "{version}"
            );
        }
    }

    #[test]
    fn other_versions_are_unsupported() {
        for version in ["0.45.0", "0.44.3", "0.47.0", "1.0.0"] {
            assert_eq!(
                zellij_support(version),
                ZellijSupport::Unsupported,
                "{version}"
            );
        }
    }

    #[test]
    fn text_that_is_not_a_version_is_unparseable() {
        for version in ["", "zellij", "0.46"] {
            assert_eq!(
                zellij_support(version),
                ZellijSupport::Unparseable,
                "{version:?}"
            );
        }
    }

    #[test]
    fn a_suffix_after_the_patch_number_is_ignored() {
        assert_eq!(zellij_support("0.46.0-rc1"), ZellijSupport::Supported);
        assert_eq!(zellij_support("0.45.0-rc1"), ZellijSupport::Unsupported);
        assert_eq!(zellij_support("0.46.0+build"), ZellijSupport::Supported);
        assert_eq!(zellij_support(" 0.46.0\n"), ZellijSupport::Supported);
    }

    #[test]
    fn only_a_tag_may_follow_the_patch_number() {
        for version in [
            "0.46.0.1", "0.46.0x", "0.46.", "0.46.x", "v0.46.0", "+0.46.0", "0.+46.0", "0.46.+0",
            "-0.46.0", "0.46.-1",
        ] {
            assert_eq!(
                zellij_support(version),
                ZellijSupport::Unparseable,
                "{version:?}"
            );
        }
    }

    /// The linked crate is the newest supported line: bumping the pin without
    /// adding a row fails here.
    #[test]
    fn the_linked_crate_is_the_newest_supported_line() {
        let (major, minor, _) = parse_version_triple(zellij_utils::consts::VERSION)
            .expect("the linked zellij-utils version is MAJOR.MINOR.PATCH");
        let newest = SUPPORTED_ZELLIJ.last().expect("the table is not empty");
        assert_eq!(
            (newest.major, newest.minor),
            (major, minor),
            "the last SUPPORTED_ZELLIJ row must be the linked zellij-utils line"
        );
        assert_eq!(
            zellij_support(zellij_utils::consts::VERSION),
            ZellijSupport::Supported
        );
    }

    #[test]
    fn the_table_is_in_ascending_order() {
        assert!(
            SUPPORTED_ZELLIJ
                .windows(2)
                .all(|pair| (pair[0].major, pair[0].minor) < (pair[1].major, pair[1].minor))
        );
    }

    #[test]
    fn the_description_names_every_line_from_the_table() {
        assert_eq!(
            supported_zellij_description(),
            "0.45.1 or newer 0.45.x, 0.46.x"
        );
    }

    #[test]
    fn the_unavailable_reason_says_what_was_found_and_what_is_supported() {
        let supported = supported_zellij_description();
        let unsupported = unavailable_reason(Some("0.44.3"));
        assert!(unsupported.contains("found zellij 0.44.3"), "{unsupported}");
        assert!(unsupported.contains(&supported), "{unsupported}");
        assert!(
            unsupported.contains(SKIP_VERSION_CHECK_ENV),
            "{unsupported}"
        );
        let garbled = unavailable_reason(Some("zellij"));
        assert!(garbled.contains("not a version number"), "{garbled}");
        assert!(garbled.contains(&supported), "{garbled}");
        let missing = unavailable_reason(None);
        assert!(missing.contains("not found on PATH"), "{missing}");
        assert!(missing.contains(&supported), "{missing}");
        assert!(missing.contains(SKIP_VERSION_CHECK_ENV), "{missing}");
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
    /// Only asserts if a supported zellij is actually installed.
    #[test]
    fn detect_some_zellij_returns_single_elem_vec_on_pass() {
        if probe_zellij() {
            let result = detect_backends(Some(BackendKind::Zellij));
            assert!(result.is_ok(), "probe passed but detect returned Err");
            assert_eq!(result.unwrap(), vec![BackendKind::Zellij]);
        }
        // If zellij is absent or not a supported version: nothing to assert.
    }
}
