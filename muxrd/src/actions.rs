//! actions — send-action-and-await-ack helper (Phase D1).
//!
//! Generalises the C1 query flow into a **mutating action** path.  Mutating ops
//! use the SAME IPC channel as queries: a short-lived cli-client connection that
//! sends `ClientToServerMsg::Action { is_cli_client: true, … }` and reads the
//! server's reply.
//!
//! ## Ack semantics (verified against zellij 0.45.1 `zellij-server/src/route.rs`)
//!
//! `route_thread_main` runs the action with a completion timeout, then — per the
//! tail of `route_action` (route.rs:2065-2124) — sends to the cli-client, in order:
//!   - `LogError { lines }`  when the action produced an error message, then
//!   - `Log { lines }`       carrying any `stdout_message`, then
//!   - `Log { lines }`       carrying the affected **tab id** (tab ops), then
//!   - `Log { lines }`       carrying the affected **pane id** (e.g. a new pane,
//!     format `terminal_<n>` / `plugin_<n>`).
//!
//! Then, *after every* routed client message, the route loop unconditionally
//! sends `ServerToClientMsg::UnblockInputThread` (route.rs:2661).  That message
//! is therefore the reliable **terminator** for "the action finished processing".
//!
//! So the helper drains messages, accumulating `Log` lines as `info` and
//! `LogError` lines as `error`, until it sees `UnblockInputThread` — at which
//! point it returns an [`ActionAck`].  This is robust across every op: ops that
//! emit no Log (the common mutation case) still terminate on UnblockInputThread.
//!
//! **Caveat — the AttachClient terminator.**  Like the C1 query path, we must do
//! a minimal `AttachClient` handshake first (the server won't route Log replies
//! to an un-attached client in 0.45.1).  But `AttachClient` is itself a routed
//! client message, so it produces its *own* `UnblockInputThread`.  We therefore
//! skip exactly one `UnblockInputThread` (the attach's) before arming the
//! terminator logic for the action.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use zellij_utils::data::PaneId;
use zellij_utils::input::actions::Action;
use zellij_utils::ipc::{ClientToServerMsg, IpcSenderWithContext, ServerToClientMsg};
use zellij_utils::pane_size::Size;

/// Maximum number of messages to drain while waiting for the action terminator.
const DRAIN_LIMIT: usize = 500;

/// How long to wait for the action to finish processing before giving up.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

// ─── Ack type ──────────────────────────────────────────────────────────────────

/// Result of a send-action-and-await-ack round trip.
#[derive(Debug, Clone, Default)]
pub struct ActionAck {
    /// `true` if the action completed without a `LogError`.
    pub ok: bool,
    /// Concatenated `LogError` lines, if any (the surfaced failure message).
    pub error: Option<String>,
    /// Concatenated `Log` lines, if any (stdout / new pane id / tab id).
    pub info: Option<String>,
}

// ─── Public API ────────────────────────────────────────────────────────────────

/// Send a (mutating) [`Action`] to a named session and await its completion ack.
///
/// Opens a short-lived cli-client IPC connection, performs the minimal
/// `AttachClient` handshake required by 0.45.1, sends the action with
/// `is_cli_client=true`, then drains messages until the action's
/// `UnblockInputThread` terminator — mapping any `Log`/`LogError` replies into
/// an [`ActionAck`].
///
/// This is **blocking** — call from a `tokio::task::spawn_blocking` context.
pub fn send_action(session: &str, action: Action) -> Result<ActionAck> {
    // Defence in depth (Major G): validate before building a socket path.
    crate::ipc::validate_session_name(session).map_err(|e| anyhow::anyhow!(e))?;
    let socket_path = session_socket_path(session);
    if !socket_path.exists() {
        bail!(
            "session socket {} does not exist — is '{}' running?",
            socket_path.display(),
            session
        );
    }

    let stream = zellij_utils::consts::ipc_connect(&socket_path)
        .with_context(|| format!("ipc_connect failed for {}", socket_path.display()))?;

    let mut sender: IpcSenderWithContext<ClientToServerMsg> = IpcSenderWithContext::new(stream);
    let mut receiver = sender.get_receiver::<ServerToClientMsg>();

    // ── Minimal AttachClient handshake ───────────────────────────────────────
    // Required (see C1 findings): the server won't route Log replies to an
    // un-attached client.  This attach is itself routed and yields its own
    // UnblockInputThread, which we skip below before arming the action ack.
    //
    // FA: attach at a NEUTRAL-LARGE size (not 24×80) so this ephemeral action
    // client never becomes the smallest client on whichever tab it lands on
    // and shrinks a real client's geometry. Since 0.45.1 zellij recomputes a
    // tab's size from only the clients currently focused on that tab (no more
    // session-wide minimum), so a large neutral attach's blast radius is just
    // that one tab rather than the whole session. See `crate::query::NEUTRAL_ATTACH_*`.
    use zellij_utils::input::cli_assets::CliAssets;
    let cli_assets = CliAssets {
        terminal_window_size: Size {
            rows: crate::query::NEUTRAL_ATTACH_ROWS as usize,
            cols: crate::query::NEUTRAL_ATTACH_COLS as usize,
        },
        ..Default::default()
    };
    sender
        .send_client_msg(ClientToServerMsg::AttachClient {
            cli_assets,
            tab_position_to_focus: None,
            pane_to_focus: None,
            is_web_client: false,
        })
        .context("send_action: failed to send AttachClient")?;

    // ── Send the action ──────────────────────────────────────────────────────
    sender
        .send_client_msg(ClientToServerMsg::Action {
            action,
            terminal_id: None,
            client_id: None,
            is_cli_client: true,
        })
        .context("send_action: failed to send Action")?;

    // ── Drain until the action's UnblockInputThread terminator ───────────────
    // We expect TWO UnblockInputThread messages: the first terminates the
    // AttachClient, the second terminates our Action.  Log/LogError replies for
    // the action arrive *before* the second terminator.
    let mut skipped_attach_unblock = false;
    let mut info_lines: Vec<String> = Vec::new();
    let mut error_lines: Vec<String> = Vec::new();

    let deadline = std::time::Instant::now() + ACK_TIMEOUT;
    for i in 0..DRAIN_LIMIT {
        if std::time::Instant::now() > deadline {
            bail!(
                "send_action: timed out after {} messages waiting for action ack",
                i
            );
        }
        let Some((msg, _ctx)) = receiver.recv_server_msg() else {
            bail!("send_action: IPC stream closed before action ack arrived");
        };
        match msg {
            ServerToClientMsg::UnblockInputThread => {
                if skipped_attach_unblock {
                    // Second terminator → the action is done.
                    let error = if error_lines.is_empty() {
                        None
                    } else {
                        Some(error_lines.join("\n"))
                    };
                    let info = if info_lines.is_empty() {
                        None
                    } else {
                        Some(info_lines.join("\n"))
                    };
                    return Ok(ActionAck {
                        ok: error.is_none(),
                        error,
                        info,
                    });
                }
                // First terminator → belonged to the AttachClient; arm for the action.
                skipped_attach_unblock = true;
            }
            ServerToClientMsg::Log { lines } => {
                log::trace!("send_action: Log({} lines)", lines.len());
                info_lines.extend(lines);
            }
            ServerToClientMsg::LogError { lines } => {
                log::debug!("send_action: LogError({} lines)", lines.len());
                error_lines.extend(lines);
            }
            ServerToClientMsg::Exit { exit_reason } => {
                bail!("send_action: session exited during action: {exit_reason:?}");
            }
            other => {
                log::trace!("send_action: draining message {i}: {other:?}");
            }
        }
    }
    bail!(
        "send_action: exceeded {} messages without an action ack",
        DRAIN_LIMIT
    )
}

/// Resolve the IPC socket path for a named session.
///
/// Delegates to [`crate::ipc::session_socket_path`] — one canonical implementation.
fn session_socket_path(session_name: &str) -> PathBuf {
    crate::ipc::session_socket_path(session_name)
}

// ─── PaneId mapping ──────────────────────────────────────────────────────────

/// Map a `(pane_id, is_plugin)` pair from a [`crate::proto::PaneTarget`] into the
/// zellij [`PaneId`] enum.
///
/// `is_plugin == false → PaneId::Terminal(id)`, else `PaneId::Plugin(id)`
/// (verified `data.rs:2826`).
pub fn pane_id_from_target(pane_id: u32, is_plugin: bool) -> PaneId {
    if is_plugin {
        PaneId::Plugin(pane_id)
    } else {
        PaneId::Terminal(pane_id)
    }
}

// ─── Typed action helpers (Phase D1: pane ops) ──────────────────────────────────

/// `WriteToPaneId { bytes, pane_id }` — write raw bytes to a specific pane.
pub fn write_to_pane(session: &str, pane: PaneId, bytes: Vec<u8>) -> Result<ActionAck> {
    send_action(
        session,
        Action::WriteToPaneId {
            bytes,
            pane_id: pane,
        },
    )
}

/// `FocusPaneByPaneId { pane_id }` — focus a specific pane (allowed for read-only).
pub fn focus_pane(session: &str, pane: PaneId) -> Result<ActionAck> {
    send_action(session, Action::FocusPaneByPaneId { pane_id: pane })
}

/// Close a pane.  Terminal panes → `CloseTerminalPane`, plugin panes →
/// `ClosePluginPane` (both take a raw `u32`).
pub fn close_pane(session: &str, pane: PaneId) -> Result<ActionAck> {
    let action = match pane {
        PaneId::Terminal(id) => Action::CloseTerminalPane { pane_id: id },
        PaneId::Plugin(id) => Action::ClosePluginPane { pane_id: id },
    };
    send_action(session, action)
}

/// `NewPane` / `NewFloatingPane` / `NewTiledPane` — open a new pane.
///
/// The newly-created pane id is returned by the server as a `Log` line
/// (`terminal_<n>` / `plugin_<n>`) and surfaces in [`ActionAck::info`].
///
/// - `floating == true` → `NewFloatingPane` (a richer variant).
/// - otherwise → `NewPane` (focused, tiled in the biggest available space).
pub fn new_pane(session: &str, floating: bool, pane_name: Option<String>) -> Result<ActionAck> {
    let action = if floating {
        Action::NewFloatingPane {
            command: None,
            pane_name,
            coordinates: None,
            near_current_pane: false,
            tab_id: None,
            no_focus: false,
        }
    } else {
        Action::NewPane {
            direction: None,
            pane_name,
            start_suppressed: false,
        }
    };
    send_action(session, action)
}

/// Rename a pane.  Terminal panes → `RenameTerminalPane`, plugin panes →
/// `RenamePluginPane` (both take a raw `u32` + `Vec<u8>` name).
pub fn rename_pane(session: &str, pane: PaneId, name: String) -> Result<ActionAck> {
    let name_bytes = name.into_bytes();
    let action = match pane {
        PaneId::Terminal(id) => Action::RenameTerminalPane {
            pane_id: id,
            name: name_bytes,
        },
        PaneId::Plugin(id) => Action::RenamePluginPane {
            pane_id: id,
            name: name_bytes,
        },
    };
    send_action(session, action)
}

/// `ResizeByPaneId { pane_id, resize, direction }` — resize a specific pane.
pub fn resize_pane(
    session: &str,
    pane: PaneId,
    resize: zellij_utils::data::Resize,
    direction: Option<zellij_utils::data::Direction>,
) -> Result<ActionAck> {
    send_action(
        session,
        Action::ResizeByPaneId {
            pane_id: pane,
            resize,
            direction,
        },
    )
}

/// `TogglePaneEmbedOrFloatingByPaneId { pane_id }` — toggle a pane between
/// floating and embedded (tiled).
pub fn toggle_pane_floating(session: &str, pane: PaneId) -> Result<ActionAck> {
    send_action(
        session,
        Action::TogglePaneEmbedOrFloatingByPaneId { pane_id: pane },
    )
}

/// `ToggleFocusFullscreenByPaneId { pane_id }` — toggle fullscreen for a pane.
pub fn toggle_pane_fullscreen(session: &str, pane: PaneId) -> Result<ActionAck> {
    send_action(
        session,
        Action::ToggleFocusFullscreenByPaneId { pane_id: pane },
    )
}

// ─── Phase D2: tab ops ──────────────────────────────────────────────────────────

/// `NewTab { tab_name, … }` — open a new tab.
///
/// The new tab id is returned by the server as a `Log` line (affected_tab_id)
/// and surfaces in [`ActionAck::info`].
pub fn new_tab(session: &str, tab_name: Option<String>) -> Result<ActionAck> {
    send_action(
        session,
        Action::NewTab {
            tiled_layout: None,
            floating_layouts: vec![],
            swap_tiled_layouts: None,
            swap_floating_layouts: None,
            tab_name,
            should_change_focus_to_new_tab: true,
            cwd: None,
            initial_panes: None,
            first_pane_unblock_condition: None,
        },
    )
}

/// `CloseTabById { id }` — close a tab by its stable id.
pub fn close_tab(session: &str, tab_id: u64) -> Result<ActionAck> {
    send_action(session, Action::CloseTabById { id: tab_id })
}

/// `GoToTabById { id }` — switch focus to a tab by its stable id.
pub fn go_to_tab(session: &str, tab_id: u64) -> Result<ActionAck> {
    send_action(session, Action::GoToTabById { id: tab_id })
}

/// `RenameTabById { id, name }` — rename a tab by its stable id.
pub fn rename_tab(session: &str, tab_id: u64, name: String) -> Result<ActionAck> {
    send_action(session, Action::RenameTabById { id: tab_id, name })
}

// ─── Phase D2: scroll ──────────────────────────────────────────────────────────

/// Scroll direction for [`scroll_pane`].
#[derive(Debug, Clone, Copy)]
pub enum ScrollDir {
    Up,
    Down,
    ToTop,
    ToBottom,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
}

/// Scroll a specific pane in the given direction (read-only allowed).
pub fn scroll_pane(session: &str, pane: PaneId, dir: ScrollDir) -> Result<ActionAck> {
    let action = match dir {
        ScrollDir::Up => Action::ScrollUpByPaneId { pane_id: pane },
        ScrollDir::Down => Action::ScrollDownByPaneId { pane_id: pane },
        ScrollDir::ToTop => Action::ScrollToTopByPaneId { pane_id: pane },
        ScrollDir::ToBottom => Action::ScrollToBottomByPaneId { pane_id: pane },
        ScrollDir::PageUp => Action::PageScrollUpByPaneId { pane_id: pane },
        ScrollDir::PageDown => Action::PageScrollDownByPaneId { pane_id: pane },
        ScrollDir::HalfPageUp => Action::HalfPageScrollUpByPaneId { pane_id: pane },
        ScrollDir::HalfPageDown => Action::HalfPageScrollDownByPaneId { pane_id: pane },
    };
    send_action(session, action)
}

// ─── Phase D2: session lifecycle ───────────────────────────────────────────────

/// `Action::RenameSession { name }` — rename the running session.
pub fn rename_session(session: &str, new_name: String) -> Result<ActionAck> {
    send_action(session, Action::RenameSession { name: new_name })
}

/// Send `ClientToServerMsg::KillSession` to the named session socket directly.
///
/// This is NOT an `Action` — it is a direct IPC message (mirrors
/// `zellij_utils::sessions::kill_session` but returns a `Result` instead of
/// exiting the process on error).
pub fn kill_session(session: &str) -> Result<()> {
    crate::ipc::validate_session_name(session).map_err(|e| anyhow::anyhow!(e))?;
    let socket_path = session_socket_path(session);
    if !socket_path.exists() {
        bail!(
            "kill_session: socket {} does not exist — is '{}' running?",
            socket_path.display(),
            session
        );
    }
    let stream = zellij_utils::consts::ipc_connect(&socket_path).with_context(|| {
        format!(
            "kill_session: ipc_connect failed for {}",
            socket_path.display()
        )
    })?;
    let mut sender: IpcSenderWithContext<ClientToServerMsg> = IpcSenderWithContext::new(stream);
    sender
        .send_client_msg(ClientToServerMsg::KillSession)
        .context("kill_session: failed to send KillSession")?;
    log::info!("kill_session: sent KillSession to '{session}'");
    Ok(())
}

/// How long to poll for a freshly-created session before giving up. Generous
/// enough to cover a cold first-create just after the server starts.
const CREATE_SESSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Polling interval while waiting for the new session's socket to appear.
const CREATE_SESSION_POLL: Duration = Duration::from_millis(50);

/// Build the zellij argv (everything after the binary path) for creating a
/// detached session: `zellij [--layout <path>] attach --create-background <name>`.
///
/// CRITICAL ordering: `--layout` is a TOP-LEVEL flag and MUST come BEFORE the
/// `attach` subcommand. zellij's `attach` subcommand does not accept `--layout`
/// — placing it after fails arg parsing with "Found argument '--layout' which
/// wasn't expected" (exit 2), the session never starts, and (because stderr is
/// detached to /dev/null) the caller only sees the socket-poll timeout. This is
/// exactly how the dev-rig entrypoint invokes it (`zellij --layout … attach …`).
fn build_create_session_args(name: &str, layout: Option<&str>) -> Vec<String> {
    let mut args = Vec::with_capacity(5);
    if let Some(layout) = layout
        && !layout.is_empty()
    {
        args.push("--layout".to_string());
        args.push(layout.to_string());
    }
    args.push("attach".to_string());
    args.push("--create-background".to_string());
    args.push(name.to_string());
    args
}

/// Spawn a detached zellij session by name using
/// `zellij [--layout <path>] attach --create-background <name>`.
///
/// `--create-background` (not `--create`) is essential: the server runs zellij
/// fully detached — its own session via a `pre_exec` `libc::setsid()` hook, with
/// stdin/stdout/stderr redirected to `/dev/null` and NO controlling terminal.
/// `attach --create` would create the session and then try to ATTACH a client,
/// which needs a TTY; with no terminal that attach phase fails and the session
/// never comes up (the socket never appears → caller times out).
/// `--create-background` creates the detached session without attaching, which
/// is exactly what a server-spawned background session needs (and what the dev
/// rig's entrypoint uses).
///
/// Rather than sleeping a fixed interval (the old D2 behaviour: a hard-coded
/// ~1.2 s), this **polls** for the session's IPC socket to appear (up to
/// [`CREATE_SESSION_TIMEOUT`]) and returns as soon as it is live — typically far
/// faster.  Returns an error if the session never materialises in time.
pub fn create_session(name: &str, layout: Option<String>) -> Result<ActionAck> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    // Defence in depth (Major G): the name is passed as an argv to the spawned
    // `zellij` binary and used to build the poll socket path — validate first.
    crate::ipc::validate_session_name(name).map_err(|e| anyhow::anyhow!(e))?;

    // Find the zellij binary.
    let zellij_bin = which_zellij()?;

    // Resolve the layout. A client-supplied (already name-validated) layout wins;
    // otherwise default to the bundled BAR-LESS layout so app-created sessions
    // hide zellij's tab-bar/status-bar (the mobile client renders those controls
    // itself). The default is a server-authored file at a fixed, server-controlled
    // path — not client input — so passing its absolute path to `--layout` is safe
    // (it is exempt from the client-layout name allowlist in grpc::session_ops).
    // If materialising it fails, fall back to zellij's own default rather than
    // failing the whole create.
    let resolved_layout: Option<String> = match layout {
        Some(l) if !l.is_empty() => Some(l),
        _ => match crate::config::ensure_default_session_layout() {
            Ok(p) => Some(p.to_string_lossy().into_owned()),
            Err(e) => {
                log::warn!(
                    "create_session: bar-less default layout unavailable ({e}); \
                     using zellij's built-in default (bars will be shown)"
                );
                None
            }
        },
    };

    let mut cmd = Command::new(&zellij_bin);
    cmd.args(build_create_session_args(name, resolved_layout.as_deref()));
    // Detach stdin/stdout/stderr so the process doesn't inherit ours.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Reap strategy (review Major E — zombie accumulation): perform a
    // **double-fork** in the pre-exec hook.  The `Command` we spawn is an
    // intermediate child that immediately forks again; the grandchild calls
    // `setsid()` + `exec`s zellij, and the intermediate child `_exit(0)`s right
    // away.  Because the server `wait()`s the intermediate child below, the
    // intermediate is reaped immediately and zellij — now reparented to init —
    // is never owned by the server, so no `<defunct>` can accumulate.
    //
    // SAFETY: the pre_exec hook runs in the freshly-forked child between fork()
    // and exec(); only async-signal-safe syscalls (fork/setsid/_exit) are used.
    unsafe {
        cmd.pre_exec(|| {
            // First fork: detach zellij from the intermediate child.
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => {
                    // Grandchild: become a session leader, then fall through to
                    // exec(zellij).  Check setsid()'s return (minor fix).
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                }
                _ => {
                    // Intermediate child: exit immediately so the grandchild is
                    // reparented to init.  _exit (not exit) — no atexit/flush in
                    // a post-fork child.
                    libc::_exit(0);
                }
            }
        });
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("create_session: failed to spawn '{}'", zellij_bin.display()))?;

    // Reap the intermediate child right away (it _exit(0)s after the second
    // fork).  This prevents the intermediate from lingering as a zombie; the
    // grandchild (zellij) is owned by init, not us.
    match child.wait() {
        Ok(status) => log::debug!("create_session: intermediate child reaped ({status})"),
        Err(e) => log::warn!("create_session: failed to reap intermediate child: {e}"),
    }

    log::info!(
        "create_session: spawned '{name}' via {}",
        zellij_bin.display()
    );

    // ── Poll for the session to become live ──────────────────────────────────
    // zellij registers an IPC socket under ZELLIJ_SOCK_DIR/<name> once the
    // session is up.  We poll on that path (cheap, no IPC) until it appears or
    // the timeout elapses — returning as soon as the session is live instead of
    // a fixed 1.2 s sleep.
    let socket_path = session_socket_path(name);
    let deadline = std::time::Instant::now() + CREATE_SESSION_TIMEOUT;
    loop {
        if socket_path.exists() {
            log::info!(
                "create_session: '{name}' live after {:?}",
                CREATE_SESSION_TIMEOUT
                    .checked_sub(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap_or_default()
            );
            return Ok(ActionAck {
                ok: true,
                error: None,
                info: Some(name.to_owned()),
            });
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "create_session: '{name}' did not appear within {:?} (socket {} never showed up)",
                CREATE_SESSION_TIMEOUT,
                socket_path.display()
            );
        }
        std::thread::sleep(CREATE_SESSION_POLL);
    }
}

/// Locate the `zellij` binary.
///
/// Resolution order: PATH (via `which`), then a `~/bin/zellij` fallback for
/// setups where `~/bin` isn't on a non-login `PATH`.  PATH-first avoids a
/// hardcoded install location while still finding the dev host install (which IS
/// on the login PATH at `~/bin/zellij`).
pub fn which_zellij() -> Result<std::path::PathBuf> {
    // Prefer whatever the environment's PATH resolves to.
    if let Ok(p) = which::which("zellij") {
        return Ok(p);
    }
    // Fall back to ~/bin/zellij (the dev host install location) if PATH didn't have it.
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::PathBuf::from(home).join("bin").join("zellij");
        if p.exists() {
            return Ok(p);
        }
    }
    bail!("could not find zellij binary on PATH or in ~/bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard: `--layout` MUST precede the `attach` subcommand, else
    /// zellij rejects it ("unexpected argument") and the session never starts.
    #[test]
    fn create_session_args_put_layout_before_attach() {
        let args = build_create_session_args("sess", Some("/abs/barless.kdl"));
        assert_eq!(
            args,
            vec![
                "--layout",
                "/abs/barless.kdl",
                "attach",
                "--create-background",
                "sess"
            ]
        );
        let layout_idx = args.iter().position(|a| a == "--layout").unwrap();
        let attach_idx = args.iter().position(|a| a == "attach").unwrap();
        assert!(
            layout_idx < attach_idx,
            "--layout must come before the attach subcommand"
        );
    }

    /// No layout → no `--layout` flag; just `attach --create-background <name>`.
    #[test]
    fn create_session_args_without_layout_omit_flag() {
        assert_eq!(
            build_create_session_args("sess", None),
            vec!["attach", "--create-background", "sess"]
        );
        assert_eq!(
            build_create_session_args("sess", Some("")),
            vec!["attach", "--create-background", "sess"]
        );
    }
}
