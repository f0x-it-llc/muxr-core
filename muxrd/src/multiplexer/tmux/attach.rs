//! Pty attach: `tmux attach-session` in a daemon-owned pty, receiver and sender.
//!
//! [`open`] allocates a pty, sizes it, and runs an ordinary tmux client
//! (`tmux -S <socket> attach-session [-r] -t =<session>`) on the slave side as
//! the leader of its own session with the pty as its controlling terminal. The
//! master side is split into the two relay halves:
//!
//! - [`TmuxMuxReceiver`] reads the composited screen tmux draws for that client
//!   and yields it as [`MuxServerMsg::Render`]. While forwarding it tracks the
//!   DEC private modes the client switched on (mouse reporting, SGR mouse
//!   encoding, bracketed paste) in a shared [`ModeState`]. When the client goes
//!   away (EOF or EIO on the master) it yields one [`MuxEvent::Exit`] and then
//!   `None`. Between chunks it wakes every 250 ms with [`MuxServerMsg::Other`]
//!   so the relay's reader can observe its stop flag without a backend nudge.
//! - [`TmuxMuxSender`] writes input bytes to the master, encodes wheel events
//!   as the mouse reports tmux asked for, resizes the pty, and routes tab, pane
//!   and zoom changes plus layout reads through an [`AttachControl`]
//!   implementation (tmux commands), never through the pty.
//!
//! **Focus is session-wide.** `select-window`, `select-pane` and
//! `resize-pane -Z` change the session's current window and the window's active
//! pane, which every client attached to the session follows. A tab or pane
//! switch made by one connection therefore moves every other client of the same
//! session too, as it does on zellij.
//!
//! **Read-only.** A read-only attach passes `-r`: tmux itself ignores keys from
//! the client and does not let its size drive the window. The sender also
//! refuses to write input or mouse reports for it. A resize still reaches the
//! pty so the client's own view is redrawn at its size.
//!
//! **Process hygiene.** The master fd is close-on-exec and non-blocking; the
//! child gets the slave as stdin/stdout/stderr and every other descriptor is
//! closed when it execs. Its environment is built from scratch (terminal type,
//! locale, colour depth), so nothing from muxrd's own environment or
//! configuration reaches it. The child is reaped exactly once, by
//! `send_client_exited` or when the last half is dropped, with `waitpid`
//! rather than SIGCHLD: SIGHUP first, SIGKILL after one second.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::paths::{TmuxSocket, tmux_bin};
use crate::multiplexer::types::{
    FullscreenHint, LayoutSnapshot, MuxEvent, MuxMouseKind, MuxServerMsg, PaneRef,
};
use crate::multiplexer::{DualHandle, MuxReceiver, MuxSender};

/// Largest single read from the pty master.
const READ_CHUNK: usize = 64 * 1024;
/// How long the receiver waits for output before yielding
/// [`MuxServerMsg::Other`].
const READ_POLL: Duration = Duration::from_millis(250);
/// How long a write may wait for room in the pty before it fails.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the client gets to exit after SIGHUP before it is killed.
const REAP_GRACE: Duration = Duration::from_secs(1);
/// How often the reaper re-checks the child during the grace period.
const REAP_STEP: Duration = Duration::from_millis(10);
/// Bytes of trailing output kept to word the exit reason.
const TAIL_MAX: usize = 512;
/// Longest parameter string kept for one `ESC [ ? ... h|l` sequence.
const MODE_PARAMS_MAX: usize = 32;
/// Exit reason when the client's last output names none.
const DEFAULT_EXIT_REASON: &str = "tmux client exited";
/// Bracketed-paste delimiters.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

// ─── AttachControl ──────────────────────────────────────────────────────────────

/// The tmux commands an attach routes out of band: tab, pane and zoom changes
/// and the layout read. Implemented by the backend over its command runner.
///
/// Every method is blocking.
pub trait AttachControl: Send + Sync {
    /// Make `window` the session's current window (`select-window`).
    fn select_window(&self, window: u32) -> anyhow::Result<()>;
    /// Make `pane` its window's active pane (`select-pane`).
    fn select_pane(&self, pane: u32) -> anyhow::Result<()>;
    /// Toggle zoom on `pane` (`resize-pane -Z`).
    fn zoom_toggle(&self, pane: u32) -> anyhow::Result<()>;
    /// Read the neutral layout of `session`.
    fn layout(&self, session: &str) -> anyhow::Result<LayoutSnapshot>;
}

// ─── ModeState ──────────────────────────────────────────────────────────────────

/// Terminal modes the attached tmux client has switched on, as seen in its
/// output. Written by the receiver, read by the sender.
#[derive(Debug, Default)]
pub struct ModeState {
    mouse_normal: AtomicBool,
    mouse_button: AtomicBool,
    mouse_motion: AtomicBool,
    sgr_mouse: AtomicBool,
    bracketed_paste: AtomicBool,
}

impl ModeState {
    /// Any mouse-reporting mode (1000, 1002 or 1003) is on.
    pub fn mouse_any(&self) -> bool {
        self.mouse_normal.load(Ordering::Relaxed)
            || self.mouse_button.load(Ordering::Relaxed)
            || self.mouse_motion.load(Ordering::Relaxed)
    }

    /// SGR mouse encoding (1006) is on.
    pub fn sgr_mouse(&self) -> bool {
        self.sgr_mouse.load(Ordering::Relaxed)
    }

    /// Bracketed paste (2004) is on.
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste.load(Ordering::Relaxed)
    }

    fn apply(&self, mode: u32, on: bool) {
        let flag = match mode {
            1000 => &self.mouse_normal,
            1002 => &self.mouse_button,
            1003 => &self.mouse_motion,
            1006 => &self.sgr_mouse,
            2004 => &self.bracketed_paste,
            _ => return,
        };
        flag.store(on, Ordering::Relaxed);
    }
}

/// Incremental scanner for `ESC [ ? <n>[;<n>...] h|l`. Its state (including
/// the parameter bytes read so far) carries across chunks, so a sequence split
/// between two reads is still recognised.
#[derive(Debug, Default)]
struct DecModeScanner {
    state: ScanState,
    params: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Csi,
    Private,
}

impl DecModeScanner {
    fn feed(&mut self, bytes: &[u8], modes: &ModeState) {
        for &b in bytes {
            self.state = match (self.state, b) {
                (_, 0x1b) => ScanState::Escape,
                (ScanState::Ground, _) => ScanState::Ground,
                (ScanState::Escape, b'[') => ScanState::Csi,
                (ScanState::Csi, b'?') => {
                    self.params.clear();
                    ScanState::Private
                }
                (ScanState::Private, b'0'..=b'9' | b';') if self.params.len() < MODE_PARAMS_MAX => {
                    self.params.push(b);
                    ScanState::Private
                }
                (ScanState::Private, b'h' | b'l') => {
                    self.apply(b == b'h', modes);
                    ScanState::Ground
                }
                _ => ScanState::Ground,
            };
        }
    }

    fn apply(&self, on: bool, modes: &ModeState) {
        for param in self.params.split(|&b| b == b';') {
            if let Some(mode) = std::str::from_utf8(param)
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            {
                modes.apply(mode, on);
            }
        }
    }
}

// ─── Encoding helpers ───────────────────────────────────────────────────────────

/// The bytes to write for text input: wrapped in bracketed-paste delimiters
/// when the client asked for bracketed paste and the text looks like a paste
/// (a newline, or more than one character) made only of printable characters,
/// newlines and tabs. Anything carrying another control character (a key
/// sequence such as an arrow key, Enter as `\r`, or a paste the app already
/// bracketed) is written unchanged, as the zellij sender writes all text.
fn input_payload(text: &str, bracketed_paste: bool) -> Cow<'_, [u8]> {
    let looks_pasted = text.contains('\n') || text.chars().nth(1).is_some();
    let plain = !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t');
    if bracketed_paste && looks_pasted && plain {
        let mut out = Vec::with_capacity(text.len() + PASTE_START.len() + PASTE_END.len());
        out.extend_from_slice(PASTE_START);
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(PASTE_END);
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text.as_bytes())
    }
}

/// A wheel report at zero-based (`col`, `row`): SGR (`ESC [ < b ; x ; y M`,
/// one-based) when the client turned SGR encoding on, otherwise the legacy
/// `ESC [ M` form, which cannot express a coordinate past 222 (`None`).
fn encode_wheel(kind: MuxMouseKind, col: u16, row: u16, sgr: bool) -> Option<Vec<u8>> {
    let button: u32 = match kind {
        MuxMouseKind::WheelUp => 64,
        MuxMouseKind::WheelDown => 65,
    };
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    if sgr {
        return Some(format!("\x1b[<{button};{x};{y}M").into_bytes());
    }
    let byte = |v: u32| u8::try_from(v + 32).ok();
    Some(vec![0x1b, b'[', b'M', byte(button)?, byte(x)?, byte(y)?])
}

/// Remove escape sequences (CSI, OSC and two-byte forms) from `text`.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Word the exit reason from the client's last output: tmux's own bracketed
/// exit message (`[detached (from session s)]`, `[exited]`, `[server exited]`,
/// …) without its brackets, or, when the client never drew a screen (no escape
/// sequence at all), its last error line (`can't find session: =s`).
fn exit_reason(tail: &[u8]) -> String {
    let raw = String::from_utf8_lossy(tail);
    let drew = raw.contains('\x1b');
    let text = strip_escapes(&raw);
    let last = text
        .split(['\r', '\n'])
        .map(str::trim)
        .rfind(|line| !line.is_empty());
    match last {
        Some(line) if line.len() > 2 && line.starts_with('[') && line.ends_with(']') => {
            line[1..line.len() - 1].to_owned()
        }
        Some(line) if !drew && line.chars().count() <= 200 => line.to_owned(),
        _ => DEFAULT_EXIT_REASON.to_owned(),
    }
}

// ─── The shared pty ─────────────────────────────────────────────────────────────

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The pty master and the client process, shared by every half of one attach.
/// The master stays open until the last half is dropped, so no half ever sees
/// its descriptor number reused underneath it.
struct Pty {
    master: File,
    /// `None` once reaped (or for a test double without a process).
    child: Mutex<Option<Child>>,
    /// Serialises writes so concurrent senders never interleave input.
    write_lock: Mutex<()>,
    /// Set once the client has been told to leave; writes then fail.
    closed: AtomicBool,
}

impl Pty {
    fn new(master: OwnedFd, child: Option<Child>) -> Self {
        Self {
            master: File::from(master),
            child: Mutex::new(child),
            write_lock: Mutex::new(()),
            closed: AtomicBool::new(false),
        }
    }

    /// Write all of `bytes`, waiting (bounded) whenever the pty is full.
    fn write_all(&self, bytes: &[u8]) -> Result<()> {
        let _guard = lock(&self.write_lock);
        if self.closed.load(Ordering::Acquire) {
            bail!("tmux attach: client already closed");
        }
        let deadline = Instant::now() + WRITE_TIMEOUT;
        let mut rest = bytes;
        while !rest.is_empty() {
            match (&self.master).write(rest) {
                Ok(0) => bail!("tmux attach: pty accepted no bytes"),
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        bail!("tmux attach: pty write timed out after {WRITE_TIMEOUT:?}");
                    }
                    sys::poll(self.master.as_fd(), libc::POLLOUT, left.min(READ_POLL))
                        .context("tmux attach: wait for pty space")?;
                }
                Err(e) => return Err(e).context("tmux attach: write to pty"),
            }
        }
        Ok(())
    }

    /// Size the pty and tell the client to re-read it.
    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        sys::set_winsize(self.master.as_fd(), rows.max(1), cols.max(1))
            .context("tmux attach: set pty size")?;
        // The child lock keeps the pid from being reaped (and reused) while
        // it is signalled.
        if let Some(child) = lock(&self.child).as_ref()
            && let Err(e) = sys::signal(child.id(), libc::SIGWINCH)
        {
            log::debug!("tmux attach: SIGWINCH to {} failed: {e}", child.id());
        }
        Ok(())
    }

    /// Make the client leave and reap it: SIGHUP (what a terminal hangup
    /// delivers), SIGKILL if it is still running after [`REAP_GRACE`]. Runs at
    /// most once; later calls find no child.
    fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let Some(mut child) = lock(&self.child).take() else {
            return;
        };
        let pid = child.id();
        match child.try_wait() {
            Ok(Some(status)) => {
                log::debug!("tmux attach: client {pid} had already exited ({status})");
                return;
            }
            Ok(None) => {}
            Err(e) => {
                // Someone else reaped it; the pid may already be reused, so
                // it must not be signalled.
                log::warn!("tmux attach: cannot wait for client {pid}: {e}");
                return;
            }
        }
        if let Err(e) = sys::signal(pid, libc::SIGHUP) {
            log::debug!("tmux attach: SIGHUP to {pid} failed: {e}");
        }
        let deadline = Instant::now() + REAP_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    log::debug!("tmux attach: client {pid} exited ({status})");
                    return;
                }
                Ok(None) if Instant::now() < deadline => std::thread::sleep(REAP_STEP),
                Ok(None) => break,
                Err(e) => {
                    log::warn!("tmux attach: cannot wait for client {pid}: {e}");
                    return;
                }
            }
        }
        log::warn!("tmux attach: client {pid} ignored SIGHUP for {REAP_GRACE:?}; killing it");
        let _ = child.kill();
        match child.wait() {
            Ok(status) => log::debug!("tmux attach: client {pid} killed ({status})"),
            Err(e) => log::warn!("tmux attach: cannot reap client {pid}: {e}"),
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ─── open ───────────────────────────────────────────────────────────────────────

/// The tmux binary as an absolute path, resolved against muxrd's own `PATH`
/// (the child's environment carries none).
fn resolve_tmux_bin() -> Result<PathBuf> {
    let bin = PathBuf::from(tmux_bin());
    if bin.components().count() > 1 {
        return Ok(bin);
    }
    which::which(&bin).with_context(|| format!("tmux attach: '{}' not on PATH", bin.display()))
}

/// The client's argument vector after the binary.
fn attach_args(socket: &TmuxSocket, session: &str, read_only: bool) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = socket.base_args().into();
    args.push("attach-session".into());
    if read_only {
        args.push("-r".into());
    }
    args.push("-t".into());
    args.push(format!("={session}").into());
    args
}

/// The client's whole environment: terminal type, colour depth and a locale
/// (muxrd's `LANG`/`LC_ALL`, `C.UTF-8` when unset). An unset `LC_ALL` takes
/// the chosen `LANG`, so a fallback never overrides the operator's locale.
fn child_env(lang: Option<String>, lc_all: Option<String>) -> Vec<(&'static str, String)> {
    let lang = lang
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "C.UTF-8".to_owned());
    let lc_all = lc_all
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| lang.clone());
    vec![
        ("TERM", "xterm-256color".to_owned()),
        ("COLORTERM", "truecolor".to_owned()),
        ("LANG", lang),
        ("LC_ALL", lc_all),
    ]
}

/// Attach a tmux client to `session` in a new pty of `rows`×`cols` and return
/// the relay halves over it.
///
/// `ctl` serves the routed controls (tab, pane, zoom, layout); the pty carries
/// only input and the screen.
pub fn open(
    socket: &TmuxSocket,
    session: &str,
    rows: u16,
    cols: u16,
    read_only: bool,
    ctl: Arc<dyn AttachControl>,
) -> Result<DualHandle> {
    if session.is_empty() {
        bail!("tmux attach: empty session name");
    }
    let bin = resolve_tmux_bin()?;
    let (master, slave) =
        sys::open_pty(rows.max(1), cols.max(1)).context("tmux attach: allocate a pty")?;

    let mut cmd = Command::new(&bin);
    cmd.args(attach_args(socket, session, read_only))
        .env_clear()
        .envs(child_env(
            std::env::var("LANG").ok(),
            std::env::var("LC_ALL").ok(),
        ))
        .stdin(Stdio::from(
            slave.try_clone().context("tmux attach: dup pty")?,
        ))
        .stdout(Stdio::from(
            slave.try_clone().context("tmux attach: dup pty")?,
        ))
        .stderr(Stdio::from(slave));
    sys::lead_new_session_on_stdin(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("tmux attach: run '{}'", bin.display()))?;
    // Dropping the command closes muxrd's copies of the slave, so the master
    // sees EOF/EIO as soon as the client exits.
    drop(cmd);
    log::info!(
        "tmux attach: client {} on '{session}' at {rows}x{cols}{}",
        child.id(),
        if read_only { " (read-only)" } else { "" }
    );

    let pty = Arc::new(Pty::new(master, Some(child)));
    let modes = Arc::new(ModeState::default());
    Ok(DualHandle {
        sender: Box::new(TmuxMuxSender::new(
            Arc::clone(&pty),
            Arc::clone(&modes),
            ctl,
            session,
            read_only,
        )),
        receiver: Box::new(TmuxMuxReceiver::new(pty, modes)),
        session_name: session.to_owned(),
        resumed_view: None,
    })
}

// ─── TmuxMuxReceiver ────────────────────────────────────────────────────────────

/// The render half of a tmux attach: the client's screen bytes.
pub struct TmuxMuxReceiver {
    pty: Arc<Pty>,
    modes: Arc<ModeState>,
    scanner: DecModeScanner,
    tail: Vec<u8>,
    buf: Vec<u8>,
    finished: bool,
}

impl TmuxMuxReceiver {
    fn new(pty: Arc<Pty>, modes: Arc<ModeState>) -> Self {
        Self {
            pty,
            modes,
            scanner: DecModeScanner::default(),
            tail: Vec::new(),
            buf: vec![0; READ_CHUNK],
            finished: false,
        }
    }

    fn remember(&mut self, bytes: &[u8]) {
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > TAIL_MAX {
            let cut = self.tail.len() - TAIL_MAX;
            self.tail.drain(..cut);
        }
    }

    fn finish(&mut self, reason: String) -> Option<MuxServerMsg> {
        self.finished = true;
        log::info!("tmux attach: client stream ended: {reason}");
        Some(MuxServerMsg::Event(MuxEvent::Exit { reason }))
    }
}

impl MuxReceiver for TmuxMuxReceiver {
    fn recv(&mut self) -> Option<MuxServerMsg> {
        if self.finished {
            return None;
        }
        loop {
            match sys::poll(self.pty.master.as_fd(), libc::POLLIN, READ_POLL) {
                Ok(true) => {}
                Ok(false) if self.pty.closed.load(Ordering::Acquire) => {
                    return self.finish(DEFAULT_EXIT_REASON.to_owned());
                }
                Ok(false) => return Some(MuxServerMsg::Other),
                Err(e) => return self.finish(format!("tmux attach: poll failed: {e}")),
            }
            match (&self.pty.master).read(&mut self.buf) {
                Ok(0) => return self.finish(exit_reason(&self.tail)),
                Ok(n) => {
                    let chunk = self.buf[..n].to_vec();
                    self.scanner.feed(&chunk, &self.modes);
                    self.remember(&chunk);
                    return Some(MuxServerMsg::Render(chunk));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                // EIO: every holder of the slave side has closed it.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                    return self.finish(exit_reason(&self.tail));
                }
                Err(e) => return self.finish(format!("tmux attach: read failed: {e}")),
            }
        }
    }
}

// ─── TmuxMuxSender ──────────────────────────────────────────────────────────────

/// The input/control half of a tmux attach.
pub struct TmuxMuxSender {
    pty: Arc<Pty>,
    modes: Arc<ModeState>,
    ctl: Arc<dyn AttachControl>,
    session: String,
    read_only: bool,
    dropped_mouse: Arc<AtomicU64>,
}

impl TmuxMuxSender {
    fn new(
        pty: Arc<Pty>,
        modes: Arc<ModeState>,
        ctl: Arc<dyn AttachControl>,
        session: &str,
        read_only: bool,
    ) -> Self {
        Self {
            pty,
            modes,
            ctl,
            session: session.to_owned(),
            read_only,
            dropped_mouse: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Whether input must be refused (read-only attach); logs the drop.
    fn refuse_input(&self, what: &str) -> bool {
        if self.read_only {
            log::debug!(
                "tmux attach [{}]: dropping {what} (read-only attach)",
                self.session
            );
        }
        self.read_only
    }

    fn drop_mouse(&self, why: &str) {
        let n = self.dropped_mouse.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!(
            "tmux attach [{}]: dropping wheel event ({why}; {n} dropped)",
            self.session
        );
    }
}

/// A tmux pane id from a neutral pane ref (tmux has no plugin panes).
fn tmux_pane(pane: PaneRef) -> Result<u32> {
    if pane.is_plugin {
        bail!("tmux has no plugin panes (pane {})", pane.id);
    }
    Ok(pane.id)
}

impl MuxSender for TmuxMuxSender {
    fn go_to_tab(&mut self, tab_id: u64) -> Result<()> {
        let window = u32::try_from(tab_id).map_err(|_| anyhow!("tmux window id {tab_id}"))?;
        self.ctl.select_window(window)
    }

    fn focus_pane(&mut self, pane: PaneRef) -> Result<()> {
        self.ctl.select_pane(tmux_pane(pane)?)
    }

    fn toggle_fullscreen(&mut self, pane: PaneRef, hint: FullscreenHint) -> Result<()> {
        // tmux zoom has no floating-pane variants, so the hint has nothing to
        // choose between.
        let _ = hint;
        self.ctl.zoom_toggle(tmux_pane(pane)?)
    }

    fn has_sync_layout(&self) -> bool {
        true
    }

    fn query_layout_result(&mut self) -> Option<Result<LayoutSnapshot>> {
        Some(self.ctl.layout(&self.session))
    }

    fn query_layout(&mut self) -> Result<()> {
        log::debug!("tmux attach: in-band layout query ignored (answered out of band)");
        Ok(())
    }

    fn send_input_chars(&mut self, text: &str) -> Result<()> {
        if self.refuse_input("text input") {
            return Ok(());
        }
        self.pty
            .write_all(&input_payload(text, self.modes.bracketed_paste()))
    }

    fn send_input_bytes(&mut self, bytes: Vec<u8>) -> Result<()> {
        if self.refuse_input("byte input") {
            return Ok(());
        }
        self.pty.write_all(&bytes)
    }

    fn send_mouse(&mut self, kind: MuxMouseKind, col: u16, row: u16) -> Result<()> {
        if self.refuse_input("wheel event") {
            return Ok(());
        }
        if !self.modes.mouse_any() {
            self.drop_mouse("mouse reporting is off");
            return Ok(());
        }
        match encode_wheel(kind, col, row, self.modes.sgr_mouse()) {
            Some(report) => self.pty.write_all(&report),
            None => {
                self.drop_mouse("position beyond the legacy encoding");
                Ok(())
            }
        }
    }

    fn send_resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.pty.resize(rows, cols)
    }

    fn send_client_exited(&mut self) -> Result<()> {
        self.pty.shutdown();
        Ok(())
    }

    fn box_clone(&self) -> Box<dyn MuxSender> {
        Box::new(Self {
            pty: Arc::clone(&self.pty),
            modes: Arc::clone(&self.modes),
            ctl: Arc::clone(&self.ctl),
            session: self.session.clone(),
            read_only: self.read_only,
            dropped_mouse: Arc::clone(&self.dropped_mouse),
        })
    }
}

// ─── sys: the module's only unsafe code ────────────────────────────────────────

/// Thin wrappers over the libc calls the attach needs. Every `unsafe` block of
/// this module lives here.
mod sys {
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::Duration;

    fn winsize(rows: u16, cols: u16) -> libc::winsize {
        libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }

    fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
        if rc == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }

    fn add_fd_flags(
        fd: RawFd,
        get: libc::c_int,
        set: libc::c_int,
        flag: libc::c_int,
    ) -> io::Result<()> {
        // SAFETY: fcntl with F_GETFD/F_GETFL and F_SETFD/F_SETFL takes and
        // returns plain integers; `fd` is a descriptor the caller owns.
        let flags = check(unsafe { libc::fcntl(fd, get) })?;
        // SAFETY: as above.
        check(unsafe { libc::fcntl(fd, set, flags | flag) })?;
        Ok(())
    }

    /// Open a pty pair sized `rows`×`cols`. Both ends are close-on-exec; the
    /// master is non-blocking.
    pub(super) fn open_pty(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut size = winsize(rows, cols);
        // SAFETY: the two out-pointers address live locals that openpty fills
        // with descriptors; `name` and `termp` are null (no name buffer, default
        // termios); `winp` addresses an initialised winsize that outlives the
        // call and is only read.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut::<libc::termios>(),
                &raw mut size,
            )
        };
        check(rc)?;
        // SAFETY: openpty succeeded, so both are fresh descriptors nothing else
        // owns; each is wrapped exactly once.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        add_fd_flags(
            master.as_raw_fd(),
            libc::F_GETFD,
            libc::F_SETFD,
            libc::FD_CLOEXEC,
        )?;
        add_fd_flags(
            slave.as_raw_fd(),
            libc::F_GETFD,
            libc::F_SETFD,
            libc::FD_CLOEXEC,
        )?;
        add_fd_flags(
            master.as_raw_fd(),
            libc::F_GETFL,
            libc::F_SETFL,
            libc::O_NONBLOCK,
        )?;
        Ok((master, slave))
    }

    /// Set the pty size (the kernel also signals the slave's foreground group).
    pub(super) fn set_winsize(fd: BorrowedFd<'_>, rows: u16, cols: u16) -> io::Result<()> {
        let size = winsize(rows, cols);
        // SAFETY: TIOCSWINSZ reads one winsize through the pointer, which
        // addresses an initialised local; `fd` is a live borrowed descriptor.
        check(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ as _, &size) })?;
        Ok(())
    }

    /// Wait up to `timeout` for `events` on `fd`. `Ok(false)` on timeout or
    /// interruption; any returned event (including hangup) is `Ok(true)`.
    pub(super) fn poll(
        fd: BorrowedFd<'_>,
        events: libc::c_short,
        timeout: Duration,
    ) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one pollfd, addressed by a live local, with a count of 1.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        match check(rc) {
            Ok(n) => Ok(n > 0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Send `sig` to process `pid`. The caller guarantees `pid` is an unreaped
    /// child, so it cannot name an unrelated process.
    pub(super) fn signal(pid: u32, sig: libc::c_int) -> io::Result<()> {
        let pid =
            libc::pid_t::try_from(pid).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: kill takes plain integers and has no memory preconditions.
        check(unsafe { libc::kill(pid, sig) })?;
        Ok(())
    }

    /// The highest descriptor number worth sweeping, computed before fork.
    fn fd_ceiling() -> libc::c_int {
        // SAFETY: sysconf takes and returns plain integers.
        let max = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        if max <= 0 {
            1024
        } else {
            libc::c_int::try_from(max).unwrap_or(libc::c_int::MAX)
        }
    }

    /// Make the child the leader of a new session whose controlling terminal
    /// is its stdin (the pty slave), and close every inherited descriptor above
    /// stderr when it execs.
    pub(super) fn lead_new_session_on_stdin(cmd: &mut Command) {
        let ceiling = fd_ceiling();
        // SAFETY: the hook runs in the forked child before exec, after std has
        // placed the slave on fds 0-2. It only calls async-signal-safe
        // functions (setsid, ioctl, close_range, fcntl), touches no lock or
        // allocator, and reads only the copied `ceiling`. Descriptors are
        // marked close-on-exec rather than closed so std's own exec-error pipe
        // stays usable until exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0 as libc::c_int) == -1 {
                    return Err(io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                if libc::syscall(
                    libc::SYS_close_range,
                    3 as libc::c_uint,
                    libc::c_uint::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                ) == 0
                {
                    return Ok(());
                }
                for fd in 3..ceiling {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                Ok(())
            });
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[derive(Default)]
    struct RecordingControl {
        calls: Mutex<Vec<String>>,
    }

    impl AttachControl for RecordingControl {
        fn select_window(&self, window: u32) -> Result<()> {
            lock(&self.calls).push(format!("window {window}"));
            Ok(())
        }
        fn select_pane(&self, pane: u32) -> Result<()> {
            lock(&self.calls).push(format!("pane {pane}"));
            Ok(())
        }
        fn zoom_toggle(&self, pane: u32) -> Result<()> {
            lock(&self.calls).push(format!("zoom {pane}"));
            Ok(())
        }
        fn layout(&self, session: &str) -> Result<LayoutSnapshot> {
            lock(&self.calls).push(format!("layout {session}"));
            Ok(LayoutSnapshot::default())
        }
    }

    /// A pty double over a socketpair: the sender writes to `ours`, the test
    /// reads what arrived from the returned peer.
    fn double() -> (Arc<Pty>, UnixStream) {
        let (ours, peer) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        (Arc::new(Pty::new(OwnedFd::from(ours), None)), peer)
    }

    fn sender(
        read_only: bool,
    ) -> (
        TmuxMuxSender,
        Arc<ModeState>,
        Arc<RecordingControl>,
        UnixStream,
    ) {
        let (pty, peer) = double();
        let modes = Arc::new(ModeState::default());
        let ctl = Arc::new(RecordingControl::default());
        let s = TmuxMuxSender::new(pty, Arc::clone(&modes), ctl.clone(), "work", read_only);
        (s, modes, ctl, peer)
    }

    fn drain(peer: &mut UnixStream) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("read: {e}"),
            }
        }
        out
    }

    fn scan(chunks: &[&[u8]]) -> ModeState {
        let modes = ModeState::default();
        let mut scanner = DecModeScanner::default();
        for chunk in chunks {
            scanner.feed(chunk, &modes);
        }
        modes
    }

    #[test]
    fn scanner_joins_a_sequence_split_across_chunks() {
        let modes = scan(&[b"text\x1b[?10", b"06h more"]);
        assert!(modes.sgr_mouse());
        assert!(!modes.mouse_any());
    }

    #[test]
    fn scanner_handles_every_split_point() {
        let seq = b"ab\x1b[?2004hcd";
        for cut in 0..=seq.len() {
            let (a, b) = seq.split_at(cut);
            assert!(scan(&[a, b]).bracketed_paste(), "split at {cut}");
        }
    }

    #[test]
    fn scanner_sets_and_resets_tracked_modes() {
        let modes = ModeState::default();
        let mut scanner = DecModeScanner::default();
        scanner.feed(b"\x1b[?1000h\x1b[?1002;1006h", &modes);
        assert!(modes.mouse_any() && modes.sgr_mouse());
        scanner.feed(b"\x1b[?1000l", &modes);
        assert!(modes.mouse_any(), "1002 is still on");
        scanner.feed(b"\x1b[?1002l\x1b[?1006l", &modes);
        assert!(!modes.mouse_any() && !modes.sgr_mouse());
        scanner.feed(b"\x1b[?1003h", &modes);
        assert!(modes.mouse_any());
    }

    #[test]
    fn scanner_ignores_other_sequences() {
        let modes = scan(&[b"\x1b[?25h\x1b[1000h\x1b[?1000x\x1b[?1049h\x1b[?20\x1b[04h"]);
        assert!(!modes.mouse_any());
        assert!(!modes.bracketed_paste());
        // An interrupted sequence does not swallow the next one.
        let modes = scan(&[b"\x1b[?10\x1b[?2004h"]);
        assert!(modes.bracketed_paste());
    }

    #[test]
    fn wheel_encodes_as_one_based_sgr() {
        assert_eq!(
            encode_wheel(MuxMouseKind::WheelUp, 0, 0, true).unwrap(),
            b"\x1b[<64;1;1M"
        );
        assert_eq!(
            encode_wheel(MuxMouseKind::WheelDown, 9, 4, true).unwrap(),
            b"\x1b[<65;10;5M"
        );
        assert_eq!(
            encode_wheel(MuxMouseKind::WheelUp, 300, 400, true).unwrap(),
            b"\x1b[<64;301;401M"
        );
    }

    #[test]
    fn wheel_falls_back_to_legacy_encoding() {
        assert_eq!(
            encode_wheel(MuxMouseKind::WheelDown, 0, 2, false).unwrap(),
            vec![0x1b, b'[', b'M', 97, 33, 35]
        );
        assert!(encode_wheel(MuxMouseKind::WheelUp, 223, 0, false).is_none());
        assert!(encode_wheel(MuxMouseKind::WheelUp, 222, 0, false).is_some());
    }

    #[test]
    fn bracketed_paste_wraps_only_pasted_text() {
        let wrapped = |t: &str| [PASTE_START, t.as_bytes(), PASTE_END].concat();
        assert_eq!(&*input_payload("hello", true), &wrapped("hello")[..]);
        assert_eq!(&*input_payload("a\nb", true), &wrapped("a\nb")[..]);
        assert_eq!(&*input_payload("\n", true), &wrapped("\n")[..]);
        // Off, single characters and key sequences are written as they are.
        assert_eq!(&*input_payload("hello", false), b"hello");
        assert_eq!(&*input_payload("x", true), b"x");
        assert_eq!(&*input_payload("\x1b[A", true), b"\x1b[A");
        assert_eq!(&*input_payload("ls\r", true), b"ls\r");
        let bracketed = "\x1b[200~ls\x1b[201~";
        assert_eq!(&*input_payload(bracketed, true), bracketed.as_bytes());
    }

    #[test]
    fn sender_writes_input_and_brackets_pastes() {
        let (mut s, modes, _, mut peer) = sender(false);
        s.send_input_bytes(b"\x1b[B".to_vec()).unwrap();
        s.send_input_chars("hi").unwrap();
        assert_eq!(drain(&mut peer), b"\x1b[Bhi");
        modes.apply(2004, true);
        s.send_input_chars("hi").unwrap();
        assert_eq!(drain(&mut peer), b"\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn sender_drops_wheel_unless_mouse_reporting_is_on() {
        let (mut s, modes, _, mut peer) = sender(false);
        s.send_mouse(MuxMouseKind::WheelUp, 3, 4).unwrap();
        assert!(drain(&mut peer).is_empty());
        assert_eq!(s.dropped_mouse.load(Ordering::Relaxed), 1);
        modes.apply(1000, true);
        modes.apply(1006, true);
        s.send_mouse(MuxMouseKind::WheelUp, 3, 4).unwrap();
        assert_eq!(drain(&mut peer), b"\x1b[<64;4;5M");
    }

    #[test]
    fn read_only_sender_writes_nothing() {
        let (mut s, modes, _, mut peer) = sender(true);
        modes.apply(1000, true);
        modes.apply(1006, true);
        s.send_input_chars("rm -rf ~\r").unwrap();
        s.send_input_bytes(b"\x03".to_vec()).unwrap();
        s.send_mouse(MuxMouseKind::WheelDown, 0, 0).unwrap();
        s.box_clone().send_input_chars("x").unwrap();
        assert!(drain(&mut peer).is_empty());
    }

    #[test]
    fn routed_controls_go_through_attach_control() {
        let (mut s, _, ctl, mut peer) = sender(false);
        s.go_to_tab(2).unwrap();
        s.focus_pane(PaneRef::terminal(7)).unwrap();
        let hint = FullscreenHint {
            is_floating: false,
            floating_visible: false,
            is_focused_floating: false,
            tab_fullscreen: false,
            target_is_relay_focus: true,
        };
        s.toggle_fullscreen(PaneRef::terminal(7), hint).unwrap();
        assert!(s.has_sync_layout());
        assert!(s.query_layout_result().unwrap().is_ok());
        s.query_layout().unwrap();
        assert!(s.go_to_tab(u64::MAX).is_err());
        assert!(s.focus_pane(PaneRef::plugin(1)).is_err());
        assert_eq!(
            *lock(&ctl.calls),
            ["window 2", "pane 7", "zoom 7", "layout work"]
        );
        assert!(drain(&mut peer).is_empty(), "controls never touch the pty");
    }

    #[test]
    fn closed_sender_refuses_writes() {
        let (mut s, _, _, _peer) = sender(false);
        let mut clone = s.box_clone();
        s.send_client_exited().unwrap();
        assert!(clone.send_input_chars("x").is_err());
    }

    #[test]
    fn receiver_forwards_tracks_modes_then_exits_once() {
        let (pty, mut peer) = double();
        let modes = Arc::new(ModeState::default());
        let mut rx = TmuxMuxReceiver::new(pty, Arc::clone(&modes));
        peer.write_all(b"\x1b[?1049h\x1b[?10").unwrap();
        match rx.recv() {
            Some(MuxServerMsg::Render(b)) => assert_eq!(b, b"\x1b[?1049h\x1b[?10"),
            other => panic!("expected render, got {other:?}"),
        }
        peer.write_all(b"00h").unwrap();
        assert!(matches!(rx.recv(), Some(MuxServerMsg::Render(_))));
        assert!(modes.mouse_any());
        peer.write_all(b"\x1b[?1049l[detached (from session work)]\r\n")
            .unwrap();
        assert!(matches!(rx.recv(), Some(MuxServerMsg::Render(_))));
        drop(peer);
        match rx.recv() {
            Some(MuxServerMsg::Event(MuxEvent::Exit { reason })) => {
                assert_eq!(reason, "detached (from session work)")
            }
            other => panic!("expected exit, got {other:?}"),
        }
        assert!(rx.recv().is_none());
        assert!(rx.recv().is_none());
    }

    #[test]
    fn receiver_wakes_with_other_while_idle() {
        let (pty, _peer) = double();
        let mut rx = TmuxMuxReceiver::new(pty, Arc::new(ModeState::default()));
        assert!(matches!(rx.recv(), Some(MuxServerMsg::Other)));
    }

    #[test]
    fn exit_reason_prefers_tmux_message() {
        assert_eq!(exit_reason(b"\x1b[H\x1b[2J[exited]\r\n"), "exited");
        assert_eq!(
            exit_reason(b"\x1b[?1049l[server exited]\r\n"),
            "server exited"
        );
        assert_eq!(
            exit_reason(b"can't find session: =nope\r\n"),
            "can't find session: =nope"
        );
        // Screen content is never mistaken for a reason.
        assert_eq!(exit_reason(b"\x1b[1;1Huser@host:~$ "), DEFAULT_EXIT_REASON);
        assert_eq!(exit_reason(b""), DEFAULT_EXIT_REASON);
        assert_eq!(exit_reason(b"\x1b]0;title\x07[lost tty]"), "lost tty");
    }

    #[test]
    fn attach_args_target_the_exact_session() {
        let socket = TmuxSocket {
            path: PathBuf::from("/tmp/tmux-1/s"),
        };
        let args = |ro| {
            attach_args(&socket, "dev", ro)
                .into_iter()
                .map(|a| a.into_string().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            args(false),
            ["-S", "/tmp/tmux-1/s", "attach-session", "-t", "=dev"]
        );
        assert_eq!(
            args(true),
            ["-S", "/tmp/tmux-1/s", "attach-session", "-r", "-t", "=dev"]
        );
    }

    #[test]
    fn child_env_is_fixed_and_carries_a_locale() {
        let env = child_env(None, None);
        let get = |env: &[(&str, String)], k: &str| {
            env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        };
        assert_eq!(get(&env, "TERM").as_deref(), Some("xterm-256color"));
        assert_eq!(get(&env, "COLORTERM").as_deref(), Some("truecolor"));
        assert_eq!(get(&env, "LANG").as_deref(), Some("C.UTF-8"));
        assert_eq!(get(&env, "LC_ALL").as_deref(), Some("C.UTF-8"));
        assert_eq!(get(&env, "TMUX"), None);
        assert_eq!(env.len(), 4);
        let env = child_env(Some("en_GB.UTF-8".into()), None);
        assert_eq!(get(&env, "LC_ALL").as_deref(), Some("en_GB.UTF-8"));
        let env = child_env(Some("en_GB.UTF-8".into()), Some("de_DE.UTF-8".into()));
        assert_eq!(get(&env, "LANG").as_deref(), Some("en_GB.UTF-8"));
        assert_eq!(get(&env, "LC_ALL").as_deref(), Some("de_DE.UTF-8"));
    }

    /// Live tests against a private tmux server (`-L <unique>`, no config),
    /// killed when the test ends. Skipped when tmux is not on `PATH`.
    mod live {
        use super::*;

        const SESSION: &str = "t5";

        struct PrivateServer {
            bin: PathBuf,
            name: String,
            socket: TmuxSocket,
        }

        impl PrivateServer {
            fn start() -> Option<Self> {
                let Ok(bin) = resolve_tmux_bin() else {
                    eprintln!("skipping: tmux not on PATH");
                    return None;
                };
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let name = format!("muxr-attach-test-{}-{nanos}", std::process::id());
                let mut server = Self {
                    bin,
                    name,
                    socket: TmuxSocket {
                        path: PathBuf::new(),
                    },
                };
                server.tmux(&[
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    SESSION,
                    "-x",
                    "80",
                    "-y",
                    "24",
                    "sh",
                ]);
                let path = server.tmux(&["display-message", "-p", "#{socket_path}"]);
                server.socket.path = PathBuf::from(path);
                Some(server)
            }

            fn tmux(&self, args: &[&str]) -> String {
                let out = Command::new(&self.bin)
                    .arg("-L")
                    .arg(&self.name)
                    .args(args)
                    .env_remove("TMUX")
                    .stdin(Stdio::null())
                    .output()
                    .expect("run tmux");
                assert!(
                    out.status.success(),
                    "tmux {args:?}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8_lossy(&out.stdout).trim().to_owned()
            }

            fn window_width(&self) -> String {
                // A pane target needs the trailing colon: a bare `=t5` names
                // no pane and formats as empty.
                self.tmux(&["display-message", "-p", "-t", "=t5:", "#{window_width}"])
            }
        }

        impl Drop for PrivateServer {
            fn drop(&mut self) {
                let _ = Command::new(&self.bin)
                    .arg("-L")
                    .arg(&self.name)
                    .arg("kill-server")
                    .env_remove("TMUX")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                // tmux leaves the socket file behind; it is this test's own.
                if self.socket.path.file_name().is_some() {
                    let _ = std::fs::remove_file(&self.socket.path);
                }
            }
        }

        struct NoControl;

        impl AttachControl for NoControl {
            fn select_window(&self, _: u32) -> Result<()> {
                Ok(())
            }
            fn select_pane(&self, _: u32) -> Result<()> {
                Ok(())
            }
            fn zoom_toggle(&self, _: u32) -> Result<()> {
                Ok(())
            }
            fn layout(&self, _: &str) -> Result<LayoutSnapshot> {
                Ok(LayoutSnapshot::default())
            }
        }

        /// Read renders until `seen` holds `needle` or `timeout` passes.
        fn wait_for(rx: &mut dyn MuxReceiver, needle: &[u8], timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            let mut seen = Vec::new();
            while Instant::now() < deadline {
                match rx.recv() {
                    Some(MuxServerMsg::Render(b)) => {
                        seen.extend_from_slice(&b);
                        if seen.windows(needle.len()).any(|w| w == needle) {
                            return true;
                        }
                    }
                    Some(MuxServerMsg::Other) => {}
                    other => panic!("stream ended early: {other:?}"),
                }
            }
            false
        }

        fn open_count() -> usize {
            std::fs::read_dir("/proc/self/fd").unwrap().count()
        }

        /// Children of this process (live or zombie), from `/proc/*/stat`.
        fn child_processes() -> Vec<String> {
            let me = std::process::id().to_string();
            let mut found = Vec::new();
            for entry in std::fs::read_dir("/proc").unwrap().flatten() {
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                // pid (comm) state ppid ...: comm may hold spaces, so split
                // after its closing parenthesis.
                let Some((_, rest)) = stat.rsplit_once(')') else {
                    continue;
                };
                let fields: Vec<&str> = rest.split_whitespace().collect();
                if fields.get(1) == Some(&me.as_str()) {
                    found.push(stat);
                }
            }
            found
        }

        #[test]
        #[ignore = "needs tmux; owns a private tmux server"]
        fn attach_renders_alt_screen_and_follows_resize() {
            let Some(server) = PrivateServer::start() else {
                return;
            };
            let handle = open(&server.socket, SESSION, 30, 90, false, Arc::new(NoControl)).unwrap();
            assert_eq!(handle.session_name, SESSION);
            assert!(handle.resumed_view.is_none());
            let (mut tx, mut rx) = handle.split();
            assert!(
                wait_for(&mut *rx, b"\x1b[?1049h", Duration::from_secs(2)),
                "no alternate-screen switch within 2 s"
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            while server.window_width() != "90" && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(server.window_width(), "90");

            tx.send_resize(40, 120).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while server.window_width() != "120" && Instant::now() < deadline {
                // Keep the pty drained so the client can redraw.
                let _ = rx.recv();
            }
            assert_eq!(server.window_width(), "120");

            // Typed bytes reach the pane.
            tx.send_input_chars("echo muxr-t5-$((6*7))\r").unwrap();
            assert!(wait_for(&mut *rx, b"muxr-t5-42", Duration::from_secs(2)));

            tx.send_client_exited().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut exited = false;
            while Instant::now() < deadline {
                match rx.recv() {
                    Some(MuxServerMsg::Event(MuxEvent::Exit { reason })) => {
                        assert!(!reason.is_empty());
                        exited = true;
                        break;
                    }
                    Some(_) => {}
                    None => panic!("None before Exit"),
                }
            }
            assert!(exited, "no Exit after send_client_exited");
            assert!(rx.recv().is_none());
        }

        #[test]
        #[ignore = "needs tmux; owns a private tmux server"]
        fn open_close_cycles_leak_no_fd_or_process() {
            let Some(server) = PrivateServer::start() else {
                return;
            };
            let cycle = |i: usize| {
                let read_only = i % 3 == 2;
                let handle = open(
                    &server.socket,
                    SESSION,
                    24,
                    80,
                    read_only,
                    Arc::new(NoControl),
                )
                .unwrap();
                let (mut tx, mut rx) = handle.split();
                assert!(
                    wait_for(&mut *rx, b"\x1b[?1049h", Duration::from_secs(2)),
                    "cycle {i}: no screen"
                );
                if i.is_multiple_of(2) {
                    tx.send_client_exited().unwrap();
                }
                drop(tx);
                drop(rx);
            };
            cycle(0);
            let fds = open_count();
            for i in 1..=100 {
                cycle(i);
            }
            assert_eq!(
                open_count(),
                fds,
                "descriptor count changed over 100 cycles"
            );
            assert!(
                child_processes().is_empty(),
                "children left behind: {:?}",
                child_processes()
            );
            // The server saw every client leave.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !server.tmux(&["list-clients"]).is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(server.tmux(&["list-clients"]), "");
        }
    }
}
