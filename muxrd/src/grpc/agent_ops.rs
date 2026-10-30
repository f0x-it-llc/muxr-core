//! Agent inventory RPC implementation: `ListAgents`.
//!
//! A session-wide read of every agent the owning backend reports, across all of
//! the session's spaces — not just the space a connection is viewing. It is a
//! **read**: no `reject_if_read_only` gate (matching `GetSpaces`/`GetLayout`, the
//! same disclosure surface), and it is **not relay-routed** — the backend answers
//! directly on the blocking pool and no focus moves anywhere.
//!
//! Routing and errors mirror `GetSpaces` (`space_ops`): the id resolves through
//! [`crate::multiplexer::routing`] — an unknown backend token is
//! `invalid_argument`, a known backend this server is not driving `NOT_FOUND` —
//! and a backend failure, timeout or panicked task is `internal`. A backend
//! without an agent concept (zellij) answers the trait's empty default, so the
//! RPC returns an empty [`AgentList`] rather than an error.
//!
//! The neutral → proto mapping of status, mark and summary lives in
//! [`super::helpers`] so `GetLayout` (`PaneMsg.agent`) and `ListSessions`
//! (`SessionInfo.agents`) put the exact same values on the wire.

use tonic::{Request, Response, Status};

use crate::multiplexer::{AgentSnapshot, AgentSummary, sort_agents};
use crate::proto::{AgentList, AgentMsg, SessionRef};

use super::MuxrService;
use super::helpers::{agent_status_msg, agent_summary_msg};

/// Ceiling on one backend `list_agents` round trip.
///
/// Mirrors `RELAY_QUERY_TIMEOUT` in `grpc/layout.rs`, the bound `GetLayout` puts
/// on its layout query: herdr answers from one `session.snapshot` call bounded by
/// its own per-call control timeout, so 18 s comfortably covers it while still
/// bounding a hung backend.
const LIST_AGENTS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(18);

impl MuxrService {
    // ── ListAgents ────────────────────────────────────────────────────────────

    /// List every agent in a session, most urgent first, with summary counts.
    ///
    /// Only `SessionRef.session` is read; `connection_id` and `space_id` are
    /// ignored (the inventory is session-wide and never relay-routed).
    pub(super) async fn list_agents_impl(
        &self,
        request: Request<SessionRef>,
    ) -> Result<Response<AgentList>, Status> {
        let session = request.into_inner().session;
        let (backend, bare) = self.resolve_session(&session)?;
        log::debug!("ListAgents: session='{session}'");

        // Blocking IPC (herdr `session.snapshot`) → spawn_blocking, bounded.
        let mut snapshots = match tokio::time::timeout(
            LIST_AGENTS_TIMEOUT,
            tokio::task::spawn_blocking(move || backend.list_agents(&bare)),
        )
        .await
        {
            Ok(Ok(Ok(snapshots))) => snapshots,
            Ok(Ok(Err(e))) => {
                // The cause goes to the log; the caller gets a terse status.
                log::warn!("ListAgents: list_agents failed for '{session}': {e:#}");
                return Err(Status::internal("ListAgents: backend query failed"));
            }
            Ok(Err(join_err)) => {
                log::warn!("ListAgents: list task panicked for '{session}': {join_err}");
                return Err(Status::internal("ListAgents: query task failed"));
            }
            Err(_elapsed) => {
                log::warn!(
                    "ListAgents: list_agents timed out for '{session}' after \
                     {LIST_AGENTS_TIMEOUT:?}"
                );
                return Err(Status::internal("ListAgents: backend query timed out"));
            }
        };

        let list = build_agent_list(&mut snapshots);
        log::info!(
            "ListAgents: session='{session}' → {} agent(s), {} blocked",
            list.agents.len(),
            list.summary.as_ref().map_or(0, |s| s.blocked)
        );
        Ok(Response::new(list))
    }
}

// ─── Pure mapping (also used by tests) ────────────────────────────────────────

/// Sort `snapshots` most urgent first and build the wire answer: the agents in
/// that order plus a summary whose `kinds` therefore follow priority order.
///
/// The sort is stable and the backends already return sorted lists, so for them
/// it is a no-op; it is done here too so the wire order (and the `kinds` order
/// the summary derives from it) never depends on a backend remembering to sort.
pub(super) fn build_agent_list(snapshots: &mut [AgentSnapshot]) -> AgentList {
    sort_agents(snapshots);
    let summary = AgentSummary::from(&*snapshots);
    AgentList {
        agents: snapshots.iter().map(agent_msg).collect(),
        summary: Some(agent_summary_msg(&summary)),
    }
}

/// Map one neutral [`AgentSnapshot`] to its proto [`AgentMsg`]. Optional text
/// fields become `""`; an absent completion counter is `has_completion_seq:
/// false` with `completion_seq: 0`.
pub(super) fn agent_msg(a: &AgentSnapshot) -> AgentMsg {
    AgentMsg {
        pane_id: a.pane,
        tab_id: a.tab_id,
        space_id: a.space_id.clone(),
        space_name: a.space_name.clone(),
        tab_name: a.tab_name.clone(),
        kind: a.kind.clone(),
        name: a.name.clone().unwrap_or_default(),
        display_kind: a.display_kind.clone().unwrap_or_default(),
        status: agent_status_msg(a.status) as i32,
        title: a.title.clone().unwrap_or_default(),
        summary: a.summary.clone().unwrap_or_default(),
        cwd: a.cwd.clone().unwrap_or_default(),
        focused: a.focused,
        change_seq: a.change_seq,
        completion_seq: a.completion_seq.unwrap_or_default(),
        has_completion_seq: a.completion_seq.is_some(),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tonic::Request;

    use crate::cli::BackendKind;
    use crate::multiplexer::{
        ActionAck, AgentSnapshot, AgentStatus, BackendSet, DualHandle, LayoutSnapshot, MuxBackend,
        PaneRef, ResizeDir, ResizeKind, ScrollDir,
    };
    use crate::proto::{AgentStatus as WireStatus, SessionRef};

    use super::super::MuxrService;

    // ── Shared fake (also used by `session_ops` and `token_ops` tests) ────────

    /// A herdr-like backend that reports agents. `list_agents` answers `agents`
    /// for every session (or errors when `fail` is set); `sessions` feeds
    /// `ListSessions`. Every other method is unreachable from these tests.
    #[derive(Debug, Default)]
    pub(in crate::grpc) struct AgentsBackend {
        pub(in crate::grpc) sessions: Vec<(String, u64, bool)>,
        pub(in crate::grpc) agents: Vec<AgentSnapshot>,
        pub(in crate::grpc) fail: bool,
    }

    impl MuxBackend for AgentsBackend {
        fn supports_agents(&self) -> bool {
            true
        }
        fn list_agents(&self, _: &str) -> anyhow::Result<Vec<AgentSnapshot>> {
            if self.fail {
                anyhow::bail!("agents: daemon unreachable at /run/stub.sock");
            }
            Ok(self.agents.clone())
        }
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
            "agents-stub".to_owned()
        }
    }

    /// A fully-populated snapshot; tests override what they assert on.
    pub(in crate::grpc) fn snapshot(
        pane: u32,
        kind: &str,
        status: AgentStatus,
        change_seq: u64,
    ) -> AgentSnapshot {
        AgentSnapshot {
            pane,
            tab_id: u64::from(pane) * 100,
            space_id: format!("ws-{pane}"),
            space_name: format!("space {pane}"),
            tab_name: format!("tab {pane}"),
            kind: kind.to_owned(),
            name: None,
            display_kind: None,
            status,
            title: None,
            summary: None,
            cwd: None,
            focused: false,
            change_seq,
            completion_seq: None,
        }
    }

    /// The two agents the herdr fake reports, already most-urgent first: a
    /// blocked one (every optional field set) and a working one (none set).
    pub(in crate::grpc) fn two_agents() -> Vec<AgentSnapshot> {
        let mut blocked = snapshot(7, "cli-a", AgentStatus::Blocked, 3);
        blocked.name = Some("reviewer".to_owned());
        blocked.display_kind = Some("CLI A".to_owned());
        blocked.title = Some("fix the build".to_owned());
        blocked.summary = Some("waiting for approval".to_owned());
        blocked.cwd = Some("/src/app".to_owned());
        blocked.focused = true;
        blocked.completion_seq = Some(2);
        let working = snapshot(9, "cli-b", AgentStatus::Working, 11);
        vec![blocked, working]
    }

    fn herdr_service(backend: AgentsBackend) -> MuxrService {
        MuxrService::with_backends(BackendSet::single(BackendKind::Herdr, Arc::new(backend)))
    }

    fn session_ref(session: &str) -> Request<SessionRef> {
        Request::new(SessionRef {
            session: session.to_owned(),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn list_agents_returns_the_backend_agents_in_order_with_summary() {
        let service = herdr_service(AgentsBackend {
            agents: two_agents(),
            ..Default::default()
        });

        let list = service
            .list_agents_impl(session_ref("herdr:herdr"))
            .await
            .expect("ListAgents must succeed")
            .into_inner();

        assert_eq!(list.agents.len(), 2);
        let (first, second) = (&list.agents[0], &list.agents[1]);

        // Order: the blocked agent first, exactly as the backend reported it.
        assert_eq!(first.pane_id, 7);
        assert_eq!(second.pane_id, 9);

        // Every field of the fully-populated snapshot reaches the wire.
        assert_eq!(first.tab_id, 700);
        assert_eq!(first.space_id, "ws-7");
        assert_eq!(first.space_name, "space 7");
        assert_eq!(first.tab_name, "tab 7");
        assert_eq!(first.kind, "cli-a");
        assert_eq!(first.name, "reviewer");
        assert_eq!(first.display_kind, "CLI A");
        assert_eq!(first.status, WireStatus::Blocked as i32);
        assert_eq!(first.title, "fix the build");
        assert_eq!(first.summary, "waiting for approval");
        assert_eq!(first.cwd, "/src/app");
        assert!(first.focused);
        assert_eq!(first.change_seq, 3);
        assert_eq!(first.completion_seq, 2);
        assert!(first.has_completion_seq);

        // Absent optionals become empty strings and an absent completion counter.
        assert_eq!(second.status, WireStatus::Working as i32);
        assert_eq!(second.name, "");
        assert_eq!(second.display_kind, "");
        assert_eq!(second.title, "");
        assert_eq!(second.summary, "");
        assert_eq!(second.cwd, "");
        assert!(!second.focused);
        assert_eq!(second.change_seq, 11);
        assert_eq!(second.completion_seq, 0);
        assert!(!second.has_completion_seq);

        let summary = list.summary.expect("summary is always set");
        assert_eq!(
            (
                summary.total,
                summary.blocked,
                summary.done,
                summary.working,
                summary.idle,
                summary.unknown
            ),
            (2, 1, 0, 1, 0, 0)
        );
        assert_eq!(summary.kinds, vec!["cli-a".to_owned(), "cli-b".to_owned()]);
    }

    #[tokio::test]
    async fn list_agents_sorts_an_unsorted_backend_answer_most_urgent_first() {
        let mut reversed = two_agents();
        reversed.reverse();
        let service = herdr_service(AgentsBackend {
            agents: reversed,
            ..Default::default()
        });

        let list = service
            .list_agents_impl(session_ref("herdr:herdr"))
            .await
            .expect("ListAgents must succeed")
            .into_inner();

        let panes: Vec<u32> = list.agents.iter().map(|a| a.pane_id).collect();
        assert_eq!(panes, vec![7, 9], "blocked before working");
        assert_eq!(
            list.summary.expect("summary").kinds,
            vec!["cli-a".to_owned(), "cli-b".to_owned()],
            "kinds follow priority order, not backend order"
        );
    }

    #[tokio::test]
    async fn zellij_only_server_answers_an_empty_agent_list() {
        // `MuxrService::new()` drives the real zellij backend, whose
        // `list_agents` is the trait's empty default (no I/O).
        let list = MuxrService::new()
            .list_agents_impl(session_ref("zellij:dev"))
            .await
            .expect("a backend without agents answers empty, not an error")
            .into_inner();

        assert!(list.agents.is_empty());
        let summary = list.summary.expect("summary is set even when empty");
        assert_eq!(summary.total, 0);
        assert!(summary.kinds.is_empty());
    }

    #[tokio::test]
    async fn unknown_backend_token_is_invalid_argument() {
        let err = MuxrService::new()
            .list_agents_impl(session_ref("tmux:dev"))
            .await
            .expect_err("unknown backend token must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn backend_not_running_is_not_found() {
        let err = MuxrService::new()
            .list_agents_impl(session_ref("herdr:herdr"))
            .await
            .expect_err("herdr is not served by a zellij-only server");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn backend_failure_is_a_terse_internal() {
        let service = herdr_service(AgentsBackend {
            fail: true,
            ..Default::default()
        });

        let err = service
            .list_agents_impl(session_ref("herdr:herdr"))
            .await
            .expect_err("a failing backend must surface an error");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(
            !err.message().contains("/run/stub.sock"),
            "the backend's error chain stays in the log: {}",
            err.message()
        );
    }
}
