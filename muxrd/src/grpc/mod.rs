//! grpc — tonic service implementation of the Muxr gRPC API.
//!
//! Surface: `GetVersion`, `Login` (public, no bearer), `AttachTerminal` (bidi
//! relay), `ListSessions`/`GetLayout` (typed reads), and the pane/tab/session
//! mutating ops.
//!
//! Auth note: `Login` and `GetVersion` are **public** (no bearer required).
//! Every other RPC requires a valid bearer token injected by the tower layer in
//! [`crate::auth::BearerAuthLayer`].  Mutating RPCs additionally call
//! [`helpers::reject_if_read_only`]; `AttachTerminal` enforces the read-only flag inside
//! the relay (render-only for RO tokens) and re-validates the token mid-stream.
//! Every session-name entry point runs [`helpers::validate_session`] (path-traversal
//! guard).

use tonic::{Request, Response, Status, Streaming};

use crate::proto::muxr_server::Muxr;
use crate::proto::{
    ActionAck as ProtoAck, ClientFrame, CloseSpaceReq, CreateSessionReq, CreateSpaceReq, Empty,
    Layout, ListPushTargetsReq, ListPushTargetsResp, LoginRequest, LoginResponse, NewPaneReq,
    NewTabReq, PaneTarget, RegisterPushTargetReq, RemovePushTargetReq, RenamePaneReq,
    RenameSessionReq, RenameSpaceReq, RenameTabReq, ResizePaneReq, ScrollReq, SessionList,
    SessionRef, SpaceList, SwitchSpaceReq, TabTarget, ToggleFullscreenReq, VersionInfo,
    WriteToPaneReq,
};

pub mod helpers;
mod layout;
mod pane_ops;
mod push_ops;
mod session_ops;
mod space_ops;
mod tab_ops;
mod token_ops;

/// The zellij source version this server was compiled against.
///
/// Asserted at service construction to catch version drift early.
pub const ZELLIJ_CONTRACT_VERSION: &str = "0.44.3";

/// muxrd's own semantic version (tracks the crate version).
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

// ─── Service ─────────────────────────────────────────────────────────────────

/// Tonic service implementing the `Muxr` gRPC service.
///
/// `Default` is hand-written (below) rather than derived: the `backends` field is
/// a [`BackendSet`](crate::multiplexer::BackendSet) of `Arc<dyn MuxBackend>` trait
/// objects, which has no `Default`. `Clone` stays derived — every backend is an
/// `Arc`, so the clone is cheap and all relays share the same backends.
#[derive(Debug, Clone)]
pub struct MuxrService {
    /// Per-session count of clients attached through this server (Phase F).
    /// Shared with every relay so `ListSessions` can report `connected_clients`.
    clients: crate::client_count::SessionClients,
    /// Per-connection registry, keyed by connection_id (process-unique, minted
    /// per AttachTerminal relay): lets `GoToTab`/`FocusPane` route through the
    /// *rendering* AttachClient (`is_cli_client:false`) instead of an ephemeral
    /// CLI client. Each concurrent relay on the same session holds its own slot.
    control: crate::relay::ControlRegistry,
    /// Per-connection registry, keyed by connection_id (process-unique, minted
    /// per AttachTerminal relay): the relay client's own active_tab +
    /// focused_pane, tracked across SwitchTab/FocusPane/ToggleFullscreen. Used
    /// by `get_layout` to override the queried is_focused/active fields with
    /// single-client-correct values — only when the relay that served the query
    /// is the caller's own relay (exact connection_id match).
    view_state: crate::relay::ViewStateRegistry,
    /// The terminal-multiplexer backends this server drives (Phase 1 seam,
    /// Phase 3 multi-backend). Holds **every** detected backend (zellij and/or
    /// herdr) behind the same `MuxBackend` trait, in detection order.
    ///
    /// Session-scoped RPCs resolve the owning backend via
    /// [`MuxrService::resolve_session`] (Option C id routing, `multiplexer::routing`).
    /// `ListSessions` fans out to all backends in iteration order; `CreateSession`
    /// selects the backend directly from the proto `backend` field (or defaults to
    /// the sole backend when `BACKEND_UNSPECIFIED`). No `backend()` shim exists.
    /// Cheap to clone (`Arc`s).
    backends: crate::multiplexer::BackendSet,
    /// The push-notification relay URL to advertise on `GetVersion`
    /// (`VersionInfo.notification_relay_url` + the `"push-notifications"`
    /// capability). `None` when the operator has no relay configured
    /// (`config::EffectiveConfig::notify_relay_url`) — advertised to clients
    /// as an empty field and the absence of that capability. Set via
    /// [`MuxrService::with_notify_relay_url`]; a small seam rather than a new
    /// constructor parameter so the existing `with_backends`/`new` call sites
    /// (including every test in `session_ops.rs`/`space_ops.rs`) stay
    /// unchanged.
    notify_relay_url: Option<String>,
    /// Handle to the on-disk push-device registry (`RegisterPushTarget` /
    /// `ListPushTargets` / `RemovePushTarget`). Cheap to clone (see
    /// [`crate::notify::devices::PushDeviceStore`] — every method does its
    /// own fresh read/write, no in-memory cache). Defaults to the standard
    /// data-dir location; overridable via
    /// [`MuxrService::with_push_device_store`] (tests point it at a temp
    /// path instead of the real data dir).
    push_devices: crate::notify::devices::PushDeviceStore,
}

impl Default for MuxrService {
    fn default() -> Self {
        Self::new()
    }
}

impl MuxrService {
    pub fn new() -> Self {
        // Assert that the linked zellij crate matches our expected contract.
        // This catches accidental version drift without waiting for a runtime
        // failure deeper in the IPC layer.
        let linked = zellij_utils::consts::VERSION;
        if linked != ZELLIJ_CONTRACT_VERSION {
            log::warn!(
                "linked zellij version '{linked}' differs from expected \
                 contract version '{ZELLIJ_CONTRACT_VERSION}' — IPC \
                 compatibility not guaranteed"
            );
        } else {
            log::info!("linked zellij contract version verified: {ZELLIJ_CONTRACT_VERSION}");
        }
        Self {
            clients: crate::client_count::SessionClients::new(),
            control: crate::relay::ControlRegistry::default(),
            view_state: crate::relay::ViewStateRegistry::default(),
            backends: crate::multiplexer::BackendSet::single(
                crate::cli::BackendKind::Zellij,
                std::sync::Arc::new(crate::multiplexer::ZellijBackend),
            ),
            notify_relay_url: None,
            push_devices: crate::notify::devices::PushDeviceStore::new(),
        }
    }

    /// Construct a [`MuxrService`] driving the supplied [`BackendSet`].
    ///
    /// Builds the service with the same shared registries as [`MuxrService::new`]
    /// but accepts a whole set of detected backends — used by `cmd_start` in
    /// `bin/muxrd.rs` so one server can serve `zellij` and `herdr` simultaneously
    /// (Phase 3) instead of hard-coding `ZellijBackend`.
    ///
    /// # Zellij version assertion
    ///
    /// [`MuxrService::new`] performs an advisory zellij-contract-version check
    /// (logged as `warn!`/`info!`).  This method intentionally **skips** that
    /// check: it is backend-agnostic, and the zellij assertion is meaningless for
    /// a herdr-only set.  The zellij startup path enforces a hard version gate via
    /// `check_zellij_version()` in `cmd_start` (when zellij is among the detected
    /// backends) before this is ever called.  Full per-backend `VersionInfo`
    /// reporting is T05.
    pub fn with_backends(backends: crate::multiplexer::BackendSet) -> Self {
        Self {
            clients: crate::client_count::SessionClients::new(),
            control: crate::relay::ControlRegistry::default(),
            view_state: crate::relay::ViewStateRegistry::default(),
            backends,
            notify_relay_url: None,
            push_devices: crate::notify::devices::PushDeviceStore::new(),
        }
    }

    /// Set the push-notification relay URL this service advertises on
    /// `GetVersion` (see the `notify_relay_url` field doc). Builder-style so
    /// callers chain it onto [`MuxrService::new`]/[`MuxrService::with_backends`]
    /// without touching either constructor's signature.
    #[must_use]
    pub fn with_notify_relay_url(mut self, notify_relay_url: Option<String>) -> Self {
        self.notify_relay_url = notify_relay_url;
        self
    }

    /// Set the push-device registry handle this service uses for
    /// `RegisterPushTarget`/`ListPushTargets`/`RemovePushTarget` (see the
    /// `push_devices` field doc). Builder-style, chainable with
    /// [`MuxrService::with_notify_relay_url`]. Tests use this to point the
    /// store at a temp path instead of the real data dir.
    #[must_use]
    pub fn with_push_device_store(
        mut self,
        push_devices: crate::notify::devices::PushDeviceStore,
    ) -> Self {
        self.push_devices = push_devices;
        self
    }

    /// Resolve an opaque session `id` (`"<backend>:<bare>"`) to the backend that
    /// owns it plus the bare session name to pass that backend (Option C routing).
    ///
    /// Every **session-scoped** RPC routes through this: it replaces the old
    /// blanket `backend()` (primary) shim so a request lands on the backend that
    /// actually owns the session, in a simultaneous multi-backend deploy. See
    /// [`crate::multiplexer::resolve_session`] for the wire format, error mapping,
    /// back-compat (legacy bare-name) fallback, and the bare-name validation invariant.
    fn resolve_session(
        &self,
        id: &str,
    ) -> Result<(std::sync::Arc<dyn crate::multiplexer::MuxBackend>, String), Status> {
        crate::multiplexer::resolve_session(&self.backends, id)
    }

    /// Returns a clone of the per-session attached-client registry.
    ///
    /// The clone shares the same underlying `DashMap` (via `Arc`), so callers
    /// observe live counts.  Used by the control-socket listener to report
    /// `StatusInfo.client_count` without restructuring the service.
    pub fn clients(&self) -> crate::client_count::SessionClients {
        self.clients.clone()
    }
}

// ─── Tonic trait impl ─────────────────────────────────────────────────────────

#[tonic::async_trait]
impl Muxr for MuxrService {
    // ── GetVersion ──────────────────────────────────────────────────────────

    /// Returns server + linked zellij version.  No auth required.
    async fn get_version(&self, request: Request<Empty>) -> Result<Response<VersionInfo>, Status> {
        self.get_version_impl(request).await
    }

    // ── Login ───────────────────────────────────────────────────────────────

    /// Exchange a long-lived auth token for a short-lived session token.
    ///
    /// The `auth_token` must exist in zellij's shared `tokens.db`.  On
    /// success, returns a `session_token` valid for 5 minutes (or 28 days
    /// if `remember_me` is true).  Pass the session token as
    /// `authorization: Bearer <session_token>` on subsequent calls.
    ///
    /// No bearer auth required on this endpoint (it's the bootstrap step).
    async fn login(
        &self,
        request: Request<LoginRequest>,
    ) -> Result<Response<LoginResponse>, Status> {
        self.login_impl(request).await
    }

    // ── AttachTerminal — bidi relay (B2) ────────────────────────────────────

    type AttachTerminalStream = crate::relay::ServerFrameStream;

    /// Bridge a gRPC bidirectional stream to a zellij IPC attach.
    ///
    /// Requires a valid `authorization: Bearer <session_token>` header
    /// (enforced by the bearer interceptor in [`crate::auth`]).
    ///
    /// The first inbound `ClientFrame` must be an `AttachReq`; from there a
    /// dedicated std thread relays render bytes outbound while a tokio task
    /// pumps input/resize inbound. See [`crate::relay`] for the lifecycle.
    async fn attach_terminal(
        &self,
        request: Request<Streaming<ClientFrame>>,
    ) -> Result<Response<Self::AttachTerminalStream>, Status> {
        self.attach_terminal_impl(request).await
    }

    // ── ListSessions (C1) ───────────────────────────────────────────────────

    /// List all live and resurrectable zellij sessions.
    ///
    /// Uses `zellij_utils::sessions::get_sessions()` + `get_resurrectable_sessions()`.
    /// No IPC connection required — this is a filesystem scan.
    /// Requires bearer auth (read-only is fine).
    async fn list_sessions(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<SessionList>, Status> {
        self.list_sessions_impl(request).await
    }

    // ── GetLayout (C1 + BE-LAYOUT) ─────────────────────────────────────────

    /// Return the typed tab/pane layout for a specific session.
    ///
    /// **B-QUERY (BE-LAYOUT):** when a relay is attached for the session,
    /// the `ListTabs`/`ListPanes` query is routed through the relay's EXISTING
    /// persistent `AttachClient` connection via `RelayControl::QueryLayout`.
    /// This eliminates the ephemeral attach that the old path opened per poll,
    /// which caused:
    ///   - pane-frame flicker (attach/detach churn on every poll)
    ///   - focus/tab desync (per-client union includes the transient client)
    ///
    /// When no relay is attached (e.g. Sessions screen querying a non-active
    /// session), the original ephemeral `query_session` path is used unchanged.
    ///
    /// **B-FOCUS (BE-LAYOUT):** after parsing the query response, `TabMsg.active`
    /// and `PaneMsg.is_focused` are overridden with per-relay-client values from
    /// the [`crate::relay::RelayViewState`] registry when available. This gives single-client
    /// correctness: even with a second desktop client attached (whose active tab
    /// and focused pane pollute the zellij union), the mobile client sees its OWN
    /// active tab and focused pane.
    ///
    /// Requires bearer auth.
    async fn get_layout(&self, request: Request<SessionRef>) -> Result<Response<Layout>, Status> {
        self.get_layout_impl(request).await
    }

    // ── Pane ops (D1) ─────────────────────────────────────────────────────────

    /// Write raw bytes to a specific pane. MUTATING (read-only rejected).
    async fn write_to_pane(
        &self,
        request: Request<WriteToPaneReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.write_to_pane_impl(request).await
    }

    /// Focus a specific pane. Allowed for read-only tokens.
    async fn focus_pane(&self, request: Request<PaneTarget>) -> Result<Response<ProtoAck>, Status> {
        self.focus_pane_impl(request).await
    }

    /// Close a specific pane. MUTATING (read-only rejected).
    async fn close_pane(&self, request: Request<PaneTarget>) -> Result<Response<ProtoAck>, Status> {
        self.close_pane_impl(request).await
    }

    /// Open a new pane; the new pane id surfaces in `ActionAck.info`. MUTATING.
    async fn new_pane(&self, request: Request<NewPaneReq>) -> Result<Response<ProtoAck>, Status> {
        self.new_pane_impl(request).await
    }

    /// Rename a specific pane. MUTATING (read-only rejected).
    async fn rename_pane(
        &self,
        request: Request<RenamePaneReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.rename_pane_impl(request).await
    }

    /// Resize a specific pane. MUTATING (read-only rejected).
    async fn resize_pane(
        &self,
        request: Request<ResizePaneReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.resize_pane_impl(request).await
    }

    /// Toggle a pane between floating and embedded. MUTATING (read-only rejected).
    async fn toggle_pane_floating(
        &self,
        request: Request<PaneTarget>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.toggle_pane_floating_impl(request).await
    }

    /// Toggle fullscreen for a pane. MUTATING (read-only rejected).
    async fn toggle_pane_fullscreen(
        &self,
        request: Request<ToggleFullscreenReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.toggle_pane_fullscreen_impl(request).await
    }

    // ── Tab ops (D2) ──────────────────────────────────────────────────────────

    /// Open a new tab; new tab id/name surface in ActionAck.info. MUTATING.
    async fn new_tab(&self, request: Request<NewTabReq>) -> Result<Response<ProtoAck>, Status> {
        self.new_tab_impl(request).await
    }

    /// Close a tab by id. MUTATING (read-only rejected).
    async fn close_tab(&self, request: Request<TabTarget>) -> Result<Response<ProtoAck>, Status> {
        self.close_tab_impl(request).await
    }

    /// Switch focus to a tab by id. MUTATING (read-only rejected).
    async fn go_to_tab(&self, request: Request<TabTarget>) -> Result<Response<ProtoAck>, Status> {
        self.go_to_tab_impl(request).await
    }

    /// Rename a tab by id. MUTATING (read-only rejected).
    async fn rename_tab(
        &self,
        request: Request<RenameTabReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.rename_tab_impl(request).await
    }

    // ── Scroll (D2) ───────────────────────────────────────────────────────────

    /// Scroll a specific pane. Allowed for read-only tokens.
    async fn scroll_pane(&self, request: Request<ScrollReq>) -> Result<Response<ProtoAck>, Status> {
        self.scroll_pane_impl(request).await
    }

    // ── Session lifecycle (D2) ────────────────────────────────────────────────

    /// Rename the session. MUTATING (read-only rejected).
    async fn rename_session(
        &self,
        request: Request<RenameSessionReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.rename_session_impl(request).await
    }

    /// Kill a named session. MUTATING (read-only rejected).
    ///
    /// Sends `ClientToServerMsg::KillSession` directly (not via send_action —
    /// KillSession is not a zellij Action variant).
    async fn kill_session(
        &self,
        request: Request<SessionRef>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.kill_session_impl(request).await
    }

    /// Create a new detached session. MUTATING (read-only rejected).
    async fn create_session(
        &self,
        request: Request<CreateSessionReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.create_session_impl(request).await
    }

    // ── Token management (Phase F) ────────────────────────────────────────────
    //
    // Thin wrappers over the same `web_authentication_tokens` ops the CLI uses,
    // against zellij's shared tokens.db.  All three are ADMIN-gated: a read-only
    // session token is rejected (`reject_if_read_only`) so observers cannot mint
    // or revoke credentials.  The token DB is shared with real zellij — these
    // operate on the same tokens the `zellij web`/`muxrd` CLI manage.

    /// Create a new auth token. MUTATING (read-only rejected).  The secret is
    /// returned ONCE in `TokenInfo.token`.
    async fn create_token(
        &self,
        request: Request<crate::proto::CreateTokenReq>,
    ) -> Result<Response<crate::proto::TokenInfo>, Status> {
        self.create_token_impl(request).await
    }

    /// List existing auth tokens (metadata only — never the secret).
    /// Read-only rejected (token names are sensitive).
    async fn list_tokens(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<crate::proto::TokenList>, Status> {
        self.list_tokens_impl(request).await
    }

    /// Revoke an auth token by name. MUTATING (read-only rejected).
    async fn revoke_token(
        &self,
        request: Request<crate::proto::RevokeTokenReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.revoke_token_impl(request).await
    }

    // ── Spaces (herdr workspaces) ─────────────────────────────────────────────
    //
    // Spaces are a herdr-only navigation axis (its workspaces). zellij sessions
    // return an empty list (GetSpaces) / a graceful failure ack (the mutating
    // ops). See [`space_ops`] for the routing details. GetSpaces marks the
    // connection-active space from the relay's tracked `current_space`; SwitchSpace
    // is relay-routed; create/rename/close are control-plane (daemon-global).

    /// List the spaces (herdr workspaces) for a session. Read-only allowed.
    async fn get_spaces(
        &self,
        request: Request<SessionRef>,
    ) -> Result<Response<SpaceList>, Status> {
        self.get_spaces_impl(request).await
    }

    /// Switch the connection's relay to a different space. MUTATING (read-only rejected).
    async fn switch_space(
        &self,
        request: Request<SwitchSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.switch_space_impl(request).await
    }

    /// Create a new space (herdr workspace). MUTATING (read-only rejected).
    async fn create_space(
        &self,
        request: Request<CreateSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.create_space_impl(request).await
    }

    /// Rename an existing space. MUTATING (read-only rejected).
    async fn rename_space(
        &self,
        request: Request<RenameSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.rename_space_impl(request).await
    }

    /// Close (delete) a space. MUTATING (read-only rejected).
    async fn close_space(
        &self,
        request: Request<CloseSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.close_space_impl(request).await
    }

    // ── Push-notification device registry ───────────────────────────────────
    //
    // ADMIN-gated (read-only rejected) — see `push_ops` for the store and
    // validation. RegisterPushTarget additionally rejects when the server has
    // no relay configured.

    /// Register (or refresh) a device's push handle. MUTATING (read-only rejected).
    async fn register_push_target(
        &self,
        request: Request<RegisterPushTargetReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.register_push_target_impl(request).await
    }

    /// List registered devices (metadata + handle prefix only). Read-only rejected.
    async fn list_push_targets(
        &self,
        request: Request<ListPushTargetsReq>,
    ) -> Result<Response<ListPushTargetsResp>, Status> {
        self.list_push_targets_impl(request).await
    }

    /// Remove a registered device by name. MUTATING (read-only rejected).
    async fn remove_push_target(
        &self,
        request: Request<RemovePushTargetReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        self.remove_push_target_impl(request).await
    }
}
