//! Session lifecycle RPC implementations: list, attach, rename, kill, create.

use tonic::{Request, Response, Status, Streaming};

use crate::proto::{
    ActionAck as ProtoAck, ClientFrame, CreateSessionReq, Empty, RenameSessionReq, SessionList,
    SessionRef,
};

use crate::multiplexer::{make_id, validate_session};

use super::MuxrService;
use super::helpers::{
    kind_from_proto, proto_backend, reject_if_read_only, run_action, validate_layout_name,
};

/// Per-backend ceiling for the concurrent ListSessions fan-out (S-M3).
///
/// Must be strictly above herdr's control-socket `READ_TIMEOUT` (3 s) so a
/// well-behaved-but-slow backend can still complete within the budget, while
/// bounding a truly hung one. Value mirrors `VERSION_CHECK_TIMEOUT` /
/// `RECV_TIMEOUT` in the codebase (5 s = conservative local-IPC budget).
const LIST_SESSIONS_BACKEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl MuxrService {
    // ── ListSessions (C1) ───────────────────────────────────────────────────

    pub(super) async fn list_sessions_impl(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<SessionList>, Status> {
        log::debug!("ListSessions: scanning session sockets across all backends");

        // Phase 3: fan ListSessions across EVERY backend this server drives and
        // merge the results. Each session is tagged with its owning backend and
        // assigned an opaque, backend-qualified `id` (`<backend>:<bare>`, via
        // `make_id`) that round-trips through `resolve_session`. A backend that
        // errors mid-run is skipped (warn-logged) rather than failing the whole
        // RPC — one unreachable backend must not blind the client to the other.
        //
        // `connected_clients` is sourced from this server's own relay registry
        // (cheap, no IPC), keyed by the SAME opaque `id` the relay's attach site
        // counts under — so two same-name sessions on different backends
        // (`zellij:dev` + `herdr:dev`) no longer share one count bucket. The
        // layout-derived enrichment fields (tab_count/pane_count/has_bell/
        // is_current) are left at their zero defaults here: filling them requires
        // a per-session layout query whose transient AttachClient can briefly
        // resize the session (Phase F G4b). Clients fall back to GetLayout.
        //
        // S-M2: concurrent fan-out — all backend tasks are spawned up-front and
        // joined together via `futures::future::join_all` so latency =
        // max(per-backend) not Σ.
        // S-M3: each backend query is wrapped in `tokio::time::timeout` so a hung
        // (non-erroring) backend (e.g. herdr 3 s control-socket READ_TIMEOUT)
        // can't stall the whole RPC; on timeout → warn + skip (partial results).
        // S-M4: a JoinError (task panic or cancellation) degrades to warn + skip,
        // same as a normal `Err`, rather than propagating via `?` and failing the
        // entire ListSessions RPC.
        let clients = self.clients.clone();

        // Build concurrent futures — one per backend, timeout-wrapped. The
        // `spawn_blocking` keeps the blocking IPC call off the async executor
        // (CODE_STANDARDS: never `.await` blocking IPC directly).
        // The backend Arc is carried through in the result tuple so it can be
        // used in the merge phase for the herdr space_count pre-fetch.
        let backend_futures: Vec<_> = self
            .backends
            .iter()
            .map(|(kind, backend)| {
                let backend = backend.clone();
                async move {
                    let result = tokio::time::timeout(
                        LIST_SESSIONS_BACKEND_TIMEOUT,
                        tokio::task::spawn_blocking({
                            let backend = backend.clone();
                            move || backend.list_sessions_with_resurrectables()
                        }),
                    )
                    .await;
                    (kind, backend, result)
                }
            })
            .collect();

        // Await all backends concurrently; latency = max(per-backend).
        let backend_results = futures::future::join_all(backend_futures).await;

        // Merge sessions from all backends that succeeded within the timeout.
        let mut proto_sessions: Vec<crate::proto::SessionInfo> = Vec::new();
        for (kind, backend, result) in backend_results {
            let sessions = match result {
                // S-M3: timeout — warn + skip, return whatever other backends had.
                Err(_elapsed) => {
                    log::warn!(
                        "backend {kind:?} timed out after {:?} in ListSessions, skipping",
                        LIST_SESSIONS_BACKEND_TIMEOUT
                    );
                    continue;
                }
                // S-M4: spawn_blocking task panicked or was cancelled — warn + skip.
                Ok(Err(join_err)) => {
                    log::warn!(
                        "backend {kind:?} task panicked in ListSessions: {join_err}, skipping"
                    );
                    continue;
                }
                // Mid-run-unreachable tolerance: skip this backend, keep the
                // others. The client still sees every reachable backend's sessions.
                Ok(Ok(Err(e))) => {
                    log::warn!("backend {kind:?} unreachable, skipping in ListSessions: {e:#}");
                    continue;
                }
                // Happy path: backend returned a session list.
                Ok(Ok(Ok(sessions))) => sessions,
            };

            // For herdr, pre-fetch the space count in a single spawn_blocking
            // call. herdr collapses to one session (the HERDR_SESSION sentinel),
            // so this is at most one extra workspace.list round-trip per
            // ListSessions — a second call on top of the list_sessions probe,
            // intentional because list_sessions doesn't surface workspace count.
            // The single-session assumption holds as long as herdr yields exactly
            // one session (HERDR_SESSION); space_count is therefore per-backend,
            // not per-session, and is meaningful only for the herdr entry.
            // For every other backend herdr_space_count is 0 (encoded by the
            // outer Herdr branch, so the per-session assignment below needs no
            // redundant backend re-check).
            //
            // SM1 fix: the spawn_blocking is wrapped in the same
            // LIST_SESSIONS_BACKEND_TIMEOUT budget as the session-list fan-out
            // above (S-M3 invariant). Three degrade arms — Err (IPC error),
            // JoinError (task panic/cancel), Elapsed (timeout) — all log a
            // warning and fall back to 0; ListSessions must never hang or abort
            // on this path.
            let herdr_space_count: u32 = if kind == crate::cli::BackendKind::Herdr {
                // herdr ignores the session argument (single daemon), but we pass
                // the canonical sentinel name for clarity.
                let b = backend.clone();
                match tokio::time::timeout(
                    LIST_SESSIONS_BACKEND_TIMEOUT,
                    tokio::task::spawn_blocking(move || {
                        b.list_spaces(crate::multiplexer::HERDR_SESSION)
                    }),
                )
                .await
                {
                    Ok(Ok(Ok(spaces))) => u32::try_from(spaces.len()).unwrap_or(u32::MAX),
                    Ok(Ok(Err(e))) => {
                        log::warn!(
                            "ListSessions: list_spaces failed for herdr, \
                             defaulting space_count to 0: {e:#}"
                        );
                        0
                    }
                    Ok(Err(e)) => {
                        log::warn!(
                            "ListSessions: list_spaces task panicked for herdr, \
                             defaulting space_count to 0: {e}"
                        );
                        0
                    }
                    Err(_elapsed) => {
                        log::warn!(
                            "ListSessions: list_spaces timed out after {:?} for herdr, \
                             defaulting space_count to 0",
                            LIST_SESSIONS_BACKEND_TIMEOUT
                        );
                        0
                    }
                }
            } else {
                0
            };

            let proto_kind = proto_backend(kind) as i32;
            for (name, age_secs, resurrectable) in sessions {
                let id = make_id(kind, &name);
                let connected_clients = clients.count(&id);
                // space_count is per-backend (not per-session): herdr exposes its
                // workspace count on the single collapsed session; zellij = 0.
                // The 0-for-non-herdr invariant is encoded by the outer Herdr
                // branch above (herdr_space_count is always 0 for non-herdr
                // backends), so no redundant backend re-check is needed here.
                proto_sessions.push(crate::proto::SessionInfo {
                    name,
                    age_secs,
                    resurrectable,
                    tab_count: 0,
                    pane_count: 0,
                    has_bell: false,
                    is_current: false,
                    connected_clients,
                    backend: proto_kind,
                    id,
                    space_count: herdr_space_count,
                });
            }
        }

        let count = proto_sessions.len();
        log::info!("ListSessions: returning {count} session(s) across all backends");
        Ok(Response::new(SessionList {
            sessions: proto_sessions,
        }))
    }

    // ── AttachTerminal — bidi relay (B2) ────────────────────────────────────

    pub(super) async fn attach_terminal_impl(
        &self,
        request: Request<Streaming<ClientFrame>>,
    ) -> Result<Response<crate::relay::ServerFrameStream>, Status> {
        // ── Major A: enforce the read-only flag on the terminal stream ────────
        // A read-only token must NOT be able to inject input.  Policy:
        // **render-only** — a read-only attach still receives the live render
        // stream (good UX for observers), and its own `Resize` frames ARE
        // applied (geometry is the viewer's own viewport, not session
        // content — see `crate::relay::attach_relay`); only `Input` frames
        // and non-wheel `Mouse` frames are dropped, and routed controls
        // (focus/tab/space, fullscreen) are gated by `read_only_denies`
        // (`relay/inbound.rs`).  (We deliberately do not reject the whole
        // stream so RO viewers can still watch.)
        let read_only = request
            .extensions()
            .get::<crate::auth::SessionReadOnly>()
            .map(|ro| ro.0)
            .unwrap_or(true); // fail closed if somehow unset
        log::debug!("AttachTerminal: session read_only={read_only}");

        // ── Major H: stash the token so the relay can re-validate it mid-stream.
        // If the extension is missing (shouldn't happen post-auth) we still
        // proceed but with no token → the relay treats that as already-revoked
        // on its first re-check and tears the stream down.
        let token = request
            .extensions()
            .get::<crate::auth::SessionToken>()
            .map(|t| t.0.clone());

        let inbound = request.into_inner();
        // Option C: the relay resolves the opaque session id in the first
        // `AttachReq` frame against the whole backend set (the id isn't known
        // until that frame is read), so pass the full set rather than a single
        // primary backend.
        let stream = crate::relay::attach_relay(
            inbound,
            read_only,
            token,
            self.clients.clone(),
            self.control.clone(),
            self.view_state.clone(),
            self.backends.clone(),
        )
        .await?;
        Ok(Response::new(stream))
    }

    // ── Session lifecycle (D2) ────────────────────────────────────────────────

    /// Rename the session. MUTATING (read-only rejected).
    pub(super) async fn rename_session_impl(
        &self,
        request: Request<RenameSessionReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "RenameSession")?;
        let req = request.into_inner();
        let (backend, session) = self.resolve_session(&req.session)?;
        // The new name becomes a session name too — validate it the same way.
        // (It is a bare name, not an opaque id: the renamed session stays on the
        // same backend, so only the bare name is supplied/validated here.)
        validate_session(&req.name)?;
        let new_name = req.name;
        log::info!("RenameSession: session='{session}' → '{new_name}'");
        run_action("RenameSession", move || {
            backend.rename_session(&session, new_name)
        })
        .await
    }

    /// Kill a named session. MUTATING (read-only rejected).
    ///
    /// Sends `ClientToServerMsg::KillSession` directly (not via send_action —
    /// KillSession is not a zellij Action variant).
    pub(super) async fn kill_session_impl(
        &self,
        request: Request<SessionRef>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "KillSession")?;
        let req = request.into_inner();
        // Decision 5 (S-M3): the herdr session is singular and collapsed — it has no
        // killable session object (manage its workspaces via CloseSpace). Reject
        // cleanly with `invalid_argument` BEFORE resolving, so the client sees a
        // meaningful error instead of the backend's opaque "no workspace with label
        // 'herdr'" internal error. zellij sessions are never collapsed, so this never
        // affects a zellij KillSession (e.g. a zellij session literally named
        // "herdr" resolves as `zellij:herdr`, which is not a collapsed id).
        if crate::multiplexer::is_collapsed_backend_session(&req.session) {
            return Err(Status::invalid_argument(
                "the herdr session cannot be killed — manage its workspaces via CloseSpace",
            ));
        }
        let (backend, session) = self.resolve_session(&req.session)?;
        log::info!("KillSession: session='{session}'");
        // Routed through `run_action` like every other action RPC: a backend that
        // DECLINES the kill (herdr refusing a worktree-group primary, or the last
        // remaining space) answers `Ok(Ack{ok:false})`, and only an IPC or join
        // failure becomes a `Status`. Collapsing a refusal into `Status::internal`
        // would contradict the error-handling contract and mis-report a deliberate
        // refusal as a reachable bug.
        run_action("KillSession", move || backend.kill_session(&session)).await
    }

    /// Create a new detached session. MUTATING (read-only rejected).
    pub(super) async fn create_session_impl(
        &self,
        request: Request<CreateSessionReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "CreateSession")?;
        let req = request.into_inner();

        // ── Phase 3: route the create to the requested backend ────────────────
        // `req.backend` (proto Backend) selects which multiplexer owns the new
        // session. Resolution rules:
        //   - a named backend that is running        → use it
        //   - a named backend that is NOT running    → InvalidArgument
        //   - BACKEND_UNSPECIFIED + exactly 1 backend → default to it (ergonomics)
        //   - BACKEND_UNSPECIFIED + >1 backend       → InvalidArgument (ambiguous)
        let req_backend = crate::proto::Backend::try_from(req.backend).map_err(|_| {
            Status::invalid_argument(format!(
                "CreateSession: unknown backend tag {}",
                req.backend
            ))
        })?;
        let kind = match kind_from_proto(req_backend) {
            Some(kind) => kind,
            None => {
                // Unspecified: default only when there is a single unambiguous backend.
                let mut kinds = self.backends.kinds();
                let sole = kinds.next();
                if kinds.next().is_some() {
                    return Err(Status::invalid_argument(
                        "CreateSession: this server runs multiple backends — specify which \
                         backend to create the session on",
                    ));
                }
                sole.expect("BackendSet invariant: at least one backend")
            }
        };
        // Error-code mapping (matches `resolve_session` — see `multiplexer::routing`):
        //   recognised kind but not running on this server → NotFound (client re-lists).
        let backend = self.backends.get(kind).cloned().ok_or_else(|| {
            Status::not_found(format!(
                "CreateSession: backend '{kind}' is recognised but not running on this \
                 server — re-list to see available backends"
            ))
        })?;

        validate_session(&req.name)?;
        let name = req.name;

        // ── Major I: constrain --layout to a simple layout NAME ───────────────
        // An arbitrary client-supplied --layout path is forwarded to `zellij`,
        // which would happily load an attacker-controlled layout file — and
        // zellij layouts can carry `command`/`run` directives → host code
        // execution.  We therefore accept only a bare layout *name* (the same
        // [A-Za-z0-9_-] allowlist as session names): no absolute path, no `/`,
        // no `..`.  zellij resolves a bare name against its own layout dir.
        //
        // When the client supplies NO layout (the normal case — the Muxr
        // app always sends an empty layout), `actions::create_session` defaults
        // to the bundled BAR-LESS layout so app-created sessions hide zellij's
        // tab-bar/status-bar (the app renders those controls). That default is a
        // server-authored file at a fixed path — not client input — so it is
        // exempt from this name allowlist.
        let layout = if req.layout.is_empty() {
            None
        } else {
            validate_layout_name(&req.layout)?;
            Some(req.layout)
        };
        log::info!("CreateSession: backend='{kind}' name='{name}' layout={layout:?}");
        run_action("CreateSession", move || {
            backend.create_session(&name, layout)
        })
        .await
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Tests for the concurrent ListSessions fan-out + resilience paths (S-M2/S-M3/S-M4).
    //!
    //! Uses fake `MuxBackend` implementations to exercise:
    //!   - An erroring backend → skipped, other backend's sessions returned (Err path).
    //!   - A panicking backend → skipped via JoinError (panic/cancel path, S-M4).
    //!   - Two healthy backends → sessions from both merged.
    //!   - All backends erroring → empty list (no RPC error).

    use std::sync::Arc;
    use std::time::Duration;

    use tonic::Request;

    use crate::cli::BackendKind;
    use crate::grpc::MuxrService;
    use crate::multiplexer::{
        ActionAck, BackendSet, DualHandle, LayoutSnapshot, MuxBackend, PaneRef, ResizeDir,
        ResizeKind, ScrollDir, SpaceSnapshot,
    };
    use crate::proto::Empty;

    // ── Fake MuxBackend implementations ──────────────────────────────────────

    /// Returns a fixed session list from `list_sessions_with_resurrectables`.
    #[derive(Debug)]
    struct StubBackend {
        sessions: Vec<(String, u64, bool)>,
    }

    impl MuxBackend for StubBackend {
        fn list_sessions(&self) -> anyhow::Result<Vec<(String, Duration)>> {
            Ok(self
                .sessions
                .iter()
                .map(|(n, a, _)| (n.clone(), Duration::from_secs(*a)))
                .collect())
        }

        fn list_sessions_with_resurrectables(&self) -> anyhow::Result<Vec<(String, u64, bool)>> {
            Ok(self.sessions.clone())
        }

        fn validate_session_name(&self, _: &str) -> Result<(), String> {
            unimplemented!()
        }
        fn create_session(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn kill_session(&self, _: &str) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_session(&self, _: &str, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn write_to_pane(&self, _: &str, _: PaneRef, _: Vec<u8>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn focus_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_pane(&self, _: &str, _: bool, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_pane(&self, _: &str, _: PaneRef, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn resize_pane(
            &self,
            _: &str,
            _: PaneRef,
            _: ResizeKind,
            _: Option<ResizeDir>,
        ) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_floating(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_fullscreen(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn scroll_pane(&self, _: &str, _: PaneRef, _: ScrollDir) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_tab(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn go_to_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_tab(&self, _: &str, _: u64, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn query_layout(&self, _: &str) -> anyhow::Result<LayoutSnapshot> {
            unimplemented!()
        }
        fn query_session_size(&self, _: &str) -> anyhow::Result<(u16, u16)> {
            unimplemented!()
        }
        fn pane_is_floating_with_visibility(
            &self,
            _: &str,
            _: PaneRef,
        ) -> anyhow::Result<(bool, bool, Option<PaneRef>)> {
            unimplemented!()
        }
        fn open_attach(&self, _: &str, _: u16, _: u16, _: bool) -> anyhow::Result<DualHandle> {
            unimplemented!()
        }
        fn backend_version(&self) -> String {
            "stub".to_string()
        }
    }

    /// Always errors from `list_sessions_with_resurrectables` (simulates an
    /// unreachable backend — e.g. herdr socket not present).
    #[derive(Debug)]
    struct ErrorBackend;

    impl MuxBackend for ErrorBackend {
        fn list_sessions(&self) -> anyhow::Result<Vec<(String, Duration)>> {
            anyhow::bail!("connection refused")
        }
        fn list_sessions_with_resurrectables(&self) -> anyhow::Result<Vec<(String, u64, bool)>> {
            anyhow::bail!("connection refused")
        }
        fn validate_session_name(&self, _: &str) -> Result<(), String> {
            unimplemented!()
        }
        fn create_session(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn kill_session(&self, _: &str) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_session(&self, _: &str, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn write_to_pane(&self, _: &str, _: PaneRef, _: Vec<u8>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn focus_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_pane(&self, _: &str, _: bool, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_pane(&self, _: &str, _: PaneRef, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn resize_pane(
            &self,
            _: &str,
            _: PaneRef,
            _: ResizeKind,
            _: Option<ResizeDir>,
        ) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_floating(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_fullscreen(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn scroll_pane(&self, _: &str, _: PaneRef, _: ScrollDir) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_tab(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn go_to_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_tab(&self, _: &str, _: u64, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn query_layout(&self, _: &str) -> anyhow::Result<LayoutSnapshot> {
            unimplemented!()
        }
        fn query_session_size(&self, _: &str) -> anyhow::Result<(u16, u16)> {
            unimplemented!()
        }
        fn pane_is_floating_with_visibility(
            &self,
            _: &str,
            _: PaneRef,
        ) -> anyhow::Result<(bool, bool, Option<PaneRef>)> {
            unimplemented!()
        }
        fn open_attach(&self, _: &str, _: u16, _: u16, _: bool) -> anyhow::Result<DualHandle> {
            unimplemented!()
        }
        fn backend_version(&self) -> String {
            "error".to_string()
        }
    }

    /// Panics inside `list_sessions_with_resurrectables` to exercise S-M4
    /// (JoinError tolerance — a panic inside `spawn_blocking` surfaces as a
    /// JoinError rather than an anyhow Err).
    #[derive(Debug)]
    struct PanickingBackend;

    impl MuxBackend for PanickingBackend {
        fn list_sessions(&self) -> anyhow::Result<Vec<(String, Duration)>> {
            panic!("simulated backend panic")
        }
        fn list_sessions_with_resurrectables(&self) -> anyhow::Result<Vec<(String, u64, bool)>> {
            panic!("simulated backend panic")
        }
        fn validate_session_name(&self, _: &str) -> Result<(), String> {
            unimplemented!()
        }
        fn create_session(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn kill_session(&self, _: &str) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_session(&self, _: &str, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn write_to_pane(&self, _: &str, _: PaneRef, _: Vec<u8>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn focus_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_pane(&self, _: &str, _: bool, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_pane(&self, _: &str, _: PaneRef, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn resize_pane(
            &self,
            _: &str,
            _: PaneRef,
            _: ResizeKind,
            _: Option<ResizeDir>,
        ) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_floating(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_fullscreen(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn scroll_pane(&self, _: &str, _: PaneRef, _: ScrollDir) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_tab(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn go_to_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_tab(&self, _: &str, _: u64, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn query_layout(&self, _: &str) -> anyhow::Result<LayoutSnapshot> {
            unimplemented!()
        }
        fn query_session_size(&self, _: &str) -> anyhow::Result<(u16, u16)> {
            unimplemented!()
        }
        fn pane_is_floating_with_visibility(
            &self,
            _: &str,
            _: PaneRef,
        ) -> anyhow::Result<(bool, bool, Option<PaneRef>)> {
            unimplemented!()
        }
        fn open_attach(&self, _: &str, _: u16, _: u16, _: bool) -> anyhow::Result<DualHandle> {
            unimplemented!()
        }
        fn backend_version(&self) -> String {
            "panic".to_string()
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// S-M4 (Err path): an erroring backend is skipped; the reachable backend's
    /// sessions are returned. The RPC must not fail.
    #[tokio::test]
    async fn list_sessions_skips_erroring_backend_returns_other() {
        let error_backend: Arc<dyn MuxBackend> = Arc::new(ErrorBackend);
        let stub_backend: Arc<dyn MuxBackend> = Arc::new(StubBackend {
            sessions: vec![("my-session".to_string(), 100, false)],
        });
        let backends = BackendSet::new(vec![
            (BackendKind::Zellij, error_backend),
            (BackendKind::Herdr, stub_backend),
        ]);
        let service = MuxrService::with_backends(backends);

        let sessions = service
            .list_sessions_impl(Request::new(Empty {}))
            .await
            .expect("RPC must not error when one backend is reachable")
            .into_inner()
            .sessions;

        assert_eq!(sessions.len(), 1, "only the reachable backend's session");
        assert_eq!(sessions[0].name, "my-session");
        assert_eq!(sessions[0].id, "herdr:my-session");
    }

    /// S-M4 (JoinError path): a panicking backend's `spawn_blocking` task yields
    /// a JoinError; the healthy backend's sessions must still be returned and the
    /// RPC must not propagate the panic as an error.
    #[tokio::test]
    async fn list_sessions_tolerates_panicking_backend() {
        let panicking: Arc<dyn MuxBackend> = Arc::new(PanickingBackend);
        let stub: Arc<dyn MuxBackend> = Arc::new(StubBackend {
            sessions: vec![("live-session".to_string(), 0, false)],
        });
        let backends = BackendSet::new(vec![
            (BackendKind::Zellij, panicking),
            (BackendKind::Herdr, stub),
        ]);
        let service = MuxrService::with_backends(backends);

        let sessions = service
            .list_sessions_impl(Request::new(Empty {}))
            .await
            .expect("RPC must not propagate a backend panic as an error")
            .into_inner()
            .sessions;

        assert_eq!(
            sessions.len(),
            1,
            "non-panicking backend's session survives"
        );
        assert_eq!(sessions[0].name, "live-session");
    }

    /// S-M2 (concurrent fan-out, happy path): both backends succeed; sessions from
    /// both are merged. The opaque `id` fields carry the correct backend prefix.
    #[tokio::test]
    async fn list_sessions_both_backends_ok_returns_all() {
        let zellij_backend: Arc<dyn MuxBackend> = Arc::new(StubBackend {
            sessions: vec![("zellij-session".to_string(), 0, false)],
        });
        let herdr_backend: Arc<dyn MuxBackend> = Arc::new(StubBackend {
            sessions: vec![("herdr-session".to_string(), 0, false)],
        });
        let backends = BackendSet::new(vec![
            (BackendKind::Zellij, zellij_backend),
            (BackendKind::Herdr, herdr_backend),
        ]);
        let service = MuxrService::with_backends(backends);

        let sessions = service
            .list_sessions_impl(Request::new(Empty {}))
            .await
            .expect("RPC must succeed when both backends are reachable")
            .into_inner()
            .sessions;

        assert_eq!(sessions.len(), 2, "sessions from both backends merged");
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"zellij:zellij-session"));
        assert!(ids.contains(&"herdr:herdr-session"));
    }

    /// All backends error → RPC still succeeds with an empty list (not an error).
    #[tokio::test]
    async fn list_sessions_all_erroring_returns_empty() {
        let backends = BackendSet::new(vec![
            (
                BackendKind::Zellij,
                Arc::new(ErrorBackend) as Arc<dyn MuxBackend>,
            ),
            (
                BackendKind::Herdr,
                Arc::new(ErrorBackend) as Arc<dyn MuxBackend>,
            ),
        ]);
        let service = MuxrService::with_backends(backends);

        let sessions = service
            .list_sessions_impl(Request::new(Empty {}))
            .await
            .expect("RPC must not error even when all backends fail")
            .into_inner()
            .sessions;

        assert!(sessions.is_empty(), "no reachable backends → empty list");
    }

    // ── Additional stubs for space_count tests (SM1 fix) ─────────────────────

    /// A herdr-kind backend that succeeds at `list_sessions_with_resurrectables`
    /// AND returns N `SpaceSnapshot`s from `list_spaces`. Used to verify that
    /// `space_count` is correctly populated on the herdr session entry and that
    /// co-listed zellij sessions remain at 0.
    #[derive(Debug)]
    struct HerdrSpacesBackend {
        sessions: Vec<(String, u64, bool)>,
        spaces: Vec<SpaceSnapshot>,
    }

    impl MuxBackend for HerdrSpacesBackend {
        fn list_sessions(&self) -> anyhow::Result<Vec<(String, Duration)>> {
            Ok(self
                .sessions
                .iter()
                .map(|(n, a, _)| (n.clone(), Duration::from_secs(*a)))
                .collect())
        }
        fn list_sessions_with_resurrectables(&self) -> anyhow::Result<Vec<(String, u64, bool)>> {
            Ok(self.sessions.clone())
        }
        fn list_spaces(&self, _session: &str) -> anyhow::Result<Vec<SpaceSnapshot>> {
            Ok(self.spaces.clone())
        }
        fn validate_session_name(&self, _: &str) -> Result<(), String> {
            unimplemented!()
        }
        fn create_session(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn kill_session(&self, _: &str) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_session(&self, _: &str, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn write_to_pane(&self, _: &str, _: PaneRef, _: Vec<u8>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn focus_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_pane(&self, _: &str, _: bool, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_pane(&self, _: &str, _: PaneRef, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn resize_pane(
            &self,
            _: &str,
            _: PaneRef,
            _: ResizeKind,
            _: Option<ResizeDir>,
        ) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_floating(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_fullscreen(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn scroll_pane(&self, _: &str, _: PaneRef, _: ScrollDir) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_tab(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn go_to_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_tab(&self, _: &str, _: u64, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn query_layout(&self, _: &str) -> anyhow::Result<LayoutSnapshot> {
            unimplemented!()
        }
        fn query_session_size(&self, _: &str) -> anyhow::Result<(u16, u16)> {
            unimplemented!()
        }
        fn pane_is_floating_with_visibility(
            &self,
            _: &str,
            _: PaneRef,
        ) -> anyhow::Result<(bool, bool, Option<PaneRef>)> {
            unimplemented!()
        }
        fn open_attach(&self, _: &str, _: u16, _: u16, _: bool) -> anyhow::Result<DualHandle> {
            unimplemented!()
        }
        fn backend_version(&self) -> String {
            "herdr-spaces-stub".to_string()
        }
    }

    /// A backend that succeeds at `list_sessions_with_resurrectables` but returns
    /// `Err` from `list_spaces`. Used to verify the SM1 Err degrade arm.
    #[derive(Debug)]
    struct ErrorSpacesBackend {
        sessions: Vec<(String, u64, bool)>,
    }

    impl MuxBackend for ErrorSpacesBackend {
        fn list_sessions(&self) -> anyhow::Result<Vec<(String, Duration)>> {
            Ok(self
                .sessions
                .iter()
                .map(|(n, a, _)| (n.clone(), Duration::from_secs(*a)))
                .collect())
        }
        fn list_sessions_with_resurrectables(&self) -> anyhow::Result<Vec<(String, u64, bool)>> {
            Ok(self.sessions.clone())
        }
        fn list_spaces(&self, _session: &str) -> anyhow::Result<Vec<SpaceSnapshot>> {
            anyhow::bail!("herdr control socket not available")
        }
        fn validate_session_name(&self, _: &str) -> Result<(), String> {
            unimplemented!()
        }
        fn create_session(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn kill_session(&self, _: &str) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_session(&self, _: &str, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn write_to_pane(&self, _: &str, _: PaneRef, _: Vec<u8>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn focus_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_pane(&self, _: &str, _: bool, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_pane(&self, _: &str, _: PaneRef, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn resize_pane(
            &self,
            _: &str,
            _: PaneRef,
            _: ResizeKind,
            _: Option<ResizeDir>,
        ) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_floating(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_fullscreen(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn scroll_pane(&self, _: &str, _: PaneRef, _: ScrollDir) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_tab(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn go_to_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_tab(&self, _: &str, _: u64, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn query_layout(&self, _: &str) -> anyhow::Result<LayoutSnapshot> {
            unimplemented!()
        }
        fn query_session_size(&self, _: &str) -> anyhow::Result<(u16, u16)> {
            unimplemented!()
        }
        fn pane_is_floating_with_visibility(
            &self,
            _: &str,
            _: PaneRef,
        ) -> anyhow::Result<(bool, bool, Option<PaneRef>)> {
            unimplemented!()
        }
        fn open_attach(&self, _: &str, _: u16, _: u16, _: bool) -> anyhow::Result<DualHandle> {
            unimplemented!()
        }
        fn backend_version(&self) -> String {
            "error-spaces".to_string()
        }
    }

    // ── SM1 fix tests ─────────────────────────────────────────────────────────

    /// SM1 (happy path): a herdr backend returning N spaces yields
    /// `space_count == N` on its session entry; a co-listed zellij session keeps
    /// `space_count == 0`. Confirms the redundant backend re-check is gone and
    /// herdr_space_count flows directly into the struct.
    #[tokio::test]
    async fn herdr_space_count_populated_and_zellij_zero() {
        let zellij_backend: Arc<dyn MuxBackend> = Arc::new(StubBackend {
            sessions: vec![("zellij-session".to_string(), 0, false)],
        });
        let herdr_backend: Arc<dyn MuxBackend> = Arc::new(HerdrSpacesBackend {
            sessions: vec![("herdr".to_string(), 0, false)],
            spaces: vec![
                SpaceSnapshot {
                    id: "ws1".to_string(),
                    name: "Work".to_string(),
                    active: true,
                },
                SpaceSnapshot {
                    id: "ws2".to_string(),
                    name: "Play".to_string(),
                    active: false,
                },
            ],
        });
        let backends = BackendSet::new(vec![
            (BackendKind::Zellij, zellij_backend),
            (BackendKind::Herdr, herdr_backend),
        ]);
        let service = MuxrService::with_backends(backends);

        let sessions = service
            .list_sessions_impl(Request::new(Empty {}))
            .await
            .expect("RPC must succeed")
            .into_inner()
            .sessions;

        assert_eq!(sessions.len(), 2);
        let herdr_s = sessions
            .iter()
            .find(|s| s.id.starts_with("herdr:"))
            .expect("herdr session must be present");
        let zellij_s = sessions
            .iter()
            .find(|s| s.id.starts_with("zellij:"))
            .expect("zellij session must be present");
        assert_eq!(
            herdr_s.space_count, 2,
            "herdr space_count populated from list_spaces"
        );
        assert_eq!(zellij_s.space_count, 0, "zellij space_count is always 0");
    }

    /// SM1 (Err degrade arm): a herdr backend whose `list_spaces` returns `Err`
    /// must not abort ListSessions; `space_count` degrades to 0.
    /// Note: the Elapsed arm (timeout) is structurally identical — both warn-log
    /// and return 0 — so the Err path exercises the same degrade code shape.
    #[tokio::test]
    async fn herdr_space_count_degrades_on_list_spaces_error() {
        let herdr_backend: Arc<dyn MuxBackend> = Arc::new(ErrorSpacesBackend {
            sessions: vec![("herdr".to_string(), 0, false)],
        });
        let backends = BackendSet::new(vec![(BackendKind::Herdr, herdr_backend)]);
        let service = MuxrService::with_backends(backends);

        let sessions = service
            .list_sessions_impl(Request::new(Empty {}))
            .await
            .expect("RPC must succeed even when list_spaces errors")
            .into_inner()
            .sessions;

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].space_count, 0,
            "list_spaces Err → degrade to 0, no RPC abort"
        );
    }
}
