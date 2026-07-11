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
//! - **Focus = release-then-reconnect.** [`HerdrMuxSender::focus_pane`] /
//!   [`go_to_tab`] / [`switch_space`] re-point the stream to a new pane via
//!   [`HerdrMuxSender::reattach`], which **`Detach`es the old connection and opens a
//!   fresh one** for the new terminal (see [`HerdrMuxSender::reattach`] for the
//!   why). herdr replies on the new connection with a `full = true` frame for the
//!   new pane — the same downstream contract the old same-connection re-point path
//!   relied on.
//! - The **pane strip / layout** comes from herdr's JSON-API
//!   ([`HerdrControl::query_layout`], surfaced out-of-band via
//!   [`MuxSender::query_layout_result`]) — geometry for all panes, live content for
//!   the attached one. This needs **no client change**.
//!
//! ## One terminal per wire connection (the resize-lock leak fix)
//!
//! herdr (v0.7.1 and master) **leaks** the previous terminal's
//! `direct_attach_resize_locks` / `terminal_attach_owners` entries when the SAME
//! wire client re-points with `AttachTerminal { takeover: true }`: every pane the
//! mobile client visits stays stuck at mobile size on co-attached desktop clients.
//! herdr only cleans up on `Detach`. Upstream closed our issue as won't-fix on the
//! legacy direct-attach path and declared the sanctioned model **one terminal per
//! connection: attach → Detach (release) → fresh connection for the next terminal**.
//! So [`HerdrMuxSender::reattach`] is release-then-reconnect, not same-connection
//! re-pointing. Root cause + reproduction:
//! `workflow/plans/bug/herdr-pane-resize-leak/BUG.md`.
//!
//! ## Threading / split (parallel to `ZellijMux{Sender,Receiver}`)
//!
//! One wire [`UnixStream`] is `try_clone`d into two independently-owned fds:
//! [`HerdrMuxReceiver`] owns the **read** half (blocking [`recv`](MuxReceiver::recv)
//! on the relay reader std-thread); [`HerdrMuxSender`] owns the **write** half (an
//! `Arc<Mutex<UnixStream>>`, shared with `ShutdownGuard` clones) plus an
//! `Arc<HerdrControl>` for the JSON-API control/layout calls. The read half has
//! **no read timeout** (it must block on the reader thread); the write half carries
//! a short write timeout so a wedged socket can never stall the inbound task.
//!
//! Because a `reattach` swaps the whole socket, the write half is shared through a
//! mutex (a swap replaces it under every handle at once) and the reader is handed
//! its new read half over an `mpsc` channel, arming a [`swap_pending`](HerdrMuxSender)
//! flag so an EOF during a swap is adopted rather than treated as a disconnect.
//!
//! ## P2.04 wiring
//!
//! [`open_attach`] is the single entry point `HerdrBackend::open_attach` (P2.04)
//! calls: it performs the handshake, attaches the focused pane, and returns the
//! split [`DualHandle`]. P2.04 owns the `Arc<HerdrControl>` (and, through it, the
//! shared registries) and the resolved wire socket path; it maps the muxrd
//! `session` name to a herdr `workspace_id` before calling.

use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
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

/// Number of connect+handshake attempts a single [`HerdrMuxSender::reattach`]
/// makes before giving up and tearing down the AttachTerminal stream. The old
/// terminal is already `Detach`ed by the time we reconnect, so a single transient
/// failure — one 3 s [`WIRE_TIMEOUT`] handshake-op timeout against a slow/loaded
/// herdr is enough — would otherwise be unrecoverable. We therefore retry the full
/// connect+handshake once on a **fresh** connection (total = 2 attempts) before
/// sending the `None` sentinel. Also bounds [`SWAP_GRACE`].
const RECONNECT_ATTEMPTS: usize = 2;

/// Per-attempt time budget folded into [`SWAP_GRACE`] for [`UnixStream::connect`],
/// which is itself **unbounded** (a kernel-local Unix-socket connect; normally
/// instant). This is an allowance, not an enforced bound — adding a real connect
/// timeout (e.g. via `socket2`) is out of scope.
const CONNECT_SLACK: Duration = Duration::from_secs(1);

/// Final slack folded into [`SWAP_GRACE`] on top of the per-attempt worst case, so
/// the reader never abandons a swap that is a hair from completing.
const SWAP_SLACK: Duration = Duration::from_secs(1);

/// How long [`HerdrMuxReceiver::recv`] waits for a reconnect's new read half after
/// it hits EOF **while a swap is in flight** (`swap_pending` set). It must cover the
/// worst-case [`HerdrMuxSender::reattach`]: [`RECONNECT_ATTEMPTS`] attempts, each of
/// which may spend up to three [`WIRE_TIMEOUT`]-bounded handshake ops (`Hello`
/// write, `Welcome` read, `AttachTerminal` write) plus a [`CONNECT_SLACK`] allowance
/// for the (unbounded) connect — i.e.
/// `RECONNECT_ATTEMPTS × (3 × WIRE_TIMEOUT + CONNECT_SLACK)` — plus [`SWAP_SLACK`].
/// With `WIRE_TIMEOUT = 3 s` that is `2 × (3×3 + 1) + 1 = 21 s`. Note that
/// `UnixStream::connect` is not timeout-bounded (kernel-local, normally instant);
/// `CONNECT_SLACK` is only an allowance. A genuine disconnect (no swap pending)
/// never waits this out: the reader returns `None` immediately, so the client-side
/// auto-reattach latency does not regress.
const SWAP_GRACE: Duration = Duration::from_secs(
    RECONNECT_ATTEMPTS as u64 * (3 * WIRE_TIMEOUT.as_secs() + CONNECT_SLACK.as_secs())
        + SWAP_SLACK.as_secs(),
);

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

    // Connect + handshake + attach on a fresh wire connection, then split into the
    // blocking read half and the bounded write half.
    let stream = connect_and_attach(&wire_socket, rows, cols, &terminal_id)?;
    let (read_half, write_half) = split_wire(stream)?;

    // Shared, swappable connection state. The write half lives behind a mutex so a
    // reattach's socket swap is seen by every `box_clone`d ShutdownGuard handle at
    // once; the reader is handed its post-swap read half over `swap_tx`, gated by
    // the `swap_pending` flag so an EOF mid-swap is adopted, not misread as EOF.
    let swap_pending = Arc::new(AtomicBool::new(false));
    let (swap_tx, swap_rx) = mpsc::channel();
    let write = Arc::new(Mutex::new(write_half));

    Ok(DualHandle {
        sender: Box::new(HerdrMuxSender {
            write: Arc::clone(&write),
            control,
            workspace_id,
            current_terminal_id: terminal_id,
            swap_pending: Arc::clone(&swap_pending),
            swap_tx,
            rows,
            cols,
            wire_socket,
        }),
        receiver: Box::new(HerdrMuxReceiver {
            read: read_half,
            swap_rx,
            swap_pending,
        }),
        session_name,
    })
}

/// Connect a fresh herdr wire socket and drive the full attach handshake for
/// `terminal_id`: set the handshake timeouts, send `Hello` (carrying the client's
/// current `rows`×`cols`), read + assert `Welcome`, then send
/// `AttachTerminal { takeover: true }`. Returns the connected stream with both the
/// read and write timeouts still at [`WIRE_TIMEOUT`] — the caller splits it via
/// [`split_wire`]. Shared by [`open_attach`] and [`HerdrMuxSender::reattach`] so the
/// initial attach and every reconnect run byte-for-byte the same handshake.
fn connect_and_attach(
    wire_socket: &Path,
    rows: u16,
    cols: u16,
    terminal_id: &str,
) -> Result<UnixStream> {
    let stream = UnixStream::connect(wire_socket)
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

    let attach = ClientMessage::AttachTerminal {
        terminal_id: terminal_id.to_string(),
        takeover: true,
    };
    write_message(&mut &stream, &attach).context("send herdr AttachTerminal")?;

    Ok(stream)
}

/// Split a connected wire stream into `(read_half, write_half)`. The reader blocks
/// indefinitely (clear its read timeout); the writer keeps the bounded write
/// timeout. SO_RCVTIMEO / SO_SNDTIMEO are independent, so clearing the read timeout
/// never un-bounds writes and vice versa.
fn split_wire(stream: UnixStream) -> Result<(UnixStream, UnixStream)> {
    let read_half = stream.try_clone().context("clone herdr wire read half")?;
    read_half
        .set_read_timeout(None)
        .context("clear herdr wire reader timeout")?;
    let write_half = stream;
    write_half
        .set_write_timeout(Some(WIRE_TIMEOUT))
        .context("set herdr wire write timeout")?;
    Ok((read_half, write_half))
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
    /// Write half of the CURRENT wire connection, shared with `box_clone`d
    /// ShutdownGuard handles so a reconnect swaps the socket under every handle at
    /// once. Locked per message (all sends are short, framed writes; the mutex also
    /// serializes the ShutdownGuard `Detach` nudge against an in-flight swap).
    write: Arc<Mutex<UnixStream>>,
    /// Shared JSON-API control client (tab focus, zoom, layout) + registries.
    control: Arc<HerdrControl>,
    /// herdr workspace id this attach renders (muxrd "session").
    workspace_id: String,
    /// `terminal_id` the wire stream is currently attached to (re-attach target).
    current_terminal_id: String,
    /// Signals [`HerdrMuxReceiver`] that a connection swap is in flight, so the EOF
    /// on the old connection is adopted as a swap rather than misread as teardown.
    /// Set before the `Detach`, cleared once the new read half is handed over.
    swap_pending: Arc<AtomicBool>,
    /// Hands the reader its new read half after a reconnect (`None` = swap failed).
    swap_tx: mpsc::Sender<Option<UnixStream>>,
    /// Latest client dimensions — a reconnect's `Hello` must carry them so the fresh
    /// attach opens at the client's current size.
    rows: u16,
    cols: u16,
    /// Wire socket path for reconnects (the same instance `open_attach` resolved).
    wire_socket: PathBuf,
}

impl HerdrMuxSender {
    /// Lock the shared write half, recovering from a poisoned mutex. The guarded
    /// critical sections are short framed writes + the reconnect swap, so a torn
    /// state left by a panicking holder cannot matter after teardown — recovering
    /// (rather than propagating the poison) keeps a panic from wedging the
    /// ShutdownGuard `Detach` nudge.
    fn lock_write(&self) -> MutexGuard<'_, UnixStream> {
        self.write.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write one [`ClientMessage`] to the current wire connection (bounded by the
    /// write timeout set in [`split_wire`]).
    fn send(&mut self, msg: &ClientMessage) -> Result<()> {
        let mut write = self.lock_write();
        write_message(&mut *write, msg).map_err(|e| anyhow!("herdr wire write failed: {e}"))
    }

    /// Re-point the relay at `terminal_id` via **release-then-reconnect** — the
    /// one-terminal-per-wire-connection model herdr sanctions (see the module
    /// header; `workflow/plans/bug/herdr-pane-resize-leak/BUG.md`). Same-connection
    /// re-pointing leaks the old terminal's resize lock, so we instead:
    ///
    /// 1. arm `swap_pending` **before** `Detach` so the reader adopts the imminent
    ///    EOF as a swap rather than a disconnect;
    /// 2. best-effort `Detach` the old connection — herdr's `remove_client` then
    ///    frees the old terminal's owner entry + resize lock and the desktop layout
    ///    restores it (write errors are ignored: the server may already be gone);
    /// 3. open a **fresh** connection + handshake for `terminal_id` — retrying the
    ///    whole connect+handshake up to [`RECONNECT_ATTEMPTS`] times so a single
    ///    transient failure (e.g. one [`WIRE_TIMEOUT`] handshake-op timeout against a
    ///    loaded herdr) does not tear the stream down — then swap the write half
    ///    under the mutex, hand the new read half to the reader, and record the new
    ///    attach target.
    ///
    /// Only after **every** attempt fails is the old terminal (already released,
    /// Detach-first) unrecoverable: the reader is sent the `None` sentinel, sees EOF,
    /// ends the stream, and the client-side auto-reattach flow takes over — no
    /// half-attached state. Both the per-attempt failure (`warn!`) and the final
    /// teardown (`error!`) are logged so prod incidents are self-diagnosing.
    fn reattach(&mut self, terminal_id: String) -> Result<()> {
        // 1. Arm the swap BEFORE Detach: the reader must not misread the EOF as a
        //    genuine disconnect.
        self.swap_pending.store(true, Ordering::SeqCst);

        // 2. Best-effort Detach on the old connection (server cleans up the old
        //    terminal). Ignore write errors — the server may already be gone.
        //    Then shut the socket down ourselves: herdr removes the client on
        //    Detach but does NOT close the connection (its own CLI clients close
        //    their side), so without this the reader's blocking `read` on the old
        //    connection never sees EOF and the swap is never adopted (verified
        //    live: sizes restored but frames froze; the shutdown reaches the
        //    reader's dup'd fd because it acts on the socket, not the fd).
        {
            let mut write = self.lock_write();
            if let Err(e) = write_message(&mut *write, &ClientMessage::Detach) {
                log::debug!("herdr reattach: Detach on old connection failed (ignored): {e}");
            }
            if let Err(e) = write.shutdown(Shutdown::Both) {
                log::debug!("herdr reattach: old connection shutdown failed (ignored): {e}");
            }
        }

        // 3. Connect + handshake + attach a fresh connection for the new terminal,
        //    retrying the whole handshake up to RECONNECT_ATTEMPTS times. The old
        //    terminal is already Detached, so a single transient failure must not
        //    tear the stream down.
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=RECONNECT_ATTEMPTS {
            match connect_and_attach(&self.wire_socket, self.rows, self.cols, &terminal_id)
                .and_then(split_wire)
            {
                Ok((read_half, write_half)) => {
                    // Swap the write half under the mutex so every ShutdownGuard clone
                    // now writes to the new connection.
                    *self.lock_write() = write_half;
                    self.current_terminal_id = terminal_id.clone();
                    // Hand the reader its new read half. A send error means the reader
                    // is already gone — the swap cannot complete.
                    if self.swap_tx.send(Some(read_half)).is_err() {
                        self.swap_pending.store(false, Ordering::SeqCst);
                        return Err(anyhow!(
                            "herdr reattach: relay reader gone, cannot adopt new connection"
                        ));
                    }
                    self.swap_pending.store(false, Ordering::SeqCst);
                    return Ok(());
                }
                Err(e) => {
                    log::warn!(
                        "herdr reattach: connect+handshake attempt {attempt}/{RECONNECT_ATTEMPTS} \
                         for terminal '{terminal_id}' failed: {e:#}"
                    );
                    last_err = Some(e);
                }
            }
        }

        // Every attempt failed; the old terminal is already Detached, so the stream
        // is unrecoverable. Send the None sentinel so the reader ends the stream at
        // its EOF, and log loudly with the last error and the operation.
        let err = last_err.unwrap_or_else(|| {
            anyhow!("herdr reattach: connect+handshake failed (no error captured)")
        });
        let _ = self.swap_tx.send(None);
        self.swap_pending.store(false, Ordering::SeqCst);
        log::error!(
            "herdr reattach: all {RECONNECT_ATTEMPTS} connect+handshake attempts to terminal \
             '{terminal_id}' failed (last error: {err:#}); sending None sentinel and tearing \
             down the AttachTerminal stream"
        );
        Err(err)
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
        // Record the latest dimensions so a subsequent reattach's Hello opens the
        // fresh connection at the client's current size.
        self.rows = rows;
        self.cols = cols;
        self.send(&ClientMessage::Resize {
            cols,
            rows,
            cell_width_px: CELL_PX_DISABLED,
            cell_height_px: CELL_PX_DISABLED,
        })
    }

    fn send_client_exited(&mut self) -> Result<()> {
        // Detach, then close the socket ourselves — herdr does not close a
        // Detached client's connection, so without the shutdown the relay's
        // blocking reader thread would never see EOF at teardown.
        let sent = self.send(&ClientMessage::Detach);
        if let Err(e) = self.lock_write().shutdown(Shutdown::Both) {
            log::debug!("herdr client-exit: connection shutdown failed (ignored): {e}");
        }
        sent
    }

    fn box_clone(&self) -> Box<dyn MuxSender> {
        // Share the mutex-guarded write half (an `Arc` clone, NOT a dup'd fd) so a
        // reattach's socket swap is seen by this ShutdownGuard clone too — its
        // teardown `Detach` nudge always targets the CURRENT connection. The swap
        // channel + pending flag are shared for the same reason.
        Box::new(HerdrMuxSender {
            write: Arc::clone(&self.write),
            control: Arc::clone(&self.control),
            workspace_id: self.workspace_id.clone(),
            current_terminal_id: self.current_terminal_id.clone(),
            swap_pending: Arc::clone(&self.swap_pending),
            swap_tx: self.swap_tx.clone(),
            rows: self.rows,
            cols: self.cols,
            wire_socket: self.wire_socket.clone(),
        })
    }
}

// ─── HerdrMuxReceiver (read half) ─────────────────────────────────────────────

/// The render/event half of a herdr [`DualHandle`]. Owns the wire **read** half
/// and runs on the relay's blocking reader std-thread.
pub struct HerdrMuxReceiver {
    /// Read half of the CURRENT wire connection (blocking). Replaced in place when a
    /// [`HerdrMuxSender::reattach`] swaps the connection under us.
    read: UnixStream,
    /// Receives the post-reconnect read half from [`HerdrMuxSender::reattach`]
    /// (`Some` = adopt it, `None` = the reconnect failed → end the stream).
    swap_rx: mpsc::Receiver<Option<UnixStream>>,
    /// Set by the sender while a swap is in flight (shared). A bare EOF with this
    /// clear is a genuine disconnect and propagates immediately.
    swap_pending: Arc<AtomicBool>,
}

impl MuxReceiver for HerdrMuxReceiver {
    fn recv(&mut self) -> Option<MuxServerMsg> {
        loop {
            if let Some(msg) = recv_from(&mut self.read) {
                return Some(msg);
            }
            // The current connection ended. Adopt a swapped-in connection if a
            // reattach is in flight; otherwise this is a genuine disconnect.
            match self.try_adopt_swap() {
                Some(new_read) => {
                    self.read = new_read;
                    continue;
                }
                None => return None,
            }
        }
    }
}

impl HerdrMuxReceiver {
    /// On EOF of the current connection, decide whether a reconnect handed us a new
    /// read half to adopt. Checks the channel **first** (the swap may have completed
    /// before we noticed EOF); on an empty channel, waits up to [`SWAP_GRACE`] only
    /// while `swap_pending` says a reattach is running. A genuine disconnect with no
    /// swap pending returns `None` immediately (no grace wait) so the client-side
    /// auto-reattach latency does not regress.
    fn try_adopt_swap(&self) -> Option<UnixStream> {
        self.try_adopt_swap_within(SWAP_GRACE)
    }

    /// [`Self::try_adopt_swap`] parameterized by the grace window, so tests can drive
    /// the bounded-wait path with a short grace without a real [`SWAP_GRACE`] sleep.
    /// Production always calls it with [`SWAP_GRACE`].
    fn try_adopt_swap_within(&self, grace: Duration) -> Option<UnixStream> {
        match self.swap_rx.try_recv() {
            // A reconnect already delivered its new read half — adopt it.
            Ok(Some(stream)) => Some(stream),
            // A reconnect exhausted its retries (sentinel) — end the stream. This is
            // one of the two observable death paths (the other is grace expiry).
            Ok(None) => {
                log::warn!(
                    "herdr wire swap: reconnect failed after retries (None sentinel); \
                     ending the AttachTerminal stream"
                );
                None
            }
            // The sender (and all its clones) dropped — nothing more will arrive.
            // Genuine teardown: stay silent.
            Err(mpsc::TryRecvError::Disconnected) => None,
            Err(mpsc::TryRecvError::Empty) => {
                if self.swap_pending.load(Ordering::SeqCst) {
                    // A swap is in flight but its result has not landed yet: wait,
                    // bounded, for the reconnect to complete (or fail).
                    match self.swap_rx.recv_timeout(grace) {
                        Ok(Some(stream)) => Some(stream),
                        // Reconnect exhausted its retries mid-wait.
                        Ok(None) => {
                            log::warn!(
                                "herdr wire swap: reconnect failed after retries (None sentinel); \
                                 ending the AttachTerminal stream"
                            );
                            None
                        }
                        // The swap did not complete within the grace: abandon it.
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            log::warn!(
                                "herdr wire swap: no reconnect completed within the {grace:?} \
                                 grace; ending the AttachTerminal stream"
                            );
                            None
                        }
                        // Sender dropped mid-wait — genuine teardown, stay silent.
                        Err(mpsc::RecvTimeoutError::Disconnected) => None,
                    }
                } else {
                    // Genuine disconnect — propagate immediately (stay silent).
                    None
                }
            }
        }
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
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicUsize;

    // ── Release-then-reconnect test harness ───────────────────────────────────

    /// A `HerdrControl` over a nonexistent JSON-API socket — reattach never touches
    /// it (only the wire socket), and the layout tests want the query to fail fast.
    fn test_control() -> Arc<HerdrControl> {
        Arc::new(HerdrControl::new(
            PathBuf::from("/nonexistent/herdr.sock"),
            Arc::new(HerdrPaneRegistry::new()),
            Arc::new(HerdrTabRegistry::new()),
        ))
    }

    /// Build a `HerdrMuxSender` over `write` with a throwaway swap channel, for the
    /// sender-in-isolation tests that never exercise a reconnect.
    fn dummy_sender(write: UnixStream) -> HerdrMuxSender {
        let (swap_tx, _swap_rx) = mpsc::channel();
        HerdrMuxSender {
            write: Arc::new(Mutex::new(write)),
            control: test_control(),
            workspace_id: "ws-1".into(),
            current_terminal_id: "term-1".into(),
            swap_pending: Arc::new(AtomicBool::new(false)),
            swap_tx,
            rows: 24,
            cols: 80,
            wire_socket: PathBuf::from("/nonexistent/herdr.sock"),
        }
    }

    /// A process-unique wire socket path under the temp dir (Unix socket paths are
    /// length-limited, so keep the prefix short).
    fn unique_socket_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mxr_hr_{tag}_{}_{nanos}_{n}.sock",
            std::process::id()
        ))
    }

    /// Length-prefix + bincode-encode a [`ServerMessage`] as a wire frame.
    fn frame_server_message(msg: &ServerMessage) -> Vec<u8> {
        let payload = bincode::serde::encode_to_vec(msg, bincode::config::standard()).unwrap();
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        framed
    }

    /// Read + decode one [`ClientMessage`] frame from `reader` (test server side).
    fn read_client_message<R: Read>(reader: &mut R) -> ClientMessage {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).unwrap();
        bincode::serde::decode_from_slice::<ClientMessage, _>(&payload, bincode::config::standard())
            .unwrap()
            .0
    }

    /// Fake herdr server side of one attach handshake: read `Hello`, reply
    /// `Welcome{v14, TerminalAnsi, no error}`, read `AttachTerminal`. Returns both.
    fn serve_handshake(stream: &mut UnixStream) -> (ClientMessage, ClientMessage) {
        let hello = read_client_message(stream);
        stream
            .write_all(&frame_server_message(&ServerMessage::Welcome {
                version: HERDR_PROTOCOL_VERSION,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            }))
            .unwrap();
        stream.flush().unwrap();
        let attach = read_client_message(stream);
        (hello, attach)
    }

    /// A full-repaint `Terminal` frame carrying `seq`.
    fn term_frame(seq: u64) -> ServerMessage {
        ServerMessage::Terminal(TerminalFrame {
            seq,
            width: 80,
            height: 24,
            full: true,
            bytes: b"x".to_vec(),
        })
    }

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
        let mut sender = dummy_sender(a);
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
        let mut sender = dummy_sender(a);
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

    // ── reattach: release-then-reconnect (one terminal per connection) ─────────

    /// Happy path: `reattach` `Detach`es the old connection, opens a fresh one for
    /// the new terminal, and the reconnect's `Hello` carries the sender's LATEST
    /// dimensions (changed via `send_resize` before the switch).
    #[test]
    fn reattach_releases_old_then_reconnects_with_latest_dims() {
        let sock = unique_socket_path("happy");
        let listener = UnixListener::bind(&sock).unwrap();

        // Fake herdr server: conn1 (attach A → Resize → Detach), then conn2 (attach B).
        let server = std::thread::spawn(move || {
            let (mut c1, _) = listener.accept().unwrap();
            let (_hello1, attach1) = serve_handshake(&mut c1);
            let resize = read_client_message(&mut c1);
            let detach = read_client_message(&mut c1);

            let (mut c2, _) = listener.accept().unwrap();
            let (hello2, attach2) = serve_handshake(&mut c2);
            (attach1, resize, detach, hello2, attach2)
        });

        // Establish conn1 as the sender's current connection.
        let conn1 = connect_and_attach(&sock, 24, 80, "term-A").unwrap();
        let (_read1, write1) = split_wire(conn1).unwrap();

        let swap_pending = Arc::new(AtomicBool::new(false));
        let (swap_tx, swap_rx) = mpsc::channel();
        let mut sender = HerdrMuxSender {
            write: Arc::new(Mutex::new(write1)),
            control: test_control(),
            workspace_id: "ws-1".into(),
            current_terminal_id: "term-A".into(),
            swap_pending,
            swap_tx,
            rows: 24,
            cols: 80,
            wire_socket: sock.clone(),
        };

        // Change client dimensions, then switch to terminal B.
        sender.send_resize(50, 200).unwrap();
        sender.reattach("term-B".into()).unwrap();

        // The reader was handed conn2's read half; the attach target advanced.
        assert!(
            matches!(swap_rx.try_recv(), Ok(Some(_))),
            "reader must receive the swapped-in read half"
        );
        assert_eq!(sender.current_terminal_id, "term-B");

        let (attach1, resize, detach, hello2, attach2) = server.join().unwrap();
        assert!(
            matches!(attach1, ClientMessage::AttachTerminal { ref terminal_id, takeover: true } if terminal_id == "term-A"),
            "conn1 must have attached term-A, got {attach1:?}"
        );
        assert!(
            matches!(
                resize,
                ClientMessage::Resize {
                    rows: 50,
                    cols: 200,
                    ..
                }
            ),
            "conn1 must carry the resize, got {resize:?}"
        );
        assert!(
            matches!(detach, ClientMessage::Detach),
            "conn1 must receive Detach before the reconnect, got {detach:?}"
        );
        match hello2 {
            ClientMessage::Hello {
                rows,
                cols,
                launch_mode,
                ..
            } => {
                assert_eq!(
                    (rows, cols),
                    (50, 200),
                    "reconnect Hello must carry the latest client dimensions"
                );
                assert!(matches!(launch_mode, ClientLaunchMode::TerminalAttach));
            }
            other => panic!("expected Hello on conn2, got {other:?}"),
        }
        assert!(
            matches!(attach2, ClientMessage::AttachTerminal { ref terminal_id, takeover: true } if terminal_id == "term-B"),
            "conn2 must attach term-B with takeover, got {attach2:?}"
        );

        let _ = std::fs::remove_file(&sock);
    }

    /// Live-herdr regression: herdr does NOT close a Detached client's socket
    /// (its own CLI clients close their side), so the sender's post-Detach
    /// `shutdown` is what unblocks the reader. A reader blocked on conn1 must
    /// still adopt conn2 and deliver its frame even though the SERVER never
    /// closes conn1 (verified live: without the shutdown, pane sizes restored
    /// but frames froze after the first switch).
    #[test]
    fn reattach_unblocks_reader_when_server_keeps_old_socket_open() {
        let sock = unique_socket_path("keepopen");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = std::thread::spawn(move || {
            let (mut c1, _) = listener.accept().unwrap();
            let _ = serve_handshake(&mut c1);
            let (mut c2, _) = listener.accept().unwrap();
            let _ = serve_handshake(&mut c2);
            c2.write_all(&frame_server_message(&term_frame(2))).unwrap();
            c2.flush().unwrap();
            // Keep BOTH server ends open until the test ends — herdr's behavior.
            (c1, c2)
        });

        let conn1 = connect_and_attach(&sock, 24, 80, "term-A").unwrap();
        let (read1, write1) = split_wire(conn1).unwrap();

        let swap_pending = Arc::new(AtomicBool::new(false));
        let (swap_tx, swap_rx) = mpsc::channel();
        let mut receiver = HerdrMuxReceiver {
            read: read1,
            swap_rx,
            swap_pending: Arc::clone(&swap_pending),
        };
        let mut sender = HerdrMuxSender {
            write: Arc::new(Mutex::new(write1)),
            control: test_control(),
            workspace_id: "ws-1".into(),
            current_terminal_id: "term-A".into(),
            swap_pending,
            swap_tx,
            rows: 24,
            cols: 80,
            wire_socket: sock.clone(),
        };

        // Reader blocked on conn1 in a background thread, as in the real relay.
        let reader = std::thread::spawn(move || receiver.recv());

        sender.reattach("term-B".into()).unwrap();

        let msg = reader.join().unwrap();
        assert!(
            matches!(msg, Some(MuxServerMsg::Render(_))),
            "reader must adopt conn2 and return its frame, got {msg:?}"
        );

        let _ = server.join();
        let _ = std::fs::remove_file(&sock);
    }

    /// Reader continuity: a frame arrives on conn1, then conn1 closes AFTER the
    /// swap is armed and the new read half is delivered — `recv` returns conn2's
    /// frame next, with no `None` in between.
    #[test]
    fn recv_adopts_swapped_connection_without_none() {
        let (recv_read, mut srv1) = UnixStream::pair().unwrap();
        let swap_pending = Arc::new(AtomicBool::new(false));
        let (swap_tx, swap_rx) = mpsc::channel();
        let mut receiver = HerdrMuxReceiver {
            read: recv_read,
            swap_rx,
            swap_pending: Arc::clone(&swap_pending),
        };

        // Frame 1 on conn1.
        srv1.write_all(&frame_server_message(&term_frame(1)))
            .unwrap();
        srv1.flush().unwrap();
        assert!(matches!(receiver.recv(), Some(MuxServerMsg::Render(_))));

        // Arm the swap, hand over conn2's read half, queue frame 2 on conn2, then
        // close conn1 → the reader's next recv sees EOF and adopts conn2.
        let (recv_read2, mut srv2) = UnixStream::pair().unwrap();
        swap_pending.store(true, Ordering::SeqCst);
        swap_tx.send(Some(recv_read2)).unwrap();
        srv2.write_all(&frame_server_message(&term_frame(2)))
            .unwrap();
        srv2.flush().unwrap();
        drop(srv1);

        assert!(
            matches!(receiver.recv(), Some(MuxServerMsg::Render(_))),
            "recv must adopt the swapped-in connection and return its frame (no None)"
        );
    }

    /// A genuine disconnect (no swap pending) returns `None` immediately — it must
    /// NOT block on the swap grace (client-side auto-reattach must not regress).
    #[test]
    fn recv_genuine_disconnect_returns_none_immediately() {
        let (recv_read, srv) = UnixStream::pair().unwrap();
        // Keep the sender end of the channel alive so try_recv sees Empty (not
        // Disconnected) — this exercises the "no swap pending" fast path exactly.
        let (_swap_tx, swap_rx) = mpsc::channel::<Option<UnixStream>>();
        let mut receiver = HerdrMuxReceiver {
            read: recv_read,
            swap_rx,
            swap_pending: Arc::new(AtomicBool::new(false)),
        };

        drop(srv); // close → EOF, no swap in flight
        let start = std::time::Instant::now();
        assert!(receiver.recv().is_none());
        assert!(
            start.elapsed() < SWAP_GRACE / 2,
            "genuine disconnect must not wait out the swap grace"
        );
    }

    /// Failed reconnect: the wire socket does not exist, so **both**
    /// [`RECONNECT_ATTEMPTS`] connects fail fast, `reattach` returns `Err`, sends the
    /// `None` sentinel, and the reader at EOF ends the stream.
    #[test]
    fn reattach_failed_reconnect_errors_and_reader_ends() {
        let (client, server) = UnixStream::pair().unwrap();
        let read_half = client.try_clone().unwrap();
        let swap_pending = Arc::new(AtomicBool::new(false));
        let (swap_tx, swap_rx) = mpsc::channel();

        let mut sender = HerdrMuxSender {
            write: Arc::new(Mutex::new(client)),
            control: test_control(),
            workspace_id: "ws-1".into(),
            current_terminal_id: "term-A".into(),
            swap_pending: Arc::clone(&swap_pending),
            swap_tx,
            rows: 24,
            cols: 80,
            // Nonexistent socket → the reconnect connect fails.
            wire_socket: PathBuf::from("/nonexistent/mxr_hr_missing.sock"),
        };
        let mut receiver = HerdrMuxReceiver {
            read: read_half,
            swap_rx,
            swap_pending,
        };

        assert!(
            sender.reattach("term-B".into()).is_err(),
            "reattach must fail when the wire socket is unreachable"
        );

        // The old connection was Detached; close it so the reader hits EOF, finds
        // the None sentinel, and ends the stream.
        drop(server);
        assert!(
            receiver.recv().is_none(),
            "reader must end the stream after a failed reconnect"
        );
    }

    /// Retry-succeeds (F1 fix): the FIRST reconnect attempt fails (server accepts
    /// then closes before `Welcome`), the SECOND is served normally → `reattach`
    /// returns `Ok`, the reader adopts the second connection and delivers its frame,
    /// so the stream survives a single transient failure. **Fails against the
    /// pre-fix single-attempt code** (`reattach` would `Err` and `.unwrap()` panics).
    #[test]
    fn reattach_retries_once_then_succeeds() {
        let sock = unique_socket_path("retry_ok");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = std::thread::spawn(move || {
            // Initial attach on conn1.
            let (mut c1, _) = listener.accept().unwrap();
            let _ = serve_handshake(&mut c1);
            // First reconnect attempt: accept then close before Welcome → fails.
            let (c2, _) = listener.accept().unwrap();
            drop(c2);
            // Second reconnect attempt: serve normally and deliver a frame.
            let (mut c3, _) = listener.accept().unwrap();
            let _ = serve_handshake(&mut c3);
            c3.write_all(&frame_server_message(&term_frame(2))).unwrap();
            c3.flush().unwrap();
            (c1, c3) // keep both server ends open
        });

        let conn1 = connect_and_attach(&sock, 24, 80, "term-A").unwrap();
        let (read1, write1) = split_wire(conn1).unwrap();

        let swap_pending = Arc::new(AtomicBool::new(false));
        let (swap_tx, swap_rx) = mpsc::channel();
        let mut receiver = HerdrMuxReceiver {
            read: read1,
            swap_rx,
            swap_pending: Arc::clone(&swap_pending),
        };
        let mut sender = HerdrMuxSender {
            write: Arc::new(Mutex::new(write1)),
            control: test_control(),
            workspace_id: "ws-1".into(),
            current_terminal_id: "term-A".into(),
            swap_pending,
            swap_tx,
            rows: 24,
            cols: 80,
            wire_socket: sock.clone(),
        };

        // Reader blocked on conn1 in a background thread, as in the real relay.
        let reader = std::thread::spawn(move || receiver.recv());

        // First attempt fails, second succeeds → Ok, target advances.
        sender.reattach("term-B".into()).unwrap();
        assert_eq!(sender.current_terminal_id, "term-B");

        let msg = reader.join().unwrap();
        assert!(
            matches!(msg, Some(MuxServerMsg::Render(_))),
            "reader must adopt the second (successful) reconnect and return its frame, got {msg:?}"
        );

        let _ = server.join();
        let _ = std::fs::remove_file(&sock);
    }

    /// Retry-exhausted: BOTH reconnect attempts fail (server accepts then closes
    /// before `Welcome` each time) → `reattach` returns `Err`, the attach target is
    /// unchanged, and the reader (unblocked by the sender's post-Detach shutdown)
    /// ends the stream. Preserves the pre-fix teardown behavior once retries run out.
    #[test]
    fn reattach_retries_exhausted_ends_stream() {
        let sock = unique_socket_path("retry_fail");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = std::thread::spawn(move || {
            // Initial attach on conn1.
            let (mut c1, _) = listener.accept().unwrap();
            let _ = serve_handshake(&mut c1);
            // Both reconnect attempts: accept then close before Welcome → all fail.
            let (a, _) = listener.accept().unwrap();
            drop(a);
            let (b, _) = listener.accept().unwrap();
            drop(b);
            c1 // keep the initial server end open; the sender's shutdown ends the reader
        });

        let conn1 = connect_and_attach(&sock, 24, 80, "term-A").unwrap();
        let (read1, write1) = split_wire(conn1).unwrap();

        let swap_pending = Arc::new(AtomicBool::new(false));
        let (swap_tx, swap_rx) = mpsc::channel();
        let mut receiver = HerdrMuxReceiver {
            read: read1,
            swap_rx,
            swap_pending: Arc::clone(&swap_pending),
        };
        let mut sender = HerdrMuxSender {
            write: Arc::new(Mutex::new(write1)),
            control: test_control(),
            workspace_id: "ws-1".into(),
            current_terminal_id: "term-A".into(),
            swap_pending,
            swap_tx,
            rows: 24,
            cols: 80,
            wire_socket: sock.clone(),
        };

        let reader = std::thread::spawn(move || receiver.recv());

        assert!(
            sender.reattach("term-B".into()).is_err(),
            "reattach must fail after both reconnect attempts fail"
        );
        assert_eq!(
            sender.current_terminal_id, "term-A",
            "attach target must be unchanged after a fully-failed reattach"
        );

        assert!(
            reader.join().unwrap().is_none(),
            "reader must end the stream once the retries are exhausted"
        );

        let _ = server.join();
        let _ = std::fs::remove_file(&sock);
    }

    /// F2 fix (fails against the old `SWAP_GRACE`): the grace must cover the true
    /// worst-case reattach — `RECONNECT_ATTEMPTS × (3 × WIRE_TIMEOUT + CONNECT_SLACK)`.
    /// The pre-fix value `2 × WIRE_TIMEOUT + 1 = 7 s` was smaller than even a single
    /// slow-but-successful attempt (`3 × WIRE_TIMEOUT = 9 s`) and abandoned
    /// about-to-succeed swaps; both assertions below fail against that old constant.
    ///
    /// Because every handshake op is capped at `WIRE_TIMEOUT = 3 s`, a real >7 s
    /// single-attempt handshake cannot be staged without violating `WIRE_TIMEOUT`,
    /// so the constant's adequacy is asserted directly here (deterministic, instant)
    /// and the reader's grace-wait mechanism is exercised separately in
    /// [`adopt_swap_waits_out_a_slow_reconnect`] with a scaled grace.
    #[test]
    fn swap_grace_covers_reconnect_worst_case() {
        let worst_case_per_attempt = 3 * WIRE_TIMEOUT + CONNECT_SLACK;
        let worst_case = RECONNECT_ATTEMPTS as u32 * worst_case_per_attempt;
        assert!(
            SWAP_GRACE >= worst_case,
            "SWAP_GRACE ({SWAP_GRACE:?}) must cover the worst-case reattach ({worst_case:?})"
        );
        // Regression guard: the old 7 s grace could not even cover one slow attempt.
        assert!(
            SWAP_GRACE > 3 * WIRE_TIMEOUT,
            "SWAP_GRACE ({SWAP_GRACE:?}) must exceed a single slow attempt (3×WIRE_TIMEOUT)"
        );
    }

    /// The reader's bounded grace-wait actually blocks for a late-arriving
    /// reconnect: with a 2 s grace, a read half delivered after ~150 ms is still
    /// adopted (and the wait genuinely spanned that delay). Uses the
    /// grace-parameterized helper to stay fast+deterministic — a real `SWAP_GRACE`
    /// (21 s) wait would be far too slow for the suite.
    #[test]
    fn adopt_swap_waits_out_a_slow_reconnect() {
        let (recv_read, _srv1) = UnixStream::pair().unwrap();
        let swap_pending = Arc::new(AtomicBool::new(true));
        let (swap_tx, swap_rx) = mpsc::channel();
        let receiver = HerdrMuxReceiver {
            read: recv_read,
            swap_rx,
            swap_pending,
        };

        // Deliver the new read half after ~150 ms (kept alive via _srv2).
        let (recv_read2, _srv2) = UnixStream::pair().unwrap();
        let deliver = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let _ = swap_tx.send(Some(recv_read2));
            _srv2 // keep the peer alive so the adopted read half stays valid
        });

        let start = std::time::Instant::now();
        let adopted = receiver.try_adopt_swap_within(Duration::from_millis(2000));
        let waited = start.elapsed();

        assert!(
            adopted.is_some(),
            "reader must adopt a reconnect that lands within the grace"
        );
        assert!(
            waited >= Duration::from_millis(120),
            "reader must have waited for the slow reconnect, waited {waited:?}"
        );
        assert!(
            waited < Duration::from_millis(2000),
            "reader must not wait the full grace once the reconnect lands, waited {waited:?}"
        );

        let _srv2 = deliver.join().unwrap();
    }
}
