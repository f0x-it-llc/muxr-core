//! Agent inventory RPC implementations: `ListAgents` and `WatchAgents`.
//!
//! `ListAgents` is a session-wide read of every agent the owning backend reports,
//! across all of the session's spaces — not just the space a connection is
//! viewing. It is a **read**: no `reject_if_read_only` gate (matching
//! `GetSpaces`/`GetLayout`, the same disclosure surface), and it is **not
//! relay-routed** — the backend answers directly on the blocking pool and no
//! focus moves anywhere.
//!
//! Routing and errors mirror `GetSpaces` (`space_ops`): the id resolves through
//! [`crate::multiplexer::routing`]. The full code set: an unknown backend token
//! is `INVALID_ARGUMENT`; a known backend this server is not driving, or a herdr
//! session name that is not the `herdr` sentinel (a typed `UnknownSession`), is
//! `NOT_FOUND`; a backend failure, timeout or panicked task is `INTERNAL`. A
//! backend without an agent concept (zellij) answers the trait's empty default
//! for **any** name, since that default performs no existence check — an
//! accepted, documented divergence — so the RPC returns an empty [`AgentList`]
//! rather than an error.
//!
//! ## `WatchAgents`
//!
//! The same read as a server stream: one `snapshot` (exactly `ListAgents`'
//! answer), then deltas driven by the internal event bus
//! ([`crate::multiplexer::events`]). The handler subscribes to the bus **before**
//! taking the snapshot, so nothing published in between is lost (an event that
//! the snapshot already reflects is re-sent as an idempotent `changed`).
//!
//! - An `AgentStatusChanged` for a pane the stream already holds, whose kind, tab
//!   and space agree with what it holds, becomes a `changed` at once: the held
//!   row with the event's status (and title/name when it carries them). An event
//!   that lacks any of those, or disagrees, marks the stream for a re-read: a
//!   fresh `list_agents` on the blocking pool, **at most one per
//!   [`WATCH_LIST_DEBOUNCE`] per stream**, whose answer is diffed against what
//!   the stream holds — a `changed` per differing row, a `removed_pane_id` per
//!   row that left the inventory.
//! - A `PaneClosed` for a held pane becomes `removed_pane_id`.
//! - The bus is lossy: a receiver that lagged gets a fresh `snapshot` (same
//!   debounce), replacing everything the client holds.
//! - The session token is re-validated every [`WATCH_TOKEN_RECHECK_INTERVAL`], as
//!   the attach relay does (`relay/inbound.rs`); a revoked or unverifiable one
//!   ends the stream with `UNAUTHENTICATED`, failing closed.
//! - The response channel is bounded ([`WATCH_CHANNEL_BOUND`]), so a slow client
//!   backpressures the stream task instead of buffering; if that makes the task
//!   fall behind the bus, the lag path above re-snapshots. A client that drops
//!   the stream ends the task at once.
//! - No bus (a zellij-only server), or a session on a backend without agents:
//!   the snapshot is sent and the stream ends.
//!
//! The bus is fed only by the herdr event kernel and herdr serves a single
//! sentinel session, so every bus event belongs to the one session an
//! agents-capable stream can watch.
//!
//! The neutral → proto mapping of status, mark and summary lives in
//! [`super::helpers`] so `GetLayout` (`PaneMsg.agent`) and `ListSessions`
//! (`SessionInfo.agents`) put the exact same values on the wire.

use std::collections::{BTreeSet, HashMap};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::multiplexer::events::{AgentStatusChanged, MuxEvent};
use crate::multiplexer::{AgentSnapshot, AgentSummary, UnknownSession, sort_agents};
use crate::proto::{AgentEvent, AgentList, AgentMsg, SessionRef, agent_event};

use super::MuxrService;
use super::helpers::{agent_status_msg, agent_summary_msg};

/// Ceiling on one backend `list_agents` round trip.
///
/// Mirrors `RELAY_QUERY_TIMEOUT` in `grpc/layout.rs`, the bound `GetLayout` puts
/// on its layout query: herdr answers from one `session.snapshot` call bounded by
/// its own per-call control timeout, so 18 s comfortably covers it while still
/// bounding a hung backend.
const LIST_AGENTS_TIMEOUT: Duration = Duration::from_secs(18);

/// Bound on one `WatchAgents` response channel. Agent changes are human-paced,
/// so a handful of queued messages is ample; a client slower than that stalls
/// the stream task (backpressure) rather than growing a buffer.
const WATCH_CHANNEL_BOUND: usize = 16;

/// Minimum spacing of the `list_agents` re-reads one `WatchAgents` stream makes
/// after its snapshot. Events arriving inside the window coalesce into one read.
const WATCH_LIST_DEBOUNCE: Duration = Duration::from_millis(250);

/// How often a `WatchAgents` stream re-validates its session token — the attach
/// relay's interval (`relay::types::TOKEN_RECHECK_INTERVAL`, crate-private to
/// the relay), for the same reason: a remembered token lives 28 days.
const WATCH_TOKEN_RECHECK_INTERVAL: Duration = Duration::from_secs(30);

/// The `WatchAgents` response stream type.
pub(super) type AgentEventStream =
    Pin<Box<dyn futures::Stream<Item = Result<AgentEvent, Status>> + Send>>;

/// A blocking agent-inventory read — the backend's `list_agents` for one bare
/// session, boxed so the watch loop is testable without a whole backend. Always
/// called on the blocking pool.
type AgentLister = Arc<dyn Fn() -> anyhow::Result<Vec<AgentSnapshot>> + Send + Sync>;

/// A blocking session-token check (`ipc::validate_session_token` in
/// production). Always called on the blocking pool.
type TokenValidator = Arc<dyn Fn(&str) -> anyhow::Result<bool> + Send + Sync>;

/// The two intervals a watch runs on; tests shrink them.
#[derive(Debug, Clone, Copy)]
struct WatchTuning {
    list_debounce: Duration,
    token_recheck: Duration,
}

impl Default for WatchTuning {
    fn default() -> Self {
        Self {
            list_debounce: WATCH_LIST_DEBOUNCE,
            token_recheck: WATCH_TOKEN_RECHECK_INTERVAL,
        }
    }
}

/// Everything one `WatchAgents` stream needs besides its channels.
struct Watch {
    /// The session id as the client sent it, for logs only.
    session: String,
    lister: AgentLister,
    /// The bearer token the auth layer validated at open; `None` fails closed on
    /// the first re-check, as in the relay.
    token: Option<String>,
    validate: TokenValidator,
    tuning: WatchTuning,
}

type WatchTx = mpsc::Sender<Result<AgentEvent, Status>>;

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

        let lister: AgentLister = Arc::new(move || backend.list_agents(&bare));
        let mut snapshots = query_agents("ListAgents", &session, &lister).await?;

        let list = build_agent_list(&mut snapshots);
        log::info!(
            "ListAgents: session='{session}' → {} agent(s), {} blocked",
            list.agents.len(),
            list.summary.as_ref().map_or(0, |s| s.blocked)
        );
        Ok(Response::new(list))
    }

    // ── WatchAgents ───────────────────────────────────────────────────────────

    /// Stream a session's agents: one snapshot, then deltas (see the module doc).
    ///
    /// Resolution and the snapshot's errors are `ListAgents`' own and are
    /// returned before the stream opens. Only `SessionRef.session` is read.
    pub(super) async fn watch_agents_impl(
        &self,
        request: Request<SessionRef>,
    ) -> Result<Response<AgentEventStream>, Status> {
        let token = request
            .extensions()
            .get::<crate::auth::SessionToken>()
            .map(|t| t.0.clone());
        let session = request.into_inner().session;
        let (backend, bare) = self.resolve_session(&session)?;
        log::debug!("WatchAgents: session='{session}'");

        // Subscribe FIRST, so nothing published while the snapshot is taken is
        // lost. Only an agents backend is fed by the bus.
        let events = if backend.supports_agents() {
            self.event_bus.as_ref().map(broadcast::Sender::subscribe)
        } else {
            None
        };
        let validate: TokenValidator = Arc::new(|t: &str| crate::ipc::validate_session_token(t));
        let watch = Watch {
            session,
            lister: Arc::new(move || backend.list_agents(&bare)),
            token,
            validate,
            tuning: WatchTuning::default(),
        };
        let rx = start_watch(watch, events).await?;
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

// ─── Backend reads ────────────────────────────────────────────────────────────

/// One bounded `list_agents` round trip on the blocking pool, with every failure
/// mapped to a client status (`rpc` names the caller in logs and messages).
async fn query_agents(
    rpc: &str,
    session: &str,
    lister: &AgentLister,
) -> Result<Vec<AgentSnapshot>, Status> {
    let lister = Arc::clone(lister);
    match tokio::time::timeout(
        LIST_AGENTS_TIMEOUT,
        tokio::task::spawn_blocking(move || lister()),
    )
    .await
    {
        Ok(Ok(Ok(snapshots))) => Ok(snapshots),
        Ok(Ok(Err(e))) => Err(agents_query_status(rpc, session, &e)),
        Ok(Err(join_err)) => {
            log::warn!("{rpc}: list task panicked for '{session}': {join_err}");
            Err(Status::internal(format!("{rpc}: query task failed")))
        }
        Err(_elapsed) => {
            log::warn!(
                "{rpc}: list_agents timed out for '{session}' after {LIST_AGENTS_TIMEOUT:?}"
            );
            Err(Status::internal(format!("{rpc}: backend query timed out")))
        }
    }
}

/// Map a backend `list_agents` failure to a client status.
///
/// An [`UnknownSession`] (found through any `.context` layers, never by string
/// matching) is a client error → `NOT_FOUND`, logged at `info!`. Everything else
/// is a server fault → `INTERNAL`. Both messages are terse: the cause goes to
/// the log only, as in `space_query_status` (`grpc/layout.rs`).
fn agents_query_status(rpc: &str, session: &str, err: &anyhow::Error) -> Status {
    if UnknownSession::in_chain(err) {
        log::info!("{rpc}: session='{session}' names no live session → not_found");
        log::debug!("{rpc}: unknown session='{session}': {err:#}");
        Status::not_found("session: no such session")
    } else {
        log::warn!("{rpc}: list_agents failed for '{session}': {err:#}");
        Status::internal(format!("{rpc}: backend query failed"))
    }
}

/// Re-validate the stream's session token on the blocking pool. `true` only for
/// a token that is still valid; absence, invalidity or any error fails closed.
/// The logic of the relay's `revalidate_token` (`relay/inbound.rs`).
async fn token_still_valid(w: &Watch) -> bool {
    let Some(token) = w.token.clone() else {
        log::warn!(
            "WatchAgents [{}]: no token to re-validate → failing closed",
            w.session
        );
        return false;
    };
    let validate = Arc::clone(&w.validate);
    match tokio::task::spawn_blocking(move || validate(&token)).await {
        Ok(Ok(valid)) => valid,
        Ok(Err(e)) => {
            log::warn!(
                "WatchAgents [{}]: token re-validation DB error (failing closed): {e}",
                w.session
            );
            false
        }
        Err(e) => {
            log::warn!(
                "WatchAgents [{}]: token re-validation task panicked (failing closed): {e}",
                w.session
            );
            false
        }
    }
}

// ─── WatchAgents stream ───────────────────────────────────────────────────────

/// Take the snapshot, queue it as the first message, and — when there is a bus
/// to follow — spawn the loop that forwards deltas. Without one the sender is
/// dropped here, so the stream ends right after the snapshot.
async fn start_watch(
    w: Watch,
    events: Option<broadcast::Receiver<MuxEvent>>,
) -> Result<mpsc::Receiver<Result<AgentEvent, Status>>, Status> {
    let mut snapshots = query_agents("WatchAgents", &w.session, &w.lister).await?;
    let last_list = Instant::now();
    let list = build_agent_list(&mut snapshots);
    let held = hold(&list);
    log::info!(
        "WatchAgents: session='{}' opened with {} agent(s)",
        w.session,
        list.agents.len()
    );

    let (tx, rx) = mpsc::channel(WATCH_CHANNEL_BOUND);
    // A fresh channel with a positive bound cannot be full.
    let _ = tx.try_send(Ok(snapshot_event(list)));
    match events {
        Some(events) => {
            tokio::spawn(watch_loop(w, events, tx, held, last_list));
        }
        None => log::info!(
            "WatchAgents: session='{}' has no live agent feed — snapshot only",
            w.session
        ),
    }
    Ok(rx)
}

/// Forward bus events to one stream until the client goes, the token is
/// revoked, a re-read fails, or the bus closes.
async fn watch_loop(
    w: Watch,
    mut events: broadcast::Receiver<MuxEvent>,
    tx: WatchTx,
    mut held: HashMap<u32, AgentMsg>,
    mut last_list: Instant,
) {
    let session = w.session.clone();
    let mut recheck = tokio::time::interval(w.tuning.token_recheck);
    // The first tick fires immediately; the auth layer validated at open.
    recheck.set_missed_tick_behavior(MissedTickBehavior::Delay);
    recheck.tick().await;

    // Panes whose event could not be applied to the held row: a re-read is due.
    let mut stale: BTreeSet<u32> = BTreeSet::new();
    // The receiver lagged: the next re-read goes out as a whole snapshot.
    let mut resnapshot = false;

    loop {
        let reread_due = last_list + w.tuning.list_debounce;
        let reread_wanted = resnapshot || !stale.is_empty();
        let out: Vec<AgentEvent> = tokio::select! {
            () = tx.closed() => {
                log::debug!("WatchAgents [{session}]: client went away — ending stream");
                break;
            }
            _ = recheck.tick() => {
                if token_still_valid(&w).await {
                    continue;
                }
                log::warn!("WatchAgents [{session}]: token no longer valid — ending stream");
                let _ = tx
                    .send(Err(Status::unauthenticated("session token no longer valid")))
                    .await;
                break;
            }
            () = tokio::time::sleep_until(reread_due), if reread_wanted => {
                last_list = Instant::now();
                match query_agents("WatchAgents", &session, &w.lister).await {
                    Ok(mut snapshots) => {
                        let list = build_agent_list(&mut snapshots);
                        stale.clear();
                        if std::mem::take(&mut resnapshot) {
                            held = hold(&list);
                            vec![snapshot_event(list)]
                        } else {
                            reconcile(&mut held, &list)
                        }
                    }
                    Err(status) => {
                        let _ = tx.send(Err(status)).await;
                        break;
                    }
                }
            }
            recv = events.recv() => match recv {
                Ok(MuxEvent::AgentStatusChanged(ev)) => match apply_status(&held, &ev) {
                    Some(msg) => {
                        held.insert(msg.pane_id, msg.clone());
                        vec![changed_event(msg)]
                    }
                    None => {
                        stale.insert(ev.pane);
                        Vec::new()
                    }
                },
                Ok(MuxEvent::PaneClosed(closed)) => {
                    stale.remove(&closed.pane);
                    if held.remove(&closed.pane).is_some() {
                        vec![removed_event(closed.pane)]
                    } else {
                        Vec::new()
                    }
                }
                Ok(MuxEvent::LayoutChanged(_)) => Vec::new(),
                Err(RecvError::Lagged(n)) => {
                    log::info!(
                        "WatchAgents [{session}]: fell {n} event(s) behind — re-snapshotting"
                    );
                    resnapshot = true;
                    Vec::new()
                }
                Err(RecvError::Closed) => {
                    log::info!("WatchAgents [{session}]: event bus closed — ending stream");
                    break;
                }
            },
        };
        for ev in out {
            if tx.send(Ok(ev)).await.is_err() {
                log::debug!("WatchAgents [{session}]: client went away — ending stream");
                return;
            }
        }
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

/// The rows a stream holds after sending `list`, keyed by pane.
fn hold(list: &AgentList) -> HashMap<u32, AgentMsg> {
    list.agents.iter().map(|a| (a.pane_id, a.clone())).collect()
}

/// Apply one status event to the row the stream holds for its pane, when the
/// event carries enough to do so without a re-read: the pane is held, and the
/// event's kind, tab and space are present and agree with the held row. The
/// event's status always applies; its title and name only when it carries them.
/// `None` means a re-read is needed.
fn apply_status(held: &HashMap<u32, AgentMsg>, ev: &AgentStatusChanged) -> Option<AgentMsg> {
    let row = held.get(&ev.pane)?;
    let (kind, tab) = (ev.kind.as_deref()?, ev.tab?);
    if kind != row.kind || tab != row.tab_id || ev.workspace_id != row.space_id {
        return None;
    }
    let mut msg = row.clone();
    msg.status = agent_status_msg(ev.status) as i32;
    if let Some(title) = &ev.title {
        msg.title.clone_from(title);
    }
    if let Some(name) = &ev.name {
        msg.name.clone_from(name);
    }
    Some(msg)
}

/// Diff a fresh inventory against what the stream holds, replace the held rows,
/// and return the deltas: a `changed` per row that is new or differs (in the
/// fresh list's priority order), then a `removed_pane_id` per held pane the
/// fresh list no longer carries (ascending).
fn reconcile(held: &mut HashMap<u32, AgentMsg>, fresh: &AgentList) -> Vec<AgentEvent> {
    let next = hold(fresh);
    let mut out: Vec<AgentEvent> = fresh
        .agents
        .iter()
        .filter(|a| held.get(&a.pane_id) != Some(*a))
        .map(|a| changed_event(a.clone()))
        .collect();
    let mut gone: Vec<u32> = held
        .keys()
        .filter(|pane| !next.contains_key(pane))
        .copied()
        .collect();
    gone.sort_unstable();
    out.extend(gone.into_iter().map(removed_event));
    *held = next;
    out
}

fn snapshot_event(list: AgentList) -> AgentEvent {
    AgentEvent {
        kind: Some(agent_event::Kind::Snapshot(list)),
    }
}

fn changed_event(msg: AgentMsg) -> AgentEvent {
    AgentEvent {
        kind: Some(agent_event::Kind::Changed(msg)),
    }
}

fn removed_event(pane: u32) -> AgentEvent {
    AgentEvent {
        kind: Some(agent_event::Kind::RemovedPaneId(pane)),
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
    /// for every session (or errors when `fail` is set), except that the session
    /// named `nope` is unknown (a typed `UnknownSession`); `sessions` feeds
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
        fn list_agents(&self, session: &str) -> anyhow::Result<Vec<AgentSnapshot>> {
            if session == "nope" {
                return Err(
                    anyhow::Error::new(crate::multiplexer::UnknownSession::new("nope"))
                        .context("herdr: detail at /run/stub.sock"),
                );
            }
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
    async fn unknown_session_is_a_terse_not_found() {
        let service = herdr_service(AgentsBackend::default());

        let err = service
            .list_agents_impl(session_ref("herdr:nope"))
            .await
            .expect_err("an unknown session is a client error");
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert_eq!(err.message(), "session: no such session");
        assert!(!err.message().contains("/run/stub.sock"));
    }

    #[test]
    fn agents_query_status_separates_unknown_session_from_other_failures() {
        let unknown = anyhow::Error::new(super::UnknownSession::new("x")).context("ctx");
        assert_eq!(
            super::agents_query_status("ListAgents", "x", &unknown).code(),
            tonic::Code::NotFound
        );
        let other = anyhow::anyhow!("boom");
        let status = super::agents_query_status("ListAgents", "x", &other);
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(status.message(), "ListAgents: backend query failed");
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

    // ── WatchAgents ───────────────────────────────────────────────────────────

    use std::collections::HashMap;
    use std::sync::Mutex;

    use tokio::sync::{broadcast, mpsc};
    use tokio_stream::StreamExt;

    use crate::multiplexer::events::{AgentStatusChanged, LayoutChanged, MuxEvent, PaneClosed};
    use crate::proto::{AgentEvent, AgentMsg, agent_event::Kind};

    use super::{AgentLister, Watch, WatchTuning, apply_status, reconcile, start_watch};

    /// Per-message wait: generous, since a failure here is a hang, not a flake.
    const WAIT: Duration = Duration::from_secs(5);

    fn bus() -> crate::multiplexer::events::EventBus {
        broadcast::channel(crate::multiplexer::events::EVENT_BUS_CAPACITY).0
    }

    fn status_event(
        pane: u32,
        status: AgentStatus,
        kind: Option<&str>,
        tab: Option<u64>,
    ) -> MuxEvent {
        MuxEvent::AgentStatusChanged(AgentStatusChanged {
            pane,
            workspace_id: format!("ws-{pane}"),
            workspace_name: None,
            status,
            title: None,
            kind: kind.map(str::to_owned),
            name: None,
            tab,
            synthetic: false,
        })
    }

    fn kind(ev: Option<Result<AgentEvent, tonic::Status>>) -> Kind {
        ev.expect("the stream must not have ended")
            .expect("a message, not an error")
            .kind
            .expect("every AgentEvent sets its oneof")
    }

    fn panes_of(kind: &Kind) -> Vec<u32> {
        match kind {
            Kind::Snapshot(list) => list.agents.iter().map(|a| a.pane_id).collect(),
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    // ── through the service ──

    fn watched_service(bus: Option<crate::multiplexer::events::EventBus>) -> MuxrService {
        herdr_service(AgentsBackend {
            agents: two_agents(),
            ..Default::default()
        })
        .with_event_bus(bus)
    }

    async fn open(
        service: &MuxrService,
        session: &str,
    ) -> Result<super::AgentEventStream, tonic::Status> {
        service
            .watch_agents_impl(session_ref(session))
            .await
            .map(tonic::Response::into_inner)
    }

    async fn next(
        stream: &mut super::AgentEventStream,
    ) -> Option<Result<AgentEvent, tonic::Status>> {
        tokio::time::timeout(WAIT, stream.next())
            .await
            .expect("the stream must answer in time")
    }

    #[tokio::test]
    async fn watch_agents_opens_with_the_list_agents_snapshot() {
        let bus = bus();
        let service = watched_service(Some(bus.clone()));
        // Published before the client reads anything: the snapshot still leads.
        let mut stream = open(&service, "herdr:herdr").await.expect("stream opens");
        bus.send(status_event(9, AgentStatus::Done, Some("cli-b"), Some(900)))
            .expect("the stream is subscribed");

        let first = kind(next(&mut stream).await);
        let listed = service
            .list_agents_impl(session_ref("herdr:herdr"))
            .await
            .expect("ListAgents")
            .into_inner();
        assert_eq!(
            first,
            Kind::Snapshot(listed),
            "snapshot first, as ListAgents answers"
        );
    }

    #[tokio::test]
    async fn a_published_blocked_event_yields_a_changed_with_status_blocked() {
        let bus = bus();
        let service = watched_service(Some(bus.clone()));
        let mut stream = open(&service, "herdr:herdr").await.expect("stream opens");
        assert_eq!(panes_of(&kind(next(&mut stream).await)), vec![7, 9]);

        bus.send(status_event(
            9,
            AgentStatus::Blocked,
            Some("cli-b"),
            Some(900),
        ))
        .expect("the stream is subscribed");
        match kind(next(&mut stream).await) {
            Kind::Changed(msg) => {
                assert_eq!(msg.pane_id, 9);
                assert_eq!(msg.status, WireStatus::Blocked as i32);
                assert_eq!(msg.kind, "cli-b", "the rest of the held row is kept");
                assert_eq!(msg.tab_name, "tab 9");
            }
            other => panic!("expected changed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn without_a_bus_the_stream_sends_the_snapshot_and_ends() {
        let service = watched_service(None);
        let mut stream = open(&service, "herdr:herdr").await.expect("stream opens");
        assert_eq!(panes_of(&kind(next(&mut stream).await)), vec![7, 9]);
        assert!(
            next(&mut stream).await.is_none(),
            "no bus → the stream ends"
        );
    }

    #[tokio::test]
    async fn zellij_only_server_sends_the_empty_snapshot_and_ends() {
        let service = MuxrService::new();
        let mut stream = open(&service, "zellij:dev").await.expect("stream opens");
        match kind(next(&mut stream).await) {
            Kind::Snapshot(list) => {
                assert!(list.agents.is_empty());
                assert_eq!(list.summary.expect("summary always set").total, 0);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
        assert!(next(&mut stream).await.is_none());
    }

    #[tokio::test]
    async fn a_zellij_session_on_a_bus_carrying_server_is_snapshot_only() {
        // The bus is herdr's; a zellij session must not follow it.
        let bus = bus();
        let service = MuxrService::with_backends(BackendSet::new(vec![
            (
                BackendKind::Zellij,
                Arc::new(crate::multiplexer::ZellijBackend) as _,
            ),
            (
                BackendKind::Herdr,
                Arc::new(AgentsBackend {
                    agents: two_agents(),
                    ..Default::default()
                }) as _,
            ),
        ]))
        .with_event_bus(Some(bus.clone()));
        let mut stream = open(&service, "zellij:dev").await.expect("stream opens");
        assert!(panes_of(&kind(next(&mut stream).await)).is_empty());
        assert!(next(&mut stream).await.is_none());
        assert_eq!(bus.receiver_count(), 0, "nothing subscribed for zellij");
    }

    #[tokio::test]
    async fn watch_errors_are_list_agents_own_before_the_stream_opens() {
        let service = watched_service(Some(bus()));
        let Err(unknown) = open(&service, "herdr:nope").await else {
            panic!("an unknown session must not open a stream");
        };
        assert_eq!(unknown.code(), tonic::Code::NotFound);
        assert_eq!(unknown.message(), "session: no such session");
        let Err(token) = open(&service, "tmux:dev").await else {
            panic!("an unknown backend token must not open a stream");
        };
        assert_eq!(token.code(), tonic::Code::InvalidArgument);
    }

    // ── the loop, over a scripted inventory ──

    /// A mutable inventory standing in for the backend, recording when each
    /// `list_agents` call happened.
    #[derive(Default)]
    struct Inventory {
        agents: Mutex<Vec<AgentSnapshot>>,
        calls: Mutex<Vec<std::time::Instant>>,
    }

    impl Inventory {
        fn with(agents: Vec<AgentSnapshot>) -> Arc<Self> {
            Arc::new(Self {
                agents: Mutex::new(agents),
                ..Default::default()
            })
        }
        fn set(&self, agents: Vec<AgentSnapshot>) {
            *self.agents.lock().unwrap() = agents;
        }
        fn calls(&self) -> Vec<std::time::Instant> {
            self.calls.lock().unwrap().clone()
        }
    }

    const QUIET: WatchTuning = WatchTuning {
        list_debounce: Duration::from_millis(100),
        token_recheck: Duration::from_secs(3600),
    };

    fn watch(inv: &Arc<Inventory>, token: Option<&str>, valid: bool, tuning: WatchTuning) -> Watch {
        let inv = Arc::clone(inv);
        let lister: AgentLister = Arc::new(move || {
            inv.calls.lock().unwrap().push(std::time::Instant::now());
            Ok(inv.agents.lock().unwrap().clone())
        });
        Watch {
            session: "herdr:herdr".to_owned(),
            lister,
            token: token.map(str::to_owned),
            validate: Arc::new(move |_| Ok(valid)),
            tuning,
        }
    }

    type Rx = mpsc::Receiver<Result<AgentEvent, tonic::Status>>;

    async fn recv(rx: &mut Rx) -> Option<Result<AgentEvent, tonic::Status>> {
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("the stream must answer in time")
    }

    async fn started(
        inv: &Arc<Inventory>,
        tuning: WatchTuning,
    ) -> (crate::multiplexer::events::EventBus, Rx) {
        let bus = bus();
        let mut rx = start_watch(watch(inv, Some("tok"), true, tuning), Some(bus.subscribe()))
            .await
            .expect("the watch starts");
        let _ = panes_of(&kind(recv(&mut rx).await));
        (bus, rx)
    }

    #[tokio::test]
    async fn an_event_lacking_fields_is_re_read_from_the_backend() {
        let inv = Inventory::with(two_agents());
        let (bus, mut rx) = started(&inv, QUIET).await;

        // A new agent the stream has never held, announced with no kind/tab.
        let mut fresh = two_agents();
        fresh.push(snapshot(5, "cli-c", AgentStatus::Blocked, 20));
        inv.set(fresh);
        bus.send(status_event(5, AgentStatus::Blocked, None, None))
            .expect("subscribed");

        match kind(recv(&mut rx).await) {
            Kind::Changed(msg) => {
                assert_eq!(msg.pane_id, 5);
                assert_eq!(msg.kind, "cli-c", "filled in from the fresh inventory");
                assert_eq!(msg.tab_id, 500);
            }
            other => panic!("expected changed, got {other:?}"),
        }
        assert_eq!(
            inv.calls().len(),
            2,
            "the snapshot plus exactly one re-read"
        );
    }

    #[tokio::test]
    async fn re_reads_are_debounced_to_one_per_window() {
        let inv = Inventory::with(two_agents());
        let (bus, mut rx) = started(&inv, QUIET).await;

        // Three field-less events in one burst coalesce into one re-read.
        for status in [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked] {
            let mut agents = two_agents();
            agents[1].status = status;
            inv.set(agents);
            bus.send(status_event(9, status, None, None))
                .expect("subscribed");
        }
        match kind(recv(&mut rx).await) {
            Kind::Changed(msg) => assert_eq!(msg.status, WireStatus::Blocked as i32),
            other => panic!("expected changed, got {other:?}"),
        }
        // One more, right away: it must wait out the window.
        let mut agents = two_agents();
        agents[1].status = AgentStatus::Idle;
        inv.set(agents);
        bus.send(status_event(9, AgentStatus::Idle, None, None))
            .expect("subscribed");
        match kind(recv(&mut rx).await) {
            Kind::Changed(msg) => assert_eq!(msg.status, WireStatus::Idle as i32),
            other => panic!("expected changed, got {other:?}"),
        }

        let calls = inv.calls();
        assert_eq!(calls.len(), 3, "snapshot + one per burst: {calls:?}");
        for pair in calls.windows(2) {
            assert!(
                pair[1] - pair[0] >= QUIET.list_debounce,
                "list calls closer than the debounce window: {calls:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_lagged_receiver_yields_a_fresh_snapshot() {
        let inv = Inventory::with(two_agents());
        let (bus, _) = broadcast::channel(2);
        let lagging = bus.subscribe();
        // Overrun the two-slot receiver before the loop reads it.
        for _ in 0..4 {
            bus.send(status_event(
                9,
                AgentStatus::Working,
                Some("cli-b"),
                Some(900),
            ))
            .expect("subscribed");
        }
        let mut rx = start_watch(watch(&inv, Some("tok"), true, QUIET), Some(lagging))
            .await
            .expect("the watch starts");
        assert_eq!(panes_of(&kind(recv(&mut rx).await)), vec![7, 9]);

        loop {
            match kind(recv(&mut rx).await) {
                Kind::Snapshot(list) => {
                    assert_eq!(list.agents.len(), 2);
                    break;
                }
                // The events retained past the lag still apply first.
                Kind::Changed(msg) => assert_eq!(msg.pane_id, 9),
                other => panic!("expected changed or snapshot, got {other:?}"),
            }
        }
        assert_eq!(inv.calls().len(), 2, "the re-snapshot is one re-read");
    }

    #[tokio::test]
    async fn token_revocation_ends_the_stream_unauthenticated() {
        let inv = Inventory::with(two_agents());
        let tuning = WatchTuning {
            token_recheck: Duration::from_millis(50),
            ..QUIET
        };
        let bus = bus();
        let mut rx = start_watch(
            watch(&inv, Some("tok"), false, tuning),
            Some(bus.subscribe()),
        )
        .await
        .expect("the watch starts");
        let _ = kind(recv(&mut rx).await);

        let err = recv(&mut rx)
            .await
            .expect("an error, not a silent end")
            .expect_err("revocation is an error status");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert!(recv(&mut rx).await.is_none(), "the stream ends after it");
    }

    #[tokio::test]
    async fn a_missing_token_fails_closed_on_the_first_recheck() {
        let inv = Inventory::with(Vec::new());
        let tuning = WatchTuning {
            token_recheck: Duration::from_millis(50),
            ..QUIET
        };
        let bus = bus();
        // The validator would accept anything: absence alone must end it.
        let mut rx = start_watch(watch(&inv, None, true, tuning), Some(bus.subscribe()))
            .await
            .expect("the watch starts");
        let _ = kind(recv(&mut rx).await);
        let err = recv(&mut rx)
            .await
            .expect("an error")
            .expect_err("fails closed");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn a_held_pane_closing_yields_removed_pane_id_and_others_are_ignored() {
        let inv = Inventory::with(two_agents());
        let (bus, mut rx) = started(&inv, QUIET).await;

        bus.send(MuxEvent::PaneClosed(PaneClosed { pane: 3 }))
            .expect("subscribed");
        bus.send(MuxEvent::LayoutChanged(LayoutChanged {
            workspace_id: "ws-7".into(),
            tab: 700,
        }))
        .expect("subscribed");
        bus.send(MuxEvent::PaneClosed(PaneClosed { pane: 7 }))
            .expect("subscribed");

        assert_eq!(kind(recv(&mut rx).await), Kind::RemovedPaneId(7));
        assert_eq!(inv.calls().len(), 1, "a closure needs no re-read");
    }

    #[tokio::test]
    async fn a_re_read_that_drops_an_agent_removes_it() {
        let inv = Inventory::with(two_agents());
        let (bus, mut rx) = started(&inv, QUIET).await;

        // Pane 9's agent exits: herdr reports its status with no kind, and the
        // fresh inventory no longer lists it.
        inv.set(two_agents().into_iter().take(1).collect());
        bus.send(status_event(9, AgentStatus::Unknown, None, None))
            .expect("subscribed");
        assert_eq!(kind(recv(&mut rx).await), Kind::RemovedPaneId(9));
    }

    #[tokio::test]
    async fn a_client_drop_ends_the_stream_task() {
        let inv = Inventory::with(two_agents());
        let (bus, rx) = started(&inv, QUIET).await;
        assert_eq!(bus.receiver_count(), 1);
        drop(rx);
        tokio::time::timeout(WAIT, async {
            while bus.receiver_count() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the task must release its bus receiver once the client goes");
    }

    // ── pure helpers ──

    fn held_two() -> HashMap<u32, AgentMsg> {
        super::hold(&super::build_agent_list(&mut two_agents()))
    }

    fn status_changed(pane: u32, kind: Option<&str>, tab: Option<u64>) -> AgentStatusChanged {
        match status_event(pane, AgentStatus::Done, kind, tab) {
            MuxEvent::AgentStatusChanged(ev) => ev,
            _ => unreachable!(),
        }
    }

    #[test]
    fn apply_status_needs_a_held_pane_with_matching_kind_tab_and_space() {
        let held = held_two();
        let applied = apply_status(&held, &status_changed(9, Some("cli-b"), Some(900)))
            .expect("everything agrees");
        assert_eq!(applied.status, WireStatus::Done as i32);
        assert_eq!(
            applied.change_seq, 11,
            "fields the event lacks are held ones"
        );

        assert!(apply_status(&held, &status_changed(5, Some("cli-b"), Some(900))).is_none());
        assert!(apply_status(&held, &status_changed(9, None, Some(900))).is_none());
        assert!(apply_status(&held, &status_changed(9, Some("cli-b"), None)).is_none());
        assert!(apply_status(&held, &status_changed(9, Some("other"), Some(900))).is_none());
        assert!(apply_status(&held, &status_changed(9, Some("cli-b"), Some(1))).is_none());
        let mut moved = status_changed(9, Some("cli-b"), Some(900));
        moved.workspace_id = "ws-elsewhere".into();
        assert!(apply_status(&held, &moved).is_none());
    }

    #[test]
    fn apply_status_takes_the_events_title_and_name_only_when_carried() {
        let held = held_two();
        let mut ev = status_changed(7, Some("cli-a"), Some(700));
        let kept = apply_status(&held, &ev).expect("applies");
        assert_eq!(kept.title, "fix the build");
        assert_eq!(kept.name, "reviewer");
        ev.title = Some("new title".into());
        ev.name = Some("new name".into());
        let taken = apply_status(&held, &ev).expect("applies");
        assert_eq!(taken.title, "new title");
        assert_eq!(taken.name, "new name");
    }

    #[test]
    fn reconcile_reports_new_and_differing_rows_then_departures() {
        let mut held = held_two();
        let mut fresh = two_agents();
        fresh.remove(0); // pane 7 left
        fresh.push(snapshot(5, "cli-c", AgentStatus::Idle, 1)); // pane 5 arrived
        let fresh = super::build_agent_list(&mut fresh);

        let out: Vec<Kind> = reconcile(&mut held, &fresh)
            .into_iter()
            .map(|e| e.kind.expect("set"))
            .collect();
        assert_eq!(out.len(), 2, "pane 9 is unchanged and not re-sent: {out:?}");
        assert!(matches!(&out[0], Kind::Changed(m) if m.pane_id == 5));
        assert_eq!(out[1], Kind::RemovedPaneId(7));
        let mut panes: Vec<u32> = held.keys().copied().collect();
        panes.sort_unstable();
        assert_eq!(panes, vec![5, 9], "the held rows are the fresh ones");
    }
}
