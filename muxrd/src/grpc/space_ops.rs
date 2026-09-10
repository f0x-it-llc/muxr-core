//! Space (herdr workspace) RPC implementations: get / switch / create / rename / close.
//!
//! Spaces are a herdr-only navigation axis (its workspaces, surfaced as in-place
//! switchable sub-navigation within the single collapsed herdr session). zellij —
//! and any backend without a space concept — returns the empty list for `GetSpaces`
//! and a graceful failure ack for the mutating ops (the [`MuxBackend`] /
//! [`MuxSender`] defaults flow through unchanged; no special-casing here).
//!
//! Routing:
//! - **GetSpaces** is a read: it resolves the owning backend, lists its spaces, and
//!   marks the **connection-active** space using the relay's tracked
//!   `current_space` (per-connection view; see [`RelayViewState`]). With no relay,
//!   it falls back to the backend-reported active.
//! - **SwitchSpace** is relay-routed (like `GetLayout`/`GoToTab`): it sends
//!   [`RelayControl::SwitchSpace`] to the connection's relay and awaits the oneshot
//!   ack — the relay re-points its wire stream at the target workspace with no
//!   daemon-global focus change.
//! - **CreateSpace / RenameSpace / CloseSpace** are control-plane: they mutate the
//!   daemon's globally-shared workspaces directly through the backend (spaces are
//!   daemon-global objects). After a create the client issues GetSpaces +
//!   SwitchSpace.
//!
//! [`MuxBackend`]: crate::multiplexer::MuxBackend
//! [`MuxSender`]: crate::multiplexer::MuxSender
//! [`RelayViewState`]: crate::relay::RelayViewState

use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::actions::ActionAck;
use crate::multiplexer::{MuxBackend, SpaceSnapshot};
use crate::proto::{
    ActionAck as ProtoAck, CloseSpaceReq, CreateSpaceReq, RenameSpaceReq, SessionRef, Space,
    SpaceList, SwitchSpaceReq,
};
use crate::relay::RelayControl;

use super::MuxrService;
use super::helpers::{reject_if_read_only, short_conn};

/// Max length (bytes) accepted for a user-supplied space label.
const MAX_SPACE_LABEL_LEN: usize = 64;

/// Max length (bytes) accepted for an opaque space (herdr workspace) id.
const MAX_SPACE_ID_LEN: usize = 128;

/// Timeout for the oneshot reply when routing a `SwitchSpace` through the relay.
///
/// Mirrors `RELAY_QUERY_TIMEOUT` in `grpc/layout.rs`: a space switch is a re-attach
/// (resolve the target workspace's focused pane + re-point the wire stream), bounded
/// at the backend by herdr's per-call control timeout; 18 s comfortably covers it
/// plus channel overhead.
const SWITCH_SPACE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(18);

impl MuxrService {
    // ── GetSpaces ─────────────────────────────────────────────────────────────

    /// List the spaces for a session, marking the connection-active one.
    ///
    /// zellij sessions return an empty list (the `MuxBackend::list_spaces` default).
    pub(super) async fn get_spaces_impl(
        &self,
        request: Request<SessionRef>,
    ) -> Result<Response<SpaceList>, Status> {
        let req = request.into_inner();
        let session = req.session;
        let connection_id = req.connection_id;
        let (backend, bare) = self.resolve_session(&session)?;
        // FS3: full connection_id must not appear in info/warn logs.
        log::info!(
            "GetSpaces: session='{session}' connection_id={}…",
            short_conn(&connection_id)
        );
        log::debug!("GetSpaces: session='{session}' connection_id='{connection_id}'");

        // Blocking IPC (herdr `workspace.list`) → spawn_blocking.
        let snapshots = {
            let backend = backend.clone();
            let bare = bare.clone();
            tokio::task::spawn_blocking(move || backend.list_spaces(&bare))
                .await
                .map_err(|e| Status::internal(format!("GetSpaces: list task panicked: {e}")))?
                .map_err(|e| {
                    log::warn!("GetSpaces: list_spaces failed for '{session}': {e:#}");
                    Status::internal(format!("GetSpaces: {e:#}"))
                })?
        };

        // Per-connection active override: the relay tracks the workspace it switched
        // to (the daemon-global focus is intentionally left untouched on switch, so
        // the backend-reported `active` would otherwise be wrong for this client).
        // When no relay is attached (or it has not switched yet), fall back to the
        // backend-reported active.
        let relay_space = self.connection_current_space(&session, &connection_id);
        if let Some(ref ws) = relay_space {
            log::debug!("GetSpaces: connection-active space override → '{ws}'");
        }

        let spaces: Vec<Space> = snapshots
            .into_iter()
            .map(|s| {
                let active = match relay_space {
                    Some(ref ws) => &s.id == ws,
                    None => s.active,
                };
                Space {
                    id: s.id,
                    name: s.name,
                    active,
                }
            })
            .collect();

        log::info!("GetSpaces: session='{session}' → {} space(s)", spaces.len());
        Ok(Response::new(SpaceList { spaces }))
    }

    // ── SwitchSpace ───────────────────────────────────────────────────────────

    /// Switch the connection's relay to a different space. Permitted for
    /// read-only sessions (view-only — re-points only THIS connection's relay,
    /// not the daemon-global focus or any other connection's stream).
    ///
    /// Routed through the connection's live relay by an **exact** connection_id match
    /// (fail-closed — no session-scoped fallback; see `resolve_space_relay`). With no
    /// matching connection, returns `ActionAck{ok:false, "reattach required …"}`.
    pub(super) async fn switch_space_impl(
        &self,
        request: Request<SwitchSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        let req = request.into_inner();
        let session = req.session;
        let connection_id = req.connection_id;
        let space_id = req.space_id;
        validate_space_id(&space_id)?;
        // Resolve to validate the session id / owning backend exists (the actual
        // switch is relay-routed, but a bad id must still be a clean error).
        let _ = self.resolve_session(&session)?;
        // FS3: full connection_id must not appear in info/warn logs.
        log::info!(
            "SwitchSpace: session='{session}' space_id='{space_id}' \
             connection_id={}…",
            short_conn(&connection_id)
        );
        log::debug!(
            "SwitchSpace: session='{session}' space_id='{space_id}' \
             connection_id='{connection_id}'"
        );

        // Locate the connection's relay control sender by an EXACT connection_id
        // match (fail-closed; see `resolve_space_relay`). No session-scoped fallback:
        // on a collapsed herdr session that would re-point a co-attached client's
        // stream (S-M2/S-M4). On no match return ok:false — never steer an arbitrary
        // relay.
        let sender = match self.resolve_space_relay(&session, &connection_id) {
            Some(s) => s,
            None => {
                // FS3: the submitted connection_id may be a guessed/arbitrary value;
                // omit it from info entirely and keep only the 8-char prefix for
                // operational correlation.
                log::info!(
                    "SwitchSpace: no matching connection for '{session}' \
                     (connection_id={}…) — fail-closed",
                    short_conn(&connection_id)
                );
                log::debug!(
                    "SwitchSpace: no matching connection for '{session}' \
                     (connection_id='{connection_id}') — fail-closed"
                );
                return Ok(Response::new(ProtoAck {
                    ok: false,
                    error: "reattach required (no matching connection)".to_owned(),
                    info: String::new(),
                }));
            }
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        if sender
            .send(RelayControl::SwitchSpace {
                workspace_id: space_id.clone(),
                reply: reply_tx,
            })
            .is_err()
        {
            log::warn!("SwitchSpace: relay sender closed for '{session}'");
            return Ok(Response::new(ProtoAck {
                ok: false,
                error: "SwitchSpace: relay unavailable (tearing down)".to_owned(),
                info: String::new(),
            }));
        }

        match tokio::time::timeout(SWITCH_SPACE_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(()))) => {
                log::info!("SwitchSpace: session='{session}' space_id='{space_id}' ok");
                Ok(Response::new(ProtoAck {
                    ok: true,
                    error: String::new(),
                    info: String::new(),
                }))
            }
            Ok(Ok(Err(e))) => {
                log::warn!("SwitchSpace: relay reported failure for '{session}': {e:#}");
                Ok(Response::new(ProtoAck {
                    ok: false,
                    error: format!("SwitchSpace failed: {e:#}"),
                    info: String::new(),
                }))
            }
            Ok(Err(_cancelled)) => {
                log::warn!("SwitchSpace: relay oneshot cancelled for '{session}'");
                Ok(Response::new(ProtoAck {
                    ok: false,
                    error: "SwitchSpace: relay cancelled the request".to_owned(),
                    info: String::new(),
                }))
            }
            Err(_elapsed) => {
                log::warn!(
                    "SwitchSpace: relay timed out for '{session}' after {SWITCH_SPACE_TIMEOUT:?}"
                );
                Ok(Response::new(ProtoAck {
                    ok: false,
                    error: "SwitchSpace: timed out waiting for the relay".to_owned(),
                    info: String::new(),
                }))
            }
        }
    }

    // ── CreateSpace ───────────────────────────────────────────────────────────

    /// Create a new space (herdr workspace). MUTATING. Control-plane (daemon-global).
    pub(super) async fn create_space_impl(
        &self,
        request: Request<CreateSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "CreateSpace")?;
        let req = request.into_inner();
        let (backend, _bare) = self.resolve_session(&req.session)?;
        // An empty label means "auto-name" (herdr picks one) → None. A non-empty
        // label crosses the gRPC trust boundary into herdr's JSON-API, so bound +
        // sanitise it first.
        let label = if req.label.is_empty() {
            None
        } else {
            validate_space_label(&req.label)?;
            Some(req.label)
        };
        // Error hygiene: keep the raw label at debug only; info stays label-free.
        log::debug!("CreateSpace: session='{}' label={label:?}", req.session);
        log::info!(
            "CreateSpace: session='{}' (auto_name={})",
            req.session,
            label.is_none()
        );
        run_space_action("CreateSpace", move || backend.create_space(label)).await
    }

    // ── RenameSpace ───────────────────────────────────────────────────────────

    /// Rename an existing space. MUTATING. Control-plane (daemon-global).
    pub(super) async fn rename_space_impl(
        &self,
        request: Request<RenameSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "RenameSpace")?;
        let req = request.into_inner();
        let (backend, _bare) = self.resolve_session(&req.session)?;
        let space_id = req.space_id;
        let label = req.label;
        // Validate both the opaque id shape and the new label before forwarding to
        // herdr's JSON-API (gRPC trust boundary).
        validate_space_id(&space_id)?;
        validate_space_label(&label)?;
        // Error hygiene: raw label at debug only; info carries just the opaque id.
        log::debug!(
            "RenameSpace: session='{}' space_id='{space_id}' label='{label}'",
            req.session
        );
        log::info!(
            "RenameSpace: session='{}' space_id='{space_id}'",
            req.session
        );
        run_space_action("RenameSpace", move || {
            backend.rename_space(&space_id, &label)
        })
        .await
    }

    // ── CloseSpace ────────────────────────────────────────────────────────────

    /// Close (delete) a space. MUTATING. Control-plane (daemon-global).
    ///
    /// **Group intent is the caller's** (`CloseSpaceReq.close_group`, default
    /// `false`). A default close removes exactly the named space; on herdr a named
    /// space that is a worktree-group primary is *refused* by the backend, and that
    /// refusal is forwarded as `ActionAck{ok:false}` naming the group. Only an
    /// explicit `close_group: true` removes a whole group, and the ack's `info`
    /// then names every space that went.
    ///
    /// **Zero-space safety.** A CloseSpace must never leave the daemon with zero
    /// spaces without the caller being told, because the singular herdr session
    /// stops working the moment it has no workspaces
    /// (`active_or_first_workspace_id` errors on the next attach/query). The
    /// mechanism is a **pre-close refusal**, chosen over after-the-fact reporting
    /// because herdr's `workspace.list` exposes each workspace's worktree repo key
    /// and linked-worktree flag — enough to reproduce herdr's own group-removal
    /// rule (see [`MuxBackend::spaces_removed_by_close`]) — so the exact removal
    /// set is knowable while nothing has happened yet:
    /// - default close: one space goes, so the pre-close cardinality alone decides
    ///   it — [`would_close_last_space`], the original S-M1 guard;
    /// - group close: the removal set is resolved first and the close is refused
    ///   when it covers every space present ([`would_close_every_space`]). The old
    ///   cardinality guard is *not* sufficient here — that is precisely how an
    ///   unconditional group close could empty a two-space daemon past a guard that
    ///   counted two.
    ///
    /// A snapshot can still be raced by a concurrent close, so the post-close
    /// listing below double-checks and, when nothing remains, says so in the ack
    /// rather than only in the daemon log. That listing doubles as the re-point
    /// target lookup, so it costs one round trip, not two.
    ///
    /// **Caller re-point.** When the **caller's own** connection was viewing the
    /// just-closed space we re-point its relay to the daemon's new active-or-first
    /// workspace (via the same `RelayControl::SwitchSpace` mechanism SwitchSpace
    /// uses), so its wire stream does not keep pointing at a dead workspace. This
    /// aligns with herdr's own `workspace.close` behaviour, which refocuses another
    /// workspace when the focused one is closed.
    ///
    /// We do NOT touch *other* co-attached connections' relays (re-pointing a
    /// sibling's stream is exactly the S-M2/S-M4 isolation violation). A client that
    /// was viewing the closed space on a different connection recovers via the
    /// **client-recovery contract**: its next layout poll against the dead workspace
    /// fails, and the client re-fetches `GetSpaces` and issues `SwitchSpace` to a
    /// live space.
    pub(super) async fn close_space_impl(
        &self,
        request: Request<CloseSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "CloseSpace")?;
        let req = request.into_inner();
        let session = req.session;
        let connection_id = req.connection_id;
        let space_id = req.space_id;
        let close_group = req.close_group;
        let (backend, bare) = self.resolve_session(&session)?;
        validate_space_id(&space_id)?;
        log::info!(
            "CloseSpace: session='{session}' space_id='{space_id}' close_group={close_group}"
        );

        // ── Zero-space guard, resolved BEFORE anything is closed ──────────────
        // Enumerate first (blocking herdr `workspace.list` → spawn_blocking).
        let space_count = {
            let backend = backend.clone();
            let bare = bare.clone();
            tokio::task::spawn_blocking(move || backend.list_spaces(&bare))
                .await
                .map_err(|e| Status::internal(format!("CloseSpace: list task panicked: {e}")))?
                .map_err(|e| {
                    log::warn!("CloseSpace: pre-close list_spaces failed for '{session}': {e:#}");
                    Status::internal("CloseSpace: failed to enumerate spaces")
                })?
                .len()
        };
        // What this close would actually remove. Only a group close needs asking:
        // without the flag the backend removes the named space or refuses, so its
        // removal set is known without a second round trip.
        let removals: Vec<String> = if close_group {
            let backend = backend.clone();
            let target = space_id.clone();
            tokio::task::spawn_blocking(move || backend.spaces_removed_by_close(&target, true))
                .await
                .map_err(|e| Status::internal(format!("CloseSpace: group task panicked: {e}")))?
                .map_err(|e| {
                    // Fail closed: an unknown blast radius is not a licence to close.
                    log::warn!("CloseSpace: group resolution failed for '{session}': {e:#}");
                    Status::internal("CloseSpace: failed to resolve the group close set")
                })?
        } else {
            vec![space_id.clone()]
        };
        let refusal = if close_group {
            would_close_every_space(space_count, removals.len()).then(|| {
                format!(
                    "cannot close this group: it covers all {space_count} space(s) on \
                     this daemon, which would leave none"
                )
            })
        } else {
            would_close_last_space(space_count).then(|| "cannot close the last space".to_owned())
        };
        if let Some(error) = refusal {
            log::info!("CloseSpace: refusing '{space_id}' for '{session}': {error}");
            return Ok(Response::new(ProtoAck {
                ok: false,
                error,
                info: String::new(),
            }));
        }

        // ── Perform the close (blocking herdr `workspace.close`) ──────────────
        let ack = {
            let backend = backend.clone();
            let space_id = space_id.clone();
            tokio::task::spawn_blocking(move || backend.close_space(&space_id, close_group))
                .await
                .map_err(|e| Status::internal(format!("CloseSpace: close task panicked: {e}")))?
                .map_err(|e| {
                    // Error hygiene: full chain to the log, terse status to the client.
                    log::warn!("CloseSpace: close_space failed for '{session}': {e:#}");
                    Status::internal("CloseSpace: backend error")
                })?
        };
        if !ack.ok {
            log::warn!(
                "CloseSpace: backend reported ok:false for '{session}': {:?}",
                ack.error
            );
            return Ok(Response::new(ProtoAck {
                ok: ack.ok,
                error: ack.error.unwrap_or_default(),
                info: ack.info.unwrap_or_default(),
            }));
        }

        // ── Report the blast radius, and verify the daemon is not empty ───────
        // `info` was always empty before, which is what made a group close
        // invisible: the client saw one id go and could not learn that its
        // siblings went with it.
        let mut info = describe_removed(&removals);
        let remaining = self
            .list_spaces_after_close(&session, &backend, &bare)
            .await;
        if remaining.as_deref().is_some_and(<[_]>::is_empty) {
            // Raced by a concurrent close (the pre-close set said otherwise). The
            // caller MUST hear about it — a silent warn in the daemon log is what
            // this whole guard exists to avoid.
            log::warn!(
                "CloseSpace: no spaces remain on '{session}' after closing '{space_id}' \
                 — the daemon is left without a workspace"
            );
            info.push_str(
                "; WARNING: no spaces remain on this daemon — create a space before \
                 attaching again",
            );
        }

        // ── S-M1 recovery: re-point the CALLER's own relay if it was viewing the
        //    just-closed space (known iff its per-connection current_space == it).
        if self
            .connection_current_space(&session, &connection_id)
            .as_deref()
            == Some(space_id.as_str())
        {
            let target = remaining.as_deref().and_then(pick_repoint_target);
            self.repoint_caller_after_close(&session, &connection_id, target)
                .await;
        }

        Ok(Response::new(ProtoAck {
            ok: true,
            error: String::new(),
            info,
        }))
    }

    // ── Private routing helpers ─────────────────────────────────────────────────

    /// The space (herdr workspace) the connection's relay is currently viewing, if
    /// any. Looks up the per-connection [`RelayViewState`] by an **exact**
    /// `connection_id` match (validated against `session`).
    ///
    /// S-M2/S-M4: spaces are herdr-only, and herdr collapses every connection onto
    /// the single `herdr:herdr` session — so a session-scoped fallback here would
    /// read **another** connection's `current_space` and mark the wrong space active
    /// for this caller. We therefore drop the fallback: on an absent/mismatched
    /// connection_id we return `None`, and `get_spaces_impl` falls back to the
    /// backend-reported active (GetSpaces) rather than a sibling relay's view-state.
    ///
    /// [`RelayViewState`]: crate::relay::RelayViewState
    fn connection_current_space(&self, session: &str, connection_id: &str) -> Option<String> {
        match self.connection_space(session, connection_id) {
            ConnectionSpace::Space(id) => Some(id),
            // Both "no view state" and "attached but never switched" are `None`
            // here: GetSpaces/CloseSpace treat them identically (fall back to the
            // backend-reported active). Callers that must tell them apart use
            // [`Self::connection_space`] directly.
            ConnectionSpace::Unknown | ConnectionSpace::DaemonActive => None,
        }
    }

    /// The three-way form of [`Self::connection_current_space`]: what this
    /// connection's tracked view state says about the space it is viewing.
    ///
    /// Same fail-closed lookup rule (exact `connection_id`, validated against
    /// `session`; no session-scoped fallback — S-M2/S-M4), but it does **not**
    /// collapse [`ConnectionSpace::Unknown`] and [`ConnectionSpace::DaemonActive`]
    /// into one `None`. The space-scoped `GetLayout` read needs that distinction to
    /// decide whether a named space is the caller's OWN (see [`super::layout`]).
    ///
    /// `pub(super)` for that caller; the DashMap guard is dropped before returning
    /// (the space id is cloned out), so no guard is ever held across an `.await`.
    pub(super) fn connection_space(&self, session: &str, connection_id: &str) -> ConnectionSpace {
        if connection_id.is_empty() {
            return ConnectionSpace::Unknown;
        }
        match self
            .view_state
            .get(connection_id)
            .filter(|entry| entry.session == session)
        {
            None => ConnectionSpace::Unknown,
            Some(entry) => match entry.state.current_space.clone() {
                Some(id) => ConnectionSpace::Space(id),
                None => ConnectionSpace::DaemonActive,
            },
        }
    }

    /// Resolve the control sender for the connection's relay for a SwitchSpace.
    ///
    /// SwitchSpace is herdr-only and MUTATING, and herdr collapses every connection
    /// onto the single `herdr:herdr` session. A session-scoped fallback would
    /// re-point a **co-attached** connection's wire stream when this caller's
    /// connection_id is empty/stale (the S-M2/S-M4 isolation violation). So this is
    /// **fail-closed**: an exact `connection_id` match (validated against `session`)
    /// is required, with no fallback. Returns `None` when connection_id is empty or
    /// does not match a live relay (caller returns `ActionAck{ok:false}`).
    fn resolve_space_relay(
        &self,
        session: &str,
        connection_id: &str,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<RelayControl>> {
        if connection_id.is_empty() {
            return None;
        }
        self.control
            .get(connection_id)
            .filter(|entry| entry.session == session)
            .map(|entry| entry.sender.clone())
    }

    /// List the spaces that survived a close, or `None` when the listing itself
    /// failed (logged, never fatal — the close already succeeded).
    ///
    /// One post-close `workspace.list` serves two purposes: confirming the daemon
    /// still has a space (an empty answer is reported to the caller, not just
    /// logged) and supplying [`pick_repoint_target`] with its candidates.
    async fn list_spaces_after_close(
        &self,
        session: &str,
        backend: &Arc<dyn MuxBackend>,
        bare: &str,
    ) -> Option<Vec<SpaceSnapshot>> {
        let backend = backend.clone();
        let bare = bare.to_owned();
        match tokio::task::spawn_blocking(move || backend.list_spaces(&bare)).await {
            Ok(Ok(spaces)) => Some(spaces),
            Ok(Err(e)) => {
                log::warn!("CloseSpace: post-close list_spaces failed for '{session}': {e:#}");
                None
            }
            Err(e) => {
                log::warn!("CloseSpace: post-close list task panicked for '{session}': {e}");
                None
            }
        }
    }

    /// Re-point the caller's own relay off a just-closed space (S-M1 recovery).
    ///
    /// Called only when the caller's per-connection `current_space` was the closed
    /// id (so we KNOW the relay is viewing a now-dead workspace). `target` is the
    /// daemon's new active-or-first workspace, already resolved from the post-close
    /// listing; sending the caller's relay a [`RelayControl::SwitchSpace`] — the
    /// same mechanism `SwitchSpace` uses — re-attaches the wire stream and updates
    /// the relay's tracked `current_space`.
    ///
    /// Best-effort: any failure (no live relay, relay tearing down, herdr error,
    /// timeout) is logged and swallowed — the close already succeeded, and the
    /// client-recovery contract (next `GetSpaces` + `SwitchSpace`) is the backstop.
    /// Only the caller's OWN connection is ever steered (never a sibling's).
    async fn repoint_caller_after_close(
        &self,
        session: &str,
        connection_id: &str,
        target: Option<String>,
    ) {
        let Some(sender) = self.resolve_space_relay(session, connection_id) else {
            // No live relay for this connection (e.g. control-plane-only close);
            // nothing to re-point. The client recovers on its next GetSpaces.
            return;
        };

        let Some(target) = target else {
            // Nothing to re-point onto. This is no longer a silent warn-and-return:
            // when the reason is that no spaces remain, the caller has already been
            // told in the ack's `info` (see `close_space_impl`); this arm only
            // records the operator-facing half.
            log::warn!(
                "CloseSpace: no workspace to re-point '{session}' onto after close \
                 (reported to the caller)"
            );
            return;
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        if sender
            .send(RelayControl::SwitchSpace {
                workspace_id: target.clone(),
                reply: reply_tx,
            })
            .is_err()
        {
            log::info!("CloseSpace: caller relay closed before re-point for '{session}'");
            return;
        }
        match tokio::time::timeout(SWITCH_SPACE_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(()))) => {
                log::info!("CloseSpace: re-pointed caller's relay to '{target}' for '{session}'")
            }
            Ok(Ok(Err(e))) => {
                log::warn!("CloseSpace: re-point relay reported failure for '{session}': {e:#}")
            }
            Ok(Err(_cancelled)) => {
                log::warn!("CloseSpace: re-point oneshot cancelled for '{session}'")
            }
            Err(_elapsed) => log::warn!("CloseSpace: re-point timed out for '{session}'"),
        }
    }
}

// ─── Per-connection space resolution ────────────────────────────────────────────

/// What a connection's tracked view state says about the space it is viewing.
///
/// Three-way on purpose: [`RelayViewState::current_space`] folds two very
/// different situations into its own `None` (see its doc) —
/// - **no relay / no view state for this connection**, and
/// - **a relay that has simply never switched space**, which by construction is
///   viewing the daemon's active workspace (a hint-less attach lands there).
///
/// `GetSpaces` can ignore the difference (both fall back to the backend-reported
/// `active`), but the space-scoped `GetLayout` read cannot: only the second case
/// lets it conclude that a *named* space is the caller's own.
///
/// [`RelayViewState::current_space`]: crate::relay::RelayViewState::current_space
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ConnectionSpace {
    /// No per-connection view state (empty/mismatched `connection_id`, or no relay
    /// attached): nothing is known about this caller's view.
    Unknown,
    /// A relay is attached but has never switched space, so it is viewing the
    /// daemon's active-or-first workspace.
    DaemonActive,
    /// The relay switched to — and is viewing — this space.
    Space(String),
}

// ─── Free validation / mapping helpers ──────────────────────────────────────────

/// True when closing one more space would leave the daemon with zero workspaces.
///
/// `count` is the number of spaces present *before* the close. `<= 1` because
/// closing the only remaining space leaves none (S-M1).
///
/// Valid for a **non-group** close only, and only because such a close removes
/// exactly one space: herdr refuses a group primary rather than closing a group
/// without the flag. A group close needs [`would_close_every_space`], whose
/// removal count this is the `removed == 1` special case of.
fn would_close_last_space(count: usize) -> bool {
    count <= 1
}

/// True when a close that removes `removed` of the `count` spaces present before
/// it would leave the daemon with none.
///
/// The general form of [`would_close_last_space`], for a close whose removal set
/// is bigger than the one space the caller named — a herdr worktree-group close.
/// `>=` rather than `==` because the removal set is a snapshot: if it somehow
/// names more spaces than were listed, that is still "everything goes".
fn would_close_every_space(count: usize, removed: usize) -> bool {
    removed >= count
}

/// Human-readable statement of what a close removed, for the ack's `info`.
///
/// The client reconciles its space list from this: a group close removes spaces
/// the caller never named, and with an empty `info` it has no way to learn that
/// happened. Ids, not labels — the id is what the client keys its list on.
fn describe_removed(removed: &[String]) -> String {
    match removed {
        [] => String::new(),
        [one] => format!("closed 1 space: {one}"),
        many => format!(
            "closed {} spaces as a worktree group: {}",
            many.len(),
            many.join(", ")
        ),
    }
}

/// The daemon's active-or-first space, the target a caller's relay is re-pointed
/// onto after its space was closed. `None` when nothing remains.
fn pick_repoint_target(spaces: &[SpaceSnapshot]) -> Option<String> {
    spaces
        .iter()
        .find(|s| s.active)
        .or_else(|| spaces.first())
        .map(|s| s.id.clone())
}

/// Validate a user-supplied space **label** before it crosses the gRPC trust
/// boundary into herdr's JSON-API.
///
/// Labels are display names, so the charset is looser than the strict session
/// `[A-Za-z0-9_-]` guard: we additionally allow a space and the punctuation
/// `_-.`. We reject the empty string, anything over [`MAX_SPACE_LABEL_LEN`]
/// bytes, and any character outside that printable set (control chars, newlines,
/// non-ASCII). herdr's JSON-RPC layer escapes the value, so this is a
/// sanity/abuse bound, not an injection fix.
fn validate_space_label(label: &str) -> Result<(), Status> {
    if label.is_empty() {
        return Err(Status::invalid_argument("space label must not be empty"));
    }
    if label.len() > MAX_SPACE_LABEL_LEN {
        return Err(Status::invalid_argument(format!(
            "space label too long (max {MAX_SPACE_LABEL_LEN} bytes)"
        )));
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.'))
    {
        return Err(Status::invalid_argument(
            "invalid space label: only [A-Za-z0-9], space, and the characters _-. are allowed",
        ));
    }
    Ok(())
}

/// Validate an opaque space (herdr workspace) **id** supplied by the client
/// before it is forwarded to herdr.
///
/// herdr ids are opaque slugs/uuids; we require a non-empty, length-bounded token
/// of `[A-Za-z0-9_-.:]` (covers slug- and uuid-shaped ids) and reject whitespace,
/// control characters, and path/shell metacharacters. (If herdr ever widens its
/// id charset this guard must widen with it — as must the backend-side
/// defence-in-depth copy in `multiplexer::herdr::backend::validate_workspace_id`.)
///
/// `pub(super)` so the space-scoped `GetLayout` read in [`super::layout`] applies
/// the **same** guard to `SessionRef.space_id` as the mutating space ops do,
/// rather than growing a second, drifting validator.
pub(super) fn validate_space_id(space_id: &str) -> Result<(), Status> {
    if space_id.is_empty() {
        return Err(Status::invalid_argument("space_id must not be empty"));
    }
    if space_id.len() > MAX_SPACE_ID_LEN {
        return Err(Status::invalid_argument(format!(
            "space_id too long (max {MAX_SPACE_ID_LEN} bytes)"
        )));
    }
    if !space_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err(Status::invalid_argument(
            "invalid space_id: only [A-Za-z0-9_-.:] characters are allowed",
        ));
    }
    Ok(())
}

/// Run a blocking space control action, mapping the result into a proto ack with
/// ERROR HYGIENE.
///
/// A hard backend/IPC failure (anyhow `Err`) becomes a terse `Status::internal`
/// — the full error chain is logged server-side, never sent to the client (the
/// minor S-M3 `Status::internal`-leak fold-in). A logical `ok:false` ack is
/// forwarded as-is: herdr's logical message (e.g. "already exists") is terse and
/// client-appropriate. Mirrors `helpers::run_action` but without leaking `{e:#}`.
async fn run_space_action<F>(rpc: &'static str, f: F) -> Result<Response<ProtoAck>, Status>
where
    F: FnOnce() -> anyhow::Result<ActionAck> + Send + 'static,
{
    let ack = tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Status::internal(format!("{rpc}: action task panicked: {e}")))?
        .map_err(|e| {
            log::warn!("{rpc}: backend action failed: {e:#}");
            Status::internal(format!("{rpc}: backend error"))
        })?;
    log::debug!("{rpc}: ok={} info={:?}", ack.ok, ack.info);
    Ok(Response::new(ProtoAck {
        ok: ack.ok,
        error: ack.error.unwrap_or_default(),
        info: ack.info.unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        describe_removed, pick_repoint_target, validate_space_id, validate_space_label,
        would_close_every_space, would_close_last_space,
    };
    use crate::multiplexer::SpaceSnapshot;
    use crate::proto::Space;

    // ─── S-M1: last-space guard predicate ────────────────────────────────────

    #[test]
    fn would_close_last_space_rejects_zero_and_one() {
        // Closing when 0 or 1 spaces remain would leave the daemon non-functional.
        assert!(would_close_last_space(0));
        assert!(would_close_last_space(1));
        // Two or more → safe to close one.
        assert!(!would_close_last_space(2));
        assert!(!would_close_last_space(7));
    }

    // ─── Label validation (fold-in minor) ────────────────────────────────────

    #[test]
    fn space_label_accepts_sane_display_names() {
        assert!(validate_space_label("main").is_ok());
        assert!(validate_space_label("My Logs").is_ok());
        assert!(validate_space_label("api-v2.0").is_ok());
        assert!(validate_space_label("a_b-c.d e").is_ok());
    }

    #[test]
    fn space_label_rejects_empty_too_long_and_bad_charset() {
        assert!(validate_space_label("").is_err(), "empty rejected");
        // Over the 64-byte cap.
        let too_long = "x".repeat(super::MAX_SPACE_LABEL_LEN + 1);
        assert!(
            validate_space_label(&too_long).is_err(),
            "too long rejected"
        );
        // Exactly at the cap is allowed.
        let at_cap = "y".repeat(super::MAX_SPACE_LABEL_LEN);
        assert!(validate_space_label(&at_cap).is_ok(), "at-cap allowed");
        // Disallowed characters.
        assert!(validate_space_label("bad/slash").is_err());
        assert!(validate_space_label("new\nline").is_err());
        assert!(validate_space_label("nul\0byte").is_err());
        assert!(validate_space_label("emoji✨").is_err());
        assert!(validate_space_label("tab\tchar").is_err());
    }

    #[test]
    fn space_id_accepts_slug_and_uuid_shapes() {
        assert!(validate_space_id("ws-1").is_ok());
        assert!(validate_space_id("01HF8Z9K3T4Qm-abc.def").is_ok());
        assert!(validate_space_id("herdr:ws:7").is_ok());
    }

    #[test]
    fn space_id_rejects_empty_too_long_and_bad_charset() {
        assert!(validate_space_id("").is_err());
        let too_long = "a".repeat(super::MAX_SPACE_ID_LEN + 1);
        assert!(validate_space_id(&too_long).is_err());
        assert!(
            validate_space_id("../escape").is_err(),
            "path traversal rejected"
        );
        assert!(validate_space_id("a b").is_err(), "whitespace rejected");
        assert!(validate_space_id("has\0nul").is_err());
    }

    /// The proto mapping marks the relay-current space active and clears the
    /// backend-reported active when a per-connection override is present.
    fn map_with_override(snaps: Vec<SpaceSnapshot>, relay_space: Option<&str>) -> Vec<Space> {
        snaps
            .into_iter()
            .map(|s| {
                let active = match relay_space {
                    Some(ws) => s.id == ws,
                    None => s.active,
                };
                Space {
                    id: s.id,
                    name: s.name,
                    active,
                }
            })
            .collect()
    }

    fn snap(id: &str, name: &str, active: bool) -> SpaceSnapshot {
        SpaceSnapshot {
            id: id.to_owned(),
            name: name.to_owned(),
            active,
        }
    }

    #[test]
    fn override_marks_relay_space_active() {
        // Backend reports "a" active, but the relay switched to "b": "b" wins.
        let mapped =
            map_with_override(vec![snap("a", "A", true), snap("b", "B", false)], Some("b"));
        assert!(!mapped[0].active, "backend-active 'a' must be cleared");
        assert!(mapped[1].active, "relay-current 'b' must be active");
    }

    #[test]
    fn no_override_uses_backend_active() {
        // No relay-current space → the backend-reported active is preserved.
        let mapped = map_with_override(vec![snap("a", "A", true), snap("b", "B", false)], None);
        assert!(mapped[0].active, "backend-active 'a' must be preserved");
        assert!(!mapped[1].active);
    }

    #[test]
    fn empty_backend_list_maps_to_empty() {
        // zellij path: list_spaces returns empty → no spaces, regardless of override.
        assert!(map_with_override(vec![], None).is_empty());
        assert!(map_with_override(vec![], Some("x")).is_empty());
    }

    // ─── S-M2/S-M4: fail-closed relay/view-state resolution ──────────────────

    use crate::grpc::MuxrService;
    use crate::relay::{ControlEntry, RelayControl, RelayViewState, ViewStateEntry};
    use tokio::sync::mpsc;

    #[test]
    fn resolve_space_relay_requires_exact_connection_id() {
        // SwitchSpace is herdr-only + mutating: an empty/guessed connection_id must
        // NOT resolve to the victim's relay (no session-scoped fallback — S-M2/S-M4).
        let service = MuxrService::new();
        let (tx, _rx) = mpsc::unbounded_channel::<RelayControl>();
        service.control.insert(
            "victim-conn".to_owned(),
            ControlEntry {
                session: "herdr:herdr".to_owned(),
                sender: tx,
                read_only: false,
            },
        );
        // Exact match resolves.
        assert!(
            service
                .resolve_space_relay("herdr:herdr", "victim-conn")
                .is_some(),
            "exact connection_id must resolve"
        );
        // Empty connection_id → None (fail-closed; no steer onto the victim).
        assert!(
            service.resolve_space_relay("herdr:herdr", "").is_none(),
            "empty connection_id must fail closed (no session fallback)"
        );
        // Guessed/stale connection_id → None.
        assert!(
            service
                .resolve_space_relay("herdr:herdr", "guessed-1")
                .is_none(),
            "wrong connection_id must fail closed"
        );
    }

    #[test]
    fn connection_current_space_requires_exact_connection_id() {
        // GetSpaces read fallback: an empty/wrong connection_id must NOT read the
        // victim connection's current_space (it falls back to backend-active instead).
        let service = MuxrService::new();
        let state = RelayViewState {
            current_space: Some("ws-2".to_owned()),
            ..RelayViewState::default()
        };
        service.view_state.insert(
            "victim-conn".to_owned(),
            ViewStateEntry {
                session: "herdr:herdr".to_owned(),
                state,
            },
        );
        // Exact match reads the connection's space.
        assert_eq!(
            service
                .connection_current_space("herdr:herdr", "victim-conn")
                .as_deref(),
            Some("ws-2"),
            "exact connection_id reads the connection's current_space"
        );
        // Empty / wrong connection_id → None (won't leak the victim's view-state).
        assert!(
            service
                .connection_current_space("herdr:herdr", "")
                .is_none(),
            "empty connection_id must not read a sibling's current_space"
        );
        assert!(
            service
                .connection_current_space("herdr:herdr", "other-conn")
                .is_none(),
            "wrong connection_id must not read a sibling's current_space"
        );
    }

    // ─── Zero-space safety: the whole CloseSpace walk ────────────────────────
    //
    // Drives `close_space_impl` against a scripted spaces backend, because the
    // defect this change fixes is not in any single predicate: it was the
    // combination of a pre-close cardinality guard with a close that removed more
    // than one space.

    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tonic::Request;

    use crate::auth::SessionReadOnly;
    use crate::cli::BackendKind;
    use crate::multiplexer::{
        ActionAck, BackendSet, DualHandle, LayoutSnapshot, MuxBackend, PaneRef, ResizeDir,
        ResizeKind, ScrollDir,
    };
    use crate::proto::{CloseSpaceReq, CreateSpaceReq, RenameSpaceReq, SwitchSpaceReq};

    /// A spaces backend with scripted listings and a scripted removal set, which
    /// records every close it is actually asked to perform.
    #[derive(Debug)]
    struct ScriptedSpaces {
        /// Successive `list_spaces` answers: the pre-close listing, then the
        /// post-close one. The last entry repeats once exhausted.
        listings: Mutex<VecDeque<Vec<SpaceSnapshot>>>,
        /// What `spaces_removed_by_close` answers for a group close.
        group_removals: Vec<String>,
        /// Error `close_space` answers with, or `None` for a successful close.
        refuse_with: Option<String>,
        /// `(space_id, close_group)` of every close that reached the backend.
        closes: Mutex<Vec<(String, bool)>>,
    }

    impl ScriptedSpaces {
        fn new(listings: Vec<Vec<SpaceSnapshot>>) -> Self {
            Self {
                listings: Mutex::new(listings.into()),
                group_removals: Vec::new(),
                refuse_with: None,
                closes: Mutex::new(Vec::new()),
            }
        }

        fn with_group(mut self, ids: &[&str]) -> Self {
            self.group_removals = ids.iter().map(|s| (*s).to_owned()).collect();
            self
        }

        fn refusing(mut self, error: &str) -> Self {
            self.refuse_with = Some(error.to_owned());
            self
        }

        fn performed_closes(&self) -> Vec<(String, bool)> {
            self.closes.lock().expect("closes mutex").clone()
        }
    }

    impl MuxBackend for ScriptedSpaces {
        fn supports_spaces(&self) -> bool {
            true
        }
        fn list_spaces(&self, _session: &str) -> anyhow::Result<Vec<SpaceSnapshot>> {
            let mut listings = self.listings.lock().expect("listings mutex");
            if listings.len() > 1 {
                Ok(listings.pop_front().unwrap_or_default())
            } else {
                Ok(listings.front().cloned().unwrap_or_default())
            }
        }
        fn spaces_removed_by_close(
            &self,
            space_id: &str,
            close_group: bool,
        ) -> anyhow::Result<Vec<String>> {
            if close_group && !self.group_removals.is_empty() {
                Ok(self.group_removals.clone())
            } else {
                Ok(vec![space_id.to_owned()])
            }
        }
        fn close_space(&self, space_id: &str, close_group: bool) -> anyhow::Result<ActionAck> {
            self.closes
                .lock()
                .expect("closes mutex")
                .push((space_id.to_owned(), close_group));
            Ok(match &self.refuse_with {
                Some(error) => ActionAck {
                    ok: false,
                    error: Some(error.clone()),
                    info: None,
                },
                None => ActionAck {
                    ok: true,
                    error: None,
                    info: None,
                },
            })
        }

        // ── Everything else is out of this test's scope ──────────────────────
        fn list_sessions(&self) -> anyhow::Result<Vec<(String, Duration)>> {
            unimplemented!()
        }
        fn list_sessions_with_resurrectables(&self) -> anyhow::Result<Vec<(String, u64, bool)>> {
            unimplemented!()
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
            "scripted-spaces-stub".to_owned()
        }
    }

    /// `ids` as space snapshots, the first one active.
    fn snapshots(ids: &[&str]) -> Vec<SpaceSnapshot> {
        ids.iter()
            .enumerate()
            .map(|(i, id)| snap(id, id, i == 0))
            .collect()
    }

    fn service_with(backend: &Arc<ScriptedSpaces>) -> MuxrService {
        let backend: Arc<dyn MuxBackend> = backend.clone();
        MuxrService::with_backends(BackendSet::new(vec![(BackendKind::Herdr, backend)]))
    }

    /// A CloseSpace request carrying a writable session token (the auth layer's
    /// extension; absent it the RPC fails closed before any of this logic runs).
    fn close_req(space_id: &str, close_group: bool) -> Request<CloseSpaceReq> {
        let mut req = Request::new(CloseSpaceReq {
            session: "herdr:herdr".to_owned(),
            space_id: space_id.to_owned(),
            connection_id: String::new(),
            close_group,
        });
        req.extensions_mut().insert(SessionReadOnly(false));
        req
    }

    /// THE defect this card fixes. Two spaces, both members of one worktree group,
    /// and the caller closes the primary WITH group intent: the pre-close count is
    /// 2, so the old cardinality guard would have waved it through and herdr would
    /// have removed both, leaving the daemon with zero workspaces and an `ok:true`
    /// ack. The removal set makes it refusable before anything happens.
    #[tokio::test]
    async fn group_close_that_would_empty_the_daemon_is_refused_before_it_happens() {
        let backend = Arc::new(
            ScriptedSpaces::new(vec![snapshots(&["ws-1", "ws-2"])]).with_group(&["ws-1", "ws-2"]),
        );
        let service = service_with(&backend);

        let ack = service
            .close_space_impl(close_req("ws-1", true))
            .await
            .expect("a logical refusal is an ack, never a Status")
            .into_inner();

        assert!(!ack.ok, "closing the whole daemon must be refused");
        assert!(
            ack.error.contains("all 2 space(s)"),
            "the refusal must say why: {}",
            ack.error
        );
        assert!(
            backend.performed_closes().is_empty(),
            "the refusal must come BEFORE the close, not after it"
        );
    }

    /// The same shape without group intent is the ordinary path: one space goes,
    /// one remains, and the flag reaches the backend as `false`.
    #[tokio::test]
    async fn default_close_passes_group_intent_off_and_removes_one_space() {
        let backend = Arc::new(ScriptedSpaces::new(vec![
            snapshots(&["ws-1", "ws-2"]),
            snapshots(&["ws-2"]),
        ]));
        let service = service_with(&backend);

        let ack = service
            .close_space_impl(close_req("ws-1", false))
            .await
            .expect("close must succeed")
            .into_inner();

        assert!(ack.ok, "error: {}", ack.error);
        assert_eq!(
            backend.performed_closes(),
            vec![("ws-1".to_owned(), false)],
            "an opt-in flag left unset must reach the backend as false"
        );
        assert_eq!(ack.info, "closed 1 space: ws-1");
    }

    /// A group close that leaves something behind is allowed — and must report the
    /// spaces the caller never named, so the client can reconcile its list.
    #[tokio::test]
    async fn group_close_reports_every_space_it_removed() {
        let backend = Arc::new(
            ScriptedSpaces::new(vec![
                snapshots(&["ws-1", "ws-2", "ws-3"]),
                snapshots(&["ws-3"]),
            ])
            .with_group(&["ws-1", "ws-2"]),
        );
        let service = service_with(&backend);

        let ack = service
            .close_space_impl(close_req("ws-1", true))
            .await
            .expect("close must succeed")
            .into_inner();

        assert!(ack.ok, "error: {}", ack.error);
        assert_eq!(backend.performed_closes(), vec![("ws-1".to_owned(), true)]);
        assert!(ack.info.contains("ws-1"), "info: {}", ack.info);
        assert!(
            ack.info.contains("ws-2"),
            "the unnamed sibling must be reported: {}",
            ack.info
        );
    }

    /// The backstop for the race the pre-close snapshot cannot rule out: if the
    /// daemon turns out to be empty afterwards, the CALLER hears about it — a warn
    /// in the daemon log is exactly the silence this guard exists to break.
    #[tokio::test]
    async fn an_emptied_daemon_is_reported_to_the_caller_not_just_logged() {
        // Pre-close listing says two spaces; by the post-close listing a concurrent
        // close has taken the other one.
        let backend = Arc::new(ScriptedSpaces::new(vec![
            snapshots(&["ws-1", "ws-2"]),
            snapshots(&[]),
        ]));
        let service = service_with(&backend);

        let ack = service
            .close_space_impl(close_req("ws-1", false))
            .await
            .expect("close must succeed")
            .into_inner();

        assert!(ack.ok, "the close itself did succeed");
        assert!(
            ack.info.contains("no spaces remain"),
            "the emptied daemon must be surfaced in the ack: {}",
            ack.info
        );
    }

    /// The original S-M1 guard is untouched for the default path.
    #[tokio::test]
    async fn default_close_still_refuses_the_last_space() {
        let backend = Arc::new(ScriptedSpaces::new(vec![snapshots(&["ws-1"])]));
        let service = service_with(&backend);

        let ack = service
            .close_space_impl(close_req("ws-1", false))
            .await
            .expect("a logical refusal is an ack, never a Status")
            .into_inner();

        assert!(!ack.ok);
        assert_eq!(ack.error, "cannot close the last space");
        assert!(backend.performed_closes().is_empty());
    }

    /// herdr's `workspace_group_close_required` reaches the client as an
    /// unsuccessful acknowledgement carrying the backend's own message — never as a
    /// gRPC status, and never swallowed.
    #[tokio::test]
    async fn backend_group_refusal_is_forwarded_as_a_failed_ack() {
        let refusal = "this space is a worktree-group primary with linked worktree \
                       spaces — re-issue CloseSpace with group intent (close_group=true)";
        let backend =
            Arc::new(ScriptedSpaces::new(vec![snapshots(&["ws-1", "ws-2"])]).refusing(refusal));
        let service = service_with(&backend);

        let ack = service
            .close_space_impl(close_req("ws-1", false))
            .await
            .expect("a logical refusal is an ack, never a Status")
            .into_inner();

        assert!(!ack.ok);
        assert_eq!(ack.error, refusal);
    }

    // ─── Zero-space predicates and reporting helpers ─────────────────────────

    #[test]
    fn would_close_every_space_catches_the_group_that_takes_everything() {
        // The counterexample the old guard missed: 2 present, 2 removed.
        assert!(would_close_every_space(2, 2));
        assert!(would_close_every_space(1, 1));
        // A snapshot that somehow names more than were listed still means "all".
        assert!(would_close_every_space(2, 3));
        // Something survives.
        assert!(!would_close_every_space(3, 2));
        assert!(!would_close_every_space(2, 1));
        // It generalises the last-space guard: `removed == 1` is that predicate.
        for count in 0..4 {
            assert_eq!(
                would_close_every_space(count, 1),
                would_close_last_space(count),
                "count={count}"
            );
        }
    }

    #[test]
    fn describe_removed_names_the_group_members() {
        assert_eq!(describe_removed(&[]), "");
        assert_eq!(
            describe_removed(&["ws-1".to_owned()]),
            "closed 1 space: ws-1"
        );
        let group = describe_removed(&["ws-1".to_owned(), "ws-2".to_owned()]);
        assert!(group.contains("2 spaces"), "{group}");
        assert!(group.contains("ws-1, ws-2"), "{group}");
    }

    #[test]
    fn pick_repoint_target_prefers_the_active_space() {
        assert_eq!(
            pick_repoint_target(&[snap("a", "A", false), snap("b", "B", true)]).as_deref(),
            Some("b")
        );
        // No active flag → the first listed.
        assert_eq!(
            pick_repoint_target(&[snap("a", "A", false), snap("b", "B", false)]).as_deref(),
            Some("a")
        );
        // Nothing left → nothing to re-point onto.
        assert_eq!(pick_repoint_target(&[]), None);
    }

    // ─── Read-only-gate tests: the RPC un-gate (read-only→explorer, Phase 1) ─
    //
    // SwitchSpace is now permitted for a read-only session token; CreateSpace /
    // RenameSpace / CloseSpace stay refused. A weakened trust boundary without a
    // paired test is Critical per docs/REVIEW_FOCUS.md.

    const READ_ONLY_MESSAGE: &str =
        "session token is read-only — mutating operations are not allowed";

    fn switch_space_req(read_only: bool) -> Request<SwitchSpaceReq> {
        let mut req = Request::new(SwitchSpaceReq {
            session: "herdr:herdr".to_owned(),
            space_id: "ws-1".to_owned(),
            connection_id: String::new(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn create_space_req(read_only: bool) -> Request<CreateSpaceReq> {
        let mut req = Request::new(CreateSpaceReq {
            session: "herdr:herdr".to_owned(),
            label: String::new(),
            connection_id: String::new(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn rename_space_req(read_only: bool) -> Request<RenameSpaceReq> {
        let mut req = Request::new(RenameSpaceReq {
            session: "herdr:herdr".to_owned(),
            space_id: "ws-1".to_owned(),
            label: "renamed".to_owned(),
            connection_id: String::new(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    /// SwitchSpace is view-only (re-points only the caller's own relay), so a
    /// read-only token must reach relay resolution instead of being rejected by
    /// the gate. No relay is attached in this test, so the ack legitimately fails
    /// "no matching connection" — the point is that it is an ack, never
    /// `PermissionDenied`.
    #[tokio::test]
    async fn switch_space_is_permitted_for_a_read_only_session() {
        let backend = Arc::new(ScriptedSpaces::new(vec![snapshots(&["ws-1"])]));
        let service = service_with(&backend);

        let ack = service
            .switch_space_impl(switch_space_req(true))
            .await
            .expect("SwitchSpace must not be rejected by the read-only gate")
            .into_inner();

        assert!(!ack.ok);
        assert!(
            ack.error.contains("reattach required"),
            "error: {}",
            ack.error
        );
    }

    #[tokio::test]
    async fn create_space_still_rejects_a_read_only_session() {
        let backend = Arc::new(ScriptedSpaces::new(vec![snapshots(&["ws-1"])]));
        let service = service_with(&backend);

        let err = service
            .create_space_impl(create_space_req(true))
            .await
            .expect_err("CreateSpace must stay refused for read-only sessions");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn rename_space_still_rejects_a_read_only_session() {
        let backend = Arc::new(ScriptedSpaces::new(vec![snapshots(&["ws-1"])]));
        let service = service_with(&backend);

        let err = service
            .rename_space_impl(rename_space_req(true))
            .await
            .expect_err("RenameSpace must stay refused for read-only sessions");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn close_space_still_rejects_a_read_only_session() {
        let backend = Arc::new(ScriptedSpaces::new(vec![snapshots(&["ws-1", "ws-2"])]));
        let service = service_with(&backend);

        let mut req = close_req("ws-1", false);
        req.extensions_mut().insert(SessionReadOnly(true));

        let err = service
            .close_space_impl(req)
            .await
            .expect_err("CloseSpace must stay refused for read-only sessions");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }
}
