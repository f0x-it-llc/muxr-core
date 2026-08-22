//! multiplexer — the backend-agnostic seam between muxrd's gRPC/relay layer and
//! a concrete terminal multiplexer (P1.01).
//!
//! [`MuxBackend`] is the contract: methods take and return the neutral types in
//! [`types`] (`PaneRef`, `ResizeKind`/`ResizeDir`, `LayoutSnapshot`,
//! `MuxServerMsg`/`MuxEvent`, …) and never zellij's. [`ZellijBackend`] is the
//! first — and, in Phase 1, only — implementation; it translates neutral ↔
//! `zellij_utils` at its own boundary by delegating to the existing
//! `crate::{ipc, actions, query}` free functions. A Phase 2 *herdr* backend will
//! implement the same trait without any zellij coupling.
//!
//! ## Blocking, not async
//!
//! Every [`MuxBackend`] method is **blocking** — it wraps the blocking zellij IPC
//! free functions. Callers (the gRPC handlers and the relay) already run these on
//! `tokio::task::spawn_blocking`, exactly as they do today for the underlying
//! `ipc`/`actions`/`query` calls. The trait is deliberately NOT `async`: making
//! it so would force a `spawn_blocking` *inside* every impl method and buy
//! nothing.
//!
//! ## Object safety
//!
//! `MuxrService` holds an `Arc<dyn MuxBackend>`, so the trait must be
//! object-safe. The attach seam therefore uses **concrete boxed halves**
//! ([`DualHandle`] of `Box<dyn MuxSender>` + `Box<dyn MuxReceiver>`) rather than
//! generic associated types.
//!
//! ## Phase 1 status
//!
//! P1.01 is a **pure addition**: this module exists and `MuxrService` constructs
//! an `Arc<dyn MuxBackend>`, but no handler or the relay is rerouted through it
//! yet. P1.02 reroutes the ephemeral handlers; P1.03 drives the relay off
//! [`MuxBackend::open_attach`].

pub mod detect;
pub mod events;
pub(crate) mod herdr;
pub(crate) mod routing;
pub mod types;
mod zellij;

pub use types::{
    ActionAck, FullscreenHint, LayoutSnapshot, MuxEvent, MuxMouseKind, MuxServerMsg, PaneRef,
    PaneSnapshot, ResizeDir, ResizeKind, ResumeTarget, ResumedView, ScrollDir, SpaceSnapshot,
    TabSnapshot,
};
pub use zellij::ZellijBackend;

// Re-export routing helpers at the multiplexer level so relay and grpc can
// import them as `crate::multiplexer::{make_id, resolve_session, …}` — the
// correct layer (relay depends on multiplexer, grpc depends on multiplexer;
// neither needs to reach into each other). S-M1 fix.
pub(crate) use routing::{
    is_collapsed_backend_session, make_id, resolve_session, resolve_session_kind, validate_session,
};

/// The canonical herdr session sentinel, re-exported so grpc and relay can
/// reference it as `crate::multiplexer::HERDR_SESSION` without reaching into
/// the herdr sub-module's internals directly (architecture seam).
pub(crate) use herdr::backend::HERDR_SESSION;
/// Re-exported for the P2.05 backend selector in `bin/muxrd.rs` and the
/// integration test harness.  The herdr sub-module remains `pub(crate)` to keep
/// its wire/API internals crate-private; only the public entry point is lifted.
pub use herdr::backend::HerdrBackend;
/// The herdr event-kernel spawn entry point (task 01). Re-exported publicly so
/// `bin/muxrd.rs::serve()` can start the persistent `events.subscribe` consumer
/// without reaching into the crate-private `herdr` sub-module. Only spawned when
/// a herdr backend is present.
pub use herdr::subscribe::spawn_event_kernel;
/// The single zellij-JSON → [`LayoutSnapshot`] parse, shared by the ephemeral
/// [`MuxBackend::query_layout`] impl and the relay-routed `GetLayout` path
/// (`grpc/layout.rs`). `pub(crate)` so the gRPC layer can reuse it on the JSON
/// strings captured by the relay, instead of duplicating the deserialization.
pub(crate) use zellij::parse_zellij_layout;

use std::sync::Arc;
use std::time::Duration;

use crate::cli::BackendKind;

// ─── BackendSet ─────────────────────────────────────────────────────────────────

/// An ordered set of the multiplexer backends this server is driving.
///
/// Phase 3 lets one `muxrd` serve **every** detected backend simultaneously
/// (`zellij` and `herdr` side by side) instead of a single operator-selected one.
/// `MuxrService` holds a `BackendSet` instead of a lone `Arc<dyn MuxBackend>`;
/// `bin/muxrd.rs::cmd_start` builds it from [`detect::detect_backends`].
///
/// Insertion order is preserved (detection order: zellij first, then herdr) and
/// is significant: [`BackendSet::primary`] returns the first backend, which the
/// not-yet-id-aware handlers route through until T04 adds per-session id routing.
/// Cheap to clone — every entry is an `Arc`.
#[derive(Debug, Clone)]
pub struct BackendSet {
    /// `(kind, backend)` pairs in detection order. Small (≤ 2 today), so a `Vec`
    /// linear scan in [`BackendSet::get`] is cheaper than a hash map.
    backends: Vec<(BackendKind, Arc<dyn MuxBackend>)>,
}

impl BackendSet {
    /// Build a set from `(kind, backend)` pairs in the desired routing order.
    ///
    /// The caller (`cmd_start`) guarantees the vec is non-empty (detection
    /// fails fast when no backend is usable); [`BackendSet::primary`] relies on
    /// that invariant.
    pub fn new(backends: Vec<(BackendKind, Arc<dyn MuxBackend>)>) -> Self {
        debug_assert!(!backends.is_empty(), "BackendSet must hold ≥ 1 backend");
        Self { backends }
    }

    /// Convenience constructor for a single-backend set (used by
    /// [`MuxrService::new`](crate::grpc::MuxrService::new)'s zellij default).
    pub fn single(kind: BackendKind, backend: Arc<dyn MuxBackend>) -> Self {
        Self::new(vec![(kind, backend)])
    }

    /// The backend registered for `kind`, if present.
    pub fn get(&self, kind: BackendKind) -> Option<&Arc<dyn MuxBackend>> {
        self.backends
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, b)| b)
    }

    /// Iterate over `(kind, backend)` pairs in detection order.
    pub fn iter(&self) -> impl Iterator<Item = (BackendKind, &Arc<dyn MuxBackend>)> {
        self.backends.iter().map(|(k, b)| (*k, b))
    }

    /// The kinds in this set, in detection order.
    pub fn kinds(&self) -> impl Iterator<Item = BackendKind> + '_ {
        self.backends.iter().map(|(k, _)| *k)
    }

    /// Number of backends held.
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// Whether the set is empty (never true for a `cmd_start`-built set).
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    /// The primary (first / highest-priority) backend.
    ///
    /// Used by the not-yet-id-aware handlers via the `MuxrService::backend()`
    /// shim until T04 introduces `resolve_session(id)` routing. Panics on an
    /// empty set, which the non-empty construction invariant prevents.
    pub fn primary(&self) -> &Arc<dyn MuxBackend> {
        &self
            .backends
            .first()
            .expect("BackendSet invariant: at least one backend")
            .1
    }
}

// ─── MuxBackend ─────────────────────────────────────────────────────────────────

/// A terminal-multiplexer backend.
///
/// All methods are **blocking** (they wrap blocking IPC); call them from a
/// `spawn_blocking` context. `anyhow::Result` is used internally — handlers map
/// to `tonic::Status` at their own boundary (unchanged by this trait).
pub trait MuxBackend: Send + Sync + std::fmt::Debug {
    // ── Session lifecycle ───────────────────────────────────────────────────

    /// List live sessions as `(name, age)` pairs.
    fn list_sessions(&self) -> anyhow::Result<Vec<(String, Duration)>>;

    /// List live + resurrectable sessions as `(name, age_secs, resurrectable)`.
    fn list_sessions_with_resurrectables(&self) -> anyhow::Result<Vec<(String, u64, bool)>>;

    /// Validate a session name (path-traversal / allowlist guard). Returns the
    /// offending message on rejection.
    fn validate_session_name(&self, name: &str) -> Result<(), String>;

    /// Create a new detached session, optionally with a layout path.
    fn create_session(&self, name: &str, layout: Option<String>) -> anyhow::Result<ActionAck>;

    /// Kill a session.
    fn kill_session(&self, session: &str) -> anyhow::Result<()>;

    /// Rename a session.
    fn rename_session(&self, session: &str, new_name: String) -> anyhow::Result<ActionAck>;

    // ── Ephemeral control actions ───────────────────────────────────────────

    /// Send raw bytes to the focused pane.
    fn write_to_pane(
        &self,
        session: &str,
        pane: PaneRef,
        bytes: Vec<u8>,
    ) -> anyhow::Result<ActionAck>;
    /// Move keyboard focus to the specified pane.
    fn focus_pane(&self, session: &str, pane: PaneRef) -> anyhow::Result<ActionAck>;
    /// Close the specified pane.
    fn close_pane(&self, session: &str, pane: PaneRef) -> anyhow::Result<ActionAck>;
    /// Create a new pane in the active tab, optionally floating.
    fn new_pane(
        &self,
        session: &str,
        floating: bool,
        name: Option<String>,
    ) -> anyhow::Result<ActionAck>;
    /// Assign a display name to a pane.
    fn rename_pane(&self, session: &str, pane: PaneRef, name: String) -> anyhow::Result<ActionAck>;
    /// Resize a pane in the specified direction.
    fn resize_pane(
        &self,
        session: &str,
        pane: PaneRef,
        kind: ResizeKind,
        dir: Option<ResizeDir>,
    ) -> anyhow::Result<ActionAck>;
    /// Toggle a floating pane between floating and tiled layout.
    fn toggle_pane_floating(&self, session: &str, pane: PaneRef) -> anyhow::Result<ActionAck>;
    /// Toggle fullscreen for a pane (ephemeral; relay uses `MuxSender::toggle_fullscreen`).
    fn toggle_pane_fullscreen(&self, session: &str, pane: PaneRef) -> anyhow::Result<ActionAck>;
    /// Scroll a pane's scrollback buffer up or down.
    fn scroll_pane(
        &self,
        session: &str,
        pane: PaneRef,
        dir: ScrollDir,
    ) -> anyhow::Result<ActionAck>;
    /// Create a new tab in the session, optionally with a name.
    fn new_tab(&self, session: &str, name: Option<String>) -> anyhow::Result<ActionAck>;
    /// Close the specified tab.
    fn close_tab(&self, session: &str, tab_id: u64) -> anyhow::Result<ActionAck>;
    /// Switch the active tab to the specified tab id.
    fn go_to_tab(&self, session: &str, tab_id: u64) -> anyhow::Result<ActionAck>;
    /// Assign a display name to a tab.
    fn rename_tab(&self, session: &str, tab_id: u64, name: String) -> anyhow::Result<ActionAck>;

    // ── Spaces (herdr workspaces; default: unsupported) ──────────────────────

    /// Whether this backend has a **space** axis at all.
    ///
    /// A cheap, side-effect-free capability predicate (no I/O): the gRPC layer
    /// checks it before honouring an explicitly space-scoped request, so a client
    /// that names a space on a backend without one gets a clean
    /// `invalid_argument` instead of a silently space-less answer. The **default
    /// is `false`** (zellij and every mock); herdr overrides it to `true`.
    ///
    /// It is deliberately NOT derived from `list_spaces` — that performs IPC and
    /// an empty list is a legitimate answer for a live spaces backend.
    fn supports_spaces(&self) -> bool {
        false
    }

    /// Build a neutral [`LayoutSnapshot`] for a **specific** space of `session`.
    ///
    /// `space_id`:
    /// - `None` → the session's ordinary layout; **byte-identical** to
    ///   [`Self::query_layout`] (that is exactly what the default does).
    /// - `Some(id)` → the layout of THAT space, read directly by its opaque id.
    ///
    /// **Read-only contract:** an override MUST NOT move any focus — not the
    /// connection's, not the daemon's. Naming a foreign space here is a *peek*
    /// (herdr's `workspace.*` reads are already workspace-scoped and
    /// side-effect-free); a backend that could only answer by focusing the space
    /// must return an error instead.
    ///
    /// The **default ignores `space_id`** and delegates to [`Self::query_layout`],
    /// so zellij — and every mock/test impl — is untouched. Only a backend that
    /// returns `true` from [`Self::supports_spaces`] is ever handed a `Some(id)`
    /// by the gRPC layer.
    fn query_layout_for_space(
        &self,
        session: &str,
        space_id: Option<&str>,
    ) -> anyhow::Result<LayoutSnapshot> {
        let _ = space_id;
        self.query_layout(session)
    }

    /// List the backend's "spaces" for `session`.
    ///
    /// Spaces are a herdr-only axis (its workspaces, surfaced as in-place
    /// switchable sub-navigation within the single collapsed herdr session). The
    /// **default returns the empty vec** so zellij — and any backend without a
    /// space concept — is untouched. herdr overrides this to map
    /// `workspace.list` → [`SpaceSnapshot`].
    fn list_spaces(&self, session: &str) -> anyhow::Result<Vec<SpaceSnapshot>> {
        let _ = session;
        Ok(vec![])
    }

    /// Create a new space with an optional label. Default: unsupported (zellij).
    fn create_space(&self, label: Option<String>) -> anyhow::Result<ActionAck> {
        let _ = label;
        Ok(space_unsupported_ack("create_space"))
    }

    /// Rename the space `space_id` to `label`. Default: unsupported (zellij).
    fn rename_space(&self, space_id: &str, label: &str) -> anyhow::Result<ActionAck> {
        let _ = (space_id, label);
        Ok(space_unsupported_ack("rename_space"))
    }

    /// Close the space `space_id`. Default: unsupported (zellij).
    fn close_space(&self, space_id: &str) -> anyhow::Result<ActionAck> {
        let _ = space_id;
        Ok(space_unsupported_ack("close_space"))
    }

    // ── Read-only queries ───────────────────────────────────────────────────

    /// Build a neutral [`LayoutSnapshot`] for `session` (raw queried values;
    /// no per-relay-client override, plugin panes included).
    fn query_layout(&self, session: &str) -> anyhow::Result<LayoutSnapshot>;

    /// Current `(rows, cols)` of the session's active tab display area.
    fn query_session_size(&self, session: &str) -> anyhow::Result<(u16, u16)>;

    /// `(is_floating, floating_panes_visible, focused_floating_pane)` for `pane`.
    fn pane_is_floating_with_visibility(
        &self,
        session: &str,
        pane: PaneRef,
    ) -> anyhow::Result<(bool, bool, Option<PaneRef>)>;

    // ── Attach (the relay seam) ─────────────────────────────────────────────

    /// Open an attach to `session` at `rows`×`cols`, returning a [`DualHandle`]
    /// of boxed neutral sender/receiver halves. `read_only` is part of the
    /// contract for backends that vary the open by mode; the zellij backend's
    /// read-only geometry handling lives at the call site (the relay pre-resolves
    /// the size) and in the shutdown path (`send_client_exited` vs resize nudge).
    fn open_attach(
        &self,
        session: &str,
        rows: u16,
        cols: u16,
        read_only: bool,
    ) -> anyhow::Result<DualHandle>;

    /// Open an attach that honours a client's best-effort [`ResumeTarget`],
    /// reporting the view it landed on via [`DualHandle::resumed_view`].
    ///
    /// **This is the fork where the resume hint stops mattering for every
    /// backend but herdr:** the default implementation *ignores* `resume` and
    /// delegates to [`Self::open_attach`], so zellij (whose attach streams the
    /// whole composited session and follows the server's own focus — there is no
    /// per-connection pane to resume) and any future backend without a resume
    /// concept keep byte-identical behavior. Only [`HerdrBackend`] overrides it.
    ///
    /// An override MUST NOT fail the attach on a stale/unknown hint: every field
    /// is best-effort and falls through to the backend's current focus.
    ///
    /// **Recursion trap:** this default delegates to [`Self::open_attach`], so a
    /// backend that implements [`Self::open_attach`] by delegating *here* — a
    /// natural way to funnel both entry points through one resume-aware code path
    /// — would recurse forever. Override **at least one** of the pair with a real
    /// implementation ([`HerdrBackend`] overrides this one and keeps its own
    /// `open_attach`).
    fn open_attach_with_resume(
        &self,
        session: &str,
        rows: u16,
        cols: u16,
        read_only: bool,
        resume: &ResumeTarget,
    ) -> anyhow::Result<DualHandle> {
        let _ = resume;
        self.open_attach(session, rows, cols, read_only)
    }

    // ── Backend identity ────────────────────────────────────────────────────

    /// Backend version string (today: `zellij_utils::consts::VERSION`).
    fn backend_version(&self) -> String;
}

/// A failed [`ActionAck`] for a space operation on a backend that has no space
/// concept (the [`MuxBackend`] space-lifecycle defaults — zellij). Surfaced as a
/// failure so the client reports it rather than silently dropping the request.
fn space_unsupported_ack(op: &str) -> ActionAck {
    ActionAck {
        ok: false,
        error: Some(format!("{op}: spaces are not supported on this backend")),
        info: None,
    }
}

// ─── DualHandle + split halves ──────────────────────────────────────────────────

/// An open attach, split into a [`MuxSender`] (input/control) and a
/// [`MuxReceiver`] (render/event stream), plus the resolved session name.
///
/// Concrete boxed halves keep `dyn MuxBackend` object-safe (no generic
/// associated types) and let the relay move the receiver onto a dedicated
/// blocking reader thread while keeping the sender on the async side — exactly
/// the split it does today with `ipc::AttachHandle::split`.
pub struct DualHandle {
    pub sender: Box<dyn MuxSender>,
    pub receiver: Box<dyn MuxReceiver>,
    pub session_name: String,
    /// The view this attach resolved from a [`ResumeTarget`], when the backend
    /// honours resume hints (herdr). `None` for every hint-less attach and for
    /// backends that ignore the hint — the relay then initialises its view state
    /// exactly as it does today.
    pub resumed_view: Option<ResumedView>,
}

impl DualHandle {
    /// Consume the handle into its two halves.
    pub fn split(self) -> (Box<dyn MuxSender>, Box<dyn MuxReceiver>) {
        (self.sender, self.receiver)
    }
}

/// The input/control half of a [`DualHandle`].
///
/// Exposes the neutral operations the relay's inbound task + `ShutdownGuard`
/// drive today (matching the current `AttachSender` call sites so P1.03 can
/// consume it). All sends are **blocking** but cheap.
pub trait MuxSender: Send {
    /// Switch the rendering client to the tab with this id.
    fn go_to_tab(&mut self, tab_id: u64) -> anyhow::Result<()>;

    /// Focus a specific pane (as this rendering client).
    fn focus_pane(&mut self, pane: PaneRef) -> anyhow::Result<()>;

    /// Switch this rendering client's view to the space `space_id` **in place**
    /// (per-connection view; no daemon-global focus change).
    ///
    /// Spaces are a herdr-only axis: the herdr sender re-points its wire stream at
    /// the target workspace's focused pane and records the new workspace as its
    /// current one, so a subsequent [`Self::query_layout_result`] returns that
    /// space's layout — all without calling herdr's daemon-global `workspace.focus`
    /// (so co-attached desktop clients are never yanked). The **default errors**:
    /// a backend with no space concept (zellij) cannot honour the request.
    fn switch_space(&mut self, space_id: &str) -> anyhow::Result<()> {
        let _ = space_id;
        Err(anyhow::anyhow!("this backend does not support spaces"))
    }

    /// Toggle fullscreen / floating-fill for `pane`, given the resolved floating
    /// context `hint`. Encapsulates the fill-vs-hide-vs-tiled action sequence the
    /// relay performs today. The caller (relay) updates its own view state from
    /// the same `hint` it passes, so no return value is needed.
    fn toggle_fullscreen(&mut self, pane: PaneRef, hint: FullscreenHint) -> anyhow::Result<()>;

    /// Whether this backend answers layout **synchronously**, out-of-band of the
    /// render stream (via [`Self::query_layout_result`]).
    ///
    /// This is a cheap, side-effect-free predicate the relay's `QueryLayout` arm
    /// checks to choose its dispatch BEFORE doing any query work — we cannot
    /// "peek" by calling [`Self::query_layout_result`], since that call performs
    /// the (blocking) query. `true` routes the query onto the blocking pool;
    /// `false` (the default, zellij) takes the in-band `Log` path.
    ///
    /// The default is `false` — backends with an out-of-band layout channel
    /// (herdr's JSON-API socket) override it to `true`. Any override returning
    /// `true` MUST also return `Some(_)` from [`Self::query_layout_result`].
    fn has_sync_layout(&self) -> bool {
        false
    }

    /// Answer a layout query **synchronously**, out-of-band of the render stream.
    ///
    /// A backend whose layout lives behind a separate control channel (herdr's
    /// JSON-API socket) returns `Some(snapshot)`; the relay then fulfills the
    /// `QueryLayout` reply directly and never arms the in-band `Log` path. A
    /// backend whose layout query is answered **in-band** via the render stream's
    /// `Log` replies (zellij's `ListTabs`/`ListPanes`) returns `None` (the
    /// default) — the relay falls through to [`Self::query_layout`].
    ///
    /// **Contract for overrides:** this performs blocking local-socket I/O, so the
    /// relay runs it on the **blocking pool** (`spawn_blocking`), never inline on
    /// the inbound `select!` task — a backend selects that path by returning `true`
    /// from [`Self::has_sync_layout`]. It should still be bounded (a short
    /// request/response per the backend's own per-call timeout) so the blocking
    /// thread is freed promptly. The zellij default returns `None` immediately and
    /// pays nothing.
    fn query_layout_result(&mut self) -> Option<anyhow::Result<LayoutSnapshot>> {
        None
    }

    /// Fire a layout query over the persistent connection (ListTabs then
    /// ListPanes). The replies arrive as two [`MuxServerMsg::Log`] messages on
    /// the [`MuxReceiver`], which the relay's reader pairs (tabs then panes).
    /// Only used by backends that return `None` from [`Self::query_layout_result`].
    fn query_layout(&mut self) -> anyhow::Result<()>;

    /// Send UTF-8 text input to this client's focused pane.
    fn send_input_chars(&mut self, text: &str) -> anyhow::Result<()>;

    /// Send raw byte input (e.g. ESC sequences) to this client's focused pane.
    fn send_input_bytes(&mut self, bytes: Vec<u8>) -> anyhow::Result<()>;

    /// Forward a structured mouse event at zero-based (`col`,`row`) in THIS
    /// relay client's grid. The multiplexer resolves the pane under the
    /// position and either forwards the event to the pane's application (if
    /// it captured the mouse) or handles it itself (pane scrollback).
    fn send_mouse(&mut self, kind: MuxMouseKind, col: u16, row: u16) -> anyhow::Result<()>;

    /// Notify the server of a new terminal size.
    fn send_resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()>;

    /// Tell the server this client is leaving (read-only teardown nudge).
    fn send_client_exited(&mut self) -> anyhow::Result<()>;

    /// Clone an independent sender over the same underlying connection — used by
    /// the relay's `ShutdownGuard` to nudge a parked reader on teardown.
    fn box_clone(&self) -> Box<dyn MuxSender>;
}

/// The render/event half of a [`DualHandle`].
///
/// Moved onto the relay's dedicated blocking reader thread; [`Self::recv`] is
/// **blocking** and returns `None` on stream close (EOF / decode error).
pub trait MuxReceiver: Send {
    /// Receive the next neutral server message (blocking). `None` ends the stream.
    fn recv(&mut self) -> Option<MuxServerMsg>;
}
