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
use crate::multiplexer::MuxBackend;
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

    /// Switch the connection's relay to a different space. MUTATING.
    ///
    /// Routed through the connection's live relay by an **exact** connection_id match
    /// (fail-closed — no session-scoped fallback; see `resolve_space_relay`). With no
    /// matching connection, returns `ActionAck{ok:false, "reattach required …"}`.
    pub(super) async fn switch_space_impl(
        &self,
        request: Request<SwitchSpaceReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "SwitchSpace")?;
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
    /// **S-M1 — last/viewed-space safety:**
    /// - The **last** remaining space is never closed: that would leave the daemon
    ///   with zero workspaces, making the singular herdr session non-functional
    ///   (`active_or_first_workspace_id` would error on the next attach/query). We
    ///   return `ActionAck{ok:false, "cannot close the last space"}`.
    /// - When the **caller's own** connection was viewing the just-closed space we
    ///   re-point its relay to the daemon's new active-or-first workspace (via the
    ///   same `RelayControl::SwitchSpace` mechanism SwitchSpace uses), so its wire
    ///   stream does not keep pointing at a dead workspace. This aligns with herdr's
    ///   own `workspace.close` behaviour, which refocuses another workspace when the
    ///   focused one is closed.
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
        let (backend, bare) = self.resolve_session(&session)?;
        validate_space_id(&space_id)?;
        log::info!("CloseSpace: session='{session}' space_id='{space_id}'");

        // ── S-M1 guard: refuse to close the LAST space ────────────────────────
        // Enumerate first (blocking herdr `workspace.list` → spawn_blocking) so we
        // never leave the daemon with zero workspaces.
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
        if would_close_last_space(space_count) {
            log::info!("CloseSpace: refusing to close the last space for '{session}'");
            return Ok(Response::new(ProtoAck {
                ok: false,
                error: "cannot close the last space".to_owned(),
                info: String::new(),
            }));
        }

        // ── Perform the close (blocking herdr `workspace.close`) ──────────────
        let ack = {
            let backend = backend.clone();
            let space_id = space_id.clone();
            tokio::task::spawn_blocking(move || backend.close_space(&space_id))
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

        // ── S-M1 recovery: re-point the CALLER's own relay if it was viewing the
        //    just-closed space (known iff its per-connection current_space == it).
        if self
            .connection_current_space(&session, &connection_id)
            .as_deref()
            == Some(space_id.as_str())
        {
            self.repoint_caller_after_close(&session, &connection_id, &backend, &bare)
                .await;
        }

        Ok(Response::new(ProtoAck {
            ok: true,
            error: String::new(),
            info: String::new(),
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
        // Exact per-connection lookup only (clone out of the DashMap guard — never
        // held across an `.await`; this is a sync helper anyway). No session-scoped
        // fallback: it would leak a sibling connection's current_space.
        if connection_id.is_empty() {
            return None;
        }
        self.view_state
            .get(connection_id)
            .filter(|entry| entry.session == session)
            .and_then(|entry| entry.state.current_space.clone())
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

    /// Re-point the caller's own relay off a just-closed space (S-M1 recovery).
    ///
    /// Called only when the caller's per-connection `current_space` was the closed
    /// id (so we KNOW the relay is viewing a now-dead workspace). Computes the
    /// daemon's new active-or-first workspace and sends the caller's relay a
    /// [`RelayControl::SwitchSpace`] — the same mechanism `SwitchSpace` uses — which
    /// re-attaches the wire stream and updates the relay's tracked `current_space`.
    ///
    /// Best-effort: any failure (no live relay, relay tearing down, herdr error,
    /// timeout) is logged and swallowed — the close already succeeded, and the
    /// client-recovery contract (next `GetSpaces` + `SwitchSpace`) is the backstop.
    /// Only the caller's OWN connection is ever steered (never a sibling's).
    async fn repoint_caller_after_close(
        &self,
        session: &str,
        connection_id: &str,
        backend: &Arc<dyn MuxBackend>,
        bare: &str,
    ) {
        let Some(sender) = self.resolve_space_relay(session, connection_id) else {
            // No live relay for this connection (e.g. control-plane-only close);
            // nothing to re-point. The client recovers on its next GetSpaces.
            return;
        };

        // Pick the daemon's new active-or-first workspace (post-close). Blocking
        // herdr `workspace.list` → spawn_blocking.
        let target = {
            let backend = backend.clone();
            let bare = bare.to_owned();
            match tokio::task::spawn_blocking(move || backend.list_spaces(&bare)).await {
                Ok(Ok(spaces)) => spaces
                    .iter()
                    .find(|s| s.active)
                    .or_else(|| spaces.first())
                    .map(|s| s.id.clone()),
                Ok(Err(e)) => {
                    log::warn!("CloseSpace: re-point list_spaces failed for '{session}': {e:#}");
                    None
                }
                Err(e) => {
                    log::warn!("CloseSpace: re-point list task panicked for '{session}': {e}");
                    None
                }
            }
        };
        let Some(target) = target else {
            log::warn!("CloseSpace: no workspace to re-point '{session}' onto after close");
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

// ─── Free validation / mapping helpers ──────────────────────────────────────────

/// True when closing one more space would leave the daemon with zero workspaces.
///
/// `count` is the number of spaces present *before* the close. `<= 1` because
/// closing the only remaining space leaves none (S-M1).
fn would_close_last_space(count: usize) -> bool {
    count <= 1
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
    use super::{validate_space_id, validate_space_label, would_close_last_space};
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
}
