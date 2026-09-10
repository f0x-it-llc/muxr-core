//! Pane mutation RPC implementations: write, focus, close, new, rename, resize,
//! toggle floating/fullscreen, scroll.

use tonic::{Request, Response, Status};

use crate::multiplexer::{ResizeDir, ResizeKind as NeutralResizeKind, ScrollDir};
use crate::proto::{
    ActionAck as ProtoAck, NewPaneReq, PaneTarget, RenamePaneReq, ResizeKind, ResizePaneReq,
    ScrollDirection, ScrollReq, ToggleFullscreenReq, WriteToPaneReq,
};

use super::MuxrService;
use super::helpers::{
    pane_ref, reject_if_read_only, run_action, session_is_read_only, short_conn, try_route_control,
    validate_display_name,
};

/// Upper bound on a single `WriteToPane` payload (1 MiB).  Guards against a
/// client pushing an unbounded write into the session IPC channel.
const MAX_WRITE_TO_PANE_BYTES: usize = 1024 * 1024;

impl MuxrService {
    // ── Pane ops (D1) ─────────────────────────────────────────────────────────

    /// Write raw bytes to a specific pane. MUTATING (read-only rejected).
    pub(super) async fn write_to_pane_impl(
        &self,
        request: Request<WriteToPaneReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "WriteToPane")?;
        let req = request.into_inner();
        // Cap payload size to avoid a single RPC pushing an unbounded write into
        // the session IPC channel (review minor).
        if req.data.len() > MAX_WRITE_TO_PANE_BYTES {
            return Err(Status::invalid_argument(format!(
                "WriteToPane: payload {} bytes exceeds the {} byte limit",
                req.data.len(),
                MAX_WRITE_TO_PANE_BYTES
            )));
        }
        let target = req
            .target
            .ok_or_else(|| Status::invalid_argument("WriteToPane: target is required"))?;
        let pane = pane_ref(&target);
        let (backend, session) = self.resolve_session(&target.session)?;
        log::info!(
            "WriteToPane: session='{session}' pane={pane:?} ({} bytes)",
            req.data.len()
        );
        let data = req.data;
        run_action("WriteToPane", move || {
            backend.write_to_pane(&session, pane, data)
        })
        .await
    }

    /// Focus a specific pane. Allowed for read-only tokens.
    pub(super) async fn focus_pane_impl(
        &self,
        request: Request<PaneTarget>,
    ) -> Result<Response<ProtoAck>, Status> {
        // Focus is a read — no read-only gate. Still thread the caller's
        // read-only status into `try_route_control` (the extension must be
        // read BEFORE `into_inner()` drops it) so a read-only caller with no
        // exact connection_id match is never steered to a co-attached
        // WRITABLE relay via the session fallback.
        let read_only = session_is_read_only(&request);
        let target = request.into_inner();
        let connection_id = target.connection_id.clone();
        let pane = pane_ref(&target);
        let (backend, session) = self.resolve_session(&target.session)?;
        // FS3: full connection_id must not appear in info/warn logs.
        log::info!(
            "FocusPane: session='{session}' pane={pane:?} connection_id={}…",
            short_conn(&connection_id)
        );
        log::debug!("FocusPane: session='{session}' pane={pane:?} connection_id='{connection_id}'");
        // Route through the live relay client if attached, so focus applies to
        // the rendering client (and re-points the single-pane sub).
        // connection_id targets the exact relay that sent the request.
        // RelayControl::FocusPane carries the neutral PaneRef directly (P1.03).
        // Option C: route with the opaque id the client echoed (target.session),
        // which is what the control registry stores — not the stripped bare name.
        if let Some(resp) = try_route_control(
            &self.control,
            &target.session,
            &connection_id,
            read_only,
            crate::relay::RelayControl::FocusPane(pane),
        ) {
            log::info!("FocusPane: routed via relay client (session='{session}')");
            return Ok(resp);
        }
        run_action("FocusPane", move || backend.focus_pane(&session, pane)).await
    }

    /// Close a specific pane. MUTATING (read-only rejected).
    pub(super) async fn close_pane_impl(
        &self,
        request: Request<PaneTarget>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "ClosePane")?;
        let target = request.into_inner();
        let pane = pane_ref(&target);
        let (backend, session) = self.resolve_session(&target.session)?;
        log::info!("ClosePane: session='{session}' pane={pane:?}");
        run_action("ClosePane", move || backend.close_pane(&session, pane)).await
    }

    /// Open a new pane; the new pane id surfaces in `ActionAck.info`. MUTATING.
    pub(super) async fn new_pane_impl(
        &self,
        request: Request<NewPaneReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "NewPane")?;
        let req = request.into_inner();
        let (backend, session) = self.resolve_session(&req.session)?;
        let floating = req.floating;
        let pane_name = if req.pane_name.is_empty() {
            None
        } else {
            validate_display_name(&req.pane_name, "pane")?;
            Some(req.pane_name)
        };
        log::info!("NewPane: session='{session}' floating={floating} name={pane_name:?}");
        run_action("NewPane", move || {
            backend.new_pane(&session, floating, pane_name)
        })
        .await
    }

    /// Rename a specific pane. MUTATING (read-only rejected).
    pub(super) async fn rename_pane_impl(
        &self,
        request: Request<RenamePaneReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "RenamePane")?;
        let req = request.into_inner();
        let target = req
            .target
            .ok_or_else(|| Status::invalid_argument("RenamePane: target is required"))?;
        let pane = pane_ref(&target);
        let (backend, session) = self.resolve_session(&target.session)?;
        let name = req.name;
        // An empty name is allowed here (resets the pane to its default title);
        // only bound/sanitise a non-empty client string.
        if !name.is_empty() {
            validate_display_name(&name, "pane")?;
        }
        log::info!("RenamePane: session='{session}' pane={pane:?} name='{name}'");
        run_action("RenamePane", move || {
            backend.rename_pane(&session, pane, name)
        })
        .await
    }

    /// Resize a specific pane. MUTATING (read-only rejected).
    pub(super) async fn resize_pane_impl(
        &self,
        request: Request<ResizePaneReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "ResizePane")?;
        let req = request.into_inner();
        let target = req
            .target
            .ok_or_else(|| Status::invalid_argument("ResizePane: target is required"))?;
        let pane = pane_ref(&target);
        let (backend, session) = self.resolve_session(&target.session)?;

        // Convert proto ResizeKind → neutral ResizeKind.
        let resize_kind = match ResizeKind::try_from(req.resize) {
            Ok(ResizeKind::Decrease) => NeutralResizeKind::Decrease,
            _ => NeutralResizeKind::Increase,
        };
        // ResizeDirection: 0 = UNSPECIFIED → None (uniform resize).
        let resize_dir: Option<ResizeDir> = match req.direction {
            1 => Some(ResizeDir::Left),
            2 => Some(ResizeDir::Right),
            3 => Some(ResizeDir::Up),
            4 => Some(ResizeDir::Down),
            _ => None,
        };
        log::info!(
            "ResizePane: session='{session}' pane={pane:?} resize={resize_kind:?} \
             dir={resize_dir:?}"
        );
        run_action("ResizePane", move || {
            backend.resize_pane(&session, pane, resize_kind, resize_dir)
        })
        .await
    }

    /// Toggle a pane between floating and embedded. MUTATING (read-only rejected).
    pub(super) async fn toggle_pane_floating_impl(
        &self,
        request: Request<PaneTarget>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "TogglePaneFloating")?;
        let target = request.into_inner();
        let pane = pane_ref(&target);
        let (backend, session) = self.resolve_session(&target.session)?;
        log::info!("TogglePaneFloating: session='{session}' pane={pane:?}");
        run_action("TogglePaneFloating", move || {
            backend.toggle_pane_floating(&session, pane)
        })
        .await
    }

    /// Toggle fullscreen for a pane. MUTATING (read-only rejected).
    pub(super) async fn toggle_pane_fullscreen_impl(
        &self,
        request: Request<ToggleFullscreenReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "TogglePaneFullscreen")?;
        // Read BEFORE `into_inner()` drops the extension. See the routing note
        // below for why this is the real value and not a literal `false`.
        let read_only = session_is_read_only(&request);
        let req = request.into_inner();
        let target = req
            .target
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("TogglePaneFullscreen: missing target"))?;
        let connection_id = target.connection_id.clone();
        let pane = pane_ref(target);
        let (backend, session) = self.resolve_session(&target.session)?;
        // Bug 2c: forward the client's floating hint so the relay can skip a
        // synchronous IPC query on its hot path. Only trust the hint when the
        // caller explicitly attests it via `has_floating_hint` — proto3 bools
        // default to false, so an all-false hint from a target-only request must
        // NOT be read as "definitely tiled" (that would mis-route a floating
        // pane). Without the flag we pass `None`, and the relay runs the live
        // query as a safety net.
        let hint = if req.has_floating_hint {
            Some(crate::relay::FloatingHint {
                target_is_floating: req.target_is_floating,
                floating_visible: req.floating_visible,
                target_is_focused_floating: req.target_is_focused_floating,
            })
        } else {
            None
        };
        // FS3: full connection_id must not appear in info/warn logs.
        log::info!(
            "TogglePaneFullscreen: session='{session}' pane={pane:?} hint={hint:?} \
             connection_id={}…",
            short_conn(&connection_id)
        );
        log::debug!(
            "TogglePaneFullscreen: session='{session}' pane={pane:?} hint={hint:?} \
             connection_id='{connection_id}'"
        );
        // Route through the live relay client if attached so the fullscreen
        // toggle applies to the *rendering* client (is_cli_client:false).
        // connection_id targets the exact relay that sent the request.
        // RelayControl::ToggleFullscreen carries the neutral PaneRef directly (P1.03).
        // Option C: route with the opaque id the client echoed (target.session) —
        // what the control registry stores — not the stripped bare name.
        // `reject_if_read_only` above already denies a read-only or
        // absent-extension caller, so in practice `read_only` is false here.
        // Pass the real value rather than a literal anyway: hardcoding `false`
        // asserts "writable caller" with no link to that gate, so removing or
        // relaxing the gate would silently restore the writable-session
        // fallback for a read-only caller, with nothing to catch it.
        if let Some(resp) = try_route_control(
            &self.control,
            &target.session,
            &connection_id,
            read_only,
            crate::relay::RelayControl::ToggleFullscreen { pane, hint },
        ) {
            log::info!("TogglePaneFullscreen: routed via relay client (session='{session}')");
            return Ok(resp);
        }
        run_action("TogglePaneFullscreen", move || {
            backend.toggle_pane_fullscreen(&session, pane)
        })
        .await
    }

    // ── Scroll (D2) ───────────────────────────────────────────────────────────

    /// Scroll a specific pane. Allowed for read-only tokens.
    pub(super) async fn scroll_pane_impl(
        &self,
        request: Request<ScrollReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        // NOTE: scroll is explicitly allowed for read-only tokens — no gate here.
        let req = request.into_inner();
        let target = req
            .target
            .ok_or_else(|| Status::invalid_argument("ScrollPane: target is required"))?;
        let pane = pane_ref(&target);
        let (backend, session) = self.resolve_session(&target.session)?;
        let dir = match ScrollDirection::try_from(req.direction) {
            Ok(ScrollDirection::Down) => ScrollDir::Down,
            Ok(ScrollDirection::ToTop) => ScrollDir::ToTop,
            Ok(ScrollDirection::ToBottom) => ScrollDir::ToBottom,
            Ok(ScrollDirection::PageUp) => ScrollDir::PageUp,
            Ok(ScrollDirection::PageDown) => ScrollDir::PageDown,
            Ok(ScrollDirection::HalfPageUp) => ScrollDir::HalfPageUp,
            Ok(ScrollDirection::HalfPageDown) => ScrollDir::HalfPageDown,
            // Up = 0 (default) and anything unrecognised → Up
            _ => ScrollDir::Up,
        };
        log::info!("ScrollPane: session='{session}' pane={pane:?} dir={dir:?}");
        run_action("ScrollPane", move || {
            backend.scroll_pane(&session, pane, dir)
        })
        .await
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Paired read-only-gate tests for the RPC un-gate (read-only→explorer, Phase
    //! 1): every mutating pane RPC — INCLUDING `ResizePane`, which an earlier
    //! round of this card wrongly un-gated — stays refused for a read-only
    //! session token. `resize_pane_impl` has no relay/connection-scoped routing:
    //! it calls straight through to the session-scoped backend, so it mutates
    //! the shared tab layout every attached client renders, not just the
    //! caller's own view. `ScrollPane` is unaffected by this card (no relay
    //! routing at all) and is not re-tested here. A weakened trust boundary
    //! without a paired test is Critical per `docs/REVIEW_FOCUS.md`.
    //!
    //! `FocusPane` has no read-only *gate* (unaffected there), but a later
    //! round (the caller-isolation fix) changed its relay *routing*: a
    //! read-only caller must route ONLY to that caller's own relay (exact
    //! `connection_id` match) and must never fall back to a co-attached
    //! WRITABLE relay, nor fall through to the ephemeral backend path — see the
    //! `focus_pane_read_only_caller_*` tests below.
    //!
    //! None of the gate-only tests below need to resolve a real session or
    //! backend: the read-only gate is the very first thing each handler
    //! checks, so a default [`MuxrService`] (zellij backend, never reached) is
    //! enough. The `FocusPane` routing tests never reach the real backend
    //! either — every scenario they cover resolves inside `try_route_control`
    //! before `run_action` would be called.

    use tokio::sync::mpsc;
    use tonic::{Code, Request};

    use crate::auth::SessionReadOnly;
    use crate::grpc::MuxrService;
    use crate::proto::{
        NewPaneReq, PaneTarget, RenamePaneReq, ResizeKind, ResizePaneReq, ToggleFullscreenReq,
        WriteToPaneReq,
    };
    use crate::relay::{ControlEntry, RelayControl};

    fn service() -> MuxrService {
        MuxrService::new()
    }

    fn pane_target() -> PaneTarget {
        PaneTarget {
            session: "zellij:test".to_owned(),
            pane_id: 1,
            is_plugin: false,
            connection_id: String::new(),
        }
    }

    fn resize_req(read_only: bool) -> Request<ResizePaneReq> {
        let mut req = Request::new(ResizePaneReq {
            target: Some(pane_target()),
            resize: ResizeKind::Increase as i32,
            direction: 0,
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn write_req(read_only: bool) -> Request<WriteToPaneReq> {
        let mut req = Request::new(WriteToPaneReq {
            target: Some(pane_target()),
            data: b"x".to_vec(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn pane_target_req(read_only: bool) -> Request<PaneTarget> {
        let mut req = Request::new(pane_target());
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn new_pane_req(read_only: bool) -> Request<NewPaneReq> {
        let mut req = Request::new(NewPaneReq {
            session: "zellij:test".to_owned(),
            floating: false,
            pane_name: String::new(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn rename_pane_req(read_only: bool) -> Request<RenamePaneReq> {
        let mut req = Request::new(RenamePaneReq {
            target: Some(pane_target()),
            name: "renamed".to_owned(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn toggle_fullscreen_req(read_only: bool) -> Request<ToggleFullscreenReq> {
        let mut req = Request::new(ToggleFullscreenReq {
            target: Some(pane_target()),
            target_is_floating: false,
            floating_visible: false,
            target_is_focused_floating: false,
            has_floating_hint: false,
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    const READ_ONLY_MESSAGE: &str =
        "session token is read-only — mutating operations are not allowed";

    // ─── POSITIVE: FocusPane routing for a read-only caller ──────────────────

    /// A read-only caller whose OWN relay is live (exact `connection_id`
    /// match) is routed to it and succeeds — FocusPane is genuinely usable
    /// for read-only navigation, not merely un-gated.
    #[tokio::test]
    async fn focus_pane_read_only_caller_with_exact_connection_id_routes_to_own_relay() {
        let service = service();
        let (tx, mut rx) = mpsc::unbounded_channel::<RelayControl>();
        service.control.insert(
            "conn-self".to_owned(),
            ControlEntry {
                session: "zellij:test".to_owned(),
                sender: tx,
                read_only: true,
            },
        );

        let mut req = Request::new(PaneTarget {
            session: "zellij:test".to_owned(),
            pane_id: 5,
            is_plugin: false,
            connection_id: "conn-self".to_owned(),
        });
        req.extensions_mut().insert(SessionReadOnly(true));

        let ack = service
            .focus_pane_impl(req)
            .await
            .expect("FocusPane must not be rejected by the read-only gate")
            .into_inner();

        assert!(ack.ok, "exact own-relay match must succeed: {}", ack.error);
        match rx.try_recv() {
            Ok(RelayControl::FocusPane(pane)) => assert_eq!(pane.id, 5),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    /// The single most important property this card establishes: a read-only
    /// caller that omits `connection_id` must never be routed to a co-attached
    /// WRITABLE relay on the same session (that would move a DIFFERENT
    /// client's view — the exact isolation violation relay routing exists to
    /// prevent), and must not silently succeed via the ephemeral backend path
    /// either.
    #[tokio::test]
    async fn focus_pane_read_only_caller_without_connection_id_never_reaches_a_co_attached_writable_relay()
     {
        let service = service();
        let (tx_rw, mut rx_rw) = mpsc::unbounded_channel::<RelayControl>();
        service.control.insert(
            "conn-other-writer".to_owned(),
            ControlEntry {
                session: "zellij:test".to_owned(),
                sender: tx_rw,
                read_only: false,
            },
        );

        let mut req = Request::new(PaneTarget {
            session: "zellij:test".to_owned(),
            pane_id: 5,
            is_plugin: false,
            connection_id: String::new(), // absent — read-only caller
        });
        req.extensions_mut().insert(SessionReadOnly(true));

        let ack = service
            .focus_pane_impl(req)
            .await
            .expect("FocusPane must not be rejected by the read-only gate")
            .into_inner();

        assert!(
            !ack.ok,
            "must fail closed rather than route to a sibling relay"
        );
        assert!(
            ack.error.contains("reattach required"),
            "error: {}",
            ack.error
        );
        assert!(
            rx_rw.try_recv().is_err(),
            "the co-attached writable relay must NOT receive FocusPane routed from a \
             read-only caller with no connection_id"
        );
    }

    /// Read-write behaviour is untouched: with a co-attached read-only relay
    /// present but no writable relay, a read-write caller's session fallback
    /// still finds nothing and returns `None` from `try_route_control` (the
    /// pre-existing Issue B contract, unit-tested directly in
    /// `helpers.rs::fallback_returns_none_when_only_read_only_relay_exists`);
    /// at the RPC layer that surfaces as routing being skipped, i.e. FocusPane
    /// still reaches the same route as `read_only=false` did before this
    /// card — proven directly on `try_route_control` rather than re-exercised
    /// here against the real backend (see `helpers.rs` for the full
    /// read-write no-regression coverage this card names).
    #[tokio::test]
    async fn focus_pane_read_write_caller_with_exact_connection_id_routes_to_own_relay() {
        let service = service();
        let (tx, mut rx) = mpsc::unbounded_channel::<RelayControl>();
        service.control.insert(
            "conn-rw-self".to_owned(),
            ControlEntry {
                session: "zellij:test".to_owned(),
                sender: tx,
                read_only: false,
            },
        );

        let mut req = Request::new(PaneTarget {
            session: "zellij:test".to_owned(),
            pane_id: 9,
            is_plugin: false,
            connection_id: "conn-rw-self".to_owned(),
        });
        req.extensions_mut().insert(SessionReadOnly(false));

        let ack = service
            .focus_pane_impl(req)
            .await
            .expect("FocusPane must not be rejected by the read-only gate")
            .into_inner();

        assert!(ack.ok, "exact own-relay match must succeed: {}", ack.error);
        match rx.try_recv() {
            Ok(RelayControl::FocusPane(pane)) => assert_eq!(pane.id, 9),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    // ─── NEGATIVE: every mutating pane RPC stays refused ─────────────────────

    #[tokio::test]
    async fn write_to_pane_still_rejects_a_read_only_session() {
        let err = service()
            .write_to_pane_impl(write_req(true))
            .await
            .expect_err("WriteToPane must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn close_pane_still_rejects_a_read_only_session() {
        let err = service()
            .close_pane_impl(pane_target_req(true))
            .await
            .expect_err("ClosePane must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn new_pane_still_rejects_a_read_only_session() {
        let err = service()
            .new_pane_impl(new_pane_req(true))
            .await
            .expect_err("NewPane must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn rename_pane_still_rejects_a_read_only_session() {
        let err = service()
            .rename_pane_impl(rename_pane_req(true))
            .await
            .expect_err("RenamePane must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    /// `resize_pane_impl` has no relay/connection-scoped routing — it calls
    /// straight through to the session-scoped backend, mutating the shared tab
    /// layout every attached client renders — so a read-only session must not
    /// reach it. (Re-added by rework round 1: a prior round of this card wrongly
    /// un-gated ResizePane on the mistaken premise that it was view-only.)
    #[tokio::test]
    async fn resize_pane_still_rejects_a_read_only_session() {
        let err = service()
            .resize_pane_impl(resize_req(true))
            .await
            .expect_err("ResizePane must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn toggle_pane_floating_still_rejects_a_read_only_session() {
        let err = service()
            .toggle_pane_floating_impl(pane_target_req(true))
            .await
            .expect_err("TogglePaneFloating must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn toggle_pane_fullscreen_still_rejects_a_read_only_session() {
        let err = service()
            .toggle_pane_fullscreen_impl(toggle_fullscreen_req(true))
            .await
            .expect_err("TogglePaneFullscreen must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }
}
