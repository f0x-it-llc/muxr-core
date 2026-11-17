//! Tab operation RPC implementations: new, close, go_to, rename.

use tonic::{Request, Response, Status};

use crate::proto::{ActionAck as ProtoAck, NewTabReq, RenameTabReq, TabTarget};

use super::MuxrService;
use super::helpers::{
    CreateRoute, reject_if_read_only, route_create, run_action, session_is_read_only, short_conn,
    try_route_control, validate_display_name,
};

impl MuxrService {
    // ── Tab ops (D2) ──────────────────────────────────────────────────────────

    /// Open a new tab; new tab id/name surface in ActionAck.info. MUTATING.
    ///
    /// With a `connection_id` naming the caller's live relay, the tab is created
    /// THROUGH that relay (`route_create`): on herdr in that connection's space,
    /// without moving herdr's focus, and the connection's stream follows the new
    /// tab's pane (`info` = the new tab id). A non-empty id naming no live relay
    /// fails closed on herdr. An empty id, or a backend whose relay cannot create
    /// (zellij), takes the ephemeral backend path — today's behaviour.
    pub(super) async fn new_tab_impl(
        &self,
        request: Request<NewTabReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "NewTab")?;
        let req = request.into_inner();
        let (backend, session) = self.resolve_session(&req.session)?;
        let tab_name = if req.tab_name.is_empty() {
            None
        } else {
            validate_display_name(&req.tab_name, "tab")?;
            Some(req.tab_name)
        };
        // FS3: full connection_id must not appear in info/warn logs.
        log::info!(
            "NewTab: session='{session}' name={tab_name:?} connection_id={}…",
            short_conn(&req.connection_id)
        );
        let routed_name = tab_name.clone();
        if let CreateRoute::Answered(ack) = route_create(
            &self.control,
            &req.session,
            &req.connection_id,
            "NewTab",
            move |reply| crate::relay::RelayControl::NewTab {
                name: routed_name,
                reply,
            },
            |view| view.tab_id.to_string(),
        )
        .await
        {
            log::info!(
                "NewTab: routed via relay client (session='{session}') ok={} {}",
                ack.ok,
                ack.error
            );
            return Ok(Response::new(ack));
        }
        run_action("NewTab", move || backend.new_tab(&session, tab_name)).await
    }

    /// Close a tab by id. MUTATING (read-only rejected).
    pub(super) async fn close_tab_impl(
        &self,
        request: Request<TabTarget>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "CloseTab")?;
        let req = request.into_inner();
        let tab_id = req.tab_id;
        let (backend, session) = self.resolve_session(&req.session)?;
        log::info!("CloseTab: session='{session}' tab_id={tab_id}");
        run_action("CloseTab", move || backend.close_tab(&session, tab_id)).await
    }

    /// Switch focus to a tab by id. Permitted for read-only sessions (view-only —
    /// changes only which tab THIS viewer looks at, not session content).
    pub(super) async fn go_to_tab_impl(
        &self,
        request: Request<TabTarget>,
    ) -> Result<Response<ProtoAck>, Status> {
        // GoToTab is permitted for read-only tokens (no `reject_if_read_only`
        // call), so thread the caller's read-only status into
        // `try_route_control` ourselves — read BEFORE `into_inner()` drops the
        // extension — so a read-only caller with no exact connection_id match
        // is never steered to a co-attached WRITABLE relay via the session
        // fallback.
        let read_only = session_is_read_only(&request);
        let req = request.into_inner();
        let connection_id = req.connection_id.clone();
        let tab_id = req.tab_id;
        let (backend, session) = self.resolve_session(&req.session)?;
        // FS3: full connection_id must not appear in info/warn logs.
        log::info!(
            "GoToTab: session='{session}' tab_id={tab_id} connection_id={}…",
            short_conn(&connection_id)
        );
        log::debug!("GoToTab: session='{session}' tab_id={tab_id} connection_id='{connection_id}'");
        // Route through the live relay client if attached, so the tab switch
        // applies to the *rendering* client (deterministic, no ephemeral).
        // connection_id targets the exact relay that sent the request; falls
        // back to any relay for the session when id is absent/stale.
        //
        // Option C: the control registry stores the opaque id the client echoes,
        // so we route with the original `req.session` (the id), NOT the stripped
        // bare name — `entry.session == req.session` stays an id-vs-id match.
        if let Some(resp) = try_route_control(
            &self.control,
            &req.session,
            &connection_id,
            read_only,
            crate::relay::RelayControl::SwitchTab(tab_id),
        ) {
            log::info!("GoToTab: routed via relay client (session='{session}', tab_id={tab_id})");
            return Ok(resp);
        }
        run_action("GoToTab", move || backend.go_to_tab(&session, tab_id)).await
    }

    /// Rename a tab by id. MUTATING (read-only rejected).
    pub(super) async fn rename_tab_impl(
        &self,
        request: Request<RenameTabReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "RenameTab")?;
        let req = request.into_inner();
        let (backend, session) = self.resolve_session(&req.session)?;
        let tab_id = req.tab_id;
        let name = req.name;
        validate_display_name(&name, "tab")?;
        log::info!("RenameTab: session='{session}' tab_id={tab_id} name='{name}'");
        run_action("RenameTab", move || {
            backend.rename_tab(&session, tab_id, name)
        })
        .await
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Paired read-only-gate tests for the RPC un-gate (read-only→explorer, Phase
    //! 1): `GoToTab` is now permitted for a read-only session token, while
    //! `NewTab`/`CloseTab`/`RenameTab` stay refused. A weakened trust boundary
    //! without a paired test is Critical per `docs/REVIEW_FOCUS.md`.
    //!
    //! A second round (the caller-isolation fix) adds routing-level coverage:
    //! `GoToTab` for a read-only caller must route ONLY to that caller's own
    //! relay (exact `connection_id` match) and must never fall back to a
    //! co-attached WRITABLE relay, nor fall through to the ephemeral backend
    //! path — see the `go_to_tab_read_only_caller_*` tests below.

    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tonic::{Code, Request};

    use crate::auth::SessionReadOnly;
    use crate::cli::BackendKind;
    use crate::grpc::MuxrService;
    use crate::multiplexer::{
        ActionAck, BackendSet, DualHandle, LayoutSnapshot, MuxBackend, PaneRef, ResizeDir,
        ResizeKind, ScrollDir,
    };
    use crate::proto::{NewTabReq, RenameTabReq, TabTarget};
    use crate::relay::{ControlEntry, RelayControl};

    /// A tab backend that always acknowledges `go_to_tab` successfully — used to
    /// prove a request reached the backend (i.e. passed the read-only gate)
    /// rather than being rejected by it. Every other method is out of scope for
    /// these tests and panics if reached.
    #[derive(Debug, Default)]
    struct StubTabs;

    impl MuxBackend for StubTabs {
        fn go_to_tab(&self, _session: &str, _tab_id: u64) -> anyhow::Result<ActionAck> {
            Ok(ActionAck {
                ok: true,
                error: None,
                info: None,
            })
        }

        // ── Everything else is out of scope for these tests ──────────────────
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
            "stub-tabs".to_owned()
        }
    }

    fn service() -> MuxrService {
        let backend: Arc<dyn MuxBackend> = Arc::new(StubTabs);
        MuxrService::with_backends(BackendSet::single(BackendKind::Zellij, backend))
    }

    fn tab_target(tab_id: u64, read_only: bool) -> Request<TabTarget> {
        let mut req = Request::new(TabTarget {
            session: "zellij:test".to_owned(),
            tab_id,
            connection_id: String::new(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn new_tab_req(read_only: bool) -> Request<NewTabReq> {
        let mut req = Request::new(NewTabReq {
            session: "zellij:test".to_owned(),
            tab_name: String::new(),
            connection_id: String::new(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    fn rename_tab_req(read_only: bool) -> Request<RenameTabReq> {
        let mut req = Request::new(RenameTabReq {
            session: "zellij:test".to_owned(),
            tab_id: 1,
            name: "renamed".to_owned(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    const READ_ONLY_MESSAGE: &str =
        "session token is read-only — mutating operations are not allowed";

    // ─── POSITIVE: GoToTab is permitted for a read-only session ──────────────

    /// GoToTab is view-only (re-points only the caller's own relay/view), so a
    /// read-only token must reach relay routing instead of being rejected by
    /// the gate — never `Status::PermissionDenied`. No relay is attached in
    /// this test, so the ack legitimately fails "reattach required": a
    /// read-only caller's navigation must never fall through to the
    /// ephemeral/session-level backend path (this card's fix) just because no
    /// relay happens to be live. See
    /// `go_to_tab_read_only_caller_with_exact_connection_id_routes_to_own_relay`
    /// for the case where it does succeed.
    #[tokio::test]
    async fn go_to_tab_is_permitted_for_a_read_only_session() {
        let ack = service()
            .go_to_tab_impl(tab_target(1, true))
            .await
            .expect("GoToTab must not be rejected by the read-only gate")
            .into_inner();
        assert!(
            !ack.ok,
            "no relay attached — must fail closed, not silently reach the backend"
        );
        assert!(
            ack.error.contains("reattach required"),
            "error: {}",
            ack.error
        );
    }

    /// A read-only caller whose OWN relay is live (exact `connection_id`
    /// match) is routed to it and succeeds — proving GoToTab is genuinely
    /// usable for read-only navigation, not merely "not rejected".
    #[tokio::test]
    async fn go_to_tab_read_only_caller_with_exact_connection_id_routes_to_own_relay() {
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

        let mut req = Request::new(TabTarget {
            session: "zellij:test".to_owned(),
            tab_id: 3,
            connection_id: "conn-self".to_owned(),
        });
        req.extensions_mut().insert(SessionReadOnly(true));

        let ack = service
            .go_to_tab_impl(req)
            .await
            .expect("GoToTab must not be rejected by the read-only gate")
            .into_inner();

        assert!(ack.ok, "exact own-relay match must succeed: {}", ack.error);
        match rx.try_recv() {
            Ok(RelayControl::SwitchTab(3)) => {}
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
    async fn go_to_tab_read_only_caller_without_connection_id_never_reaches_a_co_attached_writable_relay()
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

        let mut req = Request::new(TabTarget {
            session: "zellij:test".to_owned(),
            tab_id: 1,
            connection_id: String::new(), // absent — read-only caller
        });
        req.extensions_mut().insert(SessionReadOnly(true));

        let ack = service
            .go_to_tab_impl(req)
            .await
            .expect("GoToTab must not be rejected by the read-only gate")
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
            "the co-attached writable relay must NOT receive GoToTab routed from a \
             read-only caller with no connection_id"
        );
    }

    // ─── NEGATIVE: every other tab RPC stays refused ─────────────────────────

    #[tokio::test]
    async fn new_tab_still_rejects_a_read_only_session() {
        let err = service()
            .new_tab_impl(new_tab_req(true))
            .await
            .expect_err("NewTab must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn close_tab_still_rejects_a_read_only_session() {
        let err = service()
            .close_tab_impl(tab_target(1, true))
            .await
            .expect_err("CloseTab must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    #[tokio::test]
    async fn rename_tab_still_rejects_a_read_only_session() {
        let err = service()
            .rename_tab_impl(rename_tab_req(true))
            .await
            .expect_err("RenameTab must stay refused for read-only sessions");
        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
    }

    // ─── NewTab: routed through the caller's relay ───────────────────────────

    use crate::grpc::helpers::create_test_support::{
        self as support, HERDR, RecordingCreates, ZELLIJ, answer_create, created, relay,
    };
    use crate::multiplexer::CreateUnsupported;

    fn new_tab_on(session: &str, connection_id: &str, read_only: bool) -> Request<NewTabReq> {
        let mut req = Request::new(NewTabReq {
            session: session.to_owned(),
            tab_name: "logs".to_owned(),
            connection_id: connection_id.to_owned(),
        });
        req.extensions_mut().insert(SessionReadOnly(read_only));
        req
    }

    /// A live connection_id: the tab is created through THAT relay (in its
    /// space, not herdr's focused one), and the ack names the new tab.
    #[tokio::test]
    async fn new_tab_with_a_live_connection_id_is_created_through_that_relay() {
        let backend = Arc::new(RecordingCreates::default());
        let service = support::service(BackendKind::Herdr, &backend);
        let mut desktop_side = relay(&service, "conn-a", HERDR, false);
        let caller = answer_create(
            relay(&service, "conn-b", HERDR, false),
            Some(Ok(created("ws-b", 9, 51))),
        );

        let ack = service
            .new_tab_impl(new_tab_on(HERDR, "conn-b", false))
            .await
            .expect("a routed create is an ack")
            .into_inner();

        assert!(ack.ok, "routed create must succeed: {}", ack.error);
        assert_eq!(ack.info, "9", "the ack names the new tab");
        assert_eq!(caller.await.unwrap(), "NewTab name=Some(\"logs\")");
        assert!(backend.calls().is_empty());
        assert!(desktop_side.try_recv().is_err());
    }

    /// An empty connection_id is today's request: the backend path, unchanged.
    #[tokio::test]
    async fn new_tab_with_an_empty_connection_id_takes_the_backend_path_unchanged() {
        let backend = Arc::new(RecordingCreates::default());
        let service = support::service(BackendKind::Herdr, &backend);
        let mut attached = relay(&service, "conn-a", HERDR, false);

        let ack = service
            .new_tab_impl(new_tab_on(HERDR, "", false))
            .await
            .unwrap()
            .into_inner();

        assert!(ack.ok);
        assert_eq!(
            backend.calls(),
            vec!["new_tab herdr name=Some(\"logs\")".to_owned()]
        );
        assert_eq!(ack.info, "backend new_tab herdr name=Some(\"logs\")");
        assert!(attached.try_recv().is_err());
    }

    /// A non-empty connection_id naming no live relay fails closed on herdr.
    #[tokio::test]
    async fn new_tab_with_a_stale_connection_id_fails_closed_on_herdr() {
        let backend = Arc::new(RecordingCreates::default());
        let service = support::service(BackendKind::Herdr, &backend);
        let mut live = relay(&service, "conn-live", HERDR, false);

        let ack = service
            .new_tab_impl(new_tab_on(HERDR, "conn-gone", false))
            .await
            .unwrap()
            .into_inner();

        assert!(!ack.ok);
        assert_eq!(ack.error, "reattach required (no matching connection)");
        assert!(backend.calls().is_empty(), "nothing may be created");
        assert!(live.try_recv().is_err());
    }

    /// The read-only gate still runs first, before any routing.
    #[tokio::test]
    async fn new_tab_read_only_with_a_live_relay_is_refused_before_routing() {
        let backend = Arc::new(RecordingCreates::default());
        let service = support::service(BackendKind::Herdr, &backend);
        let mut own = relay(&service, "conn-ro", HERDR, true);

        let err = service
            .new_tab_impl(new_tab_on(HERDR, "conn-ro", true))
            .await
            .expect_err("NewTab must stay refused for read-only sessions");

        assert_eq!(err.code(), Code::PermissionDenied);
        assert_eq!(err.message(), READ_ONLY_MESSAGE);
        assert!(own.try_recv().is_err());
        assert!(backend.calls().is_empty());
    }

    /// zellij: the relay cannot create, so the backend path runs as always.
    #[tokio::test]
    async fn new_tab_on_zellij_falls_back_when_the_relay_cannot_create() {
        let backend = Arc::new(RecordingCreates::default());
        let service = support::service(BackendKind::Zellij, &backend);
        let zellij_relay = answer_create(
            relay(&service, "conn-z", ZELLIJ, false),
            Some(Err(anyhow::Error::new(CreateUnsupported { op: "NewTab" }))),
        );

        let ack = service
            .new_tab_impl(new_tab_on(ZELLIJ, "conn-z", false))
            .await
            .unwrap()
            .into_inner();

        zellij_relay.await.unwrap();
        assert!(ack.ok);
        assert_eq!(
            backend.calls(),
            vec!["new_tab test name=Some(\"logs\")".to_owned()]
        );
    }
}
