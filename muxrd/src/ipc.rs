//! ipc — IPC attach / input / render helpers (Phase A core).
//!
//! Wraps the raw `zellij_utils::ipc` types behind a small API so the
//! gRPC layer (grpc.rs / relay.rs in B2) can open a session attach and
//! exchange render/input messages without touching zellij internals
//! directly.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use zellij_utils::data::PaneId;
use zellij_utils::input::actions::Action;
use zellij_utils::input::cli_assets::CliAssets;
use zellij_utils::ipc::{
    ClientToServerMsg, IpcReceiverWithContext, IpcSenderWithContext, ServerToClientMsg,
};
use zellij_utils::pane_size::Size;

// ─── Session helpers ──────────────────────────────────────────────────────────

/// Returns all active session names (oldest-first ordering from zellij).
///
/// Wraps `zellij_utils::sessions::get_sessions()` which returns
/// `Result<Vec<(String, Duration)>, io::ErrorKind>`.
pub fn list_sessions() -> Result<Vec<(String, Duration)>> {
    zellij_utils::sessions::get_sessions()
        .map_err(|kind| anyhow!("failed to list zellij sessions: {kind:?}"))
}

/// Pick the first live session, or return an error if none exist.
pub fn pick_first_session() -> Result<String> {
    let sessions = list_sessions()?;
    if sessions.is_empty() {
        bail!(
            "no running zellij sessions found. \
             Start one (e.g. `zellij -s spikedemo`) or pass a session name explicitly."
        );
    }
    Ok(sessions.into_iter().next().unwrap().0)
}

/// Resolve the socket path for a named session.
pub fn session_socket_path(session_name: &str) -> PathBuf {
    zellij_utils::consts::ZELLIJ_SOCK_DIR.join(session_name)
}

/// Validate a session name before it is ever used to build a socket path
/// (`ZELLIJ_SOCK_DIR.join(name)`) or passed as an argument to the spawned
/// `zellij` binary.
///
/// **Security (review Major G — path traversal):** an un-validated name like
/// `../evil` or `foo/bar` escapes `ZELLIJ_SOCK_DIR` via `Path::join`, and an
/// attacker-controlled name reaching the `zellij` CLI is a command-surface
/// risk.  We layer two checks:
///
/// 1. zellij's own [`zellij_utils::sessions::validate_session_name`] — rejects
///    empty / `.` / `..` / any name containing `/`.
/// 2. A strict allowlist on top: every byte must be `[A-Za-z0-9_-]` (also
///    rejects `\0`, control chars, whitespace, `\`, `.`, and any other shell /
///    path metacharacter that zellij's check lets through).
///
/// Returns the offending message on failure so callers can surface it.
pub fn validate_session_name(name: &str) -> std::result::Result<(), String> {
    // 1. zellij's own check (empty / "." / ".." / contains '/').
    zellij_utils::sessions::validate_session_name(name)?;

    // 2. Strict allowlist — defence in depth. zellij's check permits e.g.
    //    backslashes, dots-in-middle, NUL and control bytes; we don't.
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "invalid session name {name:?}: only [A-Za-z0-9_-] characters are allowed"
        ));
    }
    Ok(())
}

/// Re-validate a session bearer token against zellij's shared tokens DB.
///
/// Thin wrapper over
/// [`zellij_utils::web_authentication_tokens::validate_session_token`] — sibling
/// to [`validate_session_name`] — so the relay (which is `zellij_utils`-free as
/// of P1.03) can re-check a live attach's token mid-stream (Major H) without
/// naming a zellij type. **Auth is deliberately NOT part of the `MuxBackend`
/// trait** (Phase 1): a herdr backend's sockets have no auth, so muxrd's token DB
/// stays the sole security boundary — the relay calls this directly rather than
/// going through the multiplexer seam (revisited in Phase 3).
pub fn validate_session_token(token: &str) -> Result<bool> {
    zellij_utils::web_authentication_tokens::validate_session_token(token)
        .map_err(|e| anyhow!("validate_session_token: {e}"))
}

// ─── Attach handle ────────────────────────────────────────────────────────────

/// An open IPC attach to a zellij session.
///
/// Wraps sender + receiver; call [`AttachHandle::recv`] to get the next
/// server message and [`AttachHandle::send_action`] to push input into
/// the focused pane.
///
/// This is deliberately **sync/blocking** — the Phase-B relay will run
/// `recv` on a dedicated std thread so the tokio runtime isn't blocked.
pub struct AttachHandle {
    pub sender: IpcSenderWithContext<ClientToServerMsg>,
    pub receiver: zellij_utils::ipc::IpcReceiverWithContext<ServerToClientMsg>,
    pub session_name: String,
}

impl AttachHandle {
    /// Open an IPC attach to the named session with the given terminal size.
    ///
    /// Sends the `AttachClient` handshake message before returning.
    ///
    /// **Geometry (originally review round-2 Major A):** on every
    /// `AttachClient` handshake zellij recomputes the size of the tab the
    /// client lands on from only the clients currently focused on that tab
    /// (`zellij-server/src/lib.rs`) — a per-tab minimum, not a session-wide
    /// one since 0.45.1.  The relay passes THIS caller's own rows/cols here
    /// on both tiers, read-only included — a viewer's own viewport size is
    /// not session content, so it sits inside the read-only boundary, and
    /// the accepted tradeoff is that the smallest client focused on a tab
    /// sizes it for everyone there.  `attach_relay` floors every size at
    /// `MIN_TERMINAL_ROWS`/`MIN_TERMINAL_COLS` before it ever reaches here,
    /// bounding only the degenerate case of a client asking for a near-zero
    /// grid.  This function just sends whatever size it's given — the floor
    /// lives in `attach_relay`.
    pub fn open(session_name: &str, rows: u16, cols: u16) -> Result<Self> {
        // Defence in depth (Major G): never build a socket path from an
        // unvalidated name, even if a caller forgot to gate it.
        validate_session_name(session_name).map_err(|e| anyhow!(e))?;
        let socket_path = session_socket_path(session_name);
        if !socket_path.exists() {
            bail!(
                "socket {} does not exist — is session '{}' actually running? \
                 (`zellij list-sessions`)",
                socket_path.display(),
                session_name
            );
        }

        let stream = zellij_utils::consts::ipc_connect(&socket_path)
            .with_context(|| format!("ipc_connect failed for {}", socket_path.display()))?;

        let mut sender: IpcSenderWithContext<ClientToServerMsg> = IpcSenderWithContext::new(stream);
        let receiver = sender.get_receiver::<ServerToClientMsg>();

        let cli_assets = CliAssets {
            terminal_window_size: Size {
                rows: rows as usize,
                cols: cols as usize,
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
            .context("failed to send AttachClient handshake")?;

        Ok(Self {
            sender,
            receiver,
            session_name: session_name.to_owned(),
        })
    }

    /// Open a **subscription-only** connection (W2.0b spike).
    ///
    /// Connects to the session socket and sends `SubscribeToPaneRenders`
    /// **without** an `AttachClient` handshake — mirroring zellij's own
    /// `zellij subscribe` cli_client. A bare subscription does NOT register as a
    /// rendering client (so it never perturbs session geometry) and receives the
    /// initial snapshot plus ongoing `PaneRenderUpdate`s for `pane_ids`.
    pub fn open_pane_subscription(
        session_name: &str,
        pane_ids: Vec<PaneId>,
        ansi: bool,
    ) -> Result<Self> {
        validate_session_name(session_name).map_err(|e| anyhow!(e))?;
        let socket_path = session_socket_path(session_name);
        if !socket_path.exists() {
            bail!("socket {} does not exist", socket_path.display());
        }
        let stream = zellij_utils::consts::ipc_connect(&socket_path)
            .with_context(|| format!("ipc_connect failed for {}", socket_path.display()))?;
        let mut sender: IpcSenderWithContext<ClientToServerMsg> = IpcSenderWithContext::new(stream);
        let receiver = sender.get_receiver::<ServerToClientMsg>();
        sender
            .send_client_msg(ClientToServerMsg::SubscribeToPaneRenders {
                pane_ids,
                scrollback: None,
                ansi,
            })
            .context("failed to send SubscribeToPaneRenders (bare subscription)")?;
        Ok(Self {
            sender,
            receiver,
            session_name: session_name.to_owned(),
        })
    }

    /// Receive the next message from the server.  Returns `None` on
    /// EOF / decode error (connection closed).
    pub fn recv(&mut self) -> Option<ServerToClientMsg> {
        self.receiver.recv_server_msg().map(|(msg, _ctx)| msg)
    }

    /// Send a single [`Action`] as a CLI client (`is_cli_client = true`).
    ///
    /// The server routes the action to `get_last_active_client()` → that
    /// client's focused pane, NOT this connection's. With another zellij client
    /// attached and recently typed-in, that is the *other* client — so this must
    /// NOT be used for input, focus, or any per-relay-client action (those use
    /// [`Self::send_action_as_self`]). Retained only for tooling/spikes that want
    /// the last-active-client routing explicitly.
    pub fn send_action(&mut self, action: Action) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::Action {
                action,
                terminal_id: None,
                client_id: None,
                is_cli_client: true,
            })
            .context("failed to send Action")
    }

    /// Send a single [`Action`] as **this attached client**
    /// (`is_cli_client = false`).
    ///
    /// W2.0a spike: a non-cli action is routed by the server to **this
    /// connection's own `client_id`**, not `get_last_active_client()`. So an
    /// action sent over the persistent relay attach applies to the very client
    /// whose render stream the phone sees → deterministic tab/pane switching,
    /// with no ephemeral CLI client and no dependence on who last typed.
    pub fn send_action_as_self(&mut self, action: Action) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::Action {
                action,
                terminal_id: None,
                client_id: None,
                is_cli_client: false,
            })
            .context("failed to send self Action")
    }

    /// Subscribe this attached client to per-pane render updates for `pane_ids`
    /// (W2.0b spike). With `ansi = true` the viewport lines carry ANSI styling.
    /// The server then emits [`ServerToClientMsg::PaneRenderUpdate`] for these
    /// panes (a viewport snapshot), independent of which pane is focused.
    pub fn subscribe_to_panes(&mut self, pane_ids: Vec<PaneId>, ansi: bool) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::SubscribeToPaneRenders {
                pane_ids,
                scrollback: None,
                ansi,
            })
            .context("failed to send SubscribeToPaneRenders")
    }

    /// Send raw bytes as keyboard input to **this client's own focused pane**
    /// via [`Action::WriteChars`].
    ///
    /// MULTI-CLIENT FIX: routed as `is_cli_client:false` (this connection's own
    /// `client_id`), NOT `is_cli_client:true`. The latter routes to
    /// `get_last_active_client()`, so when a native zellij client is also
    /// attached and types, it becomes last-active and our keystrokes land in
    /// *its* focused pane. Sending as-self targets the relay client's own pane
    /// regardless of who typed last — symmetric with the focus/tab actions.
    pub fn send_chars(&mut self, text: &str) -> Result<()> {
        self.send_action_as_self(Action::WriteChars {
            chars: text.to_owned(),
        })
    }

    /// Send raw bytes via [`Action::Write`] (individual byte sequence, e.g. ESC)
    /// to this client's own focused pane. See [`Self::send_chars`] for why this
    /// is `is_cli_client:false`.
    pub fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.send_action_as_self(Action::Write {
            key_with_modifier: None,
            bytes,
            is_kitty_keyboard_protocol: false,
        })
    }

    /// Notify the server of a new terminal size.
    pub fn send_resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::TerminalResize {
                new_size: Size {
                    rows: rows as usize,
                    cols: cols as usize,
                },
            })
            .context("failed to send TerminalResize")
    }

    /// Split the handle into an independent sender half (cheap, cloned socket)
    /// and a receiver half.
    ///
    /// The Phase-B relay runs the blocking [`AttachReceiver::recv`] loop on a
    /// dedicated std thread while keeping the [`AttachSender`] on the async
    /// side for input / resize.  Both halves wrap *clones* of the same
    /// underlying socket, so each can be moved to a different thread.
    pub fn split(self) -> (AttachSender, AttachReceiver) {
        let session_name = self.session_name;
        (
            AttachSender {
                sender: self.sender,
                session_name: session_name.clone(),
            },
            AttachReceiver {
                receiver: self.receiver,
                session_name,
            },
        )
    }
}

// ─── Split halves (for the async relay) ────────────────────────────────────────

/// The input/control half of a split [`AttachHandle`].
///
/// Lives on the async side of the Phase-B relay; carries input bytes and
/// resize events from the gRPC client into the session.  Cheaply cloneable
/// over the underlying socket so a shutdown drop-guard can nudge the server.
pub struct AttachSender {
    pub sender: IpcSenderWithContext<ClientToServerMsg>,
    pub session_name: String,
}

impl AttachSender {
    /// Independent clone over the same socket — used by the relay drop-guard
    /// to send a final resize "nudge" so the reader thread's blocking
    /// `recv()` wakes and observes shutdown.
    pub fn try_clone(&self) -> AttachSender {
        AttachSender {
            sender: self.sender.get_receiver::<ServerToClientMsg>().get_sender(),
            session_name: self.session_name.clone(),
        }
    }

    /// Send a single [`Action`] as a CLI client (`is_cli_client = true`) — routes
    /// to `get_last_active_client()`, NOT this connection. Never use for input or
    /// focus (those use [`Self::send_action_as_self`]); see
    /// [`AttachHandle::send_action`] for the full caveat.
    pub fn send_action(&mut self, action: Action) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::Action {
                action,
                terminal_id: None,
                client_id: None,
                is_cli_client: true,
            })
            .context("failed to send Action")
    }

    /// Send an action as **this attached client** (`is_cli_client = false`) so
    /// the server applies it to this connection's own `client_id` rather than
    /// `get_last_active_client()`. W2.0a: deterministic tab/pane switching for
    /// the relay's own (rendering) client.
    pub fn send_action_as_self(&mut self, action: Action) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::Action {
                action,
                terminal_id: None,
                client_id: None,
                is_cli_client: false,
            })
            .context("failed to send self Action")
    }

    /// (Re)subscribe this connection to per-pane render updates (W2.0b).
    pub fn subscribe_to_panes(&mut self, pane_ids: Vec<PaneId>, ansi: bool) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::SubscribeToPaneRenders {
                pane_ids,
                scrollback: None,
                ansi,
            })
            .context("failed to send SubscribeToPaneRenders")
    }

    /// Keyboard input → this client's own focused pane (`is_cli_client:false`).
    /// This is the live relay input path (`forward_input`). See
    /// [`AttachHandle::send_chars`] for the multi-client routing rationale.
    pub fn send_chars(&mut self, text: &str) -> Result<()> {
        self.send_action_as_self(Action::WriteChars {
            chars: text.to_owned(),
        })
    }

    /// Raw input bytes (e.g. ESC sequences) → this client's own focused pane
    /// (`is_cli_client:false`). See [`Self::send_chars`].
    pub fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.send_action_as_self(Action::Write {
            key_with_modifier: None,
            bytes,
            is_kitty_keyboard_protocol: false,
        })
    }

    pub fn send_resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::TerminalResize {
                new_size: Size {
                    rows: rows as usize,
                    cols: cols as usize,
                },
            })
            .context("failed to send TerminalResize")
    }

    /// Tell the server this client is leaving (`ClientExited`).
    ///
    /// The server removes this client and closes its connection, so a reader
    /// thread parked in a blocking `recv()` wakes (recv returns `None`).  Used
    /// by the relay drop-guard for **read-only** attaches in place of the
    /// resize "nudge": both wake the parked reader the same way, but
    /// `ClientExited` avoids sending an extra `TerminalResize` at the exact
    /// moment the connection is closing.  (A read-only attach's own geometry
    /// already drove its tab's size on the way in — floored at
    /// `MIN_TERMINAL_ROWS`/`MIN_TERMINAL_COLS`, see
    /// `crate::relay::attach_relay` — this is not a privilege boundary.)
    pub fn send_client_exited(&mut self) -> Result<()> {
        self.sender
            .send_client_msg(ClientToServerMsg::ClientExited)
            .context("failed to send ClientExited")
    }
}

/// The render half of a split [`AttachHandle`].
///
/// Moved into the dedicated std reader thread; [`AttachReceiver::recv`] is
/// **blocking**.
pub struct AttachReceiver {
    pub receiver: IpcReceiverWithContext<ServerToClientMsg>,
    pub session_name: String,
}

impl AttachReceiver {
    /// Receive the next message (blocking).  `None` on EOF / decode error.
    pub fn recv(&mut self) -> Option<ServerToClientMsg> {
        self.receiver.recv_server_msg().map(|(msg, _ctx)| msg)
    }
}

// ─── Input helpers ────────────────────────────────────────────────────────────

/// Dismiss the "About Zellij" startup overlay.
///
/// A freshly-created session boots with the tip-of-the-day plugin pane
/// focused.  Sending ESC × 3 dismisses it; Enter lands us in the shell.
pub fn dismiss_overlay(handle: &mut AttachHandle) -> Result<()> {
    log::info!("sending Esc × 3 to dismiss startup overlay…");
    for _ in 0..3 {
        handle.send_bytes(vec![0x1b])?; // ESC
        std::thread::sleep(Duration::from_millis(150));
    }
    log::info!("sending Enter to land in shell prompt…");
    handle.send_bytes(vec![0x0d])?; // CR / Enter
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}
