//! tmux integration suite: the live gate for the tmux backend.
//!
//! Every test here is `#[ignore]`-gated so `cargo test -p muxrd` stays green on
//! a machine without tmux. Run them against a real tmux 3.5a or newer:
//!
//! ```text
//! # The tmux on PATH:
//! cargo test -p muxrd --test tmux_integration -- --ignored --test-threads=1
//!
//! # Another tmux binary (a source build, say):
//! MUXRD_TMUX_BIN=/opt/tmux-3.8/bin/tmux \
//!   cargo test -p muxrd --test tmux_integration -- --ignored --test-threads=1
//!
//! # With the flood test's CPU bound enforced (optimized builds only):
//! cargo test --release -p muxrd --test tmux_integration -- --ignored --test-threads=1
//!
//! # Inside the docker rig's tmux image (tmux 3.8 from source): see
//! # docker/README.md, "Running the tmux suite in the rig".
//! ```
//!
//! Each test starts its own private tmux server: it sets `MUXRD_TMUX_SOCKET` to
//! a unique bare name (a socket under `$TMUX_TMPDIR`, else `/tmp`, in
//! `tmux-<uid>/`, a directory the test creates owner-only when it is missing),
//! starts the server with `-f /dev/null` (no user
//! configuration) and `/bin/sh` as the default shell, and kills it and removes
//! its socket in a `Drop` guard, so the suite never touches the user's own
//! server. Because the socket is chosen through the process environment, the
//! tests run serially: pass `--test-threads=1` as above (a process-wide lock
//! also serialises them if it is forgotten).
//!
//! ## What each test exercises
//!
//! | test | exercises |
//! |------|-----------|
//! | `probe_passes_and_version_matches_tmux_v` | `detect::probe_tmux()` and `backend_version()` against `tmux -V` |
//! | `session_create_list_rename_kill` | `create_session` / `list_sessions` / `rename_session` / `kill_session` |
//! | `fresh_session_is_one_tab_one_pane` | `query_layout` and `query_session_size` of a new 80x24 session |
//! | `new_floating_pane_follows_the_version` | `new_pane(floating)`: a floating pane on 3.7+, an honest failed ack below |
//! | `toggle_fullscreen_flips_zoom` | `toggle_pane_fullscreen` sets and clears `is_fullscreen` / `fullscreen_active` |
//! | `write_to_pane_reaches_the_shell` | `write_to_pane` bytes run in the pane (`capture-pane -p`) |
//! | `attach_renders_resizes_and_takes_input` | `open_attach`: alternate screen, `send_resize`, `send_input_bytes` |
//! | `read_only_attach_never_changes_the_pane` | a read-only attach drops input and wheel, refuses focus changes |
//! | `wheel_follows_the_mouse_option` | `send_mouse` with `mouse off` (dropped) and `mouse on` (copy mode) |
//! | `agent_report_drives_status_and_list_agents` | OSC 7501 in a pane -> `AgentStatusChanged` -> `list_agents`, then `state=clear` |
//! | `second_session_has_its_own_watcher` | one control client per session; events labelled by session |
//! | `killing_a_watched_session_closes_its_panes` | `kill_session` of a watched session -> `PaneClosed` |
//! | `flood_keeps_the_watcher_cheap` | `yes \| head -c 50M` in a pane: CPU (optimized builds), control client, reports after it |
//! | `osc133_marks_drive_command_running` | tmux 3.8 only: `pane_command_running` follows OSC 133 marks |
//! | `toggle_floating_floats_then_tiles` | tmux 3.8 only: `toggle_pane_floating` both ways |
//!
//! The 3.8-only tests return early (and say so) on an older tmux. Everything
//! is driven through tmux's own command line and a pty-attached client; tmux
//! runs as a separate, unmodified binary.

use std::ffi::OsString;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use muxrd::multiplexer::detect::probe_tmux;
use muxrd::multiplexer::events::{self, EventBus};
use muxrd::multiplexer::tmux::commands::Tmux;
use muxrd::multiplexer::tmux::paths::{TMUX_BIN_ENV, TMUX_SOCKET_ENV, TmuxVersion};
use muxrd::multiplexer::{
    AgentStatus, FullscreenHint, LayoutSnapshot, MuxBackend, MuxMouseKind, MuxReceiver,
    MuxServerMsg, PaneRef, PaneSnapshot, TmuxBackend, spawn_tmux_event_kernel,
};

// ─── constants ──────────────────────────────────────────────────────────────────

/// How long a state change made by a tmux command may take to show.
const WAIT: Duration = Duration::from_secs(2);

/// How long the kernel may take to put a control client on a session: it
/// scans the server every 5 s.
const WATCH_WAIT: Duration = Duration::from_secs(8);

/// How long a status report, re-sent every [`REPORT_RESEND`], may take to
/// reach the bus. A watcher whose control client is up may still be taking
/// its first pane listing, and output from a pane it does not track yet is
/// not scanned, so the first report can be missed on a loaded host.
const REPORT_WAIT: Duration = Duration::from_secs(5);

/// How often a report that has not arrived is printed again.
const REPORT_RESEND: Duration = Duration::from_millis(300);

// ─── the private server ─────────────────────────────────────────────────────────

/// Serialises the tests: the socket is chosen through the environment.
static SERIAL: Mutex<()> = Mutex::new(());

/// A unique suffix for socket and session names.
fn seq() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// The tmux binary the suite runs: `MUXRD_TMUX_BIN`, default `tmux`.
fn tmux_bin() -> OsString {
    std::env::var_os(TMUX_BIN_ENV)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "tmux".into())
}

/// The raw `tmux -V` line of the binary under test.
fn tmux_v() -> String {
    let out = Command::new(tmux_bin())
        .arg("-V")
        .output()
        .expect("run tmux -V (set MUXRD_TMUX_BIN)");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A private tmux server on a unique socket and a backend pointed at it.
///
/// Dropping it kills the server (by signal when `kill-server` leaves it
/// running) and removes the socket file.
struct Server {
    backend: TmuxBackend,
    socket: PathBuf,
    /// The server's process id, once it is known.
    pid: Option<libc::pid_t>,
    _serial: MutexGuard<'static, ()>,
}

impl Server {
    fn start() -> Self {
        let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("muxrd-it-{}-{nanos}-{}", std::process::id(), seq());
        // SAFETY: the tests run serially (`--test-threads=1` and `SERIAL`), so
        // no other thread reads the environment while it changes.
        unsafe { std::env::set_var(TMUX_SOCKET_ENV, &name) };
        let backend = TmuxBackend::from_env().expect("TmuxBackend::from_env (set MUXRD_TMUX_BIN)");
        let socket = backend.socket().path.clone();
        assert!(
            socket.ends_with(&name),
            "the backend does not use the private socket: {}",
            socket.display()
        );
        // The bare name resolves to `$TMUX_TMPDIR` (or `/tmp`) joined with
        // `tmux-<uid>/<name>`, and tmux reaches it with `-S`, which never
        // creates the directory: on a fresh host it does not exist yet. tmux
        // wants it owner-only, as it makes its own.
        if let Some(dir) = socket.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .unwrap_or_else(|e| panic!("create socket directory {}: {e}", dir.display()));
        }
        let mut server = Self {
            backend,
            socket,
            pid: None,
            _serial: serial,
        };
        // The server starts here, with no configuration file and a fixed
        // shell, and stays up while it holds no session.
        server.tmux(&[
            "-f",
            "/dev/null",
            "start-server",
            ";",
            "set-option",
            "-s",
            "exit-empty",
            "off",
            ";",
            "set-option",
            "-g",
            "default-shell",
            "/bin/sh",
        ]);
        server.pid = server
            .tmux(&["display-message", "-p", "#{pid}"])
            .parse()
            .ok();
        server
    }

    fn version(&self) -> &TmuxVersion {
        self.backend.version()
    }

    /// Run tmux against this server.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(tmux_bin())
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .output()
            .expect("run tmux")
    }

    /// Run tmux against this server and return its trimmed stdout, panicking
    /// with stderr on failure.
    fn tmux(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Create a session through the backend and return its name.
    fn session(&self, tag: &str) -> String {
        let name = format!("it-{tag}-{}", seq());
        let ack = self
            .backend
            .create_session(&name, None)
            .expect("create_session");
        assert!(ack.ok, "create_session({name}) failed: {ack:?}");
        name
    }

    fn layout(&self, session: &str) -> LayoutSnapshot {
        self.backend.query_layout(session).expect("query_layout")
    }

    /// The pane with id `pane` in the session's layout.
    fn pane(&self, session: &str, pane: u32) -> Option<PaneSnapshot> {
        self.layout(session)
            .tabs
            .into_iter()
            .flat_map(|t| t.panes)
            .find(|p| p.id == pane)
    }

    /// The first pane of the session's first tab.
    fn first_pane(&self, session: &str) -> u32 {
        let layout = self.layout(session);
        layout
            .tabs
            .first()
            .and_then(|t| t.panes.first())
            .map(|p| p.id)
            .unwrap_or_else(|| panic!("session {session} has no pane: {layout:?}"))
    }

    /// Split the session's current window and return the new pane's id.
    fn split(&self, session: &str) -> u32 {
        let ack = self
            .backend
            .new_pane(session, false, None)
            .expect("new_pane(tiled)");
        assert!(ack.ok, "new_pane(tiled) failed: {ack:?}");
        ack.info
            .as_deref()
            .and_then(|i| i.parse().ok())
            .unwrap_or_else(|| panic!("new_pane reported no pane id: {ack:?}"))
    }

    /// Type `bytes` into `pane` through the backend.
    fn type_into(&self, session: &str, pane: u32, bytes: &[u8]) {
        let ack = self
            .backend
            .write_to_pane(session, PaneRef::terminal(pane), bytes.to_vec())
            .expect("write_to_pane");
        assert!(ack.ok, "write_to_pane failed: {ack:?}");
    }

    /// The visible text of `pane` (`capture-pane -p`).
    fn capture(&self, pane: u32) -> String {
        self.tmux(&["capture-pane", "-p", "-t", &format!("%{pane}")])
    }

    /// Expand `format` against `pane`.
    fn pane_format(&self, pane: u32, format: &str) -> String {
        self.tmux(&["display-message", "-p", "-t", &format!("%{pane}"), format])
    }

    /// `#{window_width}` of the session's current window. A pane target
    /// needs the trailing colon: a bare `=name` names no pane.
    fn window_width(&self, session: &str) -> String {
        self.tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("={session}:"),
            "#{window_width}",
        ])
    }

    /// Names of the control-mode clients attached to `session`.
    fn control_clients(&self, session: &str) -> Vec<String> {
        let out = self.run(&[
            "list-clients",
            "-t",
            &format!("={session}"),
            "-F",
            "#{client_control_mode} #{client_name}",
        ]);
        if !out.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("1 "))
            .map(str::to_owned)
            .collect()
    }

    /// Wait for the kernel's control client on `session`. The watcher may not
    /// have listed the session's panes yet: report with [`Kernel::report`].
    fn await_watcher(&self, session: &str) -> Vec<String> {
        assert!(
            wait_until(WATCH_WAIT, || !self.control_clients(session).is_empty()),
            "no control client on session {session} within {WATCH_WAIT:?}"
        );
        self.control_clients(session)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new(tmux_bin())
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .env_remove("TMUX")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(pid) = self.pid {
            // SAFETY: kill has no memory preconditions; signal 0 only checks
            // that `pid` (this test's own tmux server) still exists.
            let alive = || unsafe { libc::kill(pid, 0) } == 0;
            if !wait_until(Duration::from_secs(2), || !alive()) {
                eprintln!("[tmux it] kill-server left server {pid} running; killing it");
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        // tmux leaves the socket file behind; it is this test's own.
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Poll `f` every 20 ms until it holds or `timeout` passes.
fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `haystack` contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ─── the attach stream ──────────────────────────────────────────────────────────

/// An attach receiver drained on its own thread into one byte buffer.
struct Stream {
    bytes: Arc<Mutex<Vec<u8>>>,
    ended: Arc<AtomicBool>,
}

impl Stream {
    fn start(mut receiver: Box<dyn MuxReceiver>) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let ended = Arc::new(AtomicBool::new(false));
        let (b, e) = (Arc::clone(&bytes), Arc::clone(&ended));
        std::thread::spawn(move || {
            while let Some(msg) = receiver.recv() {
                if let MuxServerMsg::Render(chunk) = msg {
                    b.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend_from_slice(&chunk);
                }
            }
            e.store(true, Ordering::SeqCst);
        });
        Self { bytes, ended }
    }

    /// Bytes received so far.
    fn len(&self) -> usize {
        self.bytes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Wait until `needle` shows up after offset `from`.
    fn wait_for(&self, needle: &[u8], from: usize, timeout: Duration) -> bool {
        wait_until(timeout, || {
            let seen = self.bytes.lock().unwrap_or_else(PoisonError::into_inner);
            seen.len() > from && contains(&seen[from..], needle)
        })
    }

    fn ends(&self) -> bool {
        wait_until(Duration::from_secs(3), || self.ended.load(Ordering::SeqCst))
    }
}

// ─── the event kernel ───────────────────────────────────────────────────────────

/// The tmux event kernel running in a small tokio runtime, with one receiver
/// on its bus.
struct Kernel {
    rx: tokio::sync::broadcast::Receiver<events::MuxEvent>,
    shutdown: tokio::sync::watch::Sender<bool>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Kernel {
    fn start(backend: &TmuxBackend) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (bus, rx): (EventBus, _) = tokio::sync::broadcast::channel(events::EVENT_BUS_CAPACITY);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        {
            let _entered = runtime.enter();
            spawn_tmux_event_kernel(backend, backend.agent_state(), bus, shutdown_rx);
        }
        Self {
            rx,
            shutdown,
            runtime: Some(runtime),
        }
    }

    /// The first event matching `pick` within `timeout`; other events are
    /// skipped.
    fn wait_event<T>(
        &mut self,
        timeout: Duration,
        mut pick: impl FnMut(&events::MuxEvent) -> Option<T>,
    ) -> Option<T> {
        use tokio::sync::broadcast::error::TryRecvError;
        let deadline = Instant::now() + timeout;
        loop {
            match self.rx.try_recv() {
                Ok(event) => {
                    if let Some(found) = pick(&event) {
                        return Some(found);
                    }
                    continue;
                }
                Err(TryRecvError::Lagged(n)) => {
                    println!("[tmux it] bus lagged by {n} events");
                    continue;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Closed) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait for an `AgentStatusChanged` of `pane` with `status`.
    fn wait_status(
        &mut self,
        pane: u32,
        status: AgentStatus,
        timeout: Duration,
    ) -> Option<events::AgentStatusChanged> {
        self.wait_event(timeout, |event| match event {
            events::MuxEvent::AgentStatusChanged(change)
                if change.pane == pane && change.status == status =>
            {
                Some(change.clone())
            }
            _ => None,
        })
    }

    /// Print the OSC 7501 report `body` in `pane`, again every
    /// [`REPORT_RESEND`], until an `AgentStatusChanged` of `pane` with
    /// `status` arrives; panics after `timeout` ([`REPORT_WAIT`] in most
    /// tests).
    ///
    /// Re-sending covers a watcher still taking its first pane listing, which
    /// does not scan output of a pane it does not track yet. A report repeated
    /// after it applied changes nothing, so no extra event follows.
    fn report(
        &mut self,
        server: &Server,
        (session, pane): (&str, u32),
        body: &str,
        status: AgentStatus,
        timeout: Duration,
    ) -> events::AgentStatusChanged {
        let deadline = Instant::now() + timeout;
        loop {
            server.type_into(session, pane, &osc7501(body));
            if let Some(change) = self.wait_status(pane, status, REPORT_RESEND) {
                return change;
            }
            if Instant::now() >= deadline {
                panic!("no {status:?} for pane {pane} within {timeout:?} of `{body}`");
            }
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

/// A pane command that prints an OSC 7501 status report with `ESC \` as ST.
fn osc7501(body: &str) -> Vec<u8> {
    format!("printf '\\033]7501;{body}\\033\\\\'\r").into_bytes()
}

// ─── watcher pause counter ──────────────────────────────────────────────────────

/// Pauses the watcher has logged. The watcher keeps its pause count to
/// itself and reports each pause in a debug record, so the suite installs a
/// logger that counts those records and passes warnings through to stderr.
static PAUSES: AtomicU64 = AtomicU64::new(0);

struct CountingLogger;

impl log::Log for CountingLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        if record.target().contains("tmux::watch") {
            let text = record.args().to_string();
            if text.contains(" paused (") {
                PAUSES.fetch_add(1, Ordering::Relaxed);
            }
        }
        if record.level() <= log::Level::Warn {
            eprintln!(
                "[{}] {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

fn install_logger() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        if log::set_boxed_logger(Box::new(CountingLogger)).is_ok() {
            log::set_max_level(log::LevelFilter::Debug);
        }
    });
}

/// User plus system CPU time of this process, from `/proc/self/stat`.
fn process_cpu() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    // pid (comm) state ...: comm may hold spaces, so split after its closing
    // parenthesis; utime and stime are fields 14 and 15 of the whole line.
    let (_, rest) = stat.rsplit_once(')').expect("a stat line");
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = u64::try_from(hz).ok().filter(|h| *h > 0).unwrap_or(100);
    Duration::from_millis(ticks * 1000 / hz)
}

// ─── probe and version ──────────────────────────────────────────────────────────

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn probe_passes_and_version_matches_tmux_v() {
    let server = Server::start();
    assert!(probe_tmux(), "probe_tmux() failed against {:?}", tmux_bin());
    let raw = tmux_v();
    let version = server.backend.backend_version();
    println!("[tmux it] tmux -V = {raw:?}, backend_version = {version:?}");
    // `tmux next-3.9` is reported as `tmux 3.9`.
    assert_eq!(version, raw.replacen("tmux next-", "tmux ", 1));
    assert_eq!(*server.version(), TmuxVersion::parse(&raw).unwrap());
}

// ─── session lifecycle ──────────────────────────────────────────────────────────

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn session_create_list_rename_kill() {
    let server = Server::start();
    let b = &server.backend;
    let name = server.session("life");
    let listed = |n: &str| {
        b.list_sessions()
            .expect("list_sessions")
            .iter()
            .any(|(s, _)| s == n)
    };
    assert!(listed(&name), "{name} not listed after create");

    let renamed = format!("{name}-renamed");
    let ack = b.rename_session(&name, renamed.clone()).unwrap();
    assert!(ack.ok, "rename_session failed: {ack:?}");
    assert!(listed(&renamed), "{renamed} not listed after rename");
    assert!(!listed(&name), "{name} still listed after rename");

    let ack = b.kill_session(&renamed).unwrap();
    assert!(ack.ok, "kill_session failed: {ack:?}");
    assert!(!listed(&renamed), "{renamed} still listed after kill");
}

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn fresh_session_is_one_tab_one_pane() {
    let server = Server::start();
    let session = server.session("fresh");
    let layout = server.layout(&session);
    assert_eq!(layout.tabs.len(), 1, "{layout:?}");
    let tab = &layout.tabs[0];
    assert!(tab.active, "{tab:?}");
    assert!(!tab.fullscreen_active, "{tab:?}");
    assert!(!tab.floating_panes_visible, "{tab:?}");
    assert_eq!(tab.panes.len(), 1, "{tab:?}");
    let pane = &tab.panes[0];
    // A detached session is created at 80x24; with no client attached the
    // status line takes nothing from the window.
    assert_eq!(
        (pane.x, pane.y, pane.cols, pane.rows),
        (0, 0, 80, 24),
        "{pane:?}"
    );
    assert!(
        pane.is_focused && !pane.is_floating && !pane.is_fullscreen,
        "{pane:?}"
    );
    assert!(!pane.is_plugin && !pane.exited, "{pane:?}");
    assert_eq!(
        server.backend.query_session_size(&session).unwrap(),
        (24, 80)
    );
}

// ─── pane operations ────────────────────────────────────────────────────────────

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn new_floating_pane_follows_the_version() {
    let server = Server::start();
    let session = server.session("float");
    let ack = server.backend.new_pane(&session, true, None).unwrap();
    if server.version().at_least(3, 7) {
        assert!(
            ack.ok,
            "new_pane(floating) failed on tmux {}: {ack:?}",
            server.version()
        );
        let id: u32 = ack.info.as_deref().unwrap().parse().unwrap();
        let pane = server
            .pane(&session, id)
            .expect("the new pane in the layout");
        assert!(pane.is_floating, "{pane:?}");
        let tab = &server.layout(&session).tabs[0];
        assert!(tab.floating_panes_visible, "{tab:?}");
    } else {
        assert!(
            !ack.ok,
            "new_pane(floating) claimed success on tmux {}",
            server.version()
        );
        let error = ack.error.unwrap_or_default();
        assert!(
            error.contains("3.7"),
            "the refusal does not name tmux 3.7: {error}"
        );
        assert_eq!(server.layout(&session).tabs[0].panes.len(), 1);
    }
}

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn toggle_fullscreen_flips_zoom() {
    let server = Server::start();
    let session = server.session("zoom");
    let first = server.first_pane(&session);
    // tmux does not zoom a window holding a single pane.
    server.split(&session);
    let zoomed = |want: bool| {
        let layout = server.layout(&session);
        let tab = &layout.tabs[0];
        let pane = tab.panes.iter().find(|p| p.id == first).unwrap();
        tab.fullscreen_active == want && pane.is_fullscreen == want
    };
    assert!(zoomed(false));
    let ack = server
        .backend
        .toggle_pane_fullscreen(&session, PaneRef::terminal(first))
        .unwrap();
    assert!(ack.ok, "zoom in failed: {ack:?}");
    assert!(zoomed(true), "{:?}", server.layout(&session));
    let ack = server
        .backend
        .toggle_pane_fullscreen(&session, PaneRef::terminal(first))
        .unwrap();
    assert!(ack.ok, "zoom out failed: {ack:?}");
    assert!(zoomed(false), "{:?}", server.layout(&session));
}

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn write_to_pane_reaches_the_shell() {
    let server = Server::start();
    let session = server.session("write");
    let pane = server.first_pane(&session);
    // The typed line does not contain the printed text, so finding it proves
    // the shell ran the command.
    server.type_into(&session, pane, b"printf '%s_%s\\n' MARK 4242\r");
    assert!(
        wait_until(WAIT, || server
            .capture(pane)
            .lines()
            .any(|l| l == "MARK_4242")),
        "MARK_4242 not in the pane:\n{}",
        server.capture(pane)
    );
}

// ─── attach ─────────────────────────────────────────────────────────────────────

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn attach_renders_resizes_and_takes_input() {
    let server = Server::start();
    let session = server.session("attach");
    let pane = server.first_pane(&session);
    let handle = server.backend.open_attach(&session, 24, 80, false).unwrap();
    assert_eq!(handle.session_name, session);
    let (mut tx, rx) = handle.split();
    let stream = Stream::start(rx);
    assert!(
        stream.wait_for(b"\x1b[?1049h", 0, WAIT),
        "no alternate-screen switch within {WAIT:?}"
    );

    tx.send_resize(50, 160).unwrap();
    assert!(
        wait_until(WAIT, || server.window_width(&session) == "160"),
        "window_width is {} after send_resize(50, 160)",
        server.window_width(&session)
    );

    let mark = stream.len();
    tx.send_input_bytes(b"echo PTY_OK\r".to_vec()).unwrap();
    assert!(
        stream.wait_for(b"PTY_OK", mark, WAIT),
        "PTY_OK not echoed in the attach stream"
    );
    assert!(
        wait_until(WAIT, || server.capture(pane).lines().any(|l| l == "PTY_OK")),
        "echo PTY_OK did not run:\n{}",
        server.capture(pane)
    );

    tx.send_client_exited().unwrap();
    assert!(
        stream.ends(),
        "the stream did not end after send_client_exited"
    );
}

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn read_only_attach_never_changes_the_pane() {
    let server = Server::start();
    let session = server.session("viewer");
    let first = server.first_pane(&session);
    let second = server.split(&session);
    let tab = server.layout(&session).tabs[0].tab_id;
    let handle = server.backend.open_attach(&session, 24, 80, true).unwrap();
    let (mut tx, rx) = handle.split();
    let stream = Stream::start(rx);
    assert!(stream.wait_for(b"\x1b[?1049h", 0, WAIT), "no screen");
    let before: Vec<String> = [first, second].iter().map(|p| server.capture(*p)).collect();
    let active = server.pane_format(second, "#{pane_active}");

    tx.send_input_bytes(b"echo RO_LEAK\r".to_vec()).unwrap();
    tx.send_input_chars("echo RO_LEAK\r").unwrap();
    tx.send_mouse(MuxMouseKind::WheelUp, 5, 5).unwrap();
    assert!(
        tx.go_to_tab(tab).is_err(),
        "a read-only attach switched tabs"
    );
    assert!(
        tx.focus_pane(PaneRef::terminal(first)).is_err(),
        "a read-only attach moved focus"
    );
    let hint = FullscreenHint {
        is_floating: false,
        floating_visible: false,
        is_focused_floating: false,
        tab_fullscreen: false,
        target_is_relay_focus: false,
    };
    assert!(
        tx.toggle_fullscreen(PaneRef::terminal(first), hint)
            .is_err(),
        "a read-only attach zoomed a pane"
    );

    std::thread::sleep(Duration::from_millis(500));
    let after: Vec<String> = [first, second].iter().map(|p| server.capture(*p)).collect();
    assert_eq!(before, after, "a read-only attach changed the pane text");
    assert_eq!(server.pane_format(second, "#{pane_active}"), active);
    assert_eq!(server.pane_format(first, "#{window_zoomed_flag}"), "0");
    assert_eq!(server.pane_format(first, "#{pane_in_mode}"), "0");

    tx.send_client_exited().unwrap();
    assert!(stream.ends(), "the stream did not end");
}

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn wheel_follows_the_mouse_option() {
    let server = Server::start();
    let session = server.session("wheel");
    let pane = server.first_pane(&session);
    // The default changed in tmux 3.8 (`mouse on`; `off` before), so the first
    // half sets `mouse off` itself. It is set before the attach opens, so the
    // client never switches mouse reporting on in the first place.
    server.tmux(&["set-option", "-g", "mouse", "off"]);
    // Scrollback for a wheel to move through.
    server.type_into(&session, pane, b"seq 1 200\r");
    assert!(wait_until(WAIT, || server
        .capture(pane)
        .lines()
        .any(|l| l == "200")));
    let handle = server.backend.open_attach(&session, 24, 80, false).unwrap();
    let (mut tx, rx) = handle.split();
    let stream = Stream::start(rx);
    assert!(stream.wait_for(b"\x1b[?1049h", 0, WAIT), "no screen");

    // `mouse off`: the client never asks for mouse reports, so the wheel frame
    // is dropped and the pane stays as it was.
    assert_eq!(server.tmux(&["show-options", "-gv", "mouse"]), "off");
    let before = server.capture(pane);
    tx.send_mouse(MuxMouseKind::WheelUp, 5, 5).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(server.capture(pane), before, "a wheel frame moved the pane");
    assert_eq!(server.pane_format(pane, "#{pane_in_mode}"), "0");

    // `mouse on`: once the client switches mouse reporting on, a wheel-up is
    // accepted and tmux scrolls the pane into copy mode.
    server.tmux(&["set-option", "-g", "mouse", "on"]);
    assert!(
        wait_until(Duration::from_secs(3), || {
            tx.send_mouse(MuxMouseKind::WheelUp, 5, 5)
                .expect("send_mouse with mouse on");
            std::thread::sleep(Duration::from_millis(100));
            server.pane_format(pane, "#{pane_in_mode}") == "1"
        }),
        "a wheel-up with mouse on did not reach tmux"
    );

    tx.send_client_exited().unwrap();
    assert!(stream.ends(), "the stream did not end");
}

// ─── agent status through the event kernel ──────────────────────────────────────

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn agent_report_drives_status_and_list_agents() {
    let server = Server::start();
    let session = server.session("agent");
    let pane = server.first_pane(&session);
    let mut kernel = Kernel::start(&server.backend);
    server.await_watcher(&session);

    let change = kernel.report(
        &server,
        (&session, pane),
        "state=blocked:kind=permission:app=probe",
        AgentStatus::Blocked,
        REPORT_WAIT,
    );
    assert_eq!(change.workspace_id, "");
    assert_eq!(change.workspace_name.as_deref(), Some(session.as_str()));
    assert_eq!(change.kind.as_deref(), Some("probe"));
    assert!(!change.synthetic);

    let agents = server.backend.list_agents(&session).unwrap();
    assert_eq!(agents.len(), 1, "{agents:?}");
    assert_eq!(agents[0].pane, pane);
    assert_eq!(agents[0].kind, "probe");
    assert_eq!(agents[0].status, AgentStatus::Blocked);
    let mark = server.pane(&session, pane).unwrap().agent;
    assert!(mark.is_some(), "query_layout carries no agent mark");

    kernel.report(
        &server,
        (&session, pane),
        "state=clear",
        AgentStatus::Idle,
        REPORT_WAIT,
    );
}

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn second_session_has_its_own_watcher() {
    let server = Server::start();
    let first = server.session("one");
    let mut kernel = Kernel::start(&server.backend);
    let first_clients = server.await_watcher(&first);
    // Created after the kernel's first scan: picked up by a later one.
    let second = server.session("two");
    let second_clients = server.await_watcher(&second);
    assert_eq!(first_clients.len(), 1, "{first_clients:?}");
    assert_eq!(second_clients.len(), 1, "{second_clients:?}");
    assert_ne!(
        first_clients, second_clients,
        "one client watches both sessions"
    );

    let pane = server.first_pane(&second);
    let change = kernel.report(
        &server,
        (&second, pane),
        "state=working:app=probe",
        AgentStatus::Working,
        REPORT_WAIT,
    );
    assert_eq!(change.workspace_name.as_deref(), Some(second.as_str()));
    assert!(server.backend.list_agents(&first).unwrap().is_empty());
    assert_eq!(server.backend.list_agents(&second).unwrap().len(), 1);
}

#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server"]
fn killing_a_watched_session_closes_its_panes() {
    let server = Server::start();
    let session = server.session("doomed");
    let pane = server.first_pane(&session);
    let mut kernel = Kernel::start(&server.backend);
    server.await_watcher(&session);
    kernel.report(
        &server,
        (&session, pane),
        "state=working:app=probe",
        AgentStatus::Working,
        REPORT_WAIT,
    );

    let ack = server.backend.kill_session(&session).unwrap();
    assert!(ack.ok, "kill_session failed: {ack:?}");
    let closed = kernel.wait_event(Duration::from_secs(5), |event| match event {
        events::MuxEvent::PaneClosed(c) if c.pane == pane => Some(()),
        _ => None,
    });
    assert!(closed.is_some(), "no PaneClosed for pane {pane}");
}

// ─── load ───────────────────────────────────────────────────────────────────────

/// The pane flood the load test types.
const FLOOD: &[u8] = b"yes | head -c 50M\r";

/// The CPU share of one core the flood may cost the process.
const FLOOD_CPU_BOUND: f64 = 0.30;

/// A 50 MB flood in a watched pane.
///
/// Over 10 s of `yes | head -c 50M` the process (the watcher and its control
/// client's reader) stays under [`FLOOD_CPU_BOUND`] of one core, the control
/// client survives, and a status report printed after the flood still
/// arrives. The bound is enforced in an optimized build (`cargo test
/// --release`); an unoptimized build measures the debug code generation of
/// the output path, so there the share is only printed.
///
/// The pauses the watcher logged are printed, not asserted. tmux pauses a
/// pane only when its output waits `pause-after` (5 s) to be written to the
/// client, and the watcher's reader drains the client's output pipe at once
/// (bounding its own queue instead), so a client that keeps up with the flood
/// is never paused; tmux writes to that pipe itself, so stopping the
/// `tmux -C` process provokes no pause either.
#[test]
#[ignore = "needs tmux 3.5a or newer; owns a private tmux server; takes over 10 s"]
fn flood_keeps_the_watcher_cheap() {
    install_logger();
    let server = Server::start();
    let session = server.session("flood");
    let pane = server.first_pane(&session);
    let mut kernel = Kernel::start(&server.backend);
    let clients = server.await_watcher(&session);

    let pauses = PAUSES.load(Ordering::Relaxed);
    let cpu = process_cpu();
    let started = Instant::now();
    server.type_into(&session, pane, FLOOD);
    std::thread::sleep(Duration::from_secs(10));
    let used = process_cpu().saturating_sub(cpu);
    let wall = started.elapsed();
    let share = used.as_secs_f64() / wall.as_secs_f64();
    let optimized = !cfg!(debug_assertions);
    let build = if optimized {
        "optimized"
    } else {
        "unoptimized"
    };
    println!(
        "[tmux it] flood: {used:?} CPU over {wall:?} ({share:.3} of one core, {build} build), \
         {} pauses logged by the watcher",
        PAUSES.load(Ordering::Relaxed) - pauses
    );
    if optimized {
        assert!(
            share < FLOOD_CPU_BOUND,
            "the process used {share:.3} of one core (bound {FLOOD_CPU_BOUND})"
        );
    } else {
        println!("[tmux it] the CPU bound is enforced only in an optimized build");
    }
    assert_eq!(
        server.control_clients(&session),
        clients,
        "the watcher's control client did not survive the flood"
    );
    // The watcher may still be working through the flood's output.
    kernel.report(
        &server,
        (&session, pane),
        "state=working:app=probe",
        AgentStatus::Working,
        Duration::from_secs(10),
    );
}

// ─── tmux 3.8 only ──────────────────────────────────────────────────────────────

/// Whether the test runs on this tmux; prints why not.
fn needs_3_8(server: &Server) -> bool {
    let ok = server.version().at_least(3, 8);
    if !ok {
        println!(
            "[tmux it] skipping: needs tmux 3.8, found {}",
            server.version()
        );
    }
    ok
}

#[test]
#[ignore = "needs tmux 3.8 (returns early on older); owns a private tmux server"]
fn osc133_marks_drive_command_running() {
    let server = Server::start();
    if !needs_3_8(&server) {
        return;
    }
    let session = server.session("osc133");
    let pane = server.first_pane(&session);
    let runner = Tmux::new(server.backend.socket().clone(), server.version().clone());
    let running = || {
        runner
            .list_panes(&session)
            .expect("list_panes")
            .into_iter()
            .find(|p| p.id == pane)
            .and_then(|p| p.command_running)
    };

    server.type_into(
        &session,
        pane,
        b"printf '\\033]133;A\\007\\033]133;C\\007'\r",
    );
    assert!(
        wait_until(WAIT, || running() == Some(true)),
        "pane_command_running is {:?} after OSC 133 A and C",
        running()
    );
    server.type_into(&session, pane, b"printf '\\033]133;D;0\\007'\r");
    assert!(
        wait_until(WAIT, || running() == Some(false)),
        "pane_command_running is {:?} after OSC 133 D",
        running()
    );
}

#[test]
#[ignore = "needs tmux 3.8 (returns early on older); owns a private tmux server"]
fn toggle_floating_floats_then_tiles() {
    let server = Server::start();
    if !needs_3_8(&server) {
        return;
    }
    let session = server.session("tofloat");
    server.first_pane(&session);
    // Tiling a floating pane joins it to a tiled pane of its window.
    let pane = server.split(&session);
    let floating = || server.pane(&session, pane).map(|p| p.is_floating);

    let ack = server
        .backend
        .toggle_pane_floating(&session, PaneRef::terminal(pane))
        .unwrap();
    assert!(ack.ok, "float failed: {ack:?}");
    assert!(
        wait_until(WAIT, || floating() == Some(true)),
        "pane {pane} is not floating: {:?}",
        server.layout(&session)
    );

    let ack = server
        .backend
        .toggle_pane_floating(&session, PaneRef::terminal(pane))
        .unwrap();
    assert!(ack.ok, "tile failed: {ack:?}");
    assert!(
        wait_until(WAIT, || floating() == Some(false)),
        "pane {pane} is still floating: {:?}",
        server.layout(&session)
    );
}
