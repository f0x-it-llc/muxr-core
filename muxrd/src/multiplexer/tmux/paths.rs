//! Resolution of the tmux binary, its server socket and its version.
//!
//! The socket is NAMED (tmux's `default`, or a bare `MUXRD_TMUX_SOCKET` name),
//! reached with `-L <name>`, or an explicit PATH (an absolute
//! `MUXRD_TMUX_SOCKET`), reached with `-S <path>`.
//!
//! The split matters for safety. tmux refuses a socket directory that another
//! user owns or that grants group or other permission bits, but only when it
//! builds the path itself (`-L` or no flag): `-S` skips that check. A named
//! socket therefore always goes through `-L`, and an explicit path is checked
//! by [`TmuxSocket::check_socket_path`] before muxrd uses it.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::Metadata;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

/// Environment variable naming the tmux server socket (path or `-L` name).
pub const TMUX_SOCKET_ENV: &str = "MUXRD_TMUX_SOCKET";
/// Environment variable overriding the tmux binary name or path.
pub const TMUX_BIN_ENV: &str = "MUXRD_TMUX_BIN";

/// tmux's own socket name when none is given.
const DEFAULT_SOCKET_NAME: &str = "default";

const VERSION_TIMEOUT: Duration = Duration::from_secs(2);

/// The tmux server socket muxrd talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxSocket {
    /// A socket name tmux resolves itself (`-L <name>`) under
    /// `$TMUX_TMPDIR`/`/tmp` in `tmux-<uid>/`, checking that directory.
    Named {
        name: OsString,
        /// Where tmux puts the socket, as computed by muxrd: for logs only.
        display_path: PathBuf,
    },
    /// An explicit socket path (`-S <path>`). tmux checks nothing about it,
    /// so callers run [`TmuxSocket::check_socket_path`] first.
    Path(PathBuf),
}

impl TmuxSocket {
    /// Resolve the socket from the process environment.
    ///
    /// Fails for a bare `MUXRD_TMUX_SOCKET` that is not one path component.
    pub fn resolve() -> Result<Self> {
        let socket = std::env::var_os(TMUX_SOCKET_ENV).filter(|v| !v.is_empty());
        Self::resolve_from(socket, tmux_tmpdir(), current_uid())
    }

    /// A named socket, its display path computed from the process environment.
    pub fn named(name: impl Into<OsString>) -> Result<Self> {
        Self::named_in(name.into(), tmux_tmpdir(), current_uid())
    }

    fn resolve_from(socket: Option<OsString>, tmpdir: Option<OsString>, uid: u32) -> Result<Self> {
        match socket {
            Some(value) if Path::new(&value).is_absolute() => Ok(Self::Path(value.into())),
            Some(value) => Self::named_in(value, tmpdir, uid),
            None => Self::named_in(DEFAULT_SOCKET_NAME.into(), tmpdir, uid),
        }
    }

    fn named_in(name: OsString, tmpdir: Option<OsString>, uid: u32) -> Result<Self> {
        validate_name(&name)?;
        let display_path = PathBuf::from(tmpdir.unwrap_or_else(|| "/tmp".into()))
            .join(format!("tmux-{uid}"))
            .join(&name);
        Ok(Self::Named { name, display_path })
    }

    /// The leading arguments that point a tmux invocation at this socket:
    /// `-L <name>` or `-S <path>`.
    pub fn base_args(&self) -> [OsString; 2] {
        match self {
            Self::Named { name, .. } => [OsString::from("-L"), name.clone()],
            Self::Path(path) => [OsString::from("-S"), path.clone().into_os_string()],
        }
    }

    /// The socket's location, for logs. For a named socket this is muxrd's
    /// computation of tmux's path, never something muxrd opens.
    pub fn display_path(&self) -> &Path {
        match self {
            Self::Named { display_path, .. } => display_path,
            Self::Path(path) => path,
        }
    }

    /// Refuse an explicit socket path another user could have planted.
    ///
    /// The parent directory must exist, be a real directory (not a symlink),
    /// be owned by the effective uid and grant no group or other permission
    /// bits; an existing socket must be a socket owned by the effective uid.
    /// A named socket passes: tmux runs the same check itself under `-L`.
    pub fn check_socket_path(&self) -> Result<()> {
        match self {
            Self::Named { .. } => Ok(()),
            // SAFETY: geteuid has no preconditions and cannot fail.
            Self::Path(path) => check_path_for(path, unsafe { libc::geteuid() }),
        }
    }
}

/// `TMUX_TMPDIR`, when set and non-empty.
fn tmux_tmpdir() -> Option<OsString> {
    std::env::var_os("TMUX_TMPDIR").filter(|v| !v.is_empty())
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// A `-L` name is one path component: no `/`, no `..`, no control characters.
fn validate_name(name: &OsStr) -> Result<()> {
    let bytes = name.as_bytes();
    let shown = name.to_string_lossy();
    if bytes.is_empty() {
        bail!("{TMUX_SOCKET_ENV}: empty tmux socket name");
    }
    if bytes.contains(&b'/') {
        bail!(
            "{TMUX_SOCKET_ENV}={shown:?}: a tmux socket name must not contain '/'; \
             give an absolute path or a bare name"
        );
    }
    if bytes == b"." || bytes.windows(2).any(|w| w == b"..") {
        bail!("{TMUX_SOCKET_ENV}={shown:?}: a tmux socket name must not contain '..'");
    }
    if bytes.iter().any(u8::is_ascii_control) {
        bail!(
            "{TMUX_SOCKET_ENV}={shown:?}: a tmux socket name must not contain control characters"
        );
    }
    Ok(())
}

/// [`TmuxSocket::check_socket_path`] for `path`, expecting owner `euid`.
fn check_path_for(path: &Path, euid: u32) -> Result<()> {
    let shown = path.display();
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("tmux socket {shown}: the path has no parent directory"))?;
    let dir_meta = std::fs::symlink_metadata(dir)
        .with_context(|| format!("tmux socket {shown}: directory {}", dir.display()))?;
    check_socket_dir(dir, &dir_meta, euid).with_context(|| format!("tmux socket {shown}"))?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) => check_socket_file(path, &meta, euid),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!("tmux socket {shown}: {e}")),
    }
}

/// The socket's directory: a real directory owned by `euid`, no group or
/// other permission bits.
fn check_socket_dir(dir: &Path, meta: &Metadata, euid: u32) -> Result<()> {
    let shown = dir.display();
    if meta.file_type().is_symlink() {
        bail!("directory {shown} is a symlink; point {TMUX_SOCKET_ENV} at a real directory");
    }
    if !meta.is_dir() {
        bail!("{shown} is not a directory");
    }
    if meta.uid() != euid {
        bail!(
            "directory {shown} is owned by uid {}, not by muxrd's uid {euid}",
            meta.uid()
        );
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        bail!(
            "directory {shown} has unsafe permissions {mode:04o}: group or others \
             have access; use an owner-only directory (chmod 700)"
        );
    }
    Ok(())
}

/// An existing socket file: a socket owned by `euid`.
fn check_socket_file(path: &Path, meta: &Metadata, euid: u32) -> Result<()> {
    let shown = path.display();
    if !meta.file_type().is_socket() {
        bail!("tmux socket {shown} exists and is not a socket");
    }
    if meta.uid() != euid {
        bail!(
            "tmux socket {shown} is owned by uid {}, not by muxrd's uid {euid}",
            meta.uid()
        );
    }
    Ok(())
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

    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn named(name: &str, path: &str) -> TmuxSocket {
        TmuxSocket::Named {
            name: name.into(),
            display_path: path.into(),
        }
    }

    fn args(socket: &TmuxSocket) -> Vec<String> {
        socket
            .base_args()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn default_socket_is_named_default() {
        let s = TmuxSocket::resolve_from(None, None, 1000).unwrap();
        assert_eq!(s, named("default", "/tmp/tmux-1000/default"));
        assert_eq!(args(&s), ["-L", "default"]);
        assert_eq!(s.display_path(), Path::new("/tmp/tmux-1000/default"));
    }

    #[test]
    fn tmux_tmpdir_moves_only_the_display_path() {
        let s = TmuxSocket::resolve_from(None, Some("/run/x".into()), 7).unwrap();
        assert_eq!(s.display_path(), Path::new("/run/x/tmux-7/default"));
        assert_eq!(args(&s), ["-L", "default"]);
    }

    #[test]
    fn bare_socket_name_is_dash_l_name() {
        let s = TmuxSocket::resolve_from(Some("work".into()), None, 7).unwrap();
        assert_eq!(s, named("work", "/tmp/tmux-7/work"));
        assert_eq!(args(&s), ["-L", "work"]);
    }

    #[test]
    fn absolute_socket_is_dash_s_path() {
        let s =
            TmuxSocket::resolve_from(Some("/srv/t.sock".into()), Some("/run/x".into()), 7).unwrap();
        assert_eq!(s, TmuxSocket::Path("/srv/t.sock".into()));
        assert_eq!(args(&s), ["-S", "/srv/t.sock"]);
        assert_eq!(s.display_path(), Path::new("/srv/t.sock"));
    }

    #[test]
    fn names_that_are_not_one_component_are_rejected() {
        for bad in [
            "a/b",
            "./x",
            "..",
            ".",
            "a..b",
            "x\ny",
            "tab\there",
            "del\u{7f}",
        ] {
            let err = TmuxSocket::resolve_from(Some(bad.into()), None, 7)
                .expect_err(&format!("{bad:?} must be rejected"));
            assert!(format!("{err:#}").contains(TMUX_SOCKET_ENV), "{err:#}");
        }
        assert!(TmuxSocket::named("").is_err());
        assert!(TmuxSocket::named("muxr-1.2_x").is_ok());
    }

    #[test]
    fn a_named_socket_needs_no_path_check() {
        let s = named("default", "/nonexistent/tmux-1/default");
        assert!(s.check_socket_path().is_ok());
    }

    // ── check_socket_path ────────────────────────────────────────────────────

    static CASE: AtomicUsize = AtomicUsize::new(0);

    /// A fresh owner-only directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "muxrd-tmux-paths-{}-{}",
                std::process::id(),
                CASE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(dir)
        }

        /// A subdirectory with `mode`.
        fn dir(&self, name: &str, mode: u32) -> PathBuf {
            let dir = self.0.join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            dir
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn euid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    fn check(path: PathBuf) -> Result<()> {
        TmuxSocket::Path(path).check_socket_path()
    }

    #[test]
    fn check_accepts_an_owner_only_directory() {
        let s = Scratch::new();
        let dir = s.dir("ok", 0o700);
        check(dir.join("sock")).expect("a missing socket in a 0700 dir is fine");
        let sock = dir.join("live");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        check(sock).expect("a socket of ours in a 0700 dir is fine");
    }

    #[test]
    fn check_refuses_a_group_writable_directory() {
        let s = Scratch::new();
        let err = check(s.dir("g", 0o770).join("sock")).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("unsafe permissions 0770"), "{text}");
        assert!(text.contains("/g/sock"), "{text}");
    }

    #[test]
    fn check_refuses_an_other_writable_directory() {
        let s = Scratch::new();
        let err = check(s.dir("o", 0o1777).join("sock")).unwrap_err();
        assert!(
            format!("{err:#}").contains("unsafe permissions 1777"),
            "{err:#}"
        );
        // Read access alone is refused too: tmux wants no group/other bits.
        assert!(check(s.dir("r", 0o755).join("sock")).is_err());
    }

    #[test]
    fn check_refuses_a_symlinked_directory() {
        let s = Scratch::new();
        let real = s.dir("real", 0o700);
        let link = s.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = check(link.join("sock")).unwrap_err();
        assert!(format!("{err:#}").contains("symlink"), "{err:#}");
    }

    #[test]
    fn check_refuses_a_missing_or_non_directory_parent() {
        let s = Scratch::new();
        assert!(check(s.0.join("absent").join("sock")).is_err());
        let file = s.0.join("file");
        std::fs::write(&file, "").unwrap();
        let err = check(file.join("sock")).unwrap_err();
        assert!(format!("{err:#}").contains("not a directory"), "{err:#}");
        assert!(check("/".into()).is_err());
    }

    #[test]
    fn check_refuses_an_existing_non_socket() {
        let s = Scratch::new();
        let dir = s.dir("d", 0o700);
        std::fs::write(dir.join("sock"), "").unwrap();
        let err = check(dir.join("sock")).unwrap_err();
        assert!(format!("{err:#}").contains("not a socket"), "{err:#}");
        std::os::unix::fs::symlink("/elsewhere", dir.join("ln")).unwrap();
        assert!(check(dir.join("ln")).is_err());
    }

    /// Another owner, checked against a different expected uid so it runs
    /// unprivileged. Handing a directory to a real other uid needs root:
    /// [`check_refuses_a_directory_chowned_away`] covers that.
    #[test]
    fn check_refuses_another_owner() {
        let s = Scratch::new();
        let dir = s.dir("d", 0o700);
        let other = euid().wrapping_add(1);
        let err = check_path_for(&dir.join("sock"), other).unwrap_err();
        assert!(format!("{err:#}").contains("owned by uid"), "{err:#}");
        let sock = dir.join("live");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let meta = std::fs::symlink_metadata(&sock).unwrap();
        let err = check_socket_file(&sock, &meta, other).unwrap_err();
        assert!(format!("{err:#}").contains("owned by uid"), "{err:#}");
    }

    /// Skipped unless running as root, which can give a directory and a
    /// socket to another uid.
    #[test]
    fn check_refuses_a_directory_chowned_away() {
        if euid() != 0 {
            eprintln!("skipping: chowning to another uid needs root");
            return;
        }
        let s = Scratch::new();
        let dir = s.dir("d", 0o700);
        std::os::unix::fs::chown(&dir, Some(65534), None).unwrap();
        assert!(check(dir.join("sock")).is_err());
        let mine = s.dir("m", 0o700);
        let sock = mine.join("live");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::os::unix::fs::lchown(&sock, Some(65534), None).unwrap();
        assert!(check(sock).is_err());
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
