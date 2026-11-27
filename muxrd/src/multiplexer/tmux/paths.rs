//! Resolution of the tmux binary, its server socket and its version.
//!
//! The socket is `MUXRD_TMUX_SOCKET` (an absolute path as is, a bare name as the
//! `-L` name under the tmux directory) or `$TMUX_TMPDIR`/`/tmp` joined with
//! `tmux-<uid>/default`.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

/// Environment variable naming the tmux server socket (path or `-L` name).
pub const TMUX_SOCKET_ENV: &str = "MUXRD_TMUX_SOCKET";
/// Environment variable overriding the tmux binary name or path.
pub const TMUX_BIN_ENV: &str = "MUXRD_TMUX_BIN";

const VERSION_TIMEOUT: Duration = Duration::from_secs(2);

/// The tmux server socket muxrd talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSocket {
    pub path: PathBuf,
}

impl TmuxSocket {
    /// Resolve the socket from the process environment.
    pub fn resolve() -> Self {
        let socket = std::env::var_os(TMUX_SOCKET_ENV).filter(|v| !v.is_empty());
        let tmpdir = std::env::var_os("TMUX_TMPDIR").filter(|v| !v.is_empty());
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        Self::resolve_from(socket, tmpdir, uid)
    }

    fn resolve_from(socket: Option<OsString>, tmpdir: Option<OsString>, uid: u32) -> Self {
        let dir =
            PathBuf::from(tmpdir.unwrap_or_else(|| "/tmp".into())).join(format!("tmux-{uid}"));
        let path = match socket {
            Some(value) => {
                let value = PathBuf::from(value);
                if value.is_absolute() {
                    value
                } else {
                    dir.join(value)
                }
            }
            None => dir.join("default"),
        };
        Self { path }
    }

    /// The leading arguments that point a tmux invocation at this socket.
    pub fn base_args(&self) -> [OsString; 2] {
        [OsString::from("-S"), self.path.clone().into_os_string()]
    }
}

/// The tmux binary to run: `MUXRD_TMUX_BIN`, default `tmux`.
pub fn tmux_bin() -> OsString {
    std::env::var_os(TMUX_BIN_ENV)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "tmux".into())
}

/// A parsed `tmux -V` version, e.g. `3.5a` or `next-3.9`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxVersion {
    pub major: u32,
    pub minor: u32,
    pub suffix: String,
}

impl TmuxVersion {
    /// The oldest supported tmux: 3.5a.
    pub const FLOOR: (u32, u32) = (3, 5);

    /// Parse the output of `tmux -V` ("tmux 3.5a", "tmux next-3.9").
    pub fn parse(text: &str) -> Result<Self> {
        let bad = || anyhow!("not a tmux version string: {text:?}");
        let rest = text
            .trim()
            .strip_prefix("tmux")
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or_else(bad)?;
        let rest = rest.strip_prefix("next-").unwrap_or(rest);
        let (major_str, tail) = rest.split_once('.').ok_or_else(bad)?;
        let digits = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        let (minor_str, suffix) = tail.split_at(digits);
        let major = major_str.parse().map_err(|_| bad())?;
        let minor = minor_str.parse().map_err(|_| bad())?;
        Ok(Self {
            major,
            minor,
            suffix: suffix.to_owned(),
        })
    }

    /// True when this version is `major.minor` or newer (the suffix is ignored).
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        (self.major, self.minor) >= (major, minor)
    }

    /// True when this version meets [`Self::FLOOR`].
    pub fn meets_floor(&self) -> bool {
        self.at_least(Self::FLOOR.0, Self::FLOOR.1)
    }
}

impl fmt::Display for TmuxVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}{}", self.major, self.minor, self.suffix)
    }
}

/// Run `tmux -V` (2 s timeout) and parse its output.
pub fn tmux_version() -> Result<TmuxVersion> {
    let bin = tmux_bin();
    let bin_display = PathBuf::from(&bin).display().to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawn_bin = bin.clone();
    std::thread::Builder::new()
        .name("tmux-probe-version".into())
        .spawn(move || {
            let _ = tx.send(std::process::Command::new(spawn_bin).arg("-V").output());
        })
        .context("spawn tmux version thread")?;
    let output = match rx.recv_timeout(VERSION_TIMEOUT) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => bail!("run '{bin_display} -V': {e}"),
        Err(_) => bail!("'{bin_display} -V' timed out after {VERSION_TIMEOUT:?}"),
    };
    if !output.status.success() {
        bail!("'{bin_display} -V' exited with {}", output.status);
    }
    TmuxVersion::parse(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_is_under_tmp_uid_dir() {
        let s = TmuxSocket::resolve_from(None, None, 1000);
        assert_eq!(s.path, PathBuf::from("/tmp/tmux-1000/default"));
    }

    #[test]
    fn tmux_tmpdir_replaces_tmp() {
        let s = TmuxSocket::resolve_from(None, Some("/run/x".into()), 7);
        assert_eq!(s.path, PathBuf::from("/run/x/tmux-7/default"));
    }

    #[test]
    fn absolute_socket_is_used_as_is() {
        let s = TmuxSocket::resolve_from(Some("/srv/t.sock".into()), Some("/run/x".into()), 7);
        assert_eq!(s.path, PathBuf::from("/srv/t.sock"));
    }

    #[test]
    fn bare_socket_name_lives_under_tmux_dir() {
        let s = TmuxSocket::resolve_from(Some("work".into()), None, 7);
        assert_eq!(s.path, PathBuf::from("/tmp/tmux-7/work"));
    }

    #[test]
    fn base_args_is_dash_s_path() {
        let s = TmuxSocket {
            path: PathBuf::from("/a/b"),
        };
        assert_eq!(
            s.base_args(),
            [OsString::from("-S"), OsString::from("/a/b")]
        );
    }

    #[test]
    fn parses_release_suffix_and_next() {
        let v = TmuxVersion::parse("tmux 3.5a\n").unwrap();
        assert_eq!((v.major, v.minor, v.suffix.as_str()), (3, 5, "a"));
        assert_eq!(v.to_string(), "3.5a");
        let v = TmuxVersion::parse("tmux 3.8").unwrap();
        assert_eq!((v.major, v.minor, v.suffix.as_str()), (3, 8, ""));
        let v = TmuxVersion::parse("tmux next-3.9").unwrap();
        assert_eq!((v.major, v.minor), (3, 9));
    }

    #[test]
    fn floor_comparison() {
        assert!(!TmuxVersion::parse("tmux 3.4").unwrap().meets_floor());
        assert!(TmuxVersion::parse("tmux 3.5a").unwrap().meets_floor());
        assert!(TmuxVersion::parse("tmux 3.10").unwrap().meets_floor());
        assert!(TmuxVersion::parse("tmux 4.0").unwrap().at_least(3, 9));
        assert!(!TmuxVersion::parse("tmux 3.5").unwrap().at_least(3, 6));
    }

    #[test]
    fn garbage_is_an_error() {
        for bad in [
            "",
            "tmux",
            "zellij 0.46.0",
            "tmux abc",
            "tmux 3",
            "tmux x.y",
        ] {
            assert!(TmuxVersion::parse(bad).is_err(), "{bad:?} must not parse");
        }
    }
}
