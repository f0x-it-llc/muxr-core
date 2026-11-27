//! The tmux [`MuxBackend`]: every trait operation over the command runner, the
//! layout functions and the pty attach.
//!
//! # Addressing
//!
//! tmux pane (`%N`) and window (`@N`) ids are unique across the whole server,
//! so an id alone would reach a pane of any session. Every pane and tab
//! operation therefore first reads the named session's panes or windows and
//! refuses an id that session does not hold, matching the session-scoped ids of
//! the other backends. A read that fails because the session is gone answers a
//! typed [`UnknownSession`].
//!
//! # Focus and zoom
//!
//! tmux focus (current window, active pane) and zoom belong to the session, so
//! a focus change or zoom toggle moves every client attached to it.
//!
//! # Agents
//!
//! The watcher writes per-pane agent rows into the [`AgentState`] this backend
//! owns; [`MuxBackend::query_layout`] stamps them onto panes as marks and
//! [`MuxBackend::list_agents`] joins them against the live pane list.
//!
//! Every operation is blocking and runs on the caller's thread.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};

use super::agent_state::AgentState;
use super::attach::{self, AttachControl};
use super::commands::{PaneRow, Tmux, WindowRow};
use super::layout::{self, PaneIn, WindowIn};
use super::paths::{TmuxSocket, TmuxVersion, tmux_bin, tmux_version};
use crate::multiplexer::types::{
    ActionAck, AgentSnapshot, LayoutSnapshot, PaneFullscreenContext, PaneRef, ResizeDir,
    ResizeKind, ScrollDir, UnknownSession, sort_agents,
};
use crate::multiplexer::{DualHandle, MuxBackend};

/// `info` of a create that was given a layout.
const LAYOUT_IGNORED: &str = "layout ignored: tmux has no layout files";
/// Distinct unaddressable session names remembered for log de-duplication.
const UNADDRESSABLE_LOG_CAP: usize = 256;

/// The tmux-backed [`MuxBackend`].
#[derive(Debug)]
pub struct TmuxBackend {
    tmux: Tmux,
    agents: Arc<AgentState>,
    /// Session names already logged as unaddressable.
    unaddressable_logged: Mutex<HashSet<String>>,
}

impl TmuxBackend {
    /// Resolve the socket and tmux version from the environment, refusing a
    /// tmux older than the supported floor.
    pub fn from_env() -> Result<Self> {
        let socket = TmuxSocket::resolve();
        let bin = tmux_bin();
        let version = tmux_version().map_err(|e| {
            anyhow!(
                "tmux backend unavailable ({}): {e:#}",
                bin.to_string_lossy()
            )
        })?;
        if !version.meets_floor() {
            bail!(
                "tmux backend needs tmux 3.5a or newer, found tmux {version} ({})",
                bin.to_string_lossy()
            );
        }
        Ok(Self::with_runner(Tmux::new(socket, version)))
    }

    /// A backend over an already-built runner, with empty agent state.
    fn with_runner(tmux: Tmux) -> Self {
        Self {
            tmux,
            agents: Arc::new(AgentState::new()),
            unaddressable_logged: Mutex::new(HashSet::new()),
        }
    }

    /// The shared per-pane agent state.
    pub fn agent_state(&self) -> Arc<AgentState> {
        Arc::clone(&self.agents)
    }

    /// The tmux server socket this backend drives.
    pub fn socket(&self) -> &TmuxSocket {
        self.tmux.socket()
    }

    /// The tmux version this backend was built for.
    pub fn version(&self) -> &TmuxVersion {
        self.tmux.version()
    }

    /// Whether `name` can be addressed by the routing layer; logs a rejected
    /// name once.
    fn addressable(&self, name: &str) -> bool {
        let Err(why) = crate::ipc::validate_session_name(name) else {
            return true;
        };
        let mut logged = self
            .unaddressable_logged
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !logged.contains(name) {
            if logged.len() >= UNADDRESSABLE_LOG_CAP {
                logged.clear();
            }
            logged.insert(name.to_owned());
            log::debug!("tmux: omitting session {name:?} from listings: {why}");
        }
        false
    }

    /// The row of terminal `pane` in `session`, or the refusal to answer with.
    fn pane_row(
        &self,
        session: &str,
        pane: PaneRef,
    ) -> Result<std::result::Result<(PaneRow, Vec<PaneRow>), ActionAck>> {
        if pane.is_plugin {
            return Ok(Err(refused(format!(
                "tmux has no plugin panes (pane {})",
                pane.id
            ))));
        }
        let panes = read_session(&self.tmux, session, |t| t.list_panes(session))?;
        match panes.iter().find(|p| p.id == pane.id).cloned() {
            Some(row) => Ok(Ok((row, panes))),
            None => Ok(Err(refused(format!(
                "pane {} is not in session '{session}'",
                pane.id
            )))),
        }
    }

    /// The window id `tab_id` names in `session`, or the refusal to answer with.
    fn window_id(&self, session: &str, tab_id: u64) -> Result<std::result::Result<u32, ActionAck>> {
        let not_here = || refused(format!("tab {tab_id} is not in session '{session}'"));
        let Ok(id) = u32::try_from(tab_id) else {
            return Ok(Err(not_here()));
        };
        let windows = read_session(&self.tmux, session, |t| t.list_windows(session))?;
        Ok(if windows.iter().any(|w| w.id == id) {
            Ok(id)
        } else {
            Err(not_here())
        })
    }
}

/// Unwrap a lookup, returning its refusal from the enclosing operation.
macro_rules! found {
    ($lookup:expr) => {
        match $lookup? {
            Ok(v) => v,
            Err(ack) => return Ok(ack),
        }
    };
}

fn refused(error: String) -> ActionAck {
    ActionAck {
        ok: false,
        error: Some(error),
        info: None,
    }
}

/// Run a session-scoped read; when it fails and the session does not exist,
/// answer a typed [`UnknownSession`] instead of the raw tmux error.
fn read_session<T>(tmux: &Tmux, session: &str, read: impl FnOnce(&Tmux) -> Result<T>) -> Result<T> {
    match read(tmux) {
        Ok(v) => Ok(v),
        Err(e) => match tmux.has_session(session) {
            Ok(ack) if !ack.ok => Err(anyhow::Error::new(UnknownSession::new(session))),
            _ => Err(e),
        },
    }
}

fn window_in(w: &WindowRow) -> WindowIn {
    WindowIn {
        id: w.id,
        index: w.index,
        name: w.name.clone(),
        active: w.active,
        bell: w.bell,
        zoomed: w.zoomed,
        width: w.width,
        height: w.height,
    }
}

fn pane_in(p: &PaneRow) -> PaneIn {
    PaneIn {
        id: p.id,
        window_id: p.window_id,
        active: p.active,
        title: p.title.clone(),
        current_command: p.current_command.clone(),
        current_path: p.current_path.clone(),
        dead: p.dead,
        left: p.left,
        top: p.top,
        width: p.width,
        height: p.height,
        floating: p.floating,
        z: p.z,
    }
}

/// Windows and panes of `session` in the layout module's input form.
fn read_layout_rows(tmux: &Tmux, session: &str) -> Result<(Vec<WindowIn>, Vec<PaneIn>)> {
    let (windows, panes) = read_session(tmux, session, |t| {
        Ok((t.list_windows(session)?, t.list_panes(session)?))
    })?;
    Ok((
        windows.iter().map(window_in).collect(),
        panes.iter().map(pane_in).collect(),
    ))
}

/// The neutral layout of `session` with every pane's agent mark stamped.
fn layout_of(tmux: &Tmux, agents: &AgentState, session: &str) -> Result<LayoutSnapshot> {
    let (windows, panes) = read_layout_rows(tmux, session)?;
    let mut snapshot = layout::snapshot(&windows, &panes);
    for pane in snapshot.tabs.iter_mut().flat_map(|t| t.panes.iter_mut()) {
        pane.agent = agents.mark(pane.id);
    }
    Ok(snapshot)
}

/// Seconds since the Unix epoch.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

/// The routed attach controls, over the runner.
struct Control {
    tmux: Tmux,
    agents: Arc<AgentState>,
}

/// An `Ok` ack or its error.
fn ack_result(op: &str, ack: ActionAck) -> Result<()> {
    if ack.ok {
        Ok(())
    } else {
        bail!(
            "tmux {op}: {}",
            ack.error.as_deref().unwrap_or("command failed")
        )
    }
}

impl AttachControl for Control {
    fn select_window(&self, window: u32) -> Result<()> {
        ack_result("select-window", self.tmux.select_window(window)?)
    }

    fn select_pane(&self, pane: u32) -> Result<()> {
        ack_result("select-pane", self.tmux.select_pane(pane)?)
    }

    fn zoom_toggle(&self, pane: u32) -> Result<()> {
        ack_result("resize-pane -Z", self.tmux.zoom_toggle(pane)?)
    }

    fn layout(&self, session: &str) -> Result<LayoutSnapshot> {
        layout_of(&self.tmux, &self.agents, session)
    }
}

impl MuxBackend for TmuxBackend {
    // ── Session lifecycle ───────────────────────────────────────────────────

    fn list_sessions(&self) -> Result<Vec<(String, Duration)>> {
        let now = now_unix();
        Ok(self
            .tmux
            .list_sessions()?
            .into_iter()
            .filter(|s| self.addressable(&s.name))
            .map(|s| {
                let age = Duration::from_secs(now.saturating_sub(s.created_unix));
                (s.name, age)
            })
            .collect())
    }

    fn list_sessions_with_resurrectables(&self) -> Result<Vec<(String, u64, bool)>> {
        // tmux keeps no record of exited sessions.
        Ok(self
            .list_sessions()?
            .into_iter()
            .map(|(name, age)| (name, age.as_secs(), false))
            .collect())
    }

    fn validate_session_name(&self, name: &str) -> std::result::Result<(), String> {
        crate::ipc::validate_session_name(name)
    }

    fn create_session(&self, name: &str, layout: Option<String>) -> Result<ActionAck> {
        crate::ipc::validate_session_name(name).map_err(|e| anyhow!(e))?;
        let mut ack = self.tmux.new_session(name)?;
        if ack.ok && layout.is_some_and(|l| !l.is_empty()) {
            ack.info = Some(LAYOUT_IGNORED.to_owned());
        }
        Ok(ack)
    }

    fn kill_session(&self, session: &str) -> Result<ActionAck> {
        let ack = self.tmux.kill_session(session)?;
        if ack.ok {
            self.agents.remove_session(session);
        }
        Ok(ack)
    }

    fn rename_session(&self, session: &str, new_name: String) -> Result<ActionAck> {
        if let Err(why) = crate::ipc::validate_session_name(&new_name) {
            return Ok(refused(why));
        }
        self.tmux.rename_session(session, &new_name)
    }

    // ── Ephemeral control actions ───────────────────────────────────────────

    fn write_to_pane(&self, session: &str, pane: PaneRef, bytes: Vec<u8>) -> Result<ActionAck> {
        let (row, _) = found!(self.pane_row(session, pane));
        self.tmux.send_keys_hex(row.id, &bytes)
    }

    fn focus_pane(&self, session: &str, pane: PaneRef) -> Result<ActionAck> {
        let (row, _) = found!(self.pane_row(session, pane));
        let ack = self.tmux.select_window(row.window_id)?;
        if !ack.ok {
            return Ok(ack);
        }
        self.tmux.select_pane(row.id)
    }

    fn close_pane(&self, session: &str, pane: PaneRef) -> Result<ActionAck> {
        let (row, _) = found!(self.pane_row(session, pane));
        let ack = self.tmux.kill_pane(row.id)?;
        if ack.ok {
            self.agents.remove_pane(row.id);
        }
        Ok(ack)
    }

    fn new_pane(&self, session: &str, floating: bool, name: Option<String>) -> Result<ActionAck> {
        let ack = if floating {
            self.tmux.new_floating_pane(session)?
        } else {
            self.tmux.split_window(session)?
        };
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            return Ok(ack);
        };
        if !ack.ok {
            return Ok(ack);
        }
        let Some(id) = ack.info.as_deref().and_then(|i| i.parse::<u32>().ok()) else {
            return Ok(ActionAck {
                ok: false,
                error: Some(format!(
                    "created a pane but tmux reported no pane id, so it was not named {name:?}"
                )),
                info: ack.info,
            });
        };
        let titled = self.tmux.set_pane_title(id, &name)?;
        if titled.ok {
            return Ok(ack);
        }
        Ok(ActionAck {
            ok: false,
            error: Some(format!(
                "created pane {id} but could not name it: {}",
                titled.error.as_deref().unwrap_or("select-pane -T failed")
            )),
            info: ack.info,
        })
    }

    fn rename_pane(&self, session: &str, pane: PaneRef, name: String) -> Result<ActionAck> {
        let (row, _) = found!(self.pane_row(session, pane));
        self.tmux.set_pane_title(row.id, &name)
    }

    fn resize_pane(
        &self,
        session: &str,
        pane: PaneRef,
        kind: ResizeKind,
        dir: Option<ResizeDir>,
    ) -> Result<ActionAck> {
        let (row, _) = found!(self.pane_row(session, pane));
        self.tmux.resize_pane(row.id, kind, dir)
    }

    fn toggle_pane_floating(&self, session: &str, pane: PaneRef) -> Result<ActionAck> {
        let (row, panes) = found!(self.pane_row(session, pane));
        if !row.floating {
            return self.tmux.float_pane(row.id);
        }
        let onto = panes
            .iter()
            .filter(|p| p.window_id == row.window_id && !p.floating && p.id != row.id)
            .min_by_key(|p| (p.index, p.id));
        match onto {
            Some(tiled) => self.tmux.tile_pane(row.id, tiled.id),
            None => Ok(refused(format!(
                "pane {} has no tiled pane in its window to join",
                row.id
            ))),
        }
    }

    fn toggle_pane_fullscreen(&self, session: &str, pane: PaneRef) -> Result<ActionAck> {
        let (row, _) = found!(self.pane_row(session, pane));
        self.tmux.zoom_toggle(row.id)
    }

    fn scroll_pane(&self, session: &str, pane: PaneRef, dir: ScrollDir) -> Result<ActionAck> {
        let (row, _) = found!(self.pane_row(session, pane));
        self.tmux.scroll(row.id, dir)
    }

    fn new_tab(&self, session: &str, name: Option<String>) -> Result<ActionAck> {
        self.tmux.new_window(session, name.as_deref())
    }

    fn close_tab(&self, session: &str, tab_id: u64) -> Result<ActionAck> {
        let window = found!(self.window_id(session, tab_id));
        self.tmux.kill_window(window)
    }

    fn go_to_tab(&self, session: &str, tab_id: u64) -> Result<ActionAck> {
        let window = found!(self.window_id(session, tab_id));
        self.tmux.select_window(window)
    }

    fn rename_tab(&self, session: &str, tab_id: u64, name: String) -> Result<ActionAck> {
        let window = found!(self.window_id(session, tab_id));
        self.tmux.rename_window(window, &name)
    }

    // ── Agents ──────────────────────────────────────────────────────────────

    fn supports_agents(&self) -> bool {
        true
    }

    fn list_agents(&self, session: &str) -> Result<Vec<AgentSnapshot>> {
        // Rows first, panes second: a row's pane existed before the row was
        // written, so a pane missing from the later read has really gone.
        let rows = self.agents.rows_for_session(session);
        let (windows, panes) = read_session(&self.tmux, session, |t| {
            Ok((t.list_windows(session)?, t.list_panes(session)?))
        })?;
        let mut agents = Vec::with_capacity(rows.len());
        for (id, row) in rows {
            let Some(pane) = panes.iter().find(|p| p.id == id) else {
                self.agents.remove_pane(id);
                continue;
            };
            let window = windows.iter().find(|w| w.id == pane.window_id);
            agents.push(AgentSnapshot {
                pane: id,
                tab_id: u64::from(pane.window_id),
                space_id: String::new(),
                space_name: String::new(),
                tab_name: window.map_or(row.tab_name, |w| w.name.clone()),
                kind: row.kind,
                name: None,
                display_kind: row.display_kind,
                status: row.status,
                title: row.title.or_else(|| non_empty(&pane.title)),
                summary: row.summary,
                cwd: non_empty(&pane.current_path),
                focused: pane.active && window.is_some_and(|w| w.active),
                change_seq: row.change_seq,
                completion_seq: row.completion_seq,
            });
        }
        sort_agents(&mut agents);
        Ok(agents)
    }

    // ── Read-only queries ───────────────────────────────────────────────────

    fn query_layout(&self, session: &str) -> Result<LayoutSnapshot> {
        layout_of(&self.tmux, &self.agents, session)
    }

    fn query_session_size(&self, session: &str) -> Result<(u16, u16)> {
        let windows = read_session(&self.tmux, session, |t| t.list_windows(session))?;
        let windows: Vec<WindowIn> = windows.iter().map(window_in).collect();
        layout::session_size(&windows)
            .ok_or_else(|| anyhow!("tmux session '{session}' has no windows"))
    }

    fn pane_is_floating_with_visibility(
        &self,
        session: &str,
        pane: PaneRef,
    ) -> Result<(bool, bool, Option<PaneRef>)> {
        if pane.is_plugin {
            return Ok((false, false, None));
        }
        let panes = read_session(&self.tmux, session, |t| t.list_panes(session))?;
        let panes: Vec<PaneIn> = panes.iter().map(pane_in).collect();
        Ok(layout::floating_visibility(&panes, pane.id))
    }

    fn pane_fullscreen_context(
        &self,
        session: &str,
        pane: PaneRef,
        relay_focus: Option<PaneRef>,
    ) -> Result<PaneFullscreenContext> {
        // tmux zoom is addressed by pane id across the session, so the relay
        // needs no tab placement: no tab ids keep its tab gate permissive.
        let _ = relay_focus;
        if pane.is_plugin {
            return Ok(PaneFullscreenContext::not_found());
        }
        let (windows, panes) = read_layout_rows(&self.tmux, session)?;
        let Some(window) =
            layout::window_of(&panes, pane.id).and_then(|id| windows.iter().find(|w| w.id == id))
        else {
            return Ok(PaneFullscreenContext::not_found());
        };
        let (is_floating, floating_visible, focused_floating) =
            layout::floating_visibility(&panes, pane.id);
        Ok(PaneFullscreenContext {
            exists: true,
            is_floating,
            floating_visible,
            focused_floating,
            tab_fullscreen: window.zoomed,
            tab_id: None,
            tab_active: window.active,
            relay_focus_tab_id: None,
        })
    }

    // ── Attach (the relay seam) ─────────────────────────────────────────────

    fn open_attach(
        &self,
        session: &str,
        rows: u16,
        cols: u16,
        read_only: bool,
    ) -> Result<DualHandle> {
        crate::ipc::validate_session_name(session).map_err(|e| anyhow!(e))?;
        if !self.tmux.has_session(session)?.ok {
            return Err(anyhow::Error::new(UnknownSession::new(session)));
        }
        let ctl: Arc<dyn AttachControl> = Arc::new(Control {
            tmux: self.tmux.clone(),
            agents: Arc::clone(&self.agents),
        });
        attach::open(self.tmux.socket(), session, rows, cols, read_only, ctl)
    }

    // ── Backend identity ────────────────────────────────────────────────────

    fn backend_version(&self) -> String {
        format!("tmux {}", self.tmux.version())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::multiplexer::tmux::agent_state::AgentRow;
    use crate::multiplexer::tmux::paths::TMUX_BIN_ENV;
    use crate::multiplexer::types::{AgentMark, AgentStatus};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Run `f` with `MUXRD_TMUX_BIN` set to `value`, restoring it afterwards.
    fn with_bin<T>(value: &std::ffi::OsStr, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os(TMUX_BIN_ENV);
        // SAFETY: every test touching this variable holds ENV_LOCK.
        unsafe { std::env::set_var(TMUX_BIN_ENV, value) };
        let out = f();
        match saved {
            Some(v) => unsafe { std::env::set_var(TMUX_BIN_ENV, v) },
            None => unsafe { std::env::remove_var(TMUX_BIN_ENV) },
        }
        out
    }

    fn fake_tmux(tag: &str, printed: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("muxrd-tmux-fake-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tmux");
        std::fs::write(&path, format!("#!/bin/sh\necho '{printed}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn from_env_names_the_missing_binary() {
        let err = with_bin("/nonexistent/muxrd-no-tmux".as_ref(), TmuxBackend::from_env)
            .expect_err("a missing binary must fail");
        let text = format!("{err:#}");
        assert!(text.contains("tmux backend unavailable"), "{text}");
        assert!(text.contains("/nonexistent/muxrd-no-tmux"), "{text}");
    }

    #[test]
    fn from_env_refuses_below_the_floor() {
        let fake = fake_tmux("old", "tmux 3.4");
        let err = with_bin(fake.as_os_str(), TmuxBackend::from_env)
            .expect_err("tmux 3.4 is below the floor");
        let _ = std::fs::remove_dir_all(fake.parent().unwrap());
        let text = format!("{err:#}");
        assert!(text.contains("3.5a"), "{text}");
        assert!(text.contains("tmux 3.4"), "{text}");
    }

    #[test]
    fn from_env_accepts_a_supported_version() {
        let fake = fake_tmux("ok", "tmux 3.5a");
        let backend =
            with_bin(fake.as_os_str(), TmuxBackend::from_env).expect("tmux 3.5a meets the floor");
        let _ = std::fs::remove_dir_all(fake.parent().unwrap());
        assert_eq!(backend.backend_version(), "tmux 3.5a");
        assert_eq!(backend.version().to_string(), "3.5a");
    }

    // ── Hermetic fake tmux ──────────────────────────────────────────────────

    static CASE: AtomicUsize = AtomicUsize::new(0);

    /// A fake `tmux` that records each argv (after `-S <socket>`) and answers
    /// per subcommand from `out.<cmd>`, `err.<cmd>` and `code.<cmd>` in its
    /// own directory, which it finds from the socket path.
    struct Fake {
        dir: PathBuf,
        backend: TmuxBackend,
    }

    const FAKE_SCRIPT: &str = "#!/bin/sh\n\
        d=$(dirname \"$2\")\n\
        [ \"$3\" = __warm ] && exit 0\n\
        shift 2\n\
        printf '%s\\n' \"$@\" >> \"$d/log\"\n\
        printf '==\\n' >> \"$d/log\"\n\
        [ -f \"$d/out.$1\" ] && cat \"$d/out.$1\"\n\
        [ -f \"$d/err.$1\" ] && cat \"$d/err.$1\" >&2\n\
        if [ -f \"$d/code.$1\" ]; then exit \"$(cat \"$d/code.$1\")\"; fi\n\
        exit 0\n";

    /// Run the fresh script once until exec stops failing with ETXTBSY: a
    /// process forked elsewhere while it was being written may still hold it
    /// open for writing.
    fn warm_up(script: &Path, socket: &Path) {
        for _ in 0..200 {
            match std::process::Command::new(script)
                .arg("-S")
                .arg(socket)
                .arg("__warm")
                .status()
            {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => return,
            }
        }
    }

    impl Fake {
        fn new(version: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "muxrd-tmux-backend-{}-{}",
                std::process::id(),
                CASE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("tmux");
            std::fs::write(&script, FAKE_SCRIPT).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let socket = dir.join("sock");
            warm_up(&script, &socket);
            let tmux = Tmux::with_bin(
                TmuxSocket { path: socket },
                TmuxVersion::parse(&format!("tmux {version}")).unwrap(),
                script.into_os_string(),
            );
            Self {
                dir,
                backend: TmuxBackend::with_runner(tmux),
            }
        }

        /// Answer subcommand `cmd` with `stdout`.
        fn out(&self, cmd: &str, stdout: &str) {
            std::fs::write(self.dir.join(format!("out.{cmd}")), stdout).unwrap();
        }

        /// Fail subcommand `cmd` with `stderr` and exit code 1.
        fn fail(&self, cmd: &str, stderr: &str) {
            std::fs::write(self.dir.join(format!("err.{cmd}")), stderr).unwrap();
            std::fs::write(self.dir.join(format!("code.{cmd}")), "1").unwrap();
        }

        fn calls(&self) -> Vec<Vec<String>> {
            let log = std::fs::read_to_string(self.dir.join("log")).unwrap_or_default();
            let mut calls = Vec::new();
            let mut cur = Vec::new();
            for line in log.lines() {
                if line == "==" {
                    calls.push(std::mem::take(&mut cur));
                } else {
                    cur.push(line.to_owned());
                }
            }
            calls
        }

        /// Recorded subcommand names, in order.
        fn commands(&self) -> Vec<String> {
            self.calls().into_iter().map(|c| c[0].clone()).collect()
        }

        /// The recorded calls of subcommand `cmd`.
        fn calls_of(&self, cmd: &str) -> Vec<Vec<String>> {
            self.calls().into_iter().filter(|c| c[0] == cmd).collect()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn session_rec(id: u32, created: u64, name: &str) -> String {
        format!("@@\nid=${id}\ncreated={created}\nattached=0\nwindows=1\nname={name}\n")
    }

    fn window_rec(id: u32, index: u32, active: bool, zoomed: bool, name: &str) -> String {
        format!(
            "@@\nid=@{id}\nindex={index}\nactive={}\nbell=0\nzoomed={}\nwidth=80\nheight=24\n\
             panes=1\nname={name}\n",
            u8::from(active),
            u8::from(zoomed)
        )
    }

    fn pane_rec(id: u32, window: u32, index: u32, active: bool, floating: bool) -> String {
        format!(
            "@@\nid=%{id}\nwindow=@{window}\nindex={index}\nactive={}\ndead=0\nleft=0\ntop=0\n\
             width=80\nheight=24\nfloating={}\nz=0\nrunning=\npid=100\ncommand=bash\n\
             path=/home/p{id}\ntitle=t{id}\n",
            u8::from(active),
            u8::from(floating)
        )
    }

    fn agent_row(session: &str, tab_name: &str) -> AgentRow {
        AgentRow {
            status: AgentStatus::Working,
            kind: "claude".into(),
            display_kind: Some("Claude".into()),
            title: None,
            summary: Some("thinking".into()),
            tab_id: 999,
            tab_name: tab_name.into(),
            change_seq: 0,
            completion_seq: None,
            last_output: None,
            session: session.into(),
        }
    }

    // ── Sessions ────────────────────────────────────────────────────────────

    #[test]
    fn no_server_lists_no_sessions() {
        let f = Fake::new("3.7c");
        f.fail("list-sessions", "no server running on /tmp/tmux-1000/x\n");
        assert!(f.backend.list_sessions().unwrap().is_empty());
        assert!(
            f.backend
                .list_sessions_with_resurrectables()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unaddressable_session_names_are_omitted() {
        let f = Fake::new("3.7c");
        let created = now_unix() - 30;
        f.out(
            "list-sessions",
            &[
                session_rec(1, created, "work"),
                session_rec(2, created, "has.dot"),
                session_rec(3, created, "a b"),
                session_rec(4, created, "ok-2"),
            ]
            .concat(),
        );
        let listed = f.backend.list_sessions().unwrap();
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["work", "ok-2"]);
        assert!(listed[0].1 >= Duration::from_secs(30));
        assert!(listed[0].1 < Duration::from_secs(300));
        let with = f.backend.list_sessions_with_resurrectables().unwrap();
        assert_eq!(with.len(), 2);
        assert_eq!(with[0].0, "work");
        assert!(!with[0].2 && !with[1].2);
        // Each rejected name is remembered once.
        let logged = f.backend.unaddressable_logged.lock().unwrap();
        assert_eq!(logged.len(), 2);
        assert!(logged.contains("has.dot") && logged.contains("a b"));
    }

    #[test]
    fn create_kill_and_rename_argv() {
        let f = Fake::new("3.7c");
        let plain = f.backend.create_session("s", None).unwrap();
        assert!(plain.ok && plain.info.is_none());
        let with_layout = f
            .backend
            .create_session("s", Some("dev.kdl".into()))
            .unwrap();
        assert_eq!(with_layout.info.as_deref(), Some(LAYOUT_IGNORED));
        assert!(f.backend.create_session("../bad", None).is_err());
        f.backend.kill_session("s").unwrap();
        f.backend.rename_session("s", "t".into()).unwrap();
        let bad = f.backend.rename_session("s", "no/slash".into()).unwrap();
        assert!(!bad.ok);
        let calls = f.calls();
        assert_eq!(calls.len(), 4, "{calls:?}");
        let new = v(&["new-session", "-d", "-s", "s", "-x", "80", "-y", "24"]);
        assert_eq!(calls[0], new);
        assert_eq!(calls[1], new);
        assert_eq!(calls[2], v(&["kill-session", "-t", "=s"]));
        assert_eq!(calls[3], v(&["rename-session", "-t", "=s", "--", "t"]));
    }

    #[test]
    fn kill_session_drops_its_agent_rows() {
        let f = Fake::new("3.7c");
        let state = f.backend.agent_state();
        state.set_pane(1, agent_row("s", "w"));
        state.set_pane(2, agent_row("o", "w"));
        assert!(f.backend.kill_session("s").unwrap().ok);
        assert!(state.rows_for_session("s").is_empty());
        assert_eq!(state.rows_for_session("o").len(), 1);
    }

    // ── Panes and tabs ──────────────────────────────────────────────────────

    #[test]
    fn new_pane_reports_the_id_and_names_it() {
        let f = Fake::new("3.7c");
        f.out("split-window", "%7\n");
        let ack = f.backend.new_pane("s", false, Some("logs".into())).unwrap();
        assert!(ack.ok, "{ack:?}");
        assert_eq!(ack.info.as_deref(), Some("7"));
        f.out("new-pane", "%8\n");
        let ack = f.backend.new_pane("s", true, None).unwrap();
        assert_eq!(ack.info.as_deref(), Some("8"));
        let calls = f.calls();
        assert_eq!(
            calls,
            vec![
                v(&["split-window", "-t", "=s:", "-P", "-F", "#{pane_id}"]),
                v(&["select-pane", "-t", "%7", "-T", "logs"]),
                v(&["new-pane", "-t", "=s:", "-P", "-F", "#{pane_id}"]),
            ]
        );
    }

    #[test]
    fn floating_new_pane_below_3_7_fails_honestly() {
        let f = Fake::new("3.5a");
        let ack = f.backend.new_pane("s", true, Some("x".into())).unwrap();
        assert!(!ack.ok);
        assert!(ack.error.unwrap().contains("3.7"));
        assert!(f.calls().is_empty());
    }

    #[test]
    fn new_tab_reports_the_window_id() {
        let f = Fake::new("3.7c");
        f.out("new-window", "@4\n");
        let ack = f.backend.new_tab("s", Some("logs".into())).unwrap();
        assert_eq!(ack.info.as_deref(), Some("4"));
        assert_eq!(
            f.calls()[0],
            v(&[
                "new-window",
                "-t",
                "=s:",
                "-n",
                "logs",
                "-P",
                "-F",
                "#{window_id}"
            ])
        );
    }

    #[test]
    fn pane_ops_resolve_the_pane_in_the_session_first() {
        let f = Fake::new("3.7c");
        f.out("list-panes", &[pane_rec(3, 2, 0, true, false)].concat());
        assert!(f.backend.focus_pane("s", PaneRef::terminal(3)).unwrap().ok);
        assert!(
            f.backend
                .write_to_pane("s", PaneRef::terminal(3), b"hi".to_vec())
                .unwrap()
                .ok
        );
        assert!(
            f.backend
                .toggle_pane_fullscreen("s", PaneRef::terminal(3))
                .unwrap()
                .ok
        );
        let calls = f.calls();
        assert_eq!(calls[0][..4], v(&["list-panes", "-s", "-t", "=s"])[..]);
        assert_eq!(calls[1], v(&["select-window", "-t", "@2"]));
        assert_eq!(calls[2], v(&["select-pane", "-t", "%3"]));
        assert_eq!(calls[4], v(&["send-keys", "-t", "%3", "-H", "68", "69"]));
        assert_eq!(calls[6], v(&["resize-pane", "-Z", "-t", "%3"]));

        // A pane of another session, or a plugin pane, is refused untouched.
        let before = f.calls().len();
        let other = f.backend.close_pane("s", PaneRef::terminal(9)).unwrap();
        assert!(!other.ok);
        assert!(other.error.unwrap().contains("not in session 's'"));
        let plugin = f.backend.focus_pane("s", PaneRef::plugin(3)).unwrap();
        assert!(!plugin.ok);
        assert_eq!(f.commands()[before..], ["list-panes"]);
    }

    #[test]
    fn tab_ops_resolve_the_window_in_the_session_first() {
        let f = Fake::new("3.7c");
        f.out("list-windows", &window_rec(5, 0, true, false, "main"));
        assert!(f.backend.go_to_tab("s", 5).unwrap().ok);
        assert!(f.backend.rename_tab("s", 5, "x".into()).unwrap().ok);
        assert!(f.backend.close_tab("s", 5).unwrap().ok);
        assert!(!f.backend.close_tab("s", 6).unwrap().ok);
        assert!(!f.backend.close_tab("s", u64::MAX).unwrap().ok);
        let calls: Vec<Vec<String>> = f
            .calls()
            .into_iter()
            .filter(|c| c[0] != "list-windows")
            .collect();
        assert_eq!(
            calls,
            vec![
                v(&["select-window", "-t", "@5"]),
                v(&["rename-window", "-t", "@5", "--", "x"]),
                v(&["kill-window", "-t", "@5"]),
            ]
        );
    }

    #[test]
    fn a_missing_session_is_a_typed_unknown_session() {
        let f = Fake::new("3.7c");
        f.fail("list-panes", "can't find session: gone\n");
        f.fail("has-session", "can't find session: gone\n");
        let err = f
            .backend
            .focus_pane("gone", PaneRef::terminal(1))
            .expect_err("missing session");
        assert!(UnknownSession::in_chain(&err), "{err:#}");
    }

    fn toggle_floating_fake(version: &str) -> Fake {
        let f = Fake::new(version);
        // Window 1: tiled panes 2 (index 1) and 1 (index 0), floating pane 3.
        // Window 4: one floating pane 6 with no tiled neighbour.
        f.out(
            "list-panes",
            &[
                pane_rec(2, 1, 1, false, false),
                pane_rec(1, 1, 0, true, false),
                pane_rec(3, 1, 2, false, true),
                pane_rec(6, 4, 0, true, true),
            ]
            .concat(),
        );
        f
    }

    #[test]
    fn toggle_floating_on_3_8_floats_or_tiles_onto_the_first_tiled_pane() {
        let f = toggle_floating_fake("3.8");
        assert!(
            f.backend
                .toggle_pane_floating("s", PaneRef::terminal(3))
                .unwrap()
                .ok
        );
        assert!(
            f.backend
                .toggle_pane_floating("s", PaneRef::terminal(2))
                .unwrap()
                .ok
        );
        let lonely = f
            .backend
            .toggle_pane_floating("s", PaneRef::terminal(6))
            .unwrap();
        assert!(!lonely.ok);
        assert_eq!(
            f.calls_of("join-pane"),
            [v(&["join-pane", "-s", "%3", "-t", "%1"])]
        );
        assert_eq!(
            f.calls_of("break-pane"),
            [v(&["break-pane", "-W", "-s", "%2"])]
        );
    }

    #[test]
    fn toggle_floating_below_3_8_fails_honestly() {
        for version in ["3.5a", "3.7c"] {
            let f = toggle_floating_fake(version);
            for pane in [3, 2] {
                let ack = f
                    .backend
                    .toggle_pane_floating("s", PaneRef::terminal(pane))
                    .unwrap();
                assert!(!ack.ok, "{version} pane {pane}");
                assert!(ack.error.unwrap().contains("3.8"), "{version}");
            }
            assert_eq!(f.commands(), ["list-panes", "list-panes"], "{version}");
        }
    }

    // ── Layout, agents, fullscreen context ──────────────────────────────────

    #[test]
    fn query_layout_stamps_agent_marks() {
        let f = Fake::new("3.7c");
        f.out("list-windows", &window_rec(1, 0, true, false, "main"));
        f.out(
            "list-panes",
            &[
                pane_rec(1, 1, 0, true, false),
                pane_rec(2, 1, 1, false, false),
            ]
            .concat(),
        );
        f.backend.agent_state().set_pane(2, agent_row("s", "main"));
        let layout = f.backend.query_layout("s").unwrap();
        let panes = &layout.tabs[0].panes;
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].agent, None);
        assert_eq!(
            panes[1].agent,
            Some(AgentMark {
                kind: "claude".into(),
                name: Some("Claude".into()),
                status: AgentStatus::Working,
            })
        );
        assert_eq!(layout.tabs[0].name, "main");
        assert_eq!(f.backend.query_session_size("s").unwrap(), (24, 80));
    }

    #[test]
    fn list_agents_filters_prunes_and_leaves_space_empty() {
        let f = Fake::new("3.7c");
        f.out(
            "list-windows",
            &[
                window_rec(1, 0, true, false, "main"),
                window_rec(2, 1, false, false, "side"),
            ]
            .concat(),
        );
        f.out(
            "list-panes",
            &[
                pane_rec(1, 1, 0, true, false),
                pane_rec(3, 2, 0, true, false),
            ]
            .concat(),
        );
        let state = f.backend.agent_state();
        state.set_pane(1, agent_row("s", "stale"));
        state.set_pane(3, agent_row("s", "side"));
        state.set_pane(5, agent_row("s", "main"));
        state.set_pane(7, agent_row("other", "main"));
        let agents = f.backend.list_agents("s").unwrap();
        let ids: Vec<u32> = agents.iter().map(|a| a.pane).collect();
        // Same status: the later change sorts first.
        assert_eq!(ids, [3, 1]);
        let a1 = agents.iter().find(|a| a.pane == 1).unwrap();
        assert!(a1.space_id.is_empty() && a1.space_name.is_empty());
        assert_eq!(a1.name, None);
        assert_eq!(a1.tab_id, 1);
        assert_eq!(a1.tab_name, "main");
        assert_eq!(a1.cwd.as_deref(), Some("/home/p1"));
        assert_eq!(a1.title.as_deref(), Some("t1"));
        assert_eq!(a1.summary.as_deref(), Some("thinking"));
        assert!(a1.focused);
        // Pane 3 is active but in an inactive window.
        assert!(!agents.iter().find(|a| a.pane == 3).unwrap().focused);
        // The vanished pane's row is gone; the other session's row stays.
        assert!(state.mark(5).is_none());
        assert!(state.mark(7).is_some());
        assert!(f.backend.supports_agents());
    }

    #[test]
    fn fullscreen_context_reads_the_holding_window() {
        let f = Fake::new("3.7c");
        f.out(
            "list-windows",
            &[
                window_rec(1, 0, true, true, "zoomed"),
                window_rec(2, 1, false, false, "plain"),
            ]
            .concat(),
        );
        f.out(
            "list-panes",
            &[
                pane_rec(1, 1, 0, true, false),
                pane_rec(2, 2, 0, true, false),
                pane_rec(4, 2, 1, false, true),
            ]
            .concat(),
        );
        let zoomed = f
            .backend
            .pane_fullscreen_context("s", PaneRef::terminal(1), Some(PaneRef::terminal(2)))
            .unwrap();
        assert!(zoomed.exists && zoomed.tab_fullscreen && zoomed.tab_active);
        assert!(!zoomed.is_floating);
        assert_eq!((zoomed.tab_id, zoomed.relay_focus_tab_id), (None, None));

        let plain = f
            .backend
            .pane_fullscreen_context("s", PaneRef::terminal(2), None)
            .unwrap();
        assert!(plain.exists && !plain.tab_fullscreen && !plain.tab_active);
        assert_eq!(plain.tab_id, None);

        let floating = f
            .backend
            .pane_fullscreen_context("s", PaneRef::terminal(4), None)
            .unwrap();
        assert!(floating.is_floating && floating.floating_visible);
        assert_eq!(floating.focused_floating, None);

        let gone = f
            .backend
            .pane_fullscreen_context("s", PaneRef::terminal(99), None)
            .unwrap();
        assert!(!gone.exists);
        assert_eq!(
            f.backend
                .pane_is_floating_with_visibility("s", PaneRef::terminal(4))
                .unwrap(),
            (true, true, None)
        );
    }

    #[test]
    fn open_attach_on_a_missing_session_is_unknown_session() {
        let f = Fake::new("3.7c");
        f.fail("has-session", "can't find session: nope\n");
        let err = f
            .backend
            .open_attach("nope", 24, 80, false)
            .err()
            .expect("missing session");
        let unknown = err.downcast_ref::<UnknownSession>().expect("typed error");
        assert_eq!(unknown.session, "nope");
        assert_eq!(f.calls(), [v(&["has-session", "-t", "=nope"])]);
        assert!(f.backend.open_attach("../x", 24, 80, false).is_err());
        assert_eq!(f.calls().len(), 1);
    }

    #[test]
    fn accessors_expose_the_runner_identity() {
        let f = Fake::new("3.8");
        assert_eq!(f.backend.socket().path, f.dir.join("sock"));
        assert_eq!(f.backend.version().to_string(), "3.8");
        assert_eq!(f.backend.backend_version(), "tmux 3.8");
        assert!(f.backend.validate_session_name("good_name").is_ok());
        assert!(f.backend.validate_session_name("../bad").is_err());
    }

    // ── Live ────────────────────────────────────────────────────────────────

    /// A private tmux server (`-L <unique>`, no config), killed on drop.
    struct PrivateServer {
        bin: PathBuf,
        name: String,
        socket: PathBuf,
    }

    impl PrivateServer {
        fn start() -> Option<Self> {
            let Ok(bin) = which::which("tmux") else {
                eprintln!("skipping: tmux not on PATH");
                return None;
            };
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let mut server = Self {
                bin,
                name: format!("muxr-backend-test-{}-{nanos}", std::process::id()),
                socket: PathBuf::new(),
            };
            server.tmux(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "base",
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ]);
            server.socket =
                PathBuf::from(server.tmux(&["display-message", "-p", "#{socket_path}"]));
            Some(server)
        }

        fn tmux(&self, args: &[&str]) -> String {
            let out = std::process::Command::new(&self.bin)
                .arg("-L")
                .arg(&self.name)
                .args(args)
                .env_remove("TMUX")
                .stdin(std::process::Stdio::null())
                .output()
                .expect("run tmux");
            assert!(
                out.status.success(),
                "tmux {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }

        fn backend(&self) -> TmuxBackend {
            let version = TmuxVersion::parse(&self.tmux(&["-V"])).unwrap();
            TmuxBackend::with_runner(Tmux::with_bin(
                TmuxSocket {
                    path: self.socket.clone(),
                },
                version,
                self.bin.clone().into_os_string(),
            ))
        }
    }

    impl Drop for PrivateServer {
        fn drop(&mut self) {
            let _ = std::process::Command::new(&self.bin)
                .arg("-L")
                .arg(&self.name)
                .arg("kill-server")
                .env_remove("TMUX")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if self.socket.file_name().is_some() {
                let _ = std::fs::remove_file(&self.socket);
            }
        }
    }

    #[test]
    #[ignore = "needs tmux on PATH; runs a private tmux server"]
    fn live_create_layout_new_pane_zoom_kill() {
        let Some(server) = PrivateServer::start() else {
            return;
        };
        let b = server.backend();
        assert!(b.create_session("e2e", None).unwrap().ok);
        let names: Vec<String> = b
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.0)
            .collect();
        assert!(names.contains(&"e2e".to_owned()), "{names:?}");

        let layout = b.query_layout("e2e").unwrap();
        assert_eq!(layout.tabs.len(), 1);
        assert_eq!(layout.tabs[0].panes.len(), 1);

        let ack = b.new_pane("e2e", false, Some("named".into())).unwrap();
        assert!(ack.ok, "{ack:?}");
        let id: u32 = ack.info.as_deref().unwrap().parse().unwrap();
        let layout = b.query_layout("e2e").unwrap();
        let panes = &layout.tabs[0].panes;
        assert_eq!(panes.len(), 2);
        let new = panes.iter().find(|p| p.id == id).expect("new pane listed");
        assert_eq!(new.title, "named");

        assert!(
            b.toggle_pane_fullscreen("e2e", PaneRef::terminal(id))
                .unwrap()
                .ok
        );
        let layout = b.query_layout("e2e").unwrap();
        assert!(layout.tabs[0].fullscreen_active);
        let ctx = b
            .pane_fullscreen_context("e2e", PaneRef::terminal(id), None)
            .unwrap();
        assert!(ctx.exists && ctx.tab_fullscreen && ctx.tab_active);
        assert!(
            b.toggle_pane_fullscreen("e2e", PaneRef::terminal(id))
                .unwrap()
                .ok
        );
        assert!(!b.query_layout("e2e").unwrap().tabs[0].fullscreen_active);

        if b.version().at_least(3, 7) {
            let ack = b.new_pane("e2e", true, None).unwrap();
            assert!(ack.ok, "{ack:?}");
            let fid: u32 = ack.info.as_deref().unwrap().parse().unwrap();
            let layout = b.query_layout("e2e").unwrap();
            let floating = layout.tabs[0].panes.iter().find(|p| p.id == fid).unwrap();
            assert!(floating.is_floating);
            let toggled = b
                .toggle_pane_floating("e2e", PaneRef::terminal(fid))
                .unwrap();
            assert_eq!(toggled.ok, b.version().at_least(3, 8), "{toggled:?}");
        }

        let tab = b.new_tab("e2e", Some("second".into())).unwrap();
        assert!(tab.ok, "{tab:?}");
        let layout = b.query_layout("e2e").unwrap();
        assert_eq!(layout.tabs.len(), 2);
        assert_eq!(layout.tabs[1].name, "second");

        // A pane of another session is refused.
        let base_pane = b.query_layout("base").unwrap().tabs[0].panes[0].id;
        assert!(
            !b.focus_pane("e2e", PaneRef::terminal(base_pane))
                .unwrap()
                .ok
        );

        assert!(b.kill_session("e2e").unwrap().ok);
        let names: Vec<String> = b
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.0)
            .collect();
        assert!(!names.contains(&"e2e".to_owned()), "{names:?}");
        let err = b.query_layout("e2e").expect_err("killed session");
        assert!(UnknownSession::in_chain(&err), "{err:#}");
    }
}
