//! Independently-authored herdr interop — drives herdr's public v0.7.1 wire relay
//! socket for interop. Not derived from herdr's AGPL source; herdr runs as a
//! separate, unmodified, user-installed binary driven over its public sockets.
//!
//! # herdr **data plane** — the wire terminal relay (P2.03)
//!
//! This module is the herdr analogue of zellij's `ipc::AttachHandle` split: it
//! satisfies the neutral [`MuxSender`] / [`MuxReceiver`] traits and the
//! [`DualHandle`] the Phase-1 relay (`relay/*`, untouched) drives. The keystone
//! was proven live in the spike (`research/SPIKE_RESULT.md`).
//!
//! ## Single-pane attach model (load-bearing design call)
//!
//! herdr's wire [`AttachTerminal`](wire::ClientMessage::AttachTerminal) streams
//! **one pane's** ANSI content (not the whole composited tab, unlike zellij). So
//! the relay is attached to exactly **one pane's `terminal_id` at a time** —
//! initially the session's **focused pane**:
//!
//! - [`HerdrMuxReceiver`] forwards that pane's [`TerminalFrame`](wire::TerminalFrame)
//!   bytes (full-on-attach, then incrementals) verbatim into the gRPC
//!   `AttachTerminal` stream → kterm renders them unchanged.
//! - **Focus = re-attach.** [`HerdrMuxSender::focus_pane`] / [`go_to_tab`] re-point
//!   the stream by sending a fresh `AttachTerminal { takeover: true }` on the same
//!   wire socket; herdr replies with a new `full = true` frame for the new pane.
//! - The **pane strip / layout** comes from herdr's JSON-API
//!   ([`HerdrControl::query_layout`], surfaced out-of-band via
//!   [`MuxSender::query_layout_result`]) — geometry for all panes, live content for
//!   the attached one. This needs **no client change**.
//!
//! ## Threading / split (parallel to `ZellijMux{Sender,Receiver}`)
//!
//! One wire [`UnixStream`] is `try_clone`d into two independently-owned fds:
//! [`HerdrMuxReceiver`] owns the **read** half (blocking [`recv`](MuxReceiver::recv)
//! on the relay reader std-thread); [`HerdrMuxSender`] owns the **write** half plus
//! an `Arc<HerdrControl>` for the JSON-API control/layout calls. The read half has
//! **no read timeout** (it must block on the reader thread); the write half carries
//! a short write timeout so a wedged socket can never stall the inbound task.
//!
//! ## P2.04 wiring
//!
//! [`open_attach`] is the single entry point `HerdrBackend::open_attach` (P2.04)
//! calls: it performs the handshake, attaches the focused pane, and returns the
//! split [`DualHandle`]. P2.04 owns the `Arc<HerdrControl>` (and, through it, the
//! shared registries) and the resolved wire socket path; it maps the muxrd
//! `session` name to a herdr `workspace_id` before calling.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::multiplexer::types::{
    FullscreenHint, LayoutSnapshot, MuxEvent, MuxMouseKind, MuxServerMsg, PaneRef,
};
use crate::multiplexer::{DualHandle, MuxReceiver, MuxSender};

use super::api::{PaneInfo, PaneZoomMode};
use super::control::HerdrControl;
use super::registry::HerdrPaneRegistry;
use super::wire::{
    AttachScrollDirection, AttachScrollSource, ClientKeybindings, ClientLaunchMode, ClientMessage,
    FramingError, HERDR_PROTOCOL_VERSION, RenderEncoding, ServerMessage, read_server_message,
    write_message,
};

/// Bound on the blocking handshake (`Hello`/`Welcome`/`AttachTerminal`) and on
/// every wire write. herdr is co-located on a local Unix socket, so this is a
/// safety ceiling rather than an expected latency — it guarantees `open_attach`
/// and the sender's writes never wedge indefinitely. The reader half deliberately
/// has **no** read timeout (it blocks on the reader thread, which is correct).
const WIRE_TIMEOUT: Duration = Duration::from_secs(3);

/// Cell pixel dimensions advertised in `Hello`. `0` disables Kitty graphics
/// negotiation — muxrd relays raw ANSI to kterm, which carries its own renderer,
/// so we never request herdr-side graphics frames (matching the spike).
const CELL_PX_DISABLED: u32 = 0;

// ─── open_attach (P2.04 entry point) ──────────────────────────────────────────

/// Open a herdr wire attach for `workspace_id`, returning the split
/// [`DualHandle`]. Performs the v14 handshake, asserts protocol compatibility,
/// and attaches the workspace's **focused pane** (the single-pane attach model).
///
/// `session_name` is the neutral muxrd session name echoed back in
/// [`DualHandle::session_name`]; `workspace_id` is the already-resolved herdr
/// workspace id. `control` shares the JSON-API client + registries with the
/// backend (P2.04). `wire_socket` is herdr's binary relay socket
/// ([`HerdrSocketPaths::wire`](super::paths::HerdrSocketPaths)); P2.04 resolves it
/// once alongside the api socket so both planes agree on the instance.
///
/// `read_only` is logged for traceability; herdr enforces write ownership on the
/// terminal itself, and the read-only teardown nudge is `Detach`
/// ([`MuxSender::send_client_exited`]) for both modes.
pub fn open_attach(
    control: Arc<HerdrControl>,
    wire_socket: PathBuf,
    workspace_id: String,
    session_name: String,
    rows: u16,
    cols: u16,
    read_only: bool,
) -> Result<DualHandle> {
    log::debug!(
        "herdr open_attach workspace='{workspace_id}' session='{session_name}' \
         {rows}x{cols} read_only={read_only}"
    );

    // Resolve the focused pane's terminal_id (also populates the pane registry so
    // later focus_pane(PaneRef{id}) resolves), before opening the wire socket.
    let (_pane_id, terminal_id) = resolve_focused_terminal(&control, &workspace_id)?;

    // Connect the wire socket and run the handshake under a bounded timeout.
    let stream = UnixStream::connect(&wire_socket)
        .with_context(|| format!("connect herdr wire socket {}", wire_socket.display()))?;
    stream
        .set_read_timeout(Some(WIRE_TIMEOUT))
        .context("set herdr wire handshake read timeout")?;
    stream
        .set_write_timeout(Some(WIRE_TIMEOUT))
        .context("set herdr wire handshake write timeout")?;

    let hello = ClientMessage::Hello {
        version: HERDR_PROTOCOL_VERSION,
        cols,
        rows,
        cell_width_px: CELL_PX_DISABLED,
        cell_height_px: CELL_PX_DISABLED,
        requested_encoding: RenderEncoding::TerminalAnsi,
        keybindings: ClientKeybindings::Server,
        launch_mode: ClientLaunchMode::TerminalAttach,
    };
    write_message(&mut &stream, &hello).context("send herdr Hello")?;

    let welcome = read_server_message(&mut &stream).context("read herdr Welcome")?;
    assert_welcome(&welcome)?;

    // Re-point this connection at the focused pane's terminal.
    let attach = ClientMessage::AttachTerminal {
        terminal_id: terminal_id.clone(),
        takeover: true,
    };
    write_message(&mut &stream, &attach).context("send herdr AttachTerminal")?;

    // Split into read + write halves over the same socket. The reader blocks
    // indefinitely (clear the read timeout); the writer keeps a bounded write
    // timeout. SO_RCVTIMEO / SO_SNDTIMEO are independent, so clearing the read
    // timeout never un-bounds writes and vice versa.
    let read_half = stream.try_clone().context("clone herdr wire read half")?;
    read_half
        .set_read_timeout(None)
        .context("clear herdr wire reader timeout")?;
    let write_half = stream;
    write_half
        .set_write_timeout(Some(WIRE_TIMEOUT))
        .context("set herdr wire write timeout")?;

    Ok(DualHandle {
        sender: Box::new(HerdrMuxSender {
            write: write_half,
            control,
            workspace_id,
            current_terminal_id: terminal_id,
        }),
        receiver: Box::new(HerdrMuxReceiver { read: read_half }),
        session_name,
    })
}

/// Assert herdr accepted the handshake: protocol **version 14** (strict equality)
/// and no `error`. herdr is young (v0.7.1) and the wire protocol can change
/// between releases, so we fail loudly on any mismatch rather than risk
/// misinterpreting frames.
fn assert_welcome(msg: &ServerMessage) -> Result<()> {
    let ServerMessage::Welcome {
        version,
        encoding,
        error,
    } = msg
    else {
        return Err(anyhow!(
            "herdr handshake: expected Welcome, got a different message first"
        ));
    };
    if let Some(err) = error {
        return Err(anyhow!("herdr rejected the handshake: {err}"));
    }
    if *version != HERDR_PROTOCOL_VERSION {
        return Err(anyhow!(
            "herdr wire protocol mismatch: server speaks v{version}, muxrd requires v{HERDR_PROTOCOL_VERSION}"
        ));
    }
    if *encoding != RenderEncoding::TerminalAnsi {
        // Not fatal — we still receive Terminal frames — but surface it: a
        // SemanticFrame negotiation would mean no ANSI bytes arrive.
        log::warn!("herdr negotiated unexpected encoding {encoding:?}, expected TerminalAnsi");
    }
    Ok(())
}

/// Resolve the workspace's focused pane to its `(u32 pane id, terminal_id)`,
/// registering every pane in the shared registry so subsequent
/// [`HerdrMuxSender::focus_pane`] (`u32 → terminal_id`) lookups resolve. Falls
/// back to the first listed pane when herdr reports no focus; errors only when the
/// workspace has no panes at all.
fn resolve_focused_terminal(control: &HerdrControl, workspace_id: &str) -> Result<(u32, String)> {
    let panes = control
        .list_panes(workspace_id)
        .with_context(|| format!("list herdr panes for workspace '{workspace_id}'"))?;
    let reg = control.pane_registry();

    let mut first: Option<(u32, String)> = None;
    let mut focused: Option<(u32, String)> = None;
    for pane in &panes {
        let id = reg.assign_or_get(&pane.pane_id, &pane.terminal_id);
        if first.is_none() {
            first = Some((id, pane.terminal_id.clone()));
        }
        if pane.focused {
            focused = Some((id, pane.terminal_id.clone()));
        }
    }

    focused
        .or(first)
        .ok_or_else(|| anyhow!("herdr workspace '{workspace_id}' has no panes to attach"))
}

/// Pure core of [`resolve_tab_focused_pane`]: given the full workspace pane list,
/// registers ALL panes in `reg` and returns the focused-or-first pane within
/// `herdr_tab_id`. Registering all panes (not only those in the target tab) ensures
/// subsequent [`HerdrMuxSender::focus_pane`] (`u32 → terminal_id`) lookups still
/// resolve for panes on other tabs. Separated from the I/O call for
/// unit-testability, mirroring the `transcode_layout` /
/// [`resolve_focused_terminal`] pattern.
fn pick_tab_pane(
    panes: &[PaneInfo],
    reg: &HerdrPaneRegistry,
    workspace_id: &str,
    herdr_tab_id: &str,
) -> Result<(u32, String)> {
    let mut first: Option<(u32, String)> = None;
    let mut focused: Option<(u32, String)> = None;
    for pane in panes {
        // Register ALL workspace panes so subsequent focus_pane u32→terminal_id
        // lookups still resolve even for panes on other tabs.
        let id = reg.assign_or_get(&pane.pane_id, &pane.terminal_id);
        if pane.tab_id == herdr_tab_id {
            if first.is_none() {
                first = Some((id, pane.terminal_id.clone()));
            }
            if pane.focused {
                focused = Some((id, pane.terminal_id.clone()));
            }
        }
    }
    focused.or(first).ok_or_else(|| {
        anyhow!("herdr tab '{herdr_tab_id}' (ws '{workspace_id}') has no panes to attach")
    })
}

/// Resolve the target tab's focused-or-first pane to its `(u32 pane id, terminal_id)`
/// for a per-connection wire re-attach — no daemon-global `tab.focus` called.
/// Registers ALL workspace panes in the shared registry (like [`resolve_focused_terminal`])
/// so subsequent [`HerdrMuxSender::focus_pane`] (`u32 → terminal_id`) lookups resolve
/// for panes on other tabs. The tab-scoped analogue of [`resolve_focused_terminal`].
fn resolve_tab_focused_pane(
    control: &HerdrControl,
    workspace_id: &str,
    herdr_tab_id: &str,
) -> Result<(u32, String)> {
    let panes = control
        .list_panes(workspace_id)
        .with_context(|| format!("list herdr panes for workspace '{workspace_id}'"))?;
    let reg = control.pane_registry();
    pick_tab_pane(&panes, reg, workspace_id, herdr_tab_id)
}

// ─── HerdrMuxSender (write half + control plane) ──────────────────────────────

/// The input/control half of a herdr [`DualHandle`]. Owns the wire **write** half
/// and an `Arc<HerdrControl>` for JSON-API control/layout calls. Focus operations
/// re-attach the wire stream to a new pane's `terminal_id`.
pub struct HerdrMuxSender {
    /// Write half of the wire socket (raw input, resize, attach, detach).
    write: UnixStream,
    /// Shared JSON-API control client (tab focus, zoom, layout) + registries.
    control: Arc<HerdrControl>,
    /// herdr workspace id this attach renders (muxrd "session").
    workspace_id: String,
    /// `terminal_id` the wire stream is currently attached to (re-attach target).
    current_terminal_id: String,
}

impl HerdrMuxSender {
    /// Write one [`ClientMessage`] to the wire socket (bounded by the write
    /// timeout set in [`open_attach`]).
    fn send(&mut self, msg: &ClientMessage) -> Result<()> {
        write_message(&mut self.write, msg).map_err(|e| anyhow!("herdr wire write failed: {e}"))
    }

    /// Re-point the wire stream at `terminal_id` (`AttachTerminal { takeover }`)
    /// and record it as the current attach target. herdr replies on the read half
    /// with a fresh `full = true` frame for the newly-attached pane.
    fn reattach(&mut self, terminal_id: String) -> Result<()> {
        self.send(&ClientMessage::AttachTerminal {
            terminal_id: terminal_id.clone(),
            takeover: true,
        })?;
        self.current_terminal_id = terminal_id;
        Ok(())
    }
}

impl MuxSender for HerdrMuxSender {
    /// Re-point THIS connection's wire stream at the target tab's focused-or-first
    /// pane WITHOUT calling herdr's daemon-global `tab.focus`. This matches the
    /// per-connection view implemented by [`switch_space`] (Decision 2): a
    /// co-attached desktop client must not follow the mobile client's tab switch.
    /// The wire is pane-addressed (`terminal_id`), so re-attaching to the right
    /// pane in a different tab is sufficient — no daemon-global focus call needed.
    fn go_to_tab(&mut self, tab_id: u64) -> Result<()> {
        let herdr_tab = self
            .control
            .tab_registry()
            .herdr_tab_id(tab_id)
            .ok_or_else(|| anyhow!("herdr go_to_tab: unknown tab id {tab_id}"))?;
        let (_pane_id, terminal_id) =
            resolve_tab_focused_pane(&self.control, &self.workspace_id, &herdr_tab)?;
        self.reattach(terminal_id)
    }

    fn focus_pane(&mut self, pane: PaneRef) -> Result<()> {
        // Focus IS re-attach: re-point the wire stream at the target pane's
        // terminal. The pane registry was populated by the prior layout query.
        let terminal_id = self
            .control
            .pane_registry()
            .terminal_id(pane.id)
            .ok_or_else(|| anyhow!("herdr focus_pane: unknown pane id {}", pane.id))?;
        self.reattach(terminal_id)
    }

    fn switch_space(&mut self, space_id: &str) -> Result<()> {
        // Space switch IS re-attach (Option A; Decision 2 — per-connection view):
        // resolve the target workspace's focused pane, re-point THIS connection's
        // wire stream at it, and record the new workspace as current — so the next
        // `query_layout_result()` (which reads `self.workspace_id`) returns the new
        // space's tabs. We deliberately do NOT call herdr's daemon-global
        // `workspace.focus`: that would yank every co-attached desktop client. The
        // wire is workspace-agnostic (terminal_ids are globally unique), so this
        // reuses the shared pane/tab registries safely across workspaces.
        let (_pane_id, terminal_id) = resolve_focused_terminal(&self.control, space_id)?;
        self.reattach(terminal_id)?;
        self.workspace_id = space_id.to_string();
        Ok(())
    }

    fn toggle_fullscreen(&mut self, pane: PaneRef, hint: FullscreenHint) -> Result<()> {
        // herdr has no floating layer; the FullscreenHint floating fields are
        // ignored. Zoom maps to herdr's pane.zoom toggle (JSON-API).
        let _ = hint;
        let ack = self
            .control
            .zoom_pane(Some(pane.id), PaneZoomMode::Toggle)?;
        if !ack.ok {
            log::debug!(
                "herdr pane.zoom({}) reported failure: {:?}",
                pane.id,
                ack.error
            );
        }
        Ok(())
    }

    fn has_sync_layout(&self) -> bool {
        // herdr answers layout out-of-band (see query_layout_result), so the relay
        // routes the query onto the blocking pool instead of arming the in-band
        // Log path. (B1 fix: keeps the blocking JSON-API round-trips off the
        // inbound select! task.)
        true
    }

    fn query_layout_result(&mut self) -> Option<Result<LayoutSnapshot>> {
        // The P2.00 payoff: herdr answers layout out-of-band over its JSON-API
        // socket, bounded by HerdrControl's per-call timeout — so the relay never
        // arms the in-band Log path nor waits out the 18 s relay query timeout.
        // B1: the relay invokes this from spawn_blocking (has_sync_layout == true),
        // never inline on the inbound task.
        Some(self.control.query_layout(&self.workspace_id))
    }

    fn query_layout(&mut self) -> Result<()> {
        // herdr answers layout via query_layout_result (out-of-band), so the relay
        // never falls through to this in-band fire. No-op if it ever does.
        log::debug!("herdr query_layout() in-band fire ignored (answered out-of-band)");
        Ok(())
    }

    fn send_input_chars(&mut self, text: &str) -> Result<()> {
        self.send(&ClientMessage::Input {
            data: text.as_bytes().to_vec(),
        })
    }

    fn send_input_bytes(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.send(&ClientMessage::Input { data: bytes })
    }

    fn send_mouse(&mut self, kind: MuxMouseKind, col: u16, row: u16) -> Result<()> {
        // Wheel events in direct-attach mode go over `AttachScroll` — herdr's
        // dedicated attach-mode scroll message (scrollback scrolling, and the
        // wheel source + position let herdr forward to a mouse-capturing app
        // in the attached pane). `InputEvents` is NOT interpreted for
        // attach-mode scrolling (verified live: accepted but ignored). This is
        // the first use of the variant — it has been tag-stable in [`wire`]
        // since the protocol was pinned (kept for discriminant alignment).
        let direction = match kind {
            MuxMouseKind::WheelUp => AttachScrollDirection::Up,
            MuxMouseKind::WheelDown => AttachScrollDirection::Down,
        };
        self.send(&ClientMessage::AttachScroll {
            source: AttachScrollSource::Wheel,
            direction,
            lines: 1,
            column: Some(col),
            row: Some(row),
            modifiers: 0,
        })
    }

    fn send_resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.send(&ClientMessage::Resize {
            cols,
            rows,
            cell_width_px: CELL_PX_DISABLED,
            cell_height_px: CELL_PX_DISABLED,
        })
    }

    fn send_client_exited(&mut self) -> Result<()> {
        self.send(&ClientMessage::Detach)
    }

    fn box_clone(&self) -> Box<dyn MuxSender> {
        // Clone the write half (a dup'd fd over the same socket) + shared Arc +
        // current attach state, for the relay's ShutdownGuard nudge handle. fd
        // duplication of a live socket effectively never fails (only on fd
        // exhaustion, where teardown is already unrecoverable).
        let write = self
            .write
            .try_clone()
            .expect("clone herdr wire write half for ShutdownGuard nudge");
        Box::new(HerdrMuxSender {
            write,
            control: Arc::clone(&self.control),
            workspace_id: self.workspace_id.clone(),
            current_terminal_id: self.current_terminal_id.clone(),
        })
    }
}

// ─── HerdrMuxReceiver (read half) ─────────────────────────────────────────────

/// The render/event half of a herdr [`DualHandle`]. Owns the wire **read** half
/// and runs on the relay's blocking reader std-thread.
pub struct HerdrMuxReceiver {
    /// Read half of the wire socket (blocking).
    read: UnixStream,
}

impl MuxReceiver for HerdrMuxReceiver {
    fn recv(&mut self) -> Option<MuxServerMsg> {
        recv_from(&mut self.read)
    }
}

/// Read + map one wire message from `reader`, generic over the transport so the
/// EOF / mapping behaviour is unit-testable without a live socket.
///
/// Returns `None` on a clean stream close (graceful EOF, which ends the relay) and
/// on any framing/decode error (logged at debug) — the relay cannot continue past
/// a corrupt frame.
fn recv_from<R: io::Read>(reader: &mut R) -> Option<MuxServerMsg> {
    match read_server_message(reader) {
        Ok(msg) => Some(map_server_message(msg)),
        Err(FramingError::UnexpectedEof) => None,
        Err(e) => {
            log::debug!("herdr wire recv ended on framing error: {e}");
            None
        }
    }
}

/// Pure [`ServerMessage`] → [`MuxServerMsg`] mapping (no I/O; unit-tested).
///
/// | herdr `ServerMessage` | neutral `MuxServerMsg` |
/// |---|---|
/// | `Terminal(frame)` | `Render(frame.bytes)` — ANSI forwarded verbatim (full or incremental) |
/// | `ServerShutdown { reason }` | `Event(Exit { reason })` (`reason` defaulted to `""`) |
/// | `Welcome` / `Frame` / `Graphics` / `Notify` / `Clipboard` / `WindowTitle` / `ReloadSoundConfig` / `MouseCapture` | `Other` (drained, loop cadence preserved) |
///
/// EOF / framing errors are handled one level up in [`recv_from`] (→ `None`), as
/// they are transport conditions, not `ServerMessage` values.
fn map_server_message(msg: ServerMessage) -> MuxServerMsg {
    match msg {
        // The primary render payload: raw ANSI bytes kterm writes directly. Full
        // (on attach / re-attach) and incremental diffs are both just bytes.
        ServerMessage::Terminal(frame) => MuxServerMsg::Render(frame.bytes),
        // herdr is shutting down — surface as a neutral Exit event.
        ServerMessage::ServerShutdown { reason } => MuxServerMsg::Event(MuxEvent::Exit {
            reason: reason.unwrap_or_default(),
        }),
        // No remote-client semantics for the single-pane ANSI relay: drain them,
        // preserving the per-message reader cadence (parallel to zellij's `Other`).
        ServerMessage::Welcome { .. }
        | ServerMessage::Frame(_)
        | ServerMessage::Graphics { .. }
        | ServerMessage::Notify { .. }
        | ServerMessage::Clipboard { .. }
        | ServerMessage::WindowTitle { .. }
        | ServerMessage::ReloadSoundConfig
        | ServerMessage::MouseCapture { .. } => MuxServerMsg::Other,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::herdr::registry::{HerdrPaneRegistry, HerdrTabRegistry};
    use crate::multiplexer::herdr::wire::{NotifyKind, TerminalFrame};

    // ── ServerMessage → MuxServerMsg mapping ──────────────────────────────────

    #[test]
    fn terminal_frame_maps_to_render_bytes_verbatim() {
        let bytes = b"\x1b[2J\x1b[1;1Hhello".to_vec();
        let msg = map_server_message(ServerMessage::Terminal(TerminalFrame {
            seq: 1,
            width: 80,
            height: 24,
            full: true,
            bytes: bytes.clone(),
        }));
        match msg {
            MuxServerMsg::Render(out) => assert_eq!(out, bytes),
            other => panic!("expected Render, got {other:?}"),
        }
    }

    #[test]
    fn incremental_terminal_frame_also_maps_to_render() {
        // full=false (incremental diff) is forwarded identically to full=true.
        let msg = map_server_message(ServerMessage::Terminal(TerminalFrame {
            seq: 2,
            width: 80,
            height: 24,
            full: false,
            bytes: b"\x1b[5;1Hx".to_vec(),
        }));
        assert!(matches!(msg, MuxServerMsg::Render(_)));
    }

    #[test]
    fn server_shutdown_maps_to_exit_event_with_reason() {
        let msg = map_server_message(ServerMessage::ServerShutdown {
            reason: Some("going down".into()),
        });
        match msg {
            MuxServerMsg::Event(MuxEvent::Exit { reason }) => assert_eq!(reason, "going down"),
            other => panic!("expected Exit event, got {other:?}"),
        }
    }

    #[test]
    fn server_shutdown_without_reason_defaults_to_empty() {
        let msg = map_server_message(ServerMessage::ServerShutdown { reason: None });
        match msg {
            MuxServerMsg::Event(MuxEvent::Exit { reason }) => assert!(reason.is_empty()),
            other => panic!("expected Exit event, got {other:?}"),
        }
    }

    #[test]
    fn drained_variants_map_to_other() {
        for msg in [
            ServerMessage::Welcome {
                version: HERDR_PROTOCOL_VERSION,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            },
            ServerMessage::Graphics { bytes: vec![] },
            ServerMessage::Notify {
                kind: NotifyKind::Toast,
                message: "hi".into(),
                body: None,
            },
            ServerMessage::Clipboard { data: "x".into() },
            ServerMessage::WindowTitle {
                title: Some("t".into()),
            },
            ServerMessage::ReloadSoundConfig,
            ServerMessage::MouseCapture { enabled: true },
        ] {
            assert!(
                matches!(map_server_message(msg.clone()), MuxServerMsg::Other),
                "{msg:?} should map to Other"
            );
        }
    }

    // ── recv_from: EOF and framed-message paths ───────────────────────────────

    #[test]
    fn recv_from_empty_stream_returns_none() {
        // A clean EOF (no bytes) ends the relay gracefully.
        let mut empty: &[u8] = &[];
        assert!(recv_from(&mut empty).is_none());
    }

    #[test]
    fn recv_from_truncated_length_prefix_returns_none() {
        // Partial frame (EOF mid length-prefix) → UnexpectedEof → None.
        let mut partial: &[u8] = &[0x01, 0x02];
        assert!(recv_from(&mut partial).is_none());
    }

    #[test]
    fn recv_from_framed_shutdown_maps_to_exit() {
        let payload = bincode::serde::encode_to_vec(
            &ServerMessage::ServerShutdown {
                reason: Some("bye".into()),
            },
            bincode::config::standard(),
        )
        .unwrap();
        let mut framed = Vec::new();
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);

        let mut cursor: &[u8] = &framed;
        match recv_from(&mut cursor) {
            Some(MuxServerMsg::Event(MuxEvent::Exit { reason })) => assert_eq!(reason, "bye"),
            other => panic!("expected Exit event, got {other:?}"),
        }
    }

    // ── assert_welcome: the v14 gate ──────────────────────────────────────────

    #[test]
    fn assert_welcome_accepts_v14_no_error() {
        assert!(
            assert_welcome(&ServerMessage::Welcome {
                version: HERDR_PROTOCOL_VERSION,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            })
            .is_ok()
        );
    }

    #[test]
    fn assert_welcome_rejects_version_mismatch() {
        assert!(
            assert_welcome(&ServerMessage::Welcome {
                version: 13,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            })
            .is_err()
        );
    }

    #[test]
    fn assert_welcome_rejects_handshake_error() {
        assert!(
            assert_welcome(&ServerMessage::Welcome {
                version: HERDR_PROTOCOL_VERSION,
                encoding: RenderEncoding::TerminalAnsi,
                error: Some("no such terminal".into()),
            })
            .is_err()
        );
    }

    #[test]
    fn assert_welcome_rejects_non_welcome_first_message() {
        assert!(assert_welcome(&ServerMessage::ReloadSoundConfig).is_err());
    }

    // ── query_layout_result is the bounded override (returns Some) ─────────────

    #[test]
    fn query_layout_result_returns_some_and_is_bounded() {
        // Construct a sender over a socketpair (no live herdr). query_layout_result
        // must return Some (the override, not the None default) and must RETURN
        // (bounded) — here it returns Some(Err) because the JSON-API socket path
        // does not exist, proving it neither hangs nor falls through to None.
        let (a, _b) = UnixStream::pair().unwrap();
        let control = Arc::new(HerdrControl::new(
            PathBuf::from("/nonexistent/herdr.sock"),
            Arc::new(HerdrPaneRegistry::new()),
            Arc::new(HerdrTabRegistry::new()),
        ));
        let mut sender = HerdrMuxSender {
            write: a,
            control,
            workspace_id: "ws-1".into(),
            current_terminal_id: "term-1".into(),
        };
        let result = sender.query_layout_result();
        assert!(
            result.is_some(),
            "herdr sender must override query_layout_result with Some(...)"
        );
        // It returned (did not hang): the inner Result is an Err (no socket).
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn default_query_layout_in_band_is_noop() {
        let (a, _b) = UnixStream::pair().unwrap();
        let control = Arc::new(HerdrControl::new(
            PathBuf::from("/nonexistent/herdr.sock"),
            Arc::new(HerdrPaneRegistry::new()),
            Arc::new(HerdrTabRegistry::new()),
        ));
        let mut sender = HerdrMuxSender {
            write: a,
            control,
            workspace_id: "ws-1".into(),
            current_terminal_id: "term-1".into(),
        };
        assert!(sender.query_layout().is_ok());
    }

    // ── pick_tab_pane: tab-scoped pane resolution ─────────────────────────────

    /// Construct a minimal [`PaneInfo`] fixture, mirroring the control.rs test helpers.
    fn make_pane(
        pane_id: &str,
        terminal_id: &str,
        tab_id: &str,
        focused: bool,
    ) -> crate::multiplexer::herdr::api::PaneInfo {
        serde_json::from_value(serde_json::json!({
            "pane_id": pane_id,
            "terminal_id": terminal_id,
            "workspace_id": "ws-1",
            "tab_id": tab_id,
            "focused": focused,
            "agent_status": "idle",
            "state_labels": {},
            "revision": 1,
        }))
        .expect("PaneInfo fixture")
    }

    #[test]
    fn pick_tab_pane_returns_focused_pane_of_target_tab_not_other_tab() {
        // tab-A: one pane (not focused). tab-B: two panes (second is focused).
        // pick_tab_pane for "tab-B" must return tab-B's focused pane, never tab-A's.
        let reg = HerdrPaneRegistry::new();
        let panes = vec![
            make_pane("pane-a1", "term-a1", "tab-A", false),
            make_pane("pane-b1", "term-b1", "tab-B", false),
            make_pane("pane-b2", "term-b2", "tab-B", true), // focused
        ];

        let (_id, terminal_id) = pick_tab_pane(&panes, &reg, "ws-1", "tab-B").unwrap();
        assert_eq!(
            terminal_id, "term-b2",
            "focused pane in tab-B must win over the first pane in tab-B"
        );

        // ALL workspace panes must be registered (focus_pane lookups across tabs must resolve).
        assert_eq!(
            reg.terminal_id(reg.assign_or_get("pane-a1", "term-a1"))
                .as_deref(),
            Some("term-a1"),
            "tab-A pane must be registered even though it was not returned"
        );
        assert_eq!(
            reg.terminal_id(reg.assign_or_get("pane-b1", "term-b1"))
                .as_deref(),
            Some("term-b1"),
        );
        assert_eq!(
            reg.terminal_id(reg.assign_or_get("pane-b2", "term-b2"))
                .as_deref(),
            Some("term-b2"),
        );
    }

    #[test]
    fn pick_tab_pane_falls_back_to_first_when_none_focused() {
        // tab-B has two panes, neither focused; must return the first one.
        let reg = HerdrPaneRegistry::new();
        let panes = vec![
            make_pane("pane-a1", "term-a1", "tab-A", false),
            make_pane("pane-b1", "term-b1", "tab-B", false), // first in tab-B
            make_pane("pane-b2", "term-b2", "tab-B", false),
        ];

        let (_id, terminal_id) = pick_tab_pane(&panes, &reg, "ws-1", "tab-B").unwrap();
        assert_eq!(
            terminal_id, "term-b1",
            "first pane in tab-B returned when no pane is focused"
        );
    }

    #[test]
    fn pick_tab_pane_errors_when_target_tab_has_no_panes() {
        // Panes exist but none belong to the requested tab.
        let reg = HerdrPaneRegistry::new();
        let panes = vec![make_pane("pane-a1", "term-a1", "tab-A", false)];

        let result = pick_tab_pane(&panes, &reg, "ws-1", "tab-X");
        assert!(result.is_err(), "must error when target tab has no panes");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("tab-X"),
            "error message must name the missing tab, got: {msg}"
        );
    }
}
