//! Shared types, aliases, and constants used across the relay submodules.

use std::sync::Arc;

use tokio::sync::mpsc;
use tonic::Status;

use crate::multiplexer::{LayoutSnapshot, PaneRef};
use crate::proto::ServerFrame;

// ─── RelayControl ─────────────────────────────────────────────────────────────

/// Client-supplied floating-pane context for a fullscreen toggle (Bug 2c).
///
/// Lets the relay decide fill-vs-hide-vs-tiled without a synchronous IPC query
/// on the select-loop hot path. Derived by the mobile client from its last
/// polled layout. Staleness tradeoff: if another client toggled floating
/// visibility within the client's poll window, the hint may be stale and the
/// toggle may need a second tap — accepted for v1 (the sole-/mobile-client case
/// is always accurate). When a caller does NOT attest a hint (the
/// `ToggleFullscreen` variant carries `None`) the relay runs the live query
/// instead.
#[derive(Debug, Clone, Copy)]
pub struct FloatingHint {
    /// The target pane is a floating pane (`Pane.is_floating`).
    pub target_is_floating: bool,
    /// Floating panes are visible in the target's tab (`Tab.floating_panes_visible`).
    pub floating_visible: bool,
    /// The target is the currently-focused, visible floating pane.
    pub target_is_focused_floating: bool,
}

/// Control command routed to a live relay's AttachClient connection
/// (W2.0a/b spike). Sent by the unary `GoToTab`/`FocusPane` RPCs through the
/// per-session [`ControlRegistry`] so the action targets the *rendering* client
/// (`is_cli_client:false`) rather than an ephemeral CLI client.
#[derive(Debug)]
pub enum RelayControl {
    /// Switch the rendering client to the tab with this id.
    SwitchTab(u64),
    /// Focus a specific pane (and, in single-pane mode, re-point the display
    /// subscription to it).
    FocusPane(PaneRef),
    /// Switch this relay client's view to the space (herdr workspace) `workspace_id`
    /// **in place** (herdr Spaces, Option A — per-connection view).
    ///
    /// Mutating like [`SwitchTab`](Self::SwitchTab) (dropped for read-only relays)
    /// AND replied like [`QueryLayout`](Self::QueryLayout): the inbound arm calls
    /// [`MuxSender::switch_space`](crate::multiplexer::MuxSender::switch_space)
    /// (re-points the wire stream at the target space's focused pane, with no
    /// daemon-global focus change) and fulfills `reply` so the gRPC SwitchSpace
    /// handler (T03) knows the re-attach succeeded before refreshing the layout.
    SwitchSpace {
        workspace_id: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    /// Toggle fullscreen (or floating-fill) for a specific pane.
    ///
    /// The relay resolves the floating context (from `hint` or a live
    /// `MuxBackend::pane_is_floating_with_visibility` query) into a neutral
    /// `FullscreenHint` and calls `MuxSender::toggle_fullscreen`, which owns the
    /// fill-vs-hide-vs-tiled action sequence (backend-specific; the zellij impl
    /// lives in `multiplexer::zellij`).
    ///
    /// `hint` (Bug 2c): client-supplied floating context so the relay can skip a
    /// synchronous `pane_is_floating_with_visibility` IPC query on the hot path.
    /// `None` → the relay falls back to a live query (keyboard-driven / hint-less
    /// callers).
    ToggleFullscreen {
        pane: PaneRef,
        hint: Option<FloatingHint>,
    },
    /// Query the current tab+pane layout over this relay's existing persistent
    /// connection (B-QUERY, BE-LAYOUT; FX-QUERY redesign).
    ///
    /// The inbound arm first tries the synchronous fast-path
    /// ([`MuxSender::query_layout_result`]); a backend that answers out-of-band
    /// (herdr) fulfills `reply` directly. For the in-band (zellij) path it does
    /// NOT await the reply: it hands an [`InFlightQuery`] (carrying this `reply`)
    /// to the render thread, sends `Action::ListTabs` then `Action::ListPanes`,
    /// and returns immediately. The render thread — the sole owner of `recv()` —
    /// captures the two `Log` replies (tabs then panes), parses them into a
    /// [`LayoutSnapshot`], and fulfills `reply`. The single timeout bound is
    /// `RELAY_QUERY_TIMEOUT` in `grpc.rs`; on timeout it drops the receiver,
    /// which the render thread observes and uses to retire the query.
    ///
    /// [`MuxSender::query_layout_result`]: crate::multiplexer::MuxSender::query_layout_result
    QueryLayout {
        reply: tokio::sync::oneshot::Sender<anyhow::Result<LayoutSnapshot>>,
    },
}

// ─── RelayViewState ───────────────────────────────────────────────────────────

/// Per-session relay-client view state (B-FOCUS, BE-LAYOUT).
///
/// The relay routes every tab-switch and focus action through its own
/// persistent client, so it knows this client's exact active tab and focused
/// pane — independently of how many other desktop clients are attached (zellij
/// reports the union across all clients in ListTabs/ListPanes).
///
/// `get_layout` uses this to OVERRIDE `TabMsg.active` and `PaneMsg.is_focused`
/// with per-relay-client values, giving single-client correctness for the mobile
/// user regardless of desktop clients.
#[derive(Debug, Clone, Default)]
pub struct RelayViewState {
    /// The tab_id of the currently active tab for this relay client.
    /// `None` until first confirmed from attach-time query or a SwitchTab.
    pub active_tab: Option<u64>,
    /// The focused pane for this relay client.
    /// `None` when unknown (e.g. just after a bare tab switch or on init failure).
    pub focused_pane: Option<PaneRef>,
    /// The space (herdr workspace) this relay client is currently viewing
    /// (herdr Spaces, Option A — per-connection view).
    ///
    /// `None` for a **hint-less** attach until this relay performs its first
    /// `SwitchSpace`: such an attach lands on the daemon's focused workspace, so the
    /// backend-reported `SpaceSnapshot.active` is correct and no per-connection
    /// override is needed. A **resumed** attach is the exception — it can land in a
    /// workspace the daemon does not consider active, so `attach_relay` seeds this
    /// at attach time from the backend's reported `resumed_view`.
    /// After a `SwitchSpace` (which deliberately does NOT move the daemon-global
    /// focus), this holds the relay's chosen `workspace_id` so the gRPC `GetSpaces`
    /// handler can mark the connection-active space independently of the daemon's
    /// reported active one. zellij relays never set this (no space concept).
    pub current_space: Option<String>,
}

/// A registry entry pairing a relay's owning session name with its control sender.
///
/// Keyed by `connection_id` (a process-unique monotonic string minted per
/// `AttachTerminal` relay). Storing `session` alongside allows routing code to
/// validate that a request's session matches the registered relay's session
/// before sending commands to it.
#[derive(Clone, Debug)]
pub struct ControlEntry {
    /// The zellij session name this relay is attached to.
    pub session: String,
    /// The channel used to send [`RelayControl`] commands to the relay's inbound task.
    pub sender: mpsc::UnboundedSender<RelayControl>,
    /// Whether the relay was opened with a read-only token.
    ///
    /// The inbound task drops all mutating commands (`SwitchTab`, `FocusPane`,
    /// `ToggleFullscreen`) for read-only relays at its own guard. This flag is
    /// stored here so the session-scoped fallback path in `try_route_control` can
    /// skip read-only entries when routing a mutating command — preventing a silent
    /// false-success where the channel send returns `ok:true` but the command is
    /// then dropped by the inbound guard (Issue B).
    pub read_only: bool,
}

/// A registry entry pairing a relay's owning session name with its view state.
///
/// Keyed by `connection_id`. Storing `session` alongside allows `get_layout` to
/// locate the view state for the exact relay that served the query.
#[derive(Clone, Debug)]
pub struct ViewStateEntry {
    /// The zellij session name this relay is attached to.
    pub session: String,
    /// Per-relay-client view state (active tab + focused pane).
    pub state: RelayViewState,
}

/// Per-connection registry of relay view state (B-FOCUS).
///
/// Keyed by `connection_id` (process-unique, minted per `AttachTerminal`).
/// Each entry stores the owning session name alongside the view state so that
/// routing can validate session match and the session-scoped fallback can scan
/// all entries for "any relay attached to session S".
///
/// Old invariant was "last attach for a session wins"; this registry instead
/// retains ALL concurrently-attached relays for a session — fixing the
/// multi-client misroute bug.
pub type ViewStateRegistry = Arc<dashmap::DashMap<String, ViewStateEntry>>;

/// Per-connection registry of live relay control senders.
///
/// Keyed by `connection_id` (process-unique, minted per `AttachTerminal`).
/// Each entry stores the owning session name alongside the sender so that
/// routing can validate session match and the session-scoped fallback can scan
/// all entries for "any relay attached to session S".
///
/// Replaces the old session-keyed registry (which caused the multi-client
/// misroute bug: last attach for a session overwrote all previous entries).
pub type ControlRegistry = Arc<dashmap::DashMap<String, ControlEntry>>;

// ─── Internal types ───────────────────────────────────────────────────────────

/// Reply channel handed to `get_layout` for a single layout query (FX-QUERY).
///
/// Carries a neutral [`LayoutSnapshot`] (P2.00 A-2): the render thread parses the
/// two captured `Log` JSON payloads into the snapshot before sending, so the
/// gRPC layer is backend-agnostic (no `parse_zellij_layout` at the call site).
pub(crate) type QueryReply = tokio::sync::oneshot::Sender<anyhow::Result<LayoutSnapshot>>;

/// An in-flight layout query, OWNED by the render thread (FX-QUERY).
///
/// The inbound `QueryLayout` arm constructs one (with a fresh monotonic `seq`)
/// and hands it to the render thread over `query_tx`, then sends the two query
/// actions and returns — it never awaits. The render thread fills `tabs` from
/// the first captured `Log` and `panes` from the second, then fulfills `reply`
/// and clears its slot. See the module-level FX-QUERY notes for the invariants.
pub(crate) struct InFlightQuery {
    /// Monotonic id; lets the render thread log/assert ordering and lets a newer
    /// query replace an older one deterministically.
    pub(crate) seq: u64,
    /// Fulfilled with a parsed [`LayoutSnapshot`] once both Logs are captured
    /// (tabs then panes), or dropped (cancelling the receiver) if replaced.
    /// `get_layout`'s outer timeout drops the *receiver*, which we detect via
    /// `reply.is_closed()`.
    pub(crate) reply: QueryReply,
    /// First captured Log → ListTabs JSON. The second Log (panes) is consumed
    /// inline at fulfillment, so it needs no field.
    pub(crate) tabs: Option<String>,
}

/// Sender side of the inbound-task → render-thread query channel (std mpsc).
pub(crate) type QueryTx = std::sync::mpsc::Sender<InFlightQuery>;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Settle gap (ms) between un-fullscreening one pane and re-fullscreening another
/// in the SAME tab. zellij's `unset_fullscreen` resets pane geom-overrides and
/// resizes the tab; an immediate re-enter computes `panes_to_hide` on transitional
/// geometry and bails (no-op). A short gap — matching the manual keyboard cadence,
/// which works — lets the layout settle so the re-enter succeeds.
// Kept in case future UX needs it (tiled parity toggle does not need a settle gap).
#[allow(dead_code)]
pub(crate) const FS_SETTLE_MS: u64 = 250;

/// Backstop timeout for the live-state floating-pane visibility query in the
/// `ToggleFullscreen` arm (BE-HANG). The per-recv socket timeout in
/// `query_session` (`RECV_TIMEOUT` = 5 s) is the first line of defence; this
/// outer bound covers two IPC round-trips plus codec overhead. On timeout the
/// arm degrades to a no-op rather than wedging the `select!` loop.
///
/// (FX-QUERY: hoisted to module level from the former block-local const for
/// consistency with the other timeout constants. The layout query no longer has
/// a per-arm timeout — its single bound is `RELAY_QUERY_TIMEOUT` in `grpc.rs`,
/// since the render thread now owns reply-fulfillment and the inbound arm never
/// awaits.)
pub(crate) const FLOAT_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// How often to re-validate the bearer token of a live attach stream.
///
/// The tower auth layer validates the token once at stream open (Major H).  A
/// revoked/expired token would otherwise keep the stream alive indefinitely
/// (remember_me tokens last 28 days), so we re-check on this interval and tear
/// the stream down on the first failure.
pub(crate) const TOKEN_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Bound on the outbound render channel.
///
/// Renders are coalesced full-screen ANSI snapshots; a small bound gives
/// backpressure (a slow client makes the reader thread block in
/// `blocking_send` rather than buffering unbounded memory) without starving
/// a healthy client.
pub(crate) const RENDER_CHANNEL_BOUND: usize = 64;

/// Neutral fallback size for a **read-only** attach when the session's current
/// size can't be queried (review round-2 Major A).
///
/// Deliberately modest — large enough not to shrink a typical writer's session,
/// but small enough to avoid a giant grid allocation (a huge sentinel like
/// 1000×1000 is rejected on memory grounds). The preferred path is the actual
/// current session size; this is only a last resort.
pub(crate) const RO_FALLBACK_ROWS: u16 = 50;
pub(crate) const RO_FALLBACK_COLS: u16 = 200;

/// Upper bound on a single inbound terminal `Input` frame (1 MiB), matching the
/// `WriteToPane` cap in `grpc.rs`.  A read-write client could otherwise push an
/// unbounded write into the session IPC channel in one frame; oversized frames
/// are dropped with a warning (review round-2 minor).
pub(crate) const MAX_INPUT_FRAME_BYTES: usize = 1024 * 1024;

// ─── Types ────────────────────────────────────────────────────────────────────

/// Type of the outbound stream returned to tonic.
pub type ServerFrameStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<ServerFrame, Status>> + Send>>;
