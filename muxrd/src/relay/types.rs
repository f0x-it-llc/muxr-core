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
    /// Applied on both tiers like [`SwitchTab`](Self::SwitchTab) — it re-points
    /// only THIS connection's view, never the daemon-global focus, so it is a move
    /// a read-only viewer may make — AND replied like
    /// [`QueryLayout`](Self::QueryLayout): the inbound arm calls
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

// ─── Attach eviction (AttachReq.client_instance_id) ──────────────────────────

/// Upper bound on an accepted `AttachReq.client_instance_id`. Longer ids are
/// ignored — treated as unset, so the attach neither evicts nor is evictable —
/// rather than rejected, the same "a hint never fails the attach" rule the
/// resume hint follows. 128 bytes comfortably fits any UUID-style id.
pub const MAX_CLIENT_INSTANCE_ID_LEN: usize = 128;

/// Key of one evictable attach: the identity of the bearer token that
/// authenticated it plus the client's opaque `client_instance_id`.
///
/// The token half is a [`TokenIdentity`](crate::auth::TokenIdentity) digest,
/// never the raw secret. Its `Debug` output is redacted (the instance id is
/// client-supplied and the token half is token-derived), so a key can sit in a
/// log statement without leaking either.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AttachKey {
    token: crate::auth::TokenIdentity,
    instance_id: String,
}

impl AttachKey {
    /// The eviction key for an attach, or `None` when the attach takes no part
    /// in eviction: no authenticated token, an empty `client_instance_id`, or
    /// one longer than [`MAX_CLIENT_INSTANCE_ID_LEN`]. `None` is today's
    /// behavior — such an attach never evicts and is never evicted.
    pub fn new(token: Option<&str>, client_instance_id: &str) -> Option<Self> {
        let token = token?;
        if client_instance_id.is_empty() || client_instance_id.len() > MAX_CLIENT_INSTANCE_ID_LEN {
            return None;
        }
        Some(Self {
            token: crate::auth::TokenIdentity::of(token),
            instance_id: client_instance_id.to_owned(),
        })
    }

    /// The instance id redacted for info/warn logs: its first 8 characters
    /// (control characters escaped) and an ellipsis — the same 8-character
    /// rule `connection_id` follows. The full id is for debug logs only.
    pub fn redacted_instance_id(&self) -> String {
        redact_instance_id(&self.instance_id)
    }

    /// The full instance id, escaped, for debug logs only.
    pub(crate) fn debug_instance_id(&self) -> String {
        self.instance_id.escape_debug().to_string()
    }
}

impl std::fmt::Debug for AttachKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AttachKey(token=<redacted>, instance_id={})",
            self.redacted_instance_id()
        )
    }
}

/// First 8 characters of a client instance id, control characters escaped,
/// plus an ellipsis. Character-based (not a byte slice) because the id is an
/// arbitrary client string and may not be ASCII.
pub(crate) fn redact_instance_id(id: &str) -> String {
    let head: String = id.chars().take(8).collect();
    format!("{}…", head.escape_debug())
}

/// The registry's record of the relay currently holding a key.
#[derive(Debug)]
struct AttachSlot {
    /// `connection_id` of the holding relay — what deregistration compares,
    /// so a torn-down relay can never remove its successor's slot.
    connection_id: String,
    /// The holding relay's eviction signal.
    signal: Arc<EvictSignal>,
}

/// One-shot "you have been superseded" signal shared between the registry and
/// one relay's inbound task.
#[derive(Debug, Default)]
struct EvictSignal {
    /// Set once, on eviction; never cleared.
    fired: std::sync::atomic::AtomicBool,
    /// Wakes the inbound task's `select!`. `notify_one` stores a permit when
    /// the task is not parked on it at that instant, so a signal sent while the
    /// task is busy in another arm is not lost.
    notify: tokio::sync::Notify,
}

impl EvictSignal {
    fn fire(&self) {
        self.fired.store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_one();
    }

    fn is_fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Process-wide registry of evictable attaches (`AttachReq.client_instance_id`).
///
/// Maps `(token identity, client_instance_id)` → the relay currently holding
/// that key. When a new attach registers a key that is already held, the old
/// holder's [`AttachLease`] is signalled: its inbound task leaves its loop
/// through the same teardown a client-closed stream takes (registry cleanup,
/// then the `ShutdownGuard` drop that stops and joins the reader thread, which
/// ends the outbound stream and detaches the backend client). Registration
/// itself never waits for that teardown.
///
/// Cheap to clone; clones share one map.
#[derive(Clone, Debug, Default)]
pub struct AttachRegistry {
    slots: Arc<dashmap::DashMap<AttachKey, AttachSlot>>,
}

impl AttachRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the relay `connection_id` under `key`.
    ///
    /// With `Some(key)`: any relay already holding `key` is signalled to stop,
    /// and its `connection_id` is returned alongside the new lease. The swap is
    /// atomic per key, so of two racing attaches exactly one ends up holding it
    /// and the other is signalled. With `None` (no token, empty or over-long
    /// id): nothing is registered or signalled, and the lease is never evicted.
    pub fn register(
        &self,
        key: Option<AttachKey>,
        connection_id: &str,
    ) -> (AttachLease, Option<String>) {
        let Some(key) = key else {
            return (
                AttachLease {
                    registry: self.clone(),
                    key: None,
                    connection_id: connection_id.to_owned(),
                    signal: None,
                },
                None,
            );
        };
        let signal = Arc::new(EvictSignal::default());
        let previous = self.slots.insert(
            key.clone(),
            AttachSlot {
                connection_id: connection_id.to_owned(),
                signal: signal.clone(),
            },
        );
        let evicted = previous.map(|old| {
            old.signal.fire();
            old.connection_id
        });
        (
            AttachLease {
                registry: self.clone(),
                key: Some(key),
                connection_id: connection_id.to_owned(),
                signal: Some(signal),
            },
            evicted,
        )
    }

    /// Remove `key`'s slot only if it still belongs to `connection_id`.
    /// Returns whether a slot was removed: `false` when the key is absent or
    /// has since been taken over by a successor, whose slot is left untouched.
    pub fn deregister(&self, key: &AttachKey, connection_id: &str) -> bool {
        self.slots
            .remove_if(key, |_, slot| slot.connection_id == connection_id)
            .is_some()
    }

    /// `connection_id` of the relay currently holding `key`, if any.
    pub fn holder(&self, key: &AttachKey) -> Option<String> {
        self.slots.get(key).map(|slot| slot.connection_id.clone())
    }

    /// Number of keys currently held.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no key is currently held.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// One relay's hold on its [`AttachRegistry`] key, owned by its inbound task.
///
/// [`evicted`](Self::evicted) resolves once a later attach with the same key
/// has superseded this relay; dropping the lease deregisters the key only if
/// this relay still holds it. A lease created without a key (an attach with no
/// `client_instance_id`) is inert: never evicted, nothing to deregister.
#[derive(Debug)]
pub struct AttachLease {
    registry: AttachRegistry,
    key: Option<AttachKey>,
    connection_id: String,
    signal: Option<Arc<EvictSignal>>,
}

impl AttachLease {
    /// Resolve once this relay has been evicted; pending forever for an inert
    /// lease. Cancel-safe: dropping the future (another `select!` arm won)
    /// loses no signal, so it can be re-created on every loop iteration.
    pub async fn evicted(&self) {
        let Some(signal) = &self.signal else {
            return std::future::pending().await;
        };
        loop {
            let notified = signal.notify.notified();
            if signal.is_fired() {
                return;
            }
            notified.await;
        }
    }

    /// Whether this relay has been evicted (non-blocking).
    pub fn is_evicted(&self) -> bool {
        self.signal.as_ref().is_some_and(|s| s.is_fired())
    }

    /// The key this lease holds, or `None` for an inert lease.
    pub fn key(&self) -> Option<&AttachKey> {
        self.key.as_ref()
    }
}

impl Drop for AttachLease {
    fn drop(&mut self) {
        if let Some(key) = &self.key
            && !self.registry.deregister(key, &self.connection_id)
        {
            // A successor took the key over (this relay was evicted, or lost
            // a registration race): its slot stays.
            log::debug!(
                "relay attach registry: connection_id={} left instance id {} to its successor",
                self.connection_id,
                key.debug_instance_id()
            );
        }
    }
}

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

/// Upper bound on a single inbound terminal `Input` frame (1 MiB), matching the
/// `WriteToPane` cap in `grpc.rs`.  A read-write client could otherwise push an
/// unbounded write into the session IPC channel in one frame; oversized frames
/// are dropped with a warning (review round-2 minor).
pub(crate) const MAX_INPUT_FRAME_BYTES: usize = 1024 * 1024;

// ─── Types ────────────────────────────────────────────────────────────────────

/// Type of the outbound stream returned to tonic.
pub type ServerFrameStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<ServerFrame, Status>> + Send>>;
