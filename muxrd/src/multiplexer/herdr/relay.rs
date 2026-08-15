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
    FullscreenHint, LayoutSnapshot, MuxEvent, MuxMouseKind, MuxServerMsg, PaneRef, ResumeTarget,
    ResumedView,
};
use crate::multiplexer::{DualHandle, MuxReceiver, MuxSender};

use super::api::{PaneInfo, PaneZoomMode};
use super::control::HerdrControl;
use super::registry::{HerdrPaneRegistry, HerdrTabRegistry};
use super::wire::{
    AttachScrollDirection, AttachScrollSource, ClientKeybindings, ClientLaunchMode, ClientMessage,
    FramingError, HERDR_MAX_TESTED_PROTOCOL, HERDR_MIN_PROTOCOL, RenderEncoding, ServerMessage,
    WireFrame, read_server_message, write_message,
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
///
/// **Deliberate resilience-vs-latency trade-off:** `reattach` runs to completion
/// inside one `run_sender_op` on the relay's inbound `select!` loop (serialized —
/// see the module header), so a stuck herdr can block that loop — and therefore
/// keystrokes and detach-detection on THIS relay — for roughly
/// `RECONNECT_ATTEMPTS × 3 × WIRE_TIMEOUT` (≈10 s → ≈20 s versus a hypothetical
/// single-attempt version) before the retries give up. We accept this because a
/// torn-down stream costs a full client-side reconnect (worse for the user) and
/// herdr is co-located/local, so a stuck attempt is the rare case the retry exists
/// to survive.
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
///
/// **Coupling to `ShutdownGuard` (relay/reader.rs):** `ShutdownGuard::drop` joins
/// the reader's std thread, which can be parked inside this same grace wait if a
/// teardown races an in-flight swap. Today the inbound task's `select!` serializes
/// `run_sender_op`s, so a `reattach` and a client-exit teardown on the same relay
/// cannot overlap in practice — but if a future refactor relaxes that
/// serialization, `ShutdownGuard::drop`'s join becomes transitively bounded by
/// `SWAP_GRACE` (worst case ~21 s to tear down a relay). A change to either this
/// constant or the `select!` serialization in `relay/reader.rs` should re-check
/// that this race stays unreachable.
const SWAP_GRACE: Duration = Duration::from_secs(
    RECONNECT_ATTEMPTS as u64 * (3 * WIRE_TIMEOUT.as_secs() + CONNECT_SLACK.as_secs())
        + SWAP_SLACK.as_secs(),
);

// ─── open_attach (P2.04 entry point) ──────────────────────────────────────────

/// Open a herdr wire attach for `workspace_id`, returning the split
/// [`DualHandle`]. Performs the v14 handshake, asserts protocol compatibility,
/// and attaches the pane [`resolve_attach_target`] picks — the workspace's
/// **focused pane** by default, or the client's resume target when `resume`
/// carries a usable hint (the single-pane attach model either way).
///
/// `session_name` is the neutral muxrd session name echoed back in
/// [`DualHandle::session_name`]; `workspace_id` is the already-resolved herdr
/// workspace id (the daemon's active-or-first), used as the **fallback** when the
/// resume hint names no usable workspace. `control` shares the JSON-API client +
/// registries with the backend (P2.04). `wire_socket` is herdr's binary relay
/// socket ([`HerdrSocketPaths::wire`](super::paths::HerdrSocketPaths)); P2.04
/// resolves it once alongside the api socket so both planes agree on the instance.
///
/// `resume` is best-effort: a stale/unknown hint can never fail the attach (see
/// [`resolve_attach_target`]). When it is non-empty the resolved view is reported
/// through [`DualHandle::resumed_view`] so the relay can seed its per-connection
/// view state from the pane we actually landed on.
///
/// `read_only` is logged for traceability; herdr enforces write ownership on the
/// terminal itself, and the read-only teardown nudge is `Detach`
/// ([`MuxSender::send_client_exited`]) for both modes.
// One argument over clippy's threshold: this is the single P2.04 attach entry
// point and every parameter is an independent input to the handshake (matching
// the `#[allow]`s the relay's own `attach_relay`/`inbound_loop` carry).
#[allow(clippy::too_many_arguments)]
pub fn open_attach(
    control: Arc<HerdrControl>,
    wire_socket: PathBuf,
    workspace_id: String,
    session_name: String,
    rows: u16,
    cols: u16,
    read_only: bool,
    resume: &ResumeTarget,
) -> Result<DualHandle> {
    log::debug!(
        "herdr open_attach workspace='{workspace_id}' session='{session_name}' \
         {rows}x{cols} read_only={read_only} resume={resume:?}"
    );

    // Resolve the pane to attach — the resume target if it still resolves, else
    // the focused pane (four-rung ladder). Also populates the pane registry so
    // later focus_pane(PaneRef{id}) resolves. Done before opening the wire socket.
    let target = resolve_attach_target(&control, &workspace_id, resume)?;

    // Ask this herdr which wire protocol it speaks — never assume a version.
    let protocol = discover_protocol(&control)?;

    // Connect + handshake + attach on a fresh wire connection, then split into the
    // blocking read half and the bounded write half.
    let stream = connect_and_attach(&wire_socket, rows, cols, &target.terminal_id, protocol)?;
    let (read_half, write_half) = split_wire(stream)?;

    // Shared, swappable connection state. The write half lives behind a mutex so a
    // reattach's socket swap is seen by every `box_clone`d ShutdownGuard handle at
    // once; the reader is handed its post-swap read half over `swap_tx`, gated by
    // the `swap_pending` flag so an EOF mid-swap is adopted, not misread as EOF.
    let swap_pending = Arc::new(AtomicBool::new(false));
    let (swap_tx, swap_rx) = mpsc::channel();
    let write = Arc::new(Mutex::new(write_half));

    // Report the resolved view ONLY when the client actually asked to resume: a
    // hint-less attach must stay byte-identical to the pre-resume behavior (the
    // relay then runs its usual view-state init).
    let resumed_view = (!resume.is_empty()).then(|| ResumedView {
        space_id: target.workspace_id.clone(),
        tab_id: target.tab_id,
        // herdr has no plugin panes (the layout transcode reports is_plugin=false
        // for every pane), so a terminal PaneRef is the only correct shape here.
        pane: PaneRef::terminal(target.pane_id),
    });

    Ok(DualHandle {
        sender: Box::new(HerdrMuxSender {
            write: Arc::clone(&write),
            control,
            // The relay renders the workspace we RESOLVED, not the one we were
            // handed: query_layout_result() reads this field, so a resumed attach
            // into another space must report that space's tabs from the first poll.
            workspace_id: target.workspace_id,
            current_terminal_id: target.terminal_id,
            swap_pending: Arc::clone(&swap_pending),
            swap_tx,
            rows,
            cols,
            wire_socket,
            protocol,
        }),
        receiver: Box::new(HerdrMuxReceiver {
            read: read_half,
            swap_rx,
            swap_pending,
        }),
        session_name,
        resumed_view,
    })
}

/// Ask the connected herdr which wire protocol version it speaks.
///
/// muxrd never pins a version: herdr enforces strict equality on the relay handshake
/// and rejects clients that are older *or* newer than itself, so the only correct
/// value is whatever this specific server reports. The JSON-API socket used here is
/// stable across herdr releases and has reported `protocol` since 0.7.1.
///
/// A protocol above [`HERDR_MAX_TESTED_PROTOCOL`] is **not** an error — muxrd proceeds
/// and logs a warning. herdr's changes so far have been additive (appended message
/// variants), which the decoder tolerates, so refusing to attach would break users on
/// a new herdr for no reason. The warning is the tripwire that tells us to re-verify
/// the layout if herdr ever makes a breaking change.
fn discover_protocol(control: &HerdrControl) -> Result<u32> {
    let info = control
        .ping()
        .context("discover herdr wire protocol version via JSON-API ping")?;

    if info.protocol < HERDR_MIN_PROTOCOL {
        log::warn!(
            "herdr {} reports wire protocol v{}, which is older than the oldest version \
             muxrd vendored message layouts for (v{HERDR_MIN_PROTOCOL}). Proceeding, but \
             frames may not decode — upgrading herdr is the supported fix.",
            info.version,
            info.protocol,
        );
    } else if info.protocol > HERDR_MAX_TESTED_PROTOCOL {
        log::warn!(
            "herdr {} speaks wire protocol v{}, which is newer than the highest version \
             muxrd has been tested against (v{HERDR_MAX_TESTED_PROTOCOL}). Proceeding — \
             herdr's protocol changes have so far been additive — but if terminal output \
             misbehaves, this mismatch is the first thing to check.",
            info.version,
            info.protocol,
        );
    } else {
        log::debug!(
            "herdr {} speaks wire protocol v{}",
            info.version,
            info.protocol
        );
    }
    Ok(info.protocol)
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
    protocol: u32,
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
        version: protocol,
        cols,
        rows,
        cell_width_px: CELL_PX_DISABLED,
        cell_height_px: CELL_PX_DISABLED,
        requested_encoding: RenderEncoding::TerminalAnsi,
        keybindings: ClientKeybindings::Server,
        launch_mode: ClientLaunchMode::TerminalAttach,
    };
    write_message(&mut &stream, &hello).context("send herdr Hello")?;

    let welcome = match read_server_message(&mut &stream).context("read herdr Welcome")? {
        WireFrame::Message(msg) => msg,
        WireFrame::Unknown { tag, .. } => {
            return Err(anyhow!(
                "herdr handshake: expected Welcome, got an unknown frame (tag {tag}) — \
                 the server's wire layout is not one muxrd can interpret"
            ));
        }
    };
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

/// Assert herdr accepted the handshake: no `error`, and the negotiated encoding is
/// what we asked for.
///
/// There is deliberately **no protocol-version equality check** here. The version we
/// send is the one the server itself reported over the JSON-API `ping`, so a
/// client-side re-assertion could only ever fire if herdr contradicted itself — while
/// a hard-coded expectation would (and did) break muxrd on every herdr release that
/// bumps the protocol. herdr performs the authoritative check on its side and reports
/// any rejection through `error`, which is surfaced verbatim below.
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
    log::debug!("herdr handshake: negotiated wire protocol v{version}");
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

// ─── Resume-target resolution (four-rung fallback) ────────────────────────────

/// The pane a fresh attach lands on, in every id the relay needs afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachTarget {
    /// herdr workspace the pane lives in — becomes the relay's `workspace_id`.
    workspace_id: String,
    /// Registry tab id (`u64`) of the pane's tab, as GetLayout emits it.
    tab_id: u64,
    /// Registry pane id (`u32`) of the pane, as GetLayout emits it.
    pane_id: u32,
    /// herdr `terminal_id` — the wire `AttachTerminal` attach key.
    terminal_id: String,
}

/// The focused pane among `scoped`, falling back to the first one listed
/// (`None` only when `scoped` is empty). The shared shape of every rung below
/// and of [`resolve_focused_terminal`] / [`pick_tab_pane`].
fn focused_or_first<'a>(scoped: impl Iterator<Item = &'a PaneInfo>) -> Option<&'a PaneInfo> {
    let mut first: Option<&PaneInfo> = None;
    let mut focused: Option<&PaneInfo> = None;
    for pane in scoped {
        if first.is_none() {
            first = Some(pane);
        }
        if pane.focused {
            focused = Some(pane);
        }
    }
    focused.or(first)
}

/// Pure core of the resume resolution **within one workspace's pane list**:
/// registers ALL panes (same contract as [`resolve_focused_terminal`] /
/// [`pick_tab_pane`], so a later `focus_pane(u32)` resolves for panes on any
/// tab) and then picks the target by falling down these rungs:
///
/// 1. `hint_pane` (a herdr `pane_id`) that is still present in this workspace;
/// 2. `hint_tab` (a herdr `tab_id`) → that tab's focused-or-first pane;
/// 3. the workspace's focused-or-first pane — today's behavior.
///
/// Every rung falls through silently on a stale/unknown hint, so the only
/// failure is a workspace with **no panes at all**; the caller uses that to fall
/// back to the default workspace ([`resolve_attach_target_with`]'s rung 4).
/// Separated from the I/O call for unit-testability, mirroring [`pick_tab_pane`].
fn pick_resume_pane(
    panes: &[PaneInfo],
    pane_reg: &HerdrPaneRegistry,
    tab_reg: &HerdrTabRegistry,
    workspace_id: &str,
    hint_pane: Option<&str>,
    hint_tab: Option<&str>,
) -> Result<AttachTarget> {
    for pane in panes {
        pane_reg.assign_or_get(&pane.pane_id, &pane.terminal_id);
    }

    let chosen = hint_pane
        .and_then(|hp| panes.iter().find(|p| p.pane_id == hp))
        .or_else(|| {
            hint_tab.and_then(|ht| focused_or_first(panes.iter().filter(|p| p.tab_id == ht)))
        })
        .or_else(|| focused_or_first(panes.iter()))
        .ok_or_else(|| anyhow!("herdr workspace '{workspace_id}' has no panes to attach"))?;

    Ok(AttachTarget {
        workspace_id: workspace_id.to_string(),
        tab_id: tab_reg.assign_or_get(&chosen.tab_id),
        pane_id: pane_reg.assign_or_get(&chosen.pane_id, &chosen.terminal_id),
        terminal_id: chosen.terminal_id.clone(),
    })
}

/// Translate a [`ResumeTarget`]'s **numeric** pane/tab hints into herdr's opaque
/// string ids through the shared registries.
///
/// Deliberately read-only ([`HerdrPaneRegistry::herdr_pane_id`] /
/// [`HerdrTabRegistry::herdr_tab_id`] never assign) and deliberately called
/// **before** this attach's registration pass: on a fresh muxrd process the
/// registries are empty, so a hint minted by a previous process resolves to
/// `None` and falls through — rather than aliasing whichever pane happens to be
/// handed that id now.
fn hint_ids(
    pane_reg: &HerdrPaneRegistry,
    tab_reg: &HerdrTabRegistry,
    resume: &ResumeTarget,
) -> (Option<String>, Option<String>) {
    (
        resume.pane_id.and_then(|id| pane_reg.herdr_pane_id(id)),
        resume.tab_id.and_then(|id| tab_reg.herdr_tab_id(id)),
    )
}

/// Resolve the pane a fresh attach should land on, honouring a best-effort
/// [`ResumeTarget`]. Performs the `pane.list` I/O; see
/// [`resolve_attach_target_with`] for the (pure, unit-tested) ladder.
fn resolve_attach_target(
    control: &HerdrControl,
    default_workspace: &str,
    resume: &ResumeTarget,
) -> Result<AttachTarget> {
    let (hint_pane, hint_tab) = hint_ids(control.pane_registry(), control.tab_registry(), resume);
    resolve_attach_target_with(
        |workspace_id| {
            control
                .list_panes(workspace_id)
                .with_context(|| format!("list herdr panes for workspace '{workspace_id}'"))
        },
        control.pane_registry(),
        control.tab_registry(),
        default_workspace,
        resume.space_id.as_deref(),
        hint_pane.as_deref(),
        hint_tab.as_deref(),
    )
}

/// The four-rung resume ladder, parameterized over the pane-list query so it is
/// unit-testable without a live herdr:
///
/// 1. `hint_pane` inside `hint_space` (when set) else `default_workspace`;
/// 2. `hint_tab` → that tab's focused-or-first pane, same workspace;
/// 3. `hint_space` → that workspace's focused-or-first pane;
/// 4. nothing usable → `default_workspace`'s focused-or-first pane, i.e. exactly
///    what a hint-less attach has always done.
///
/// Rungs 1-3 all run against ONE `list_panes` of the hinted workspace; if that
/// query fails (unknown/closed workspace) or the workspace has no panes, the
/// whole thing retries against `default_workspace`, where rungs 1-2 are still
/// honoured before rung 4. **An unresolvable hint can therefore never fail the
/// attach** — only a herdr that cannot list the default workspace's panes can.
fn resolve_attach_target_with<F>(
    list_panes: F,
    pane_reg: &HerdrPaneRegistry,
    tab_reg: &HerdrTabRegistry,
    default_workspace: &str,
    hint_space: Option<&str>,
    hint_pane: Option<&str>,
    hint_tab: Option<&str>,
) -> Result<AttachTarget>
where
    F: Fn(&str) -> Result<Vec<PaneInfo>>,
{
    let resolve_in = |workspace_id: &str| -> Result<AttachTarget> {
        let panes = list_panes(workspace_id)?;
        pick_resume_pane(&panes, pane_reg, tab_reg, workspace_id, hint_pane, hint_tab)
    };

    // Rungs 1-3: the hinted space (skipped when it IS the default workspace —
    // the fallback below covers that case with one query instead of two).
    if let Some(space) = hint_space.filter(|s| *s != default_workspace) {
        match resolve_in(space) {
            Ok(target) => return Ok(target),
            Err(e) => log::debug!(
                "herdr attach: resume space '{space}' unusable ({e:#}); \
                 falling back to workspace '{default_workspace}'"
            ),
        }
    }

    // Rungs 1-2 in the default workspace, else rung 4 (today's focused pane).
    resolve_in(default_workspace)
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
    /// Wire protocol version negotiated on the CURRENT connection.
    ///
    /// A reconnect re-queries herdr (it may have been restarted, even upgraded, in
    /// the meantime) but falls back to this value if the JSON-API socket is briefly
    /// unreachable — a reconnect must not fail merely because the control plane
    /// blipped when the wire socket itself is fine.
    protocol: u32,
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
            // Re-discover the protocol rather than blindly reusing the old value: a
            // reconnect is exactly when herdr may have been restarted — possibly
            // upgraded to a release speaking a new protocol — under a running muxrd.
            // If the control socket is momentarily unreachable, fall back to the
            // version already negotiated instead of failing the reconnect outright;
            // the wire socket is what actually matters here.
            let protocol = match discover_protocol(&self.control) {
                Ok(p) => {
                    self.protocol = p;
                    p
                }
                Err(e) => {
                    log::debug!(
                        "herdr reattach: protocol re-discovery failed ({e:#}); \
                         reusing negotiated v{}",
                        self.protocol
                    );
                    self.protocol
                }
            };
            match connect_and_attach(
                &self.wire_socket,
                self.rows,
                self.cols,
                &terminal_id,
                protocol,
            )
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
            protocol: self.protocol,
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
            // reattach is in flight; otherwise this is a genuine disconnect
            // (`?` propagates the `None` and ends the stream).
            self.read = self.try_adopt_swap()?;
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
        Ok(WireFrame::Message(msg)) => Some(map_server_message(*msg)),
        // A variant appended by a newer herdr. The frame was fully consumed using its
        // length prefix, so the stream is still aligned — drain it like any other
        // message we do not act on. This must NOT end the relay: doing so would turn
        // every future additive herdr change into a silently dying terminal.
        Ok(WireFrame::Unknown { tag, len }) => {
            log::debug!("herdr wire: skipping unknown message tag {tag} ({len} bytes)");
            Some(MuxServerMsg::Other)
        }
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

    /// A `HerdrControl` over a nonexistent JSON-API socket.
    ///
    /// The layout tests want the query to fail fast — and this doubles as coverage
    /// for the reconnect fallback: `reattach` re-queries the protocol over this
    /// socket, so with it unreachable the reconnect must still succeed using the
    /// version negotiated on the original attach.
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
            protocol: HERDR_MAX_TESTED_PROTOCOL,
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
    /// `Welcome{echoed version, TerminalAnsi, no error}`, read `AttachTerminal`.
    /// Returns both.
    ///
    /// The version is echoed back from the client's `Hello` exactly as a real herdr
    /// does — it accepts only a client whose version equals its own, so an echo is
    /// the faithful stand-in and keeps these fixtures free of any pinned constant.
    fn serve_handshake(stream: &mut UnixStream) -> (ClientMessage, ClientMessage) {
        let hello = read_client_message(stream);
        let hello_version = match &hello {
            ClientMessage::Hello { version, .. } => *version,
            other => panic!("expected Hello first, got {other:?}"),
        };
        stream
            .write_all(&frame_server_message(&ServerMessage::Welcome {
                version: hello_version,
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
                version: HERDR_MIN_PROTOCOL,
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

    // ── assert_welcome: error-driven, NOT version-gated ───────────────────────

    #[test]
    fn assert_welcome_accepts_no_error() {
        assert!(
            assert_welcome(&ServerMessage::Welcome {
                version: HERDR_MIN_PROTOCOL,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            })
            .is_ok()
        );
    }

    /// The regression that broke muxrd on herdr 0.7.2+: a client-side equality check
    /// on the protocol version. muxrd now echoes the version the server itself
    /// reported, so *any* version in a clean `Welcome` must be accepted — including
    /// ones newer than anything muxrd has been tested against. herdr performs the
    /// authoritative check and signals rejection via `error`.
    #[test]
    fn assert_welcome_accepts_any_version_when_server_reports_no_error() {
        for version in [13, HERDR_MIN_PROTOCOL, 16, HERDR_MAX_TESTED_PROTOCOL, 99] {
            assert!(
                assert_welcome(&ServerMessage::Welcome {
                    version,
                    encoding: RenderEncoding::TerminalAnsi,
                    error: None,
                })
                .is_ok(),
                "version {version} must not be rejected client-side"
            );
        }
    }

    #[test]
    fn assert_welcome_rejects_handshake_error() {
        assert!(
            assert_welcome(&ServerMessage::Welcome {
                version: HERDR_MIN_PROTOCOL,
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

    // ── resume resolution: the four-rung fallback ladder ──────────────────────

    /// A two-tab workspace: tab-A holds one pane, tab-B two (the second focused).
    /// The workspace-wide focused-or-first pane is therefore `term-b2`.
    fn resume_panes() -> Vec<PaneInfo> {
        vec![
            make_pane("pane-a1", "term-a1", "tab-A", false),
            make_pane("pane-b1", "term-b1", "tab-B", false),
            make_pane("pane-b2", "term-b2", "tab-B", true), // focused (daemon-global)
        ]
    }

    /// Rung 1: a live pane hint wins over both the tab hint and the workspace's
    /// focused pane — this is the whole point of a resumable attach.
    #[test]
    fn resume_picks_the_exact_hinted_pane() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let target = pick_resume_pane(
            &resume_panes(),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("pane-a1"),
            Some("tab-B"),
        )
        .unwrap();

        assert_eq!(
            target.terminal_id, "term-a1",
            "the pane hint must beat the tab hint AND the daemon-global focus"
        );
        assert_eq!(
            target.tab_id,
            tab_reg.assign_or_get("tab-A"),
            "the reported tab id must be the hinted pane's OWN tab"
        );
        assert_eq!(target.pane_id, pane_reg.assign_or_get("pane-a1", "term-a1"));
        assert_eq!(target.workspace_id, "ws-1");
    }

    /// Rung 2: a stale pane hint (pane closed since the client last saw it) falls
    /// through to the tab hint, which resolves to that tab's focused pane.
    #[test]
    fn stale_pane_hint_falls_through_to_the_tab_hint() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let target = pick_resume_pane(
            &resume_panes(),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("pane-gone"),
            Some("tab-B"),
        )
        .unwrap();

        assert_eq!(
            target.terminal_id, "term-b2",
            "a closed pane must fall through to the tab hint's focused pane"
        );
        assert_eq!(target.tab_id, tab_reg.assign_or_get("tab-B"));
    }

    /// Rung 2, no focus: the tab hint resolves to the tab's FIRST pane when herdr
    /// reports none of its panes focused (same rule as `pick_tab_pane`).
    #[test]
    fn tab_hint_without_focus_falls_back_to_the_tabs_first_pane() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let panes = vec![
            make_pane("pane-a1", "term-a1", "tab-A", true), // focused, other tab
            make_pane("pane-b1", "term-b1", "tab-B", false),
            make_pane("pane-b2", "term-b2", "tab-B", false),
        ];
        let target =
            pick_resume_pane(&panes, &pane_reg, &tab_reg, "ws-1", None, Some("tab-B")).unwrap();
        assert_eq!(target.terminal_id, "term-b1");
    }

    /// Rung 3/4: both a stale pane hint AND a stale tab hint fall all the way
    /// through to the workspace's focused pane — the attach still happens.
    #[test]
    fn stale_tab_hint_falls_through_to_the_workspace_focused_pane() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let target = pick_resume_pane(
            &resume_panes(),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("pane-gone"),
            Some("tab-gone"),
        )
        .unwrap();

        assert_eq!(
            target.terminal_id, "term-b2",
            "a fully stale hint must land on the workspace's focused pane"
        );
    }

    /// Unset hints = unchanged behavior: identical to `resolve_focused_terminal`
    /// (focused pane, else the first listed). This is the backward-compatibility
    /// pin for every client that never sends the resume fields.
    #[test]
    fn no_resume_hints_pick_the_focused_pane_like_today() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let focused =
            pick_resume_pane(&resume_panes(), &pane_reg, &tab_reg, "ws-1", None, None).unwrap();
        assert_eq!(focused.terminal_id, "term-b2", "focused pane wins");

        // No pane focused at all → the first listed, exactly like today.
        let unfocused_panes = vec![
            make_pane("pane-a1", "term-a1", "tab-A", false),
            make_pane("pane-b1", "term-b1", "tab-B", false),
        ];
        let first =
            pick_resume_pane(&unfocused_panes, &pane_reg, &tab_reg, "ws-1", None, None).unwrap();
        assert_eq!(first.terminal_id, "term-a1", "first listed pane wins");
    }

    /// An arbitrary garbage hint still attaches (acceptance criterion 2): unknown
    /// ids on every axis degrade to the current behavior instead of erroring.
    #[test]
    fn garbage_hints_never_fail_the_attach() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let target = pick_resume_pane(
            &resume_panes(),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("../../etc/passwd"),
            Some("💥 not a tab"),
        )
        .expect("a garbage hint must never fail the attach");
        assert_eq!(target.terminal_id, "term-b2");
    }

    /// The resolution keeps `resolve_focused_terminal`'s registration contract:
    /// EVERY workspace pane is registered (so a later `focus_pane(u32)` resolves
    /// for panes on other tabs), and the reported ids are the registry ids
    /// GetLayout emits.
    #[test]
    fn resume_registers_every_pane_and_reports_registry_ids() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let target = pick_resume_pane(
            &resume_panes(),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("pane-b1"),
            None,
        )
        .unwrap();

        for (pane, terminal) in [
            ("pane-a1", "term-a1"),
            ("pane-b1", "term-b1"),
            ("pane-b2", "term-b2"),
        ] {
            let id = pane_reg
                .id_for_herdr_pane(pane)
                .unwrap_or_else(|| panic!("{pane} must be registered"));
            assert_eq!(pane_reg.terminal_id(id).as_deref(), Some(terminal));
        }
        assert_eq!(
            pane_reg.terminal_id(target.pane_id).as_deref(),
            Some("term-b1"),
            "the reported pane id must round-trip to the attached terminal"
        );
        assert_eq!(
            tab_reg.herdr_tab_id(target.tab_id).as_deref(),
            Some("tab-B")
        );
    }

    /// A workspace with no panes is the ONLY failure of the pure picker — the
    /// caller uses it to fall back to the default workspace (rung 4).
    #[test]
    fn pick_resume_pane_errors_when_the_workspace_has_no_panes() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let err = pick_resume_pane(&[], &pane_reg, &tab_reg, "ws-empty", None, None)
            .expect_err("an empty workspace has nothing to attach");
        assert!(
            err.to_string().contains("ws-empty"),
            "error must name the workspace, got: {err}"
        );
    }

    // ── hint_ids: numeric hints are translated BEFORE registration ────────────

    #[test]
    fn hint_ids_translate_registered_numeric_hints() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let pane_id = pane_reg.assign_or_get("pane-b2", "term-b2");
        let tab_id = tab_reg.assign_or_get("tab-B");

        let (hint_pane, hint_tab) = hint_ids(
            &pane_reg,
            &tab_reg,
            &ResumeTarget {
                space_id: None,
                tab_id: Some(tab_id),
                pane_id: Some(pane_id),
            },
        );
        assert_eq!(hint_pane.as_deref(), Some("pane-b2"));
        assert_eq!(hint_tab.as_deref(), Some("tab-B"));
    }

    /// Unknown numeric hints (the fresh-muxrd-process case: the registries are
    /// empty, so ids minted by a previous process mean nothing) resolve to `None`
    /// and fall through — they must NOT alias whichever pane holds that id now.
    #[test]
    fn hint_ids_drop_unknown_numeric_hints() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let (hint_pane, hint_tab) = hint_ids(
            &pane_reg,
            &tab_reg,
            &ResumeTarget {
                space_id: Some("ws-1".into()),
                tab_id: Some(999),
                pane_id: Some(999),
            },
        );
        assert!(hint_pane.is_none() && hint_tab.is_none());
        // A read-only lookup must not have assigned anything either.
        assert!(pane_reg.herdr_pane_id(1).is_none());
    }

    // ── resolve_attach_target_with: the space rung + its fallback ─────────────

    /// Fixture lister: two workspaces, everything else unknown (herdr errors on a
    /// workspace id it does not have).
    fn fixture_lister(
        queried: &std::cell::RefCell<Vec<String>>,
    ) -> impl Fn(&str) -> Result<Vec<PaneInfo>> {
        move |workspace_id: &str| {
            queried.borrow_mut().push(workspace_id.to_string());
            match workspace_id {
                "ws-1" => Ok(resume_panes()),
                "ws-2" => Ok(vec![
                    make_pane("pane-c1", "term-c1", "tab-C", false),
                    make_pane("pane-c2", "term-c2", "tab-C", true), // focused in ws-2
                ]),
                other => Err(anyhow!("herdr: no workspace '{other}'")),
            }
        }
    }

    /// Rung 3: a live space hint resolves inside THAT workspace (not the default),
    /// and the resolved workspace is what the relay will render.
    #[test]
    fn space_hint_resolves_inside_the_hinted_workspace() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let queried = std::cell::RefCell::new(Vec::new());
        let target = resolve_attach_target_with(
            fixture_lister(&queried),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("ws-2"),
            None,
            None,
        )
        .unwrap();

        assert_eq!(target.workspace_id, "ws-2");
        assert_eq!(target.terminal_id, "term-c2");
        assert_eq!(queried.borrow().as_slice(), ["ws-2"]);
    }

    /// Rungs 1-2 are evaluated INSIDE the hinted space, not the default one.
    #[test]
    fn pane_hint_resolves_within_the_hinted_space() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let queried = std::cell::RefCell::new(Vec::new());
        let target = resolve_attach_target_with(
            fixture_lister(&queried),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("ws-2"),
            Some("pane-c1"),
            None,
        )
        .unwrap();
        assert_eq!(target.workspace_id, "ws-2");
        assert_eq!(target.terminal_id, "term-c1");
    }

    /// Rung 4: a stale space hint (workspace closed) falls back to the default
    /// workspace's focused pane — today's behavior — instead of failing.
    #[test]
    fn stale_space_hint_falls_back_to_the_default_workspace() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let queried = std::cell::RefCell::new(Vec::new());
        let target = resolve_attach_target_with(
            fixture_lister(&queried),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("ws-vanished"),
            None,
            None,
        )
        .expect("a stale space hint must never fail the attach");

        assert_eq!(target.workspace_id, "ws-1");
        assert_eq!(target.terminal_id, "term-b2");
        assert_eq!(
            queried.borrow().as_slice(),
            ["ws-vanished", "ws-1"],
            "the hinted space is tried first, then the default workspace"
        );
    }

    /// A stale space hint does not disarm the other rungs: the pane hint is still
    /// honoured in the workspace we fell back to.
    #[test]
    fn stale_space_hint_still_honours_the_pane_hint_in_the_fallback() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let queried = std::cell::RefCell::new(Vec::new());
        let target = resolve_attach_target_with(
            fixture_lister(&queried),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("ws-vanished"),
            Some("pane-a1"),
            None,
        )
        .unwrap();
        assert_eq!(target.workspace_id, "ws-1");
        assert_eq!(target.terminal_id, "term-a1");
    }

    /// A space hint naming the workspace we would use anyway costs ONE query, not
    /// two (the ladder skips the redundant hinted-space attempt).
    #[test]
    fn space_hint_equal_to_the_default_queries_once() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let queried = std::cell::RefCell::new(Vec::new());
        let target = resolve_attach_target_with(
            fixture_lister(&queried),
            &pane_reg,
            &tab_reg,
            "ws-1",
            Some("ws-1"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(target.workspace_id, "ws-1");
        assert_eq!(queried.borrow().as_slice(), ["ws-1"]);
    }

    /// A hint-less attach queries only the default workspace and lands on its
    /// focused pane — the pre-resume behavior, unchanged.
    #[test]
    fn no_hints_query_only_the_default_workspace() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let queried = std::cell::RefCell::new(Vec::new());
        let target = resolve_attach_target_with(
            fixture_lister(&queried),
            &pane_reg,
            &tab_reg,
            "ws-1",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(target.workspace_id, "ws-1");
        assert_eq!(target.terminal_id, "term-b2");
        assert_eq!(queried.borrow().as_slice(), ["ws-1"]);
    }

    /// The only hard failure left: herdr cannot list the DEFAULT workspace's panes
    /// (a dead daemon / vanished workspace) — the attach must still surface that.
    #[test]
    fn unusable_default_workspace_is_the_only_hard_failure() {
        let (pane_reg, tab_reg) = (HerdrPaneRegistry::new(), HerdrTabRegistry::new());
        let queried = std::cell::RefCell::new(Vec::new());
        let err = resolve_attach_target_with(
            fixture_lister(&queried),
            &pane_reg,
            &tab_reg,
            "ws-dead",
            Some("ws-2"),
            None,
            None,
        );
        // The hinted space resolved, so this variant must SUCCEED …
        assert!(err.is_ok(), "a live space hint rescues a dead default");

        let queried = std::cell::RefCell::new(Vec::new());
        let err = resolve_attach_target_with(
            fixture_lister(&queried),
            &pane_reg,
            &tab_reg,
            "ws-dead",
            None,
            None,
            None,
        );
        assert!(
            err.is_err(),
            "with no usable workspace at all the attach must fail loudly"
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
        let conn1 = connect_and_attach(&sock, 24, 80, "term-A", HERDR_MAX_TESTED_PROTOCOL).unwrap();
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
            protocol: HERDR_MAX_TESTED_PROTOCOL,
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

        let conn1 = connect_and_attach(&sock, 24, 80, "term-A", HERDR_MAX_TESTED_PROTOCOL).unwrap();
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
            protocol: HERDR_MAX_TESTED_PROTOCOL,
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
            protocol: HERDR_MAX_TESTED_PROTOCOL,
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

        let conn1 = connect_and_attach(&sock, 24, 80, "term-A", HERDR_MAX_TESTED_PROTOCOL).unwrap();
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
            protocol: HERDR_MAX_TESTED_PROTOCOL,
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

        let conn1 = connect_and_attach(&sock, 24, 80, "term-A", HERDR_MAX_TESTED_PROTOCOL).unwrap();
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
            protocol: HERDR_MAX_TESTED_PROTOCOL,
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
    /// reconnect **long enough that a regression to the OLD `SWAP_GRACE` formula
    /// (`2 × WIRE_TIMEOUT + 1`) would fail it**, not merely that the wait mechanism
    /// blocks at all: both the grace and the delivery delay are derived from the
    /// real constants and scaled down by [`GRACE_TEST_SCALE_DOWN`] to stay
    /// fast+deterministic (a real 21 s `SWAP_GRACE` wait would be far too slow for
    /// the suite). The delay is chosen to exceed the OLD formula at the same scale
    /// but fit comfortably inside the NEW one, so this test — unlike a version
    /// pinned to arbitrary literals — actually exercises the F2 magnitude fix.
    /// [`swap_grace_covers_reconnect_worst_case`] is the const-level companion
    /// guard (asserts the unscaled relationship directly).
    #[test]
    fn adopt_swap_waits_out_a_slow_reconnect() {
        const GRACE_TEST_SCALE_DOWN: u32 = 10;

        // Scaled-down NEW grace (from the real, derived SWAP_GRACE): ≈2.1 s.
        let scaled_grace = SWAP_GRACE / GRACE_TEST_SCALE_DOWN;
        // Scaled-down OLD (pre-fix) formula (`2 × WIRE_TIMEOUT + 1` = 7 s): ≈0.7 s.
        let old_formula_scaled =
            (2 * WIRE_TIMEOUT + Duration::from_secs(1)) / GRACE_TEST_SCALE_DOWN;
        // Just over the old bound, comfortably under the new one.
        let delay = old_formula_scaled + Duration::from_millis(50);
        assert!(
            delay > old_formula_scaled && delay < scaled_grace,
            "test setup: delay ({delay:?}) must exceed the scaled OLD grace \
             ({old_formula_scaled:?}) but fit inside the scaled NEW grace ({scaled_grace:?})"
        );

        let (recv_read, _srv1) = UnixStream::pair().unwrap();
        let swap_pending = Arc::new(AtomicBool::new(true));
        let (swap_tx, swap_rx) = mpsc::channel();
        let receiver = HerdrMuxReceiver {
            read: recv_read,
            swap_rx,
            swap_pending,
        };

        // Deliver the new read half after `delay` (kept alive via _srv2).
        let (recv_read2, _srv2) = UnixStream::pair().unwrap();
        let deliver = std::thread::spawn(move || {
            std::thread::sleep(delay);
            let _ = swap_tx.send(Some(recv_read2));
            _srv2 // keep the peer alive so the adopted read half stays valid
        });

        let start = std::time::Instant::now();
        let adopted = receiver.try_adopt_swap_within(scaled_grace);
        let waited = start.elapsed();

        assert!(
            adopted.is_some(),
            "reader must adopt a reconnect that lands within the scaled grace — a \
             regression to the old SWAP_GRACE formula would time out here (waited {waited:?})"
        );
        assert!(
            waited >= delay.saturating_sub(Duration::from_millis(30)),
            "reader must have waited for the slow reconnect, waited {waited:?}, delay {delay:?}"
        );
        assert!(
            waited < scaled_grace,
            "reader must not wait the full grace once the reconnect lands, waited {waited:?}"
        );

        let _srv2 = deliver.join().unwrap();
    }
}
