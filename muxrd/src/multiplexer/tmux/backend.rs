//! The tmux [`MuxBackend`]: construction, identity and the not-yet-implemented
//! operation surface that later work fills in.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::agent_state::AgentState;
use super::paths::{TmuxSocket, TmuxVersion, tmux_bin, tmux_version};
use crate::multiplexer::types::{
    ActionAck, LayoutSnapshot, PaneRef, ResizeDir, ResizeKind, ScrollDir,
};
use crate::multiplexer::{DualHandle, MuxBackend};

/// The tmux-backed [`MuxBackend`].
#[derive(Debug)]
pub struct TmuxBackend {
    socket: TmuxSocket,
    version: TmuxVersion,
    agents: Arc<AgentState>,
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
        Ok(Self {
            socket,
            version,
            agents: Arc::new(AgentState::new()),
        })
    }

    /// The shared per-pane agent state.
    pub fn agent_state(&self) -> Arc<AgentState> {
        Arc::clone(&self.agents)
    }

    /// The tmux server socket this backend drives.
    pub fn socket(&self) -> &TmuxSocket {
        &self.socket
    }
}

fn not_implemented(op: &str) -> ActionAck {
    ActionAck {
        ok: false,
        error: Some(format!("tmux backend: not yet implemented ({op})")),
        info: None,
    }
}

impl MuxBackend for TmuxBackend {
    fn list_sessions(&self) -> Result<Vec<(String, Duration)>> {
        Ok(Vec::new())
    }

    fn list_sessions_with_resurrectables(&self) -> Result<Vec<(String, u64, bool)>> {
        Ok(Vec::new())
    }

    fn validate_session_name(&self, name: &str) -> std::result::Result<(), String> {
        crate::ipc::validate_session_name(name)
    }

    fn create_session(&self, _name: &str, _layout: Option<String>) -> Result<ActionAck> {
        Ok(not_implemented("create_session"))
    }

    fn kill_session(&self, _session: &str) -> Result<ActionAck> {
        Ok(not_implemented("kill_session"))
    }

    fn rename_session(&self, _session: &str, _new_name: String) -> Result<ActionAck> {
        Ok(not_implemented("rename_session"))
    }

    fn write_to_pane(&self, _session: &str, _pane: PaneRef, _bytes: Vec<u8>) -> Result<ActionAck> {
        Ok(not_implemented("write_to_pane"))
    }

    fn focus_pane(&self, _session: &str, _pane: PaneRef) -> Result<ActionAck> {
        Ok(not_implemented("focus_pane"))
    }

    fn close_pane(&self, _session: &str, _pane: PaneRef) -> Result<ActionAck> {
        Ok(not_implemented("close_pane"))
    }

    fn new_pane(
        &self,
        _session: &str,
        _floating: bool,
        _name: Option<String>,
    ) -> Result<ActionAck> {
        Ok(not_implemented("new_pane"))
    }

    fn rename_pane(&self, _session: &str, _pane: PaneRef, _name: String) -> Result<ActionAck> {
        Ok(not_implemented("rename_pane"))
    }

    fn resize_pane(
        &self,
        _session: &str,
        _pane: PaneRef,
        _kind: ResizeKind,
        _dir: Option<ResizeDir>,
    ) -> Result<ActionAck> {
        Ok(not_implemented("resize_pane"))
    }

    fn toggle_pane_floating(&self, _session: &str, _pane: PaneRef) -> Result<ActionAck> {
        Ok(not_implemented("toggle_pane_floating"))
    }

    fn toggle_pane_fullscreen(&self, _session: &str, _pane: PaneRef) -> Result<ActionAck> {
        Ok(not_implemented("toggle_pane_fullscreen"))
    }

    fn scroll_pane(&self, _session: &str, _pane: PaneRef, _dir: ScrollDir) -> Result<ActionAck> {
        Ok(not_implemented("scroll_pane"))
    }

    fn new_tab(&self, _session: &str, _name: Option<String>) -> Result<ActionAck> {
        Ok(not_implemented("new_tab"))
    }

    fn close_tab(&self, _session: &str, _tab_id: u64) -> Result<ActionAck> {
        Ok(not_implemented("close_tab"))
    }

    fn go_to_tab(&self, _session: &str, _tab_id: u64) -> Result<ActionAck> {
        Ok(not_implemented("go_to_tab"))
    }

    fn rename_tab(&self, _session: &str, _tab_id: u64, _name: String) -> Result<ActionAck> {
        Ok(not_implemented("rename_tab"))
    }

    fn query_layout(&self, _session: &str) -> Result<LayoutSnapshot> {
        Ok(LayoutSnapshot::default())
    }

    fn query_session_size(&self, _session: &str) -> Result<(u16, u16)> {
        bail!("tmux backend: not yet implemented (query_session_size)")
    }

    fn pane_is_floating_with_visibility(
        &self,
        _session: &str,
        _pane: PaneRef,
    ) -> Result<(bool, bool, Option<PaneRef>)> {
        Ok((false, false, None))
    }

    fn open_attach(
        &self,
        _session: &str,
        _rows: u16,
        _cols: u16,
        _read_only: bool,
    ) -> Result<DualHandle> {
        bail!("tmux backend: not yet implemented (open_attach)")
    }

    fn backend_version(&self) -> String {
        format!("tmux {}", self.version)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use super::*;
    use crate::multiplexer::tmux::paths::TMUX_BIN_ENV;

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

    fn fake_tmux(tag: &str, printed: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("muxrd-tmux-fake-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tmux");
        std::fs::write(&path, format!("#!/bin/sh\necho '{printed}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn backend() -> TmuxBackend {
        TmuxBackend {
            socket: TmuxSocket {
                path: "/nonexistent/tmux.sock".into(),
            },
            version: TmuxVersion::parse("tmux 3.7c").unwrap(),
            agents: Arc::new(AgentState::new()),
        }
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
    }

    #[test]
    fn reads_are_empty_and_mutations_are_not_implemented() {
        let b = backend();
        assert!(b.list_sessions().unwrap().is_empty());
        assert!(b.list_sessions_with_resurrectables().unwrap().is_empty());
        assert_eq!(b.query_layout("s").unwrap(), LayoutSnapshot::default());
        assert_eq!(
            b.pane_is_floating_with_visibility("s", PaneRef::terminal(1))
                .unwrap(),
            (false, false, None)
        );
        let ack = b.kill_session("s").unwrap();
        assert!(!ack.ok);
        assert_eq!(
            ack.error.as_deref(),
            Some("tmux backend: not yet implemented (kill_session)")
        );
        assert!(ack.info.is_none());
        assert!(b.query_session_size("s").is_err());
        assert!(b.open_attach("s", 24, 80, false).is_err());
        assert_eq!(b.backend_version(), "tmux 3.7c");
        assert!(b.validate_session_name("good_name").is_ok());
        assert!(b.validate_session_name("../bad").is_err());
    }
}
