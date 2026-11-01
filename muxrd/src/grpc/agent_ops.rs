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
//!   row with the event's status (and title/name when it carries them). That is
//!   the fast path, never the final word: the event cannot carry the backend's
//!   `change_seq`, `completion_seq`, `summary`, `focused` or `cwd`, so the pane is
//!   **also** marked for a re-read. An event that lacks kind/tab/space, or
//!   disagrees, is only marked. A marked pane is re-read — a fresh `list_agents`
//!   on the blocking pool, **at most one per [`WATCH_LIST_DEBOUNCE`] per
//!   stream** — and the answer is diffed whole-row against what the stream
//!   holds: a `changed` per differing row, a `removed_pane_id` per row that left
//!   the inventory. So a transition reaches the client as its status at once and
//!   as the exact `ListAgents` row within one window; the later message wins.
//! - A `PaneClosed` for a held pane becomes `removed_pane_id`.
//! - The bus is lossy: a receiver that lagged, or a `Resynced` marker (the herdr
//!   event kernel reconnected and re-observed everything), gets a fresh
//!   `snapshot` (same debounce, so a burst sends one), replacing everything the
//!   client holds.
//! - The session token is re-validated every
//!   [`TOKEN_RECHECK_INTERVAL`](crate::relay::TOKEN_RECHECK_INTERVAL) — the
//!   attach relay's own constant — and a revoked or unverifiable one ends the
//!   stream with `UNAUTHENTICATED`, failing closed, ahead of anything still
//!   queued for the client.
//! - Nothing the loop awaits can suspend that re-check or hide the client's
//!   departure: a message waits for channel room inside a `select!` beside the
//!   re-check and `closed()`, and a re-read is a future the loop's `select!`
//!   polls, never an inline await, so a backend that takes its full
//!   [`LIST_AGENTS_TIMEOUT`] stalls nothing.
//! - A failed re-read never ends the stream: it is logged (`warn!` once per
//!   failing run), the held rows stand, and the marks are kept so the next
//!   window retries. The stream ends only on `UNAUTHENTICATED`, the client
//!   going away, the bus closing (server shutdown), or an error before the
//!   first message (`ListAgents`' own).
//! - The response channel is bounded ([`WATCH_CHANNEL_BOUND`]), so a slow client
//!   backpressures the stream task instead of buffering; if that makes the task
//!   fall behind the bus, the lag path above re-snapshots.
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

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tonic::{Request, Response, Status};

use crate::multiplexer::events::{AgentStatusChanged, MuxEvent};
use crate::multiplexer::{AgentSnapshot, AgentSummary, UnknownSession, sort_agents};
use crate::proto::{AgentEvent, AgentList, AgentMsg, SessionRef, agent_event};
use crate::relay::TOKEN_RECHECK_INTERVAL;

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
            token_recheck: TOKEN_RECHECK_INTERVAL,
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

/// The loop's half of the response channel. Errors never travel on it: the one
/// error a live stream can end with travels on the [`WatchStream`]'s fatal
/// channel instead, so it overtakes whatever is still queued.
type WatchTx = mpsc::Sender<AgentEvent>;

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
        let stream = start_watch(watch, events).await?;
        Ok(Response::new(Box::pin(stream)))
    }
}

// ─── Backend reads ────────────────────────────────────────────────────────────

/// Why one `list_agents` round trip produced no inventory. Kept apart from a
/// client status so each caller decides how loudly to log it: `ListAgents` and
/// a stream's first snapshot map it to an error, a mid-stream re-read only logs.
#[derive(Debug)]
enum ReadFailure {
    /// The backend answered with an error.
    Backend(anyhow::Error),
    /// The blocking task panicked or was cancelled.
    Join(tokio::task::JoinError),
    /// The backend did not answer within [`LIST_AGENTS_TIMEOUT`].
    TimedOut,
}

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "list_agents failed: {e:#}"),
            Self::Join(e) => write!(f, "list task panicked: {e}"),
            Self::TimedOut => write!(f, "list_agents timed out after {LIST_AGENTS_TIMEOUT:?}"),
        }
    }
}

/// One bounded `list_agents` round trip on the blocking pool. Owns its lister so
/// the future is `'static`: a watch stores it and polls it from its `select!`.
async fn read_agents(lister: AgentLister) -> Result<Vec<AgentSnapshot>, ReadFailure> {
    match tokio::time::timeout(
        LIST_AGENTS_TIMEOUT,
        tokio::task::spawn_blocking(move || lister()),
    )
    .await
    {
        Ok(Ok(Ok(snapshots))) => Ok(snapshots),
        Ok(Ok(Err(e))) => Err(ReadFailure::Backend(e)),
        Ok(Err(join_err)) => Err(ReadFailure::Join(join_err)),
        Err(_elapsed) => Err(ReadFailure::TimedOut),
    }
}

/// [`read_agents`] with every failure logged and mapped to a client status
/// (`rpc` names the caller in logs and messages).
async fn query_agents(
    rpc: &str,
    session: &str,
    lister: &AgentLister,
) -> Result<Vec<AgentSnapshot>, Status> {
    match read_agents(Arc::clone(lister)).await {
        Ok(snapshots) => Ok(snapshots),
        Err(ReadFailure::Backend(e)) => Err(agents_query_status(rpc, session, &e)),
        Err(failure @ ReadFailure::Join(_)) => {
            log::warn!("{rpc}: {failure} for '{session}'");
            Err(Status::internal(format!("{rpc}: query task failed")))
        }
        Err(failure @ ReadFailure::TimedOut) => {
            log::warn!("{rpc}: {failure} for '{session}'");
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
///
/// This mirrors the attach relay's `revalidate_token` (`relay/inbound.rs`) line
/// for line rather than calling it: that helper is private to `relay/inbound.rs`,
/// a review hot spot whose surface stays as small as it is, and it is shaped
/// around the relay's own log prefix and its fixed validator. This one takes the
/// [`Watch`]'s validator so the tests can revoke a token. What must stay shared —
/// the cadence — is: both use [`TOKEN_RECHECK_INTERVAL`].
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

/// The `WatchAgents` response stream: the loop's bounded channel, plus a one-shot
/// fatal status that overtakes it. A revoked token must end the stream even when
/// the channel is full of messages a stalled client never read — and must not
/// deliver them first — so the error has a lane of its own.
struct WatchStream {
    rx: mpsc::Receiver<AgentEvent>,
    /// `None` once it resolved or its sender went away without a status (the
    /// loop ended for another reason; the channel then drains and ends).
    fatal: Option<oneshot::Receiver<Status>>,
    /// The fatal status was delivered: the stream is over.
    done: bool,
}

impl futures::Stream for WatchStream {
    type Item = Result<AgentEvent, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(None);
        }
        if let Some(fatal) = this.fatal.as_mut() {
            match Pin::new(fatal).poll(cx) {
                Poll::Ready(Ok(status)) => {
                    this.done = true;
                    this.fatal = None;
                    return Poll::Ready(Some(Err(status)));
                }
                Poll::Ready(Err(_)) => this.fatal = None,
                Poll::Pending => {}
            }
        }
        this.rx.poll_recv(cx).map(|ev| ev.map(Ok))
    }
}

/// Take the snapshot, queue it as the first message, and — when there is a bus
/// to follow — spawn the loop that forwards deltas. Without one the senders are
/// dropped here, so the stream ends right after the snapshot.
async fn start_watch(
    w: Watch,
    events: Option<broadcast::Receiver<MuxEvent>>,
) -> Result<WatchStream, Status> {
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
    let (fatal_tx, fatal_rx) = oneshot::channel();
    // A fresh channel with a positive bound cannot be full.
    let _ = tx.try_send(snapshot_event(list));
    match events {
        Some(events) => {
            tokio::spawn(watch_loop(
                w,
                events,
                tx,
                fatal_tx,
                WatchState::new(held, last_list),
            ));
        }
        None => log::info!(
            "WatchAgents: session='{}' has no live agent feed — snapshot only",
            w.session
        ),
    }
    Ok(WatchStream {
        rx,
        fatal: Some(fatal_rx),
        done: false,
    })
}

/// A re-read the loop is waiting on.
type PendingRead = Pin<Box<dyn Future<Output = Result<Vec<AgentSnapshot>, ReadFailure>> + Send>>;

/// One in-flight re-read and what it was started for.
struct InFlight {
    read: PendingRead,
    /// The marks this read answers; restored if it fails.
    stale: BTreeSet<u32>,
    /// It answers a lag or resync: its result goes out as a whole snapshot.
    resnapshot: bool,
    /// Panes whose closure arrived while the read was out: the answer may
    /// predate the closure, so it must not bring them back.
    closed: BTreeSet<u32>,
}

/// What one stream holds and owes the client.
struct WatchState {
    /// The rows the client holds, keyed by pane.
    held: HashMap<u32, AgentMsg>,
    /// Panes due a re-read (an event that could not be applied, or one applied
    /// by the fast path and owed its reconciled row).
    stale: BTreeSet<u32>,
    /// The next re-read goes out as a whole snapshot (lag or resync).
    resnapshot: bool,
    /// When the last read started; re-reads are spaced by the debounce.
    last_list: Instant,
    reading: Option<InFlight>,
    /// The previous re-read failed: further failures log quietly until one
    /// succeeds.
    read_failing: bool,
    /// Messages decided but not yet handed to the channel, in order.
    outbox: VecDeque<AgentEvent>,
}

impl WatchState {
    fn new(held: HashMap<u32, AgentMsg>, last_list: Instant) -> Self {
        Self {
            held,
            stale: BTreeSet::new(),
            resnapshot: false,
            last_list,
            reading: None,
            read_failing: false,
            outbox: VecDeque::new(),
        }
    }

    /// A re-read is wanted and none is out.
    fn reread_wanted(&self) -> bool {
        self.reading.is_none() && (self.resnapshot || !self.stale.is_empty())
    }

    /// Start a re-read answering every mark so far. It is polled by the loop's
    /// `select!`, never awaited inline.
    fn start_read(&mut self, w: &Watch) {
        self.last_list = Instant::now();
        self.reading = Some(InFlight {
            read: Box::pin(read_agents(Arc::clone(&w.lister))),
            stale: std::mem::take(&mut self.stale),
            resnapshot: std::mem::take(&mut self.resnapshot),
            closed: BTreeSet::new(),
        });
    }

    /// Fold a finished re-read into what the stream holds and owes.
    fn finish_read(
        &mut self,
        session: &str,
        flight: InFlight,
        read: Result<Vec<AgentSnapshot>, ReadFailure>,
    ) {
        match read {
            Ok(mut snapshots) => {
                if std::mem::take(&mut self.read_failing) {
                    log::info!("WatchAgents [{session}]: backend re-read recovered");
                }
                snapshots.retain(|a| !flight.closed.contains(&a.pane));
                let list = build_agent_list(&mut snapshots);
                if flight.resnapshot {
                    self.held = hold(&list);
                    self.outbox.push_back(snapshot_event(list));
                } else {
                    // Panes marked since this read started are answered by the
                    // next one; this answer may predate their event.
                    let fresh = reconcile(&mut self.held, &list, &self.stale);
                    self.outbox.extend(fresh);
                }
            }
            Err(failure) => {
                if std::mem::replace(&mut self.read_failing, true) {
                    log::debug!(
                        "WatchAgents [{session}]: backend re-read still failing ({failure})"
                    );
                } else {
                    log::warn!(
                        "WatchAgents [{session}]: backend re-read failed ({failure}) — keeping \
                         the held rows and retrying in the next window"
                    );
                }
                self.stale.extend(flight.stale);
                self.resnapshot |= flight.resnapshot;
            }
        }
    }

    /// Fold one bus event in. `false` means the bus closed and the stream ends.
    fn on_bus(&mut self, session: &str, recv: Result<MuxEvent, RecvError>) -> bool {
        match recv {
            Ok(MuxEvent::AgentStatusChanged(ev)) => {
                if let Some(msg) = apply_status(&self.held, &ev) {
                    self.held.insert(msg.pane_id, msg.clone());
                    self.outbox.push_back(changed_event(msg));
                }
                // Applied or not, the row is owed its reconciliation: the event
                // cannot carry the backend's counters, summary, focus or cwd.
                self.stale.insert(ev.pane);
            }
            Ok(MuxEvent::PaneClosed(closed)) => {
                self.stale.remove(&closed.pane);
                if let Some(flight) = self.reading.as_mut() {
                    flight.closed.insert(closed.pane);
                }
                if self.held.remove(&closed.pane).is_some() {
                    self.outbox.push_back(removed_event(closed.pane));
                }
            }
            Ok(MuxEvent::LayoutChanged(_)) => {}
            Ok(MuxEvent::Resynced) => {
                log::debug!("WatchAgents [{session}]: event feed resynced — re-snapshotting");
                self.resnapshot = true;
            }
            Err(RecvError::Lagged(n)) => {
                log::info!("WatchAgents [{session}]: fell {n} event(s) behind — re-snapshotting");
                self.resnapshot = true;
            }
            Err(RecvError::Closed) => {
                log::info!("WatchAgents [{session}]: event bus closed — ending stream");
                return false;
            }
        }
        true
    }
}

/// How a wait inside the loop ended.
enum Flow {
    Continue,
    /// The client went away.
    Gone,
    /// The token re-check failed.
    Revoked,
    /// The event bus closed (server shutdown).
    BusClosed,
}

/// Hand one message to the channel, waiting for room inside a `select!` whose
/// siblings are the token re-check and the client's departure — so a client that
/// stops reading can neither suspend the re-check nor hold the task forever.
async fn deliver(w: &Watch, tx: &WatchTx, recheck: &mut Interval, ev: AgentEvent) -> Flow {
    loop {
        tokio::select! {
            () = tx.closed() => return Flow::Gone,
            _ = recheck.tick() => {
                if !token_still_valid(w).await {
                    return Flow::Revoked;
                }
            }
            permit = tx.reserve() => {
                return match permit {
                    Ok(permit) => {
                        permit.send(ev);
                        Flow::Continue
                    }
                    Err(_) => Flow::Gone,
                };
            }
        }
    }
}

/// Forward bus events to one stream until the client goes, the token is
/// revoked, or the bus closes. A failed re-read does not end it.
async fn watch_loop(
    w: Watch,
    mut events: broadcast::Receiver<MuxEvent>,
    tx: WatchTx,
    fatal: oneshot::Sender<Status>,
    mut st: WatchState,
) {
    let session = w.session.clone();
    let mut recheck = tokio::time::interval(w.tuning.token_recheck);
    // The first tick fires immediately; the auth layer validated at open.
    recheck.set_missed_tick_behavior(MissedTickBehavior::Delay);
    recheck.tick().await;

    let flow = 'run: loop {
        while let Some(ev) = st.outbox.pop_front() {
            match deliver(&w, &tx, &mut recheck, ev).await {
                Flow::Continue => {}
                ended => break 'run ended,
            }
        }
        let reread_due = st.last_list + w.tuning.list_debounce;
        let reread_wanted = st.reread_wanted();
        tokio::select! {
            () = tx.closed() => break 'run Flow::Gone,
            _ = recheck.tick() => {
                if !token_still_valid(&w).await {
                    break 'run Flow::Revoked;
                }
            }
            read = async {
                match st.reading.as_mut() {
                    Some(flight) => flight.read.as_mut().await,
                    None => std::future::pending().await,
                }
            }, if st.reading.is_some() => {
                let flight = st.reading.take().expect("guarded by the branch condition");
                st.finish_read(&session, flight, read);
            }
            () = tokio::time::sleep_until(reread_due), if reread_wanted => st.start_read(&w),
            recv = events.recv() => {
                if !st.on_bus(&session, recv) {
                    break 'run Flow::BusClosed;
                }
            }
        }
    };
    match flow {
        Flow::Revoked => {
            log::warn!("WatchAgents [{session}]: token no longer valid — ending stream");
            let _ = fatal.send(Status::unauthenticated("session token no longer valid"));
        }
        Flow::Gone => {
            log::debug!("WatchAgents [{session}]: client went away — ending stream");
        }
        Flow::BusClosed | Flow::Continue => {}
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
/// `None` means only a re-read can place it. Either way the caller marks the
/// pane for one: this is the fast path, not the reconciled row.
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
/// and return the deltas: a `changed` per row that is new or differs in **any**
/// field (in the fresh list's priority order), then a `removed_pane_id` per held
/// pane the fresh list no longer carries (ascending).
///
/// Panes in `defer` were marked after this inventory was requested, so it may
/// predate their event: they are skipped both ways and keep their held rows
/// until the read that answers them.
fn reconcile(
    held: &mut HashMap<u32, AgentMsg>,
    fresh: &AgentList,
    defer: &BTreeSet<u32>,
) -> Vec<AgentEvent> {
    let mut next: HashMap<u32, AgentMsg> = HashMap::with_capacity(fresh.agents.len());
    let mut out: Vec<AgentEvent> = Vec::new();
    for a in fresh.agents.iter().filter(|a| !defer.contains(&a.pane_id)) {
        if held.get(&a.pane_id) != Some(a) {
            out.push(changed_event(a.clone()));
        }
        next.insert(a.pane_id, a.clone());
    }
    let mut gone: Vec<u32> = held
        .keys()
        .filter(|pane| !defer.contains(pane) && !next.contains_key(pane))
        .copied()
        .collect();
    gone.sort_unstable();
    out.extend(gone.into_iter().map(removed_event));
    for pane in defer {
        if let Some(row) = held.get(pane) {
            next.insert(*pane, row.clone());
        }
    }
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

    use std::collections::{BTreeSet, HashMap};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::sync::broadcast;
    use tokio::time::Instant;
    use tokio_stream::StreamExt;

    use crate::multiplexer::events::{AgentStatusChanged, LayoutChanged, MuxEvent, PaneClosed};
    use crate::proto::{AgentEvent, AgentList, AgentMsg, agent_event::Kind};

    use super::{
        AgentLister, Watch, WatchStream, WatchTuning, apply_status, reconcile, start_watch,
    };

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
    /// `list_agents` call happened (on the runtime's clock, so paused-time tests
    /// see virtual instants). `fail_next` makes that many calls fail first.
    #[derive(Default)]
    struct Inventory {
        agents: Mutex<Vec<AgentSnapshot>>,
        calls: Mutex<Vec<Instant>>,
        fail_next: AtomicUsize,
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
        fn calls(&self) -> Vec<Instant> {
            self.calls.lock().unwrap().clone()
        }
        /// What `ListAgents` answers for the current inventory.
        fn listed(&self) -> AgentList {
            super::build_agent_list(&mut self.agents.lock().unwrap().clone())
        }
    }

    const QUIET: WatchTuning = WatchTuning {
        list_debounce: Duration::from_millis(100),
        token_recheck: Duration::from_secs(3600),
    };

    fn lister_for(inv: &Arc<Inventory>) -> AgentLister {
        let inv = Arc::clone(inv);
        Arc::new(move || {
            inv.calls.lock().unwrap().push(Instant::now());
            let failing = inv
                .fail_next
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if failing {
                anyhow::bail!("agents: daemon unreachable at /run/stub.sock");
            }
            Ok(inv.agents.lock().unwrap().clone())
        })
    }

    fn watch(inv: &Arc<Inventory>, token: Option<&str>, valid: bool, tuning: WatchTuning) -> Watch {
        revocable_watch(inv, token, Arc::new(AtomicBool::new(valid)), tuning)
    }

    /// A watch whose token stays valid exactly as long as `valid` holds.
    fn revocable_watch(
        inv: &Arc<Inventory>,
        token: Option<&str>,
        valid: Arc<AtomicBool>,
        tuning: WatchTuning,
    ) -> Watch {
        Watch {
            session: "herdr:herdr".to_owned(),
            lister: lister_for(inv),
            token: token.map(str::to_owned),
            validate: Arc::new(move |_| Ok(valid.load(Ordering::SeqCst))),
            tuning,
        }
    }

    type Rx = WatchStream;

    async fn recv(rx: &mut Rx) -> Option<Result<AgentEvent, tonic::Status>> {
        tokio::time::timeout(WAIT, rx.next())
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

    /// Wait until the stream task has dropped its bus receiver, i.e. ended.
    async fn task_ended(bus: &crate::multiplexer::events::EventBus, within: Duration) {
        tokio::time::timeout(within, async {
            while bus.receiver_count() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the stream task must end and release its bus receiver");
    }

    fn changed(ev: Option<Result<AgentEvent, tonic::Status>>) -> AgentMsg {
        match kind(ev) {
            Kind::Changed(msg) => msg,
            other => panic!("expected changed, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
    async fn a_client_drop_ends_the_stream_task() {
        let inv = Inventory::with(two_agents());
        let (bus, rx) = started(&inv, QUIET).await;
        assert_eq!(bus.receiver_count(), 1);
        drop(rx);
        task_ended(&bus, WAIT).await;
    }

    // ── convergence of the fast path (fix round 1, A) ──

    /// Publish an applicable `status` event for `pane` after moving the backend's
    /// row with `moved`; the stream must answer the status at once, then the
    /// exact `ListAgents` row within one debounce window, and hold that row.
    async fn assert_converges(
        pane: u32,
        status: AgentStatus,
        kind_label: &str,
        moved: impl FnOnce(&mut AgentSnapshot),
    ) -> (AgentMsg, AgentMsg) {
        let inv = Inventory::with(two_agents());
        let (bus, mut rx) = started(&inv, QUIET).await;
        let opened = Instant::now();
        let held_before = inv
            .listed()
            .agents
            .into_iter()
            .find(|a| a.pane_id == pane)
            .expect("the pane is listed");

        let mut after = two_agents();
        let row = after
            .iter_mut()
            .find(|a| a.pane == pane)
            .expect("the pane is listed");
        row.status = status;
        moved(row);
        inv.set(after);
        bus.send(status_event(
            pane,
            status,
            Some(kind_label),
            Some(u64::from(pane) * 100),
        ))
        .expect("subscribed");

        let fast = changed(recv(&mut rx).await);
        let mut expected_fast = held_before;
        expected_fast.status = status_wire(status) as i32;
        assert_eq!(
            fast, expected_fast,
            "first: the held row with the new status, nothing else moved"
        );

        let reconciled = changed(recv(&mut rx).await);
        assert!(
            Instant::now() - opened <= QUIET.list_debounce,
            "the reconciled row arrives within one window"
        );
        let listed = inv
            .listed()
            .agents
            .into_iter()
            .find(|a| a.pane_id == pane)
            .expect("the pane is listed");
        assert_eq!(
            reconciled, listed,
            "second: exactly the row ListAgents answers"
        );

        // The stream now holds that row: another re-read of the same inventory
        // finds nothing to send.
        bus.send(status_event(pane, status, None, None))
            .expect("subscribed");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), rx.next())
                .await
                .is_err(),
            "the held row equals the ListAgents row, so a re-read sends nothing"
        );
        assert_eq!(inv.calls().len(), 3, "snapshot, reconciliation, re-check");
        (fast, reconciled)
    }

    fn status_wire(status: AgentStatus) -> WireStatus {
        super::agent_status_msg(status)
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_transition_converges_to_the_list_agents_row_within_one_window() {
        let (fast, reconciled) = assert_converges(9, AgentStatus::Blocked, "cli-b", |row| {
            row.change_seq = 12;
            row.summary = Some("needs approval".to_owned());
            row.cwd = Some("/src/lib".to_owned());
            row.focused = true;
        })
        .await;
        assert_eq!(
            (fast.status, fast.change_seq),
            (WireStatus::Blocked as i32, 11)
        );
        assert_eq!(
            (reconciled.status, reconciled.change_seq),
            (WireStatus::Blocked as i32, 12)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_done_transition_converges_with_its_advanced_completion_seq() {
        let (fast, reconciled) = assert_converges(7, AgentStatus::Done, "cli-a", |row| {
            row.change_seq = 4;
            row.completion_seq = Some(3);
            row.summary = Some("finished".to_owned());
            row.focused = false;
        })
        .await;
        assert_eq!(
            (fast.status, fast.completion_seq),
            (WireStatus::Done as i32, 2)
        );
        assert_eq!(
            (
                reconciled.status,
                reconciled.completion_seq,
                reconciled.change_seq
            ),
            (WireStatus::Done as i32, 3, 4)
        );
    }

    // ── resync (fix round 1, B) ──

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_resync_markers_re_snapshots_once() {
        let inv = Inventory::with(two_agents());
        let (bus, mut rx) = started(&inv, QUIET).await;
        let mut moved = two_agents();
        moved[1].status = AgentStatus::Idle;
        moved.push(snapshot(5, "cli-c", AgentStatus::Working, 1));
        inv.set(moved);
        for _ in 0..3 {
            bus.send(MuxEvent::Resynced).expect("subscribed");
        }
        assert_eq!(
            kind(recv(&mut rx).await),
            Kind::Snapshot(inv.listed()),
            "a resync re-snapshots, exactly as ListAgents answers"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), rx.next())
                .await
                .is_err(),
            "the burst coalesced into one snapshot"
        );
        assert_eq!(inv.calls().len(), 2, "the snapshot plus one re-read");
    }

    /// The whole path across a herdr event-kernel reconnect: a fake herdr drops
    /// the agent-status connection; the kernel reconnects and resyncs against a
    /// listing where one pane vanished and one went working; the stream answers
    /// with `removed_pane_id` for the vanished pane and then a fresh snapshot.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_across_a_kernel_reconnect_removes_the_vanished_pane_then_re_snapshots() {
        use crate::multiplexer::herdr::api::AgentStatus as Herdr;
        use crate::multiplexer::herdr::subscribe::tests::fake_herdr::{
            FakeHerdr, FakeState, RunningKernel, pane,
        };

        let fake = FakeHerdr::start(FakeState {
            panes: vec![
                pane("w1:p1", Herdr::Blocked),
                pane("w1:p2", Herdr::Idle),
                pane("w1:p3", Herdr::Idle),
            ],
            ..FakeState::default()
        });
        let mut kernel = RunningKernel::start_quiet(&fake);
        kernel.until_resynced().await;
        let id = |hid: &str| kernel.panes.id_for_herdr_pane(hid).expect("registered");
        let (p1, p2, p3) = (id("w1:p1"), id("w1:p2"), id("w1:p3"));

        // The inventory the stream reads, mirroring the fake's listing.
        let row = |pane: u32, status: AgentStatus| {
            let mut a = snapshot(pane, "claude", status, 1);
            a.tab_id = 1;
            a.space_id = "w1".to_owned();
            a
        };
        let inv = Inventory::with(vec![
            row(p1, AgentStatus::Blocked),
            row(p2, AgentStatus::Idle),
            row(p3, AgentStatus::Idle),
        ]);
        let mut rx = start_watch(
            watch(&inv, Some("tok"), true, QUIET),
            Some(kernel.events.resubscribe()),
        )
        .await
        .expect("the watch starts");
        assert_eq!(panes_of(&kind(recv(&mut rx).await)).len(), 3);

        inv.set(vec![
            row(p1, AgentStatus::Blocked),
            row(p2, AgentStatus::Working),
        ]);
        fake.with(|s| {
            s.panes = vec![pane("w1:p1", Herdr::Blocked), pane("w1:p2", Herdr::Working)];
            // Ends the fake's agent-status connection: the kernel reads EOF.
            s.status_tx = None;
        });

        assert_eq!(
            kind(recv(&mut rx).await),
            Kind::RemovedPaneId(p3),
            "the pane that vanished while the connection was down"
        );
        // The resync's working event for p2 carries no kind (the listing has
        // none), so it only marks the pane; a re-read may answer it before the
        // marker turns the next one into the snapshot.
        loop {
            match kind(recv(&mut rx).await) {
                Kind::Changed(msg) => assert_eq!(msg.pane_id, p2),
                Kind::Snapshot(list) => {
                    assert_eq!(list, inv.listed(), "re-snapshotted after the resync");
                    break;
                }
                other => panic!("expected changed or snapshot, got {other:?}"),
            }
        }
        drop(rx);
        kernel.stop().await;
    }

    // ── the loop never blocks the re-check (fix round 1, C) ──

    #[tokio::test(start_paused = true)]
    async fn a_client_that_stops_reading_cannot_delay_the_token_recheck() {
        let inv = Inventory::with(two_agents());
        let valid = Arc::new(AtomicBool::new(true));
        let tuning = WatchTuning {
            token_recheck: super::TOKEN_RECHECK_INTERVAL,
            ..QUIET
        };
        let bus = bus();
        let mut rx = start_watch(
            revocable_watch(&inv, Some("tok"), Arc::clone(&valid), tuning),
            Some(bus.subscribe()),
        )
        .await
        .expect("the watch starts");

        // The client never reads: flood the stream with applicable transitions
        // until its channel is full and the task is parked on a send.
        for i in 0..super::WATCH_CHANNEL_BOUND * 2 {
            let status = if i % 2 == 0 {
                AgentStatus::Blocked
            } else {
                AgentStatus::Working
            };
            bus.send(status_event(9, status, Some("cli-b"), Some(900)))
                .expect("subscribed");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !bus.is_empty(),
            "the task is parked on the full channel, no longer draining the bus"
        );

        valid.store(false, Ordering::SeqCst);
        let revoked_at = Instant::now();
        task_ended(&bus, super::TOKEN_RECHECK_INTERVAL + Duration::from_secs(1)).await;
        assert!(
            Instant::now() - revoked_at <= super::TOKEN_RECHECK_INTERVAL,
            "the re-check ran on schedule while the send was stalled"
        );

        // The revocation overtakes everything the client never read.
        let err = recv(&mut rx)
            .await
            .expect("an error, not a silent end")
            .expect_err("UNAUTHENTICATED before any queued message");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert!(recv(&mut rx).await.is_none(), "the stream ends after it");
    }

    /// Real time on purpose: the backend parks a blocking thread, which holds
    /// paused time still, and this test's guard is a wall-clock timeout.
    #[tokio::test]
    async fn a_slow_re_read_never_blocks_closures_or_the_client_drop() {
        let inv = Inventory::with(two_agents());
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let calls = Arc::new(AtomicUsize::new(0));
        let lister: AgentLister = {
            let (inv, calls) = (Arc::clone(&inv), Arc::clone(&calls));
            Arc::new(move || {
                // The second read (the first re-read) parks until released.
                if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                    let _ = gate.lock().unwrap().recv();
                }
                Ok(inv.agents.lock().unwrap().clone())
            })
        };
        let w = Watch {
            lister,
            ..watch(&inv, Some("tok"), true, QUIET)
        };
        let bus = bus();
        let mut rx = start_watch(w, Some(bus.subscribe()))
            .await
            .expect("the watch starts");
        let _ = kind(recv(&mut rx).await);

        bus.send(status_event(9, AgentStatus::Idle, None, None))
            .expect("subscribed");
        tokio::time::timeout(WAIT, async {
            while calls.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the re-read starts");

        // While it is out, a closure still goes through at once…
        bus.send(MuxEvent::PaneClosed(PaneClosed { pane: 7 }))
            .expect("subscribed");
        assert_eq!(kind(recv(&mut rx).await), Kind::RemovedPaneId(7));
        // …and the client leaving ends the task without waiting for the backend.
        drop(rx);
        task_ended(&bus, WAIT).await;
        release.send(()).expect("the parked read is still waiting");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_re_read_keeps_the_stream_and_the_next_window_reconciles() {
        let inv = Inventory::with(two_agents());
        let (bus, mut rx) = started(&inv, QUIET).await;

        let mut agents = two_agents();
        agents[1].status = AgentStatus::Done;
        agents[1].change_seq = 12;
        inv.set(agents);
        inv.fail_next.store(1, Ordering::SeqCst);
        bus.send(status_event(9, AgentStatus::Done, None, None))
            .expect("subscribed");

        let msg = changed(recv(&mut rx).await);
        let listed = inv
            .listed()
            .agents
            .into_iter()
            .find(|a| a.pane_id == 9)
            .expect("listed");
        assert_eq!(
            msg, listed,
            "the retry reconciles; no error reached the client"
        );
        let calls = inv.calls();
        assert_eq!(calls.len(), 3, "snapshot, the failed read, the retry");
        assert!(
            calls[2] - calls[1] >= QUIET.list_debounce,
            "the retry waits out the window: {calls:?}"
        );
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
    fn reconcile_counts_a_difference_in_any_field_the_event_cannot_carry() {
        type Mutation = fn(&mut AgentMsg);
        let mutations: [(&str, Mutation); 7] = [
            ("change_seq", |m| m.change_seq += 1),
            ("completion_seq", |m| m.completion_seq += 1),
            ("has_completion_seq", |m| {
                m.has_completion_seq = !m.has_completion_seq;
            }),
            ("summary", |m| m.summary.push('!')),
            ("focused", |m| m.focused = !m.focused),
            ("cwd", |m| m.cwd.push_str("/x")),
            ("title", |m| m.title.push('!')),
        ];
        for (field, mutate) in mutations {
            let mut held = held_two();
            let mut fresh = super::build_agent_list(&mut two_agents());
            mutate(&mut fresh.agents[0]);
            let out = reconcile(&mut held, &fresh, &BTreeSet::new());
            assert_eq!(
                out.len(),
                1,
                "a different {field} alone is a change: {out:?}"
            );
            assert_eq!(
                held.get(&7),
                Some(&fresh.agents[0]),
                "{field}: held updated"
            );
        }
    }

    #[test]
    fn reconcile_defers_panes_marked_after_the_read_started() {
        let mut held = held_two();
        let before = held.clone();
        let mut fresh = two_agents();
        fresh.remove(0); // pane 7 missing from this (older) answer
        fresh[0].status = AgentStatus::Idle; // pane 9 differs
        fresh.push(snapshot(5, "cli-c", AgentStatus::Idle, 1)); // pane 5 new
        let fresh = super::build_agent_list(&mut fresh);
        let defer: BTreeSet<u32> = [5, 7, 9].into_iter().collect();

        let out = reconcile(&mut held, &fresh, &defer);
        assert!(
            out.is_empty(),
            "deferred panes wait for their own read: {out:?}"
        );
        assert_eq!(held, before, "their held rows stand; nothing new is held");
    }

    #[test]
    fn reconcile_reports_new_and_differing_rows_then_departures() {
        let mut held = held_two();
        let mut fresh = two_agents();
        fresh.remove(0); // pane 7 left
        fresh.push(snapshot(5, "cli-c", AgentStatus::Idle, 1)); // pane 5 arrived
        let fresh = super::build_agent_list(&mut fresh);

        let out: Vec<Kind> = reconcile(&mut held, &fresh, &BTreeSet::new())
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
