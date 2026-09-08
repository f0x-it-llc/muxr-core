//! herdr **event kernel** — a persistent consumer of herdr's JSON-API
//! `events.subscribe` stream, republishing agent-status transitions onto the
//! internal [`EventBus`](crate::multiplexer::events::EventBus).
//!
//! ## Why a connection style of its own
//!
//! [`HerdrControl`](super::control::HerdrControl) is **one connection per
//! request** (fresh [`std::os::unix::net::UnixStream`] per call, bounded read
//! timeout). `events.subscribe` is the opposite: subscribe **once** on a
//! long-lived socket, then read newline-delimited pushed events indefinitely.
//! This task therefore owns its own [`tokio::net::UnixStream`] readers — the
//! blocking `spawn_blocking` idiom applies to the *short* control calls, not to
//! these streaming readers. The only blocking work it does (pane enumeration,
//! the post-connect resync, and the liveness probe) is routed through
//! `HerdrControl` on the blocking pool.
//!
//! ## Live-only semantics (herdr 0.9.0) — and the assumption that hid a bug
//!
//! herdr captures a sequence **watermark** the moment a subscription request is
//! accepted and delivers only events *after* it; retained history is never
//! replayed (source: `src/api/server.rs::stream_subscriptions` takes
//! `event_hub.current_sequence()` before building any subscription, and
//! `ActiveEventSubscription` starts at that sequence; the prose says it too:
//! *"Lifecycle subscriptions start when the request is accepted and do not
//! replay events retained before that point."*).
//!
//! **Earlier versions of this module asserted the opposite** — that herdr
//! replays a `pane.created` for every existing pane right after the ack — and
//! built its "is this pane new?" test on that replay. That stale assumption is
//! precisely what concealed the **bootstrap gap** below: as long as a replay was
//! believed to re-announce everything, a pane missed at connect time looked
//! self-healing. It was not. Nothing in herdr ever re-announced it, so the pane
//! stayed permanently unsubscribed and its `blocked` / `done` transitions —
//! i.e. its push notifications — silently stopped. Do not reintroduce a
//! replay-shaped assumption here; treat the stream as live-only.
//!
//! ### The bootstrap gap, and why the obvious fix is also wrong
//!
//! With one connection, the sequence was: enumerate panes → connect → subscribe
//! (one per-pane agent-status entry per enumerated pane) → ack. A pane created
//! *between* the enumeration and the ack is lost twice: it is absent from the
//! enumerated set, so no agent-status entry covers it, and its `pane.created`
//! fell below the watermark, so nothing ever widens the set for it.
//!
//! Merely reordering to subscribe-then-enumerate **inverts** the guard rather
//! than fixing it: a pane created between the subscribe and the enumeration then
//! appears in both the push and the snapshot, is judged already known, and its
//! widening is suppressed — the identical hole, reintroduced.
//!
//! ## The upstream constraint that shapes everything: no wildcard
//!
//! herdr 0.9.0 **still requires an explicit per-pane id** on agent-status
//! subscriptions. `Subscription::PaneAgentStatusChanged { pane_id: String, .. }`
//! takes a bare, required `String`; an absent id is a deserialization error that
//! rejects the *entire* `subscriptions` array; the runtime filter is an
//! unconditional `pane_id != self.pane_id` string comparison with no match-all
//! arm; and there is no all-panes agent-status event. So the per-pane set and
//! the widen-by-reconnect machinery must stay — only the widening *trigger*
//! changes. This was settled by dedicated research; it is not open.
//!
//! ## The design: two connections
//!
//! This is what herdr's own 0.9.0 documentation prescribes for exactly this
//! problem (*"first open `events.subscribe` on another connection and wait for
//! its acknowledgement. Buffer that stream while calling … then apply the
//! buffered events in order"*).
//!
//! - **Connection A — the lifecycle watcher.** Subscribes only to the *bare*
//!   `pane.created`, `pane.closed` and `pane.exited` entries. None takes a pane
//!   id, so A is **complete the moment it is acked**: it never needs to widen,
//!   never reconnects to widen, and has no bootstrap gap of its own. A is the
//!   authoritative signal that a new pane exists.
//! - **Connection B — the agent-status watcher.** Carries the per-pane
//!   `pane.agent_status_changed` entries, and is the only connection torn down
//!   and re-established when the pane set widens.
//! - **Ordering that closes the gap.** A is brought up and *acked* before the
//!   pane enumeration starts. Whatever A delivers while the enumeration is in
//!   flight is buffered and folded into B's pane set before B's request is built
//!   ([`bootstrap_pane_set`]), so a pane created during the enumeration window is
//!   announced by A instead of being lost.
//! - **The widening rule.** A `pane.created` widens iff its id is absent from
//!   the set B is **currently subscribed to** — mutable session state, not a
//!   connect-time snapshot. That is what makes the guard immune to the inversion
//!   above: a pane already folded into B's request is covered and must not
//!   trigger a rebuild, and a pane that is not covered always triggers one.
//!
//! A pane that closes is dropped from that set: B's subscription for it is inert
//! (herdr resolved it at subscribe time), and dropping it means a later pane
//! reusing the id is correctly treated as new.
//!
//! ## Idle-liveness watchdog
//!
//! There is a still-open upstream failure mode in which an acknowledged
//! subscription stops delivering while its connection stays open — herdr's event
//! hub is behind a `Mutex` whose poisoning is swallowed (`let Ok(..) else {
//! return }`), and its retained-event ring is bounded, so a wedged or
//! overrun hub goes quiet without ever closing the socket. Backoff and resync
//! are the right shape of defence but neither fires while the socket is healthy,
//! so a wedged-but-open subscription would sit silent indefinitely.
//!
//! **Reasoning behind the interval, as the design requires stating.** A raw
//! read-side idle timeout cannot work on this protocol on its own: herdr sends
//! no heartbeat, and a genuinely quiet system — no agent activity, no panes
//! opening — is *legitimately* silent for hours on **both** connections. Any
//! timeout short enough to catch a wedge would tear down a healthy, quiet
//! subscription on a fixed schedule; that is the reconnect loop the design must
//! not have. A and B do have very different expected event rates (A sees pane
//! lifecycle, B sees agent transitions), but that difference does not rescue a
//! rate-derived timeout, because *both* are legitimately silent. Nor can the
//! probe be a write: herdr's stream loop treats **any** readable inbound byte as
//! a disconnect, so a keepalive would kill the connection it was meant to test.
//!
//! So the watchdog is a **shared periodic probe on the control plane** — a
//! read-only `workspace.list` + `pane.list` round trip on the ordinary
//! connection-per-request transport, never a write to a subscription socket —
//! that fires only on *evidence that an event was missed*:
//!
//! - a pane exists that is not in B's subscribed set ⇒ A never announced it ⇒ A
//!   is wedged (rebuilding A rebuilds B, so this is the stronger verdict);
//! - a subscribed pane's status differs from the last value B delivered ⇒ B is
//!   wedged.
//!
//! A quiet system produces neither, so it never churns; the probe is skipped
//! entirely for a connection that delivered something within the interval, since
//! that connection has just proven itself. To absorb the benign race where the
//! probe reads a transition a beat before the push carrying it arrives, a
//! verdict must repeat on [`LIVENESS_STRIKES`] consecutive probes before the
//! connection is torn down and re-established through the normal backoff path.
//! [`ACK_TIMEOUT`] is the same idea at connect time: a subscription that never
//! acks is abandoned rather than parked on forever.
//!
//! A missed *close* is deliberately not treated as evidence: a dead pane's
//! subscription is inert, no notification is owed for it, and the stale set
//! entry is cleared by the next rebuild.
//!
//! ## Reconnect, backoff and the widening storm breaker
//!
//! Each connection owns its own exponential backoff ([`INITIAL_BACKOFF`] →
//! [`MAX_BACKOFF`], reset after a session that stayed up ≥ [`STABLE_THRESHOLD`]).
//! The split makes B's failure path cheap and independent: a stale pane id —
//! herdr resolves every per-pane entry at subscribe time, so an id naming a pane
//! that vanished aborts the *entire* request with an error and no ack — now
//! kills only B, which backs off, **re-enumerates**, and retries. A is untouched
//! and keeps announcing lifecycle events throughout. (Re-enumerating on every B
//! attempt is what stops a stale id from failing identically forever.)
//!
//! A widening is not a failure: it costs a short [`GROW_COALESCE_WINDOW`] during
//! which A keeps being drained, so a burst that creates N panes at once —
//! `layout.apply` emits one `pane.created` per pane, and workspace creation
//! emits one for the root pane — collapses into **one** rebuild rather than N.
//!
//! That coalescing is why [`GROW_STORM_THRESHOLD`] could be re-reasoned rather
//! than inherited. The old comment justified its threshold on "a real new pane is
//! a one-off", which was never true of a layout apply and would have tripped the
//! breaker on ordinary use under the new trigger. What is true now: (1) kernel
//! start no longer burns a widening at all — panes appearing during the
//! enumeration window are folded into B's first request instead of forcing a
//! rebuild; (2) a legitimate burst is coalesced into one widening; so a
//! legitimate widening rate is bounded by human/agent pane-opening *bursts*,
//! comfortably under 8 per 30 s, while a genuine fault (every push wrongly
//! judged new) produces one per coalesce window plus reconnect — dozens in the
//! same span, and still caught. When the breaker engages it escalates **B's**
//! backoff only, so even a storm leaves the lifecycle watcher up.
//!
//! ## Wire schema (verified against herdr v0.9.0 source, Apache-2.0)
//!
//! herdr relicensed from AGPL-3.0-or-later to **Apache-2.0** at v0.8.0, so its
//! source may now be read and derived from with attribution; the shapes below
//! are verified against it rather than inferred from captures.
//!
//! - **Request** — `{"id":…,"method":"events.subscribe","params":{"subscriptions":[…]}}`,
//!   one request per connection (herdr reads exactly one request line per
//!   connection, under a 5 s initial-request timeout). An empty `subscriptions`
//!   array is valid and simply acks with nothing to deliver.
//! - **Ack** — `{"id":…,"result":{"type":"subscription_started"}}`.
//! - **Rejection** — `{"id":…,"error":{"code":…,"message":…}}`, after which herdr
//!   closes the connection: any per-pane id that no longer resolves takes the
//!   whole request down with it.
//! - **Pushed lifecycle event** — the envelope's event name is the `snake_case`
//!   `EventKind` (`"pane_created"`), and `data` is the tagged `EventData`:
//!   `{"event":"pane_created","data":{"type":"pane_created","pane":{…,"pane_id":"w1:p2"}}}`;
//!   `pane_closed` / `pane_exited` carry `pane_id` at the top of `data`.
//! - **Pushed agent-status event** — a *different* envelope whose event name is
//!   the dotted `SubscriptionEventKind`, with an untagged payload:
//!   `{"event":"pane.agent_status_changed","data":{"pane_id":"w1:p1",
//!   "workspace_id":"w1","agent_status":"blocked","agent":…,"title":…,
//!   "display_agent":…,"state_labels":{…}}}`. herdr now carries presentation
//!   fields here, so [`parse_agent_status`] reads `title` from the push instead
//!   of leaving it `None` for resync-only enrichment. `agent`, `display_agent`
//!   and `state_labels` have no home on the neutral
//!   [`AgentStatusChanged`](crate::multiplexer::events::AgentStatusChanged) and
//!   are deliberately not plumbed from here.
//!
//! Both name forms are parsed tolerantly (dot and underscore alike), since the
//! two envelopes genuinely disagree.
//!
//! ## Never write to a subscription socket after the request
//!
//! herdr's stream loop probes the socket between polls and treats **any**
//! readable byte — including the EOF of a half-close — as a disconnect
//! (`probe_stream_closed`: `Ok(0) => true`, `Ok(_) => true`). So: one request per
//! connection, then read only, no keepalive, no heartbeat; and the write half is
//! *held* for the connection's lifetime (see [`Conn`]) because dropping it would
//! shut the write side down and herdr would read that as a disconnect.
//!
//! ## Resync and synthetic resend suppression
//!
//! The resync is now the *only* mechanism that can observe state predating a
//! subscription, so it is more load-bearing than before, not less. After every B
//! (re)connect it re-observes current pane agent states through `HerdrControl`
//! and emits synthetic [`AgentStatus::Blocked`] / [`AgentStatus::Done`] events
//! (`synthetic: true`) — but **only** where a pane's status differs from its
//! last-known value, so a flapping connection never re-pings a consumer about
//! the same still-stuck agent. The last-known table lives for the whole kernel
//! lifetime, is updated by live pushes, and is pruned for panes that vanish from
//! the enumeration. The downstream push sender deliberately skips synthetic
//! `done` events; that contract is not this module's to change.
//!
//! ## Read-only
//!
//! This task never *mutates* herdr — no `workspace.focus`, no lifecycle calls. It
//! subscribes and lists. Pane-id translation goes through the shared
//! [`HerdrPaneRegistry`] using a **non-mutating** lookup
//! ([`HerdrPaneRegistry::id_for_herdr_pane`]) so a pushed event that omits
//! `terminal_id` can never clobber a live pane's relay attach key.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::multiplexer::events::{AgentStatus, AgentStatusChanged, EventBus, MuxEvent};

use super::api::{AgentStatus as HerdrAgentStatus, ApiRequest};
use super::backend::HerdrBackend;
use super::control::HerdrControl;
use super::paths::HerdrSocketPaths;
use super::registry::HerdrPaneRegistry;

// ── Tunables ────────────────────────────────────────────────────────────────────

/// First reconnect delay after a dropped/failed subscription.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Reconnect-delay ceiling. Backoff doubles from [`INITIAL_BACKOFF`] up to this
/// cap: 1, 2, 4, 8, 16, 32, 60, 60, …
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A connection that stayed up at least this long is considered "stable" — the
/// next drop resets *that connection's* backoff to [`INITIAL_BACKOFF`] rather
/// than continuing to escalate (so a herdr that flaps once an hour never sits at
/// the 60 s cap).
const STABLE_THRESHOLD: Duration = Duration::from_secs(60);

/// How long to wait for `subscription_started` before abandoning a connection
/// attempt. herdr acks as soon as it has resolved every entry, so this is orders
/// of magnitude above the expected latency; it exists so a herdr that accepts the
/// socket and then wedges cannot park the kernel forever with no watchdog running
/// (the periodic probe only runs once both connections are up).
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// After a widening is decided, connection A keeps being drained for this long
/// before connection B is rebuilt, so a burst of pane creations (a `layout.apply`
/// emits one `pane.created` per pane) collapses into a single rebuild.
const GROW_COALESCE_WINDOW: Duration = Duration::from_millis(250);

/// Rolling window over which widenings are counted for storm detection
/// (see [`GROW_STORM_THRESHOLD`] and [`record_grow_and_check_storm`]).
const GROW_STORM_WINDOW: Duration = Duration::from_secs(30);

/// More than this many widenings within [`GROW_STORM_WINDOW`] trips the circuit
/// breaker. See the module header for the reasoning actually applied: kernel
/// start no longer burns a widening (panes appearing during the enumeration
/// window are folded into B's first request), and a creation burst is coalesced
/// into one widening by [`GROW_COALESCE_WINDOW`] — so legitimate traffic is
/// bounded by *bursts*, not panes, and 8 leaves generous headroom over hectic but
/// real use, while a fault that widens on every push produces dozens in the same
/// window and is still caught.
const GROW_STORM_THRESHOLD: usize = 8;

/// Period of the shared control-plane liveness probe. Long enough that a quiet
/// herdr costs almost nothing and a genuine push always wins the race against it;
/// short enough that a wedged subscription is noticed in minutes, not hours.
const LIVENESS_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Consecutive probes that must return the same wedged verdict before a
/// connection is torn down. Two probes means the evidence survived a full
/// [`LIVENESS_PROBE_INTERVAL`] with no delivery, which no benign
/// probe-beats-the-push race can do.
const LIVENESS_STRIKES: u32 = 2;

/// Hard ceiling on a single pushed-event line, mirroring the control plane's
/// [`MAX_RESPONSE_BYTES`](super::control::MAX_RESPONSE_BYTES) defence: a peer
/// streaming bytes without a newline must not grow the line buffer unbounded.
/// Agent-status events are tiny; 1 MiB is generous while still bounding a hostile
/// stream.
const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;

/// Per-pane agent-status subscription type (connection B).
const SUBSCRIBE_AGENT_STATUS_TYPE: &str = "pane.agent_status_changed";

/// Pane-created subscription type (bare, no `pane_id`) — connection A.
const SUBSCRIBE_PANE_CREATED_TYPE: &str = "pane.created";

/// Pane-closed subscription type (bare, no `pane_id`) — connection A.
const SUBSCRIBE_PANE_CLOSED_TYPE: &str = "pane.closed";

/// Pane-exited subscription type (bare, no `pane_id`) — connection A. The
/// classification path has always understood `pane.exited`; before the split the
/// request never asked for it, so that branch was dead.
const SUBSCRIBE_PANE_EXITED_TYPE: &str = "pane.exited";

/// Request id used for connection A, echoed back on its ack.
const LIFECYCLE_REQUEST_ID: &str = "muxrd-events-lifecycle";

/// Request id used for connection B, echoed back on its ack.
const AGENT_STATUS_REQUEST_ID: &str = "muxrd-events-agent-status";

/// Every timing/threshold knob in one place, so tests can drive the real state
/// machine at millisecond scale. Production always uses [`Tuning::default`].
#[derive(Debug, Clone, Copy)]
struct Tuning {
    initial_backoff: Duration,
    max_backoff: Duration,
    stable_threshold: Duration,
    ack_timeout: Duration,
    grow_coalesce_window: Duration,
    grow_storm_window: Duration,
    grow_storm_threshold: usize,
    probe_interval: Duration,
    probe_strikes: u32,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            initial_backoff: INITIAL_BACKOFF,
            max_backoff: MAX_BACKOFF,
            stable_threshold: STABLE_THRESHOLD,
            ack_timeout: ACK_TIMEOUT,
            grow_coalesce_window: GROW_COALESCE_WINDOW,
            grow_storm_window: GROW_STORM_WINDOW,
            grow_storm_threshold: GROW_STORM_THRESHOLD,
            probe_interval: LIVENESS_PROBE_INTERVAL,
            probe_strikes: LIVENESS_STRIKES,
        }
    }
}

// ── Public entry point ──────────────────────────────────────────────────────────

/// Spawn the herdr event kernel as a background Tokio task.
///
/// Publishes [`MuxEvent`]s onto `bus`; exits cleanly when `shutdown` is set to
/// `true`. Shares the backend's [`HerdrPaneRegistry`] / [`HerdrTabRegistry`]
/// `Arc`s so translated pane ids stay identical to those handed out by layout
/// polls, and builds its own long-lived subscription sockets (plus a per-request
/// `HerdrControl` over the same socket path for pane enumeration, resync and the
/// liveness probe).
///
/// Called from `bin/muxrd.rs::serve()` only when a herdr backend is present; a
/// zellij-only server never spawns it.
pub fn spawn_event_kernel(
    backend: &HerdrBackend,
    bus: EventBus,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let paths = HerdrSocketPaths::resolve();
    let panes = Arc::clone(backend.pane_registry());
    let tabs = Arc::clone(backend.tab_registry());
    // A dedicated control client sharing the SAME registries as the backend, so
    // resync `assign_or_get` calls and layout-poll ids agree.
    let control = Arc::new(HerdrControl::new(
        paths.api.clone(),
        Arc::clone(&panes),
        tabs,
    ));
    tokio::spawn(run(paths.api, panes, control, bus, shutdown))
}

// ── Kernel state ────────────────────────────────────────────────────────────────

/// Shared last-known agent status per neutral pane id, used to suppress duplicate
/// synthetic resync emissions and as the watchdog's baseline. Lives for the whole
/// kernel lifetime (across reconnects) so a flapping connection does not re-ping
/// unchanged blocked panes.
type StatusTable = Arc<Mutex<HashMap<u32, AgentStatus>>>;

/// Everything a running kernel needs and never rebuilds.
struct Kernel {
    api_socket: PathBuf,
    panes: Arc<HerdrPaneRegistry>,
    control: Arc<HerdrControl>,
    bus: EventBus,
    last_status: StatusTable,
    tuning: Tuning,
}

/// One live subscription connection (A or B).
struct Conn {
    reader: LineReader<BufReader<OwnedReadHalf>>,
    /// Held open for the connection's lifetime and **never written to again**.
    /// Dropping it half-closes the socket, and herdr's stream loop reads a
    /// readable byte — EOF included — as a disconnect, so letting this go would
    /// silently kill the subscription.
    _write: OwnedWriteHalf,
    /// When the connection was established, for the stable-session backoff reset.
    opened: Instant,
    /// When it last delivered anything, for the watchdog's idle pre-filter.
    last_event: Instant,
}

impl Conn {
    fn new(reader: LineReader<BufReader<OwnedReadHalf>>, write: OwnedWriteHalf) -> Self {
        let now = Instant::now();
        Self {
            reader,
            _write: write,
            opened: now,
            last_event: now,
        }
    }

    /// Next pushed line; `Ok(None)` is EOF. Cancel-safe — see [`LineReader`].
    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        self.reader.next_line().await
    }

    fn mark_event(&mut self) {
        self.last_event = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_event.elapsed()
    }
}

/// Outcome of bringing one connection up.
enum Bring {
    /// Shutdown was requested mid-attempt.
    Shutdown,
    /// The attempt failed — back off and retry this connection.
    Failed,
    /// Connection A died while connection B was being established. Only
    /// [`connect_status`] can return this.
    LifecycleLost,
    Ready(Conn),
}

/// What ended a [`pump`] cycle.
enum Pump {
    Shutdown,
    /// Connection A dropped, errored or was judged wedged. Both connections are
    /// re-established: A's fresh watermark invalidates B's pane set.
    LifecycleLost,
    /// Connection B dropped, errored or was judged wedged. A is untouched.
    StatusLost,
    /// A announced a pane outside B's subscribed set — rebuild B.
    Widen,
}

/// One `select!` outcome inside [`pump`]. Branches yield an owned value so all
/// mutation happens after the borrowed futures are dropped.
enum Tick {
    Shutdown,
    Lifecycle(std::io::Result<Option<String>>),
    Status(std::io::Result<Option<String>>),
    Probe,
}

/// Consecutive wedged verdicts per connection.
#[derive(Debug, Default, Clone, Copy)]
struct Strikes {
    lifecycle: u32,
    status: u32,
}

// ── Reconnect loop ──────────────────────────────────────────────────────────────

/// The kernel loop: keep both subscriptions up until shutdown.
async fn run(
    api_socket: PathBuf,
    panes: Arc<HerdrPaneRegistry>,
    control: Arc<HerdrControl>,
    bus: EventBus,
    shutdown: watch::Receiver<bool>,
) {
    run_tuned(api_socket, panes, control, bus, shutdown, Tuning::default()).await;
}

async fn run_tuned(
    api_socket: PathBuf,
    panes: Arc<HerdrPaneRegistry>,
    control: Arc<HerdrControl>,
    bus: EventBus,
    mut shutdown: watch::Receiver<bool>,
    tuning: Tuning,
) {
    log::info!(
        "herdr event kernel: starting (socket {})",
        api_socket.display()
    );
    let kernel = Kernel {
        api_socket,
        panes,
        control,
        bus,
        last_status: Arc::new(Mutex::new(HashMap::new())),
        tuning,
    };

    // Session state that outlives individual connections.
    let mut lifecycle: Option<Conn> = None;
    let mut status: Option<Conn> = None;
    // The pane set connection B's current subscription covers. This — not a
    // connect-time snapshot — is what the widening rule consults.
    let mut subscribed: HashSet<String> = HashSet::new();
    let mut lifecycle_backoff = tuning.initial_backoff;
    let mut status_backoff = tuning.initial_backoff;
    let mut grow_history: Vec<Instant> = Vec::new();
    let mut strikes = Strikes::default();

    loop {
        if *shutdown.borrow() {
            break;
        }

        // ── Connection A: the lifecycle watcher ─────────────────────────────
        if lifecycle.is_none() {
            match connect_lifecycle(&kernel, &mut shutdown).await {
                Bring::Shutdown => break,
                Bring::LifecycleLost | Bring::Failed => {
                    if back_off_after(
                        "lifecycle",
                        None,
                        &mut lifecycle_backoff,
                        &tuning,
                        &mut shutdown,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
                Bring::Ready(conn) => {
                    lifecycle = Some(conn);
                    // A's watermark is the reference point for everything B
                    // knows, so a fresh A always means a fresh B.
                    status = None;
                    subscribed.clear();
                    strikes = Strikes::default();
                }
            }
        }

        // ── Connection B: the agent-status watcher ──────────────────────────
        if status.is_none() {
            let lc = lifecycle
                .as_mut()
                .expect("connection A is up before connection B is built");
            match connect_status(&kernel, lc, &mut subscribed, &mut shutdown).await {
                Bring::Shutdown => break,
                Bring::LifecycleLost => {
                    let opened = lifecycle.as_ref().map(|c| c.opened);
                    lifecycle = None;
                    if back_off_after(
                        "lifecycle",
                        opened,
                        &mut lifecycle_backoff,
                        &tuning,
                        &mut shutdown,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
                Bring::Failed => {
                    if back_off_after(
                        "agent-status",
                        None,
                        &mut status_backoff,
                        &tuning,
                        &mut shutdown,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
                Bring::Ready(conn) => {
                    status = Some(conn);
                    strikes = Strikes::default();
                }
            }
        }

        // ── Both up: pump events until something ends the cycle ─────────────
        let outcome = {
            let lc = lifecycle.as_mut().expect("connection A is up");
            let st = status.as_mut().expect("connection B is up");
            pump(
                &kernel,
                lc,
                st,
                &mut subscribed,
                &mut strikes,
                &mut shutdown,
            )
            .await
        };

        match outcome {
            Pump::Shutdown => break,
            Pump::LifecycleLost => {
                let opened = lifecycle.as_ref().map(|c| c.opened);
                lifecycle = None;
                if back_off_after(
                    "lifecycle",
                    opened,
                    &mut lifecycle_backoff,
                    &tuning,
                    &mut shutdown,
                )
                .await
                {
                    break;
                }
            }
            Pump::StatusLost => {
                let opened = status.as_ref().map(|c| c.opened);
                status = None;
                if back_off_after(
                    "agent-status",
                    opened,
                    &mut status_backoff,
                    &tuning,
                    &mut shutdown,
                )
                .await
                {
                    break;
                }
            }
            Pump::Widen => {
                status = None;
                let storm = record_grow_and_check_storm(
                    &mut grow_history,
                    Instant::now(),
                    tuning.grow_storm_window,
                    tuning.grow_storm_threshold,
                );
                // Coalesce the burst. Lines read here are discarded: the rebuild
                // re-enumerates, which supersedes anything they could say, and a
                // stale last-known status is pruned by the resync.
                let lc = lifecycle.as_mut().expect("connection A is up");
                match drain_lifecycle(lc, tuning.grow_coalesce_window, &mut shutdown).await {
                    Drain::Shutdown => break,
                    Drain::Lost => {
                        let opened = lifecycle.as_ref().map(|c| c.opened);
                        lifecycle = None;
                        if back_off_after(
                            "lifecycle",
                            opened,
                            &mut lifecycle_backoff,
                            &tuning,
                            &mut shutdown,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Drain::Done => {}
                }
                if storm {
                    // A live-schema assumption is wrong and widenings are firing
                    // in a loop. Escalate the AGENT-STATUS ladder only: the
                    // lifecycle watcher stays up throughout.
                    log::warn!(
                        "herdr event kernel: widening storm — backing the agent-status watcher off {}s",
                        status_backoff.as_secs()
                    );
                    if sleep_or_shutdown(status_backoff, &mut shutdown).await {
                        break;
                    }
                    status_backoff = next_backoff(status_backoff, tuning.max_backoff);
                }
            }
        }
    }
    log::info!("herdr event kernel: stopped");
}

/// Apply one connection's post-drop backoff: a session that stayed up at least
/// [`Tuning::stable_threshold`] resets the ladder first. Returns `true` when
/// shutdown interrupted the wait.
async fn back_off_after(
    label: &str,
    opened: Option<Instant>,
    backoff: &mut Duration,
    tuning: &Tuning,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    if opened.is_some_and(|t| t.elapsed() >= tuning.stable_threshold) {
        *backoff = tuning.initial_backoff;
    }
    log::info!(
        "herdr event kernel: {label} watcher down — retrying in {:?}",
        *backoff
    );
    if sleep_or_shutdown(*backoff, shutdown).await {
        return true;
    }
    *backoff = next_backoff(*backoff, tuning.max_backoff);
    false
}

// ── Bringing the connections up ─────────────────────────────────────────────────

/// Connection A: subscribe to the three *bare* lifecycle events. Complete at ack —
/// no pane ids, so nothing about it can ever be out of date.
async fn connect_lifecycle(kernel: &Kernel, shutdown: &mut watch::Receiver<bool>) -> Bring {
    let request = match lifecycle_request_line() {
        Ok(l) => l,
        Err(e) => {
            log::error!("herdr event kernel: could not build lifecycle request: {e:#}");
            return Bring::Failed;
        }
    };
    let brought = open_subscription(
        &kernel.api_socket,
        &request,
        kernel.tuning.ack_timeout,
        shutdown,
    )
    .await;
    if matches!(brought, Bring::Ready(_)) {
        log::info!(
            "herdr event kernel: lifecycle watcher subscribed ({SUBSCRIBE_PANE_CREATED_TYPE}/{SUBSCRIBE_PANE_CLOSED_TYPE}/{SUBSCRIBE_PANE_EXITED_TYPE})"
        );
    }
    brought
}

/// Connection B: enumerate the panes with connection A already acked and
/// buffering, fold that buffer into the pane set, subscribe, then resync.
///
/// The enumeration runs on the blocking pool (connection-per-request control
/// transport) while A is read concurrently — that concurrency *is* the
/// bootstrap-gap fix, and it is why A must be up first.
async fn connect_status(
    kernel: &Kernel,
    lifecycle: &mut Conn,
    subscribed: &mut HashSet<String>,
    shutdown: &mut watch::Receiver<bool>,
) -> Bring {
    // 1. Enumerate, buffering whatever A delivers meanwhile.
    let control = Arc::clone(&kernel.control);
    let enumeration = tokio::task::spawn_blocking(move || collect_pane_ids(&control));
    tokio::pin!(enumeration);
    let mut buffered: Vec<String> = Vec::new();
    let enumerated = loop {
        enum EnumTick {
            Shutdown,
            Done(std::result::Result<Result<Vec<String>>, tokio::task::JoinError>),
            Line(std::io::Result<Option<String>>),
        }
        let tick = tokio::select! {
            stop = shutdown_requested(shutdown) => {
                if stop { EnumTick::Shutdown } else { continue }
            }
            done = &mut enumeration => EnumTick::Done(done),
            line = lifecycle.next_line() => EnumTick::Line(line),
        };
        match tick {
            EnumTick::Shutdown => return Bring::Shutdown,
            EnumTick::Done(Ok(Ok(ids))) => break ids,
            EnumTick::Done(Ok(Err(e))) => {
                log::info!("herdr event kernel: pane enumeration failed: {e:#}");
                return Bring::Failed;
            }
            EnumTick::Done(Err(e)) => {
                log::error!("herdr event kernel: pane enumeration task panicked: {e}");
                return Bring::Failed;
            }
            EnumTick::Line(Ok(Some(line))) => {
                lifecycle.mark_event();
                buffered.push(line);
            }
            EnumTick::Line(Ok(None)) => {
                log::info!("herdr event kernel: lifecycle stream closed during enumeration");
                return Bring::LifecycleLost;
            }
            EnumTick::Line(Err(e)) => {
                log::info!("herdr event kernel: lifecycle read error during enumeration: {e}");
                return Bring::LifecycleLost;
            }
        }
    };

    // 2. Fold the buffer in. A pane created during the enumeration window is
    //    announced by A and joins the set here rather than being lost; a pane
    //    that vanished in the same window leaves it, because a stale id would
    //    abort the whole subscribe request with no ack.
    let buffered_count = buffered.len();
    let pane_ids = bootstrap_pane_set(enumerated, &buffered);

    // 3. Subscribe and wait for the ack.
    let request = match agent_status_request_line(&pane_ids) {
        Ok(l) => l,
        Err(e) => {
            log::error!("herdr event kernel: could not build agent-status request: {e:#}");
            return Bring::Failed;
        }
    };
    let conn = match open_subscription(
        &kernel.api_socket,
        &request,
        kernel.tuning.ack_timeout,
        shutdown,
    )
    .await
    {
        Bring::Ready(conn) => conn,
        other => return other,
    };
    log::info!(
        "herdr event kernel: agent-status watcher subscribed ({} pane(s), {buffered_count} lifecycle line(s) buffered during enumeration)",
        pane_ids.len()
    );

    // 4. Commit the set the subscription actually covers, then resync.
    *subscribed = pane_ids.into_iter().collect();
    let resynced = resync(
        Arc::clone(&kernel.control),
        Arc::clone(&kernel.panes),
        Arc::clone(&kernel.last_status),
        kernel.bus.clone(),
    )
    .await;
    log::info!("herdr event kernel: resync emitted {resynced} synthetic event(s)");

    Bring::Ready(conn)
}

/// Connect, send one `events.subscribe` request, and wait for
/// `subscription_started`. Nothing is ever written to the socket afterwards.
async fn open_subscription(
    api_socket: &Path,
    request: &str,
    ack_timeout: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Bring {
    let stream = match UnixStream::connect(api_socket).await {
        Ok(s) => s,
        Err(e) => {
            log::info!(
                "herdr event kernel: connect to {} failed: {e}",
                api_socket.display()
            );
            return Bring::Failed;
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    if let Err(e) = write_half.write_all(request.as_bytes()).await {
        log::info!("herdr event kernel: subscribe write failed: {e}");
        return Bring::Failed;
    }

    let mut reader = LineReader::new(BufReader::new(read_half));
    let deadline = Instant::now() + ack_timeout;
    loop {
        enum AckTick {
            Shutdown,
            Timeout,
            Line(std::io::Result<Option<String>>),
        }
        let tick = tokio::select! {
            stop = shutdown_requested(shutdown) => {
                if stop { AckTick::Shutdown } else { continue }
            }
            _ = tokio::time::sleep_until(deadline) => AckTick::Timeout,
            line = reader.next_line() => AckTick::Line(line),
        };
        match tick {
            AckTick::Shutdown => return Bring::Shutdown,
            AckTick::Timeout => {
                log::warn!("herdr event kernel: no subscription ack within {ack_timeout:?}");
                return Bring::Failed;
            }
            AckTick::Line(Ok(None)) => {
                log::info!("herdr event kernel: stream closed before subscribe ack");
                return Bring::Failed;
            }
            AckTick::Line(Err(e)) => {
                log::info!("herdr event kernel: read error before ack: {e}");
                return Bring::Failed;
            }
            AckTick::Line(Ok(Some(line))) => {
                let line = line.trim_end();
                if is_subscription_ack(line) {
                    return Bring::Ready(Conn::new(reader, write_half));
                }
                if let Some(err) = subscribe_error(line) {
                    // herdr closes the connection after an error response; every
                    // per-pane id is resolved at subscribe time, so one pane that
                    // vanished mid-flight rejects the whole request.
                    log::warn!("herdr event kernel: subscribe rejected: {err}");
                    return Bring::Failed;
                }
                log::trace!("herdr event kernel: ignoring pre-ack line");
            }
        }
    }
}

// ── Pumping events ──────────────────────────────────────────────────────────────

/// Read both connections until one of them ends the cycle.
async fn pump(
    kernel: &Kernel,
    lifecycle: &mut Conn,
    status: &mut Conn,
    subscribed: &mut HashSet<String>,
    strikes: &mut Strikes,
    shutdown: &mut watch::Receiver<bool>,
) -> Pump {
    let mut probe = tokio::time::interval(kernel.tuning.probe_interval);
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    probe.tick().await; // the first tick completes immediately — consume it.

    loop {
        let tick = tokio::select! {
            stop = shutdown_requested(shutdown) => {
                if stop { Tick::Shutdown } else { continue }
            }
            line = lifecycle.next_line() => Tick::Lifecycle(line),
            line = status.next_line() => Tick::Status(line),
            _ = probe.tick() => Tick::Probe,
        };

        match tick {
            Tick::Shutdown => return Pump::Shutdown,

            Tick::Lifecycle(Ok(None)) => {
                log::info!("herdr event kernel: lifecycle stream closed (EOF)");
                return Pump::LifecycleLost;
            }
            Tick::Lifecycle(Err(e)) => {
                log::info!("herdr event kernel: lifecycle read error: {e}");
                return Pump::LifecycleLost;
            }
            Tick::Lifecycle(Ok(Some(line))) => {
                lifecycle.mark_event();
                match classify_pushed(line.trim_end(), &kernel.panes) {
                    Pushed::PaneCreated(pane_id) => {
                        if pane_needs_widening(pane_id.as_deref(), subscribed) {
                            log::info!(
                                "herdr event kernel: pane {} is outside the agent-status subscription — rebuilding it",
                                pane_id.as_deref().unwrap_or("<no id>")
                            );
                            return Pump::Widen;
                        }
                        log::debug!(
                            "herdr event kernel: pane {} already covered by the agent-status subscription",
                            pane_id.as_deref().unwrap_or("<no id>")
                        );
                    }
                    Pushed::PaneGone {
                        herdr_pane_id,
                        pane,
                    } => {
                        // A dead subscription is harmless; drop tracking so a
                        // future pane reusing the id inherits nothing.
                        if let Some(id) = pane {
                            lock(&kernel.last_status).remove(&id);
                        }
                        if let Some(hid) = herdr_pane_id {
                            subscribed.remove(&hid);
                        }
                    }
                    // Not expected on A (it carries no agent-status entry), but
                    // publishing it is strictly better than dropping it.
                    Pushed::AgentStatus(ev) => publish(kernel, ev),
                    Pushed::Ignored => {}
                }
            }

            Tick::Status(Ok(None)) => {
                log::info!("herdr event kernel: agent-status stream closed (EOF)");
                return Pump::StatusLost;
            }
            Tick::Status(Err(e)) => {
                log::info!("herdr event kernel: agent-status read error: {e}");
                return Pump::StatusLost;
            }
            Tick::Status(Ok(Some(line))) => {
                status.mark_event();
                match classify_pushed(line.trim_end(), &kernel.panes) {
                    Pushed::AgentStatus(ev) => publish(kernel, ev),
                    _ => {
                        log::trace!("herdr event kernel: ignoring non-status line on connection B");
                    }
                }
            }

            Tick::Probe => {
                // Skip a connection that has delivered within the interval: it
                // has just proven itself, and a quiet one is normal.
                let lifecycle_idle = lifecycle.idle_for() >= kernel.tuning.probe_interval;
                let status_idle = status.idle_for() >= kernel.tuning.probe_interval;
                if !lifecycle_idle && !status_idle {
                    *strikes = Strikes::default();
                    continue;
                }
                let Some(verdict) = liveness_probe(kernel, subscribed).await else {
                    continue; // inconclusive: the control plane itself is unhappy.
                };
                match verdict {
                    Verdict::Healthy => *strikes = Strikes::default(),
                    Verdict::LifecycleWedged if lifecycle_idle => {
                        strikes.status = 0;
                        strikes.lifecycle += 1;
                        if strikes.lifecycle >= kernel.tuning.probe_strikes {
                            log::warn!(
                                "herdr event kernel: lifecycle watcher silent through {} probes while panes appeared — reconnecting",
                                strikes.lifecycle
                            );
                            return Pump::LifecycleLost;
                        }
                    }
                    Verdict::StatusWedged if status_idle => {
                        strikes.lifecycle = 0;
                        strikes.status += 1;
                        if strikes.status >= kernel.tuning.probe_strikes {
                            log::warn!(
                                "herdr event kernel: agent-status watcher silent through {} probes while statuses moved — reconnecting",
                                strikes.status
                            );
                            return Pump::StatusLost;
                        }
                    }
                    // A wedged verdict about a connection that is demonstrably
                    // delivering is not evidence of anything.
                    _ => *strikes = Strikes::default(),
                }
            }
        }
    }
}

/// Publish one agent-status transition and record it as the pane's last-known
/// status (which is what the resync dedup and the watchdog compare against).
fn publish(kernel: &Kernel, ev: AgentStatusChanged) {
    lock(&kernel.last_status).insert(ev.pane, ev.status);
    log::debug!(
        "herdr event kernel: {:?} pane={} ws={}",
        ev.status,
        ev.pane,
        ev.workspace_id
    );
    // No receivers yet is fine (broadcast → recoverable Err).
    let _ = kernel.bus.send(MuxEvent::AgentStatusChanged(ev));
}

/// Outcome of draining connection A for the coalesce window.
enum Drain {
    Done,
    Lost,
    Shutdown,
}

/// Keep reading connection A for `window`, discarding what it says. Used between
/// deciding to widen and rebuilding connection B, so a burst of pane creations
/// costs one rebuild instead of one per pane.
async fn drain_lifecycle(
    lifecycle: &mut Conn,
    window: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Drain {
    let deadline = Instant::now() + window;
    loop {
        enum DrainTick {
            Shutdown,
            Done,
            Line(std::io::Result<Option<String>>),
        }
        let tick = tokio::select! {
            stop = shutdown_requested(shutdown) => {
                if stop { DrainTick::Shutdown } else { continue }
            }
            _ = tokio::time::sleep_until(deadline) => DrainTick::Done,
            line = lifecycle.next_line() => DrainTick::Line(line),
        };
        match tick {
            DrainTick::Shutdown => return Drain::Shutdown,
            DrainTick::Done => return Drain::Done,
            DrainTick::Line(Ok(Some(_))) => lifecycle.mark_event(),
            DrainTick::Line(Ok(None)) | DrainTick::Line(Err(_)) => {
                log::info!("herdr event kernel: lifecycle stream ended while coalescing");
                return Drain::Lost;
            }
        }
    }
}

// ── Enumeration, resync and the liveness probe ──────────────────────────────────

/// Blocking helper: list every workspace's panes and collect their herdr
/// `pane_id`s.
fn collect_pane_ids(control: &HerdrControl) -> Result<Vec<String>> {
    let workspaces = control
        .list_workspaces()
        .context("enumerate: workspace.list")?;
    let mut ids = Vec::new();
    for ws in &workspaces {
        let ws_panes = control
            .list_panes(&ws.workspace_id)
            .with_context(|| format!("enumerate: pane.list for {}", ws.workspace_id))?;
        for pane in ws_panes {
            ids.push(pane.pane_id);
        }
    }
    Ok(ids)
}

/// Re-observe current pane agent states after a (re)connect and publish synthetic
/// `Blocked`/`Done` events (deduped against the last-known status table). Returns
/// the number of synthetic events emitted.
///
/// Runs the blocking `HerdrControl` list calls on the blocking pool (they use the
/// synchronous connection-per-request transport, per the `spawn_blocking`
/// discipline for short herdr calls).
async fn resync(
    control: Arc<HerdrControl>,
    panes: Arc<HerdrPaneRegistry>,
    last_status: StatusTable,
    bus: EventBus,
) -> usize {
    let collected =
        tokio::task::spawn_blocking(move || collect_resync_events(&control, &panes, &last_status))
            .await;
    match collected {
        Ok(Ok(events)) => {
            let n = events.len();
            for ev in events {
                let _ = bus.send(MuxEvent::AgentStatusChanged(ev));
            }
            n
        }
        Ok(Err(e)) => {
            log::info!("herdr event kernel: resync failed: {e:#}");
            0
        }
        Err(e) => {
            log::error!("herdr event kernel: resync task panicked: {e}");
            0
        }
    }
}

/// Blocking helper: list every workspace's panes and build synthetic events for
/// those currently `Blocked`/`Done` whose status *changed* since last observed.
/// `list_panes` carries each pane's real `terminal_id`, so `assign_or_get` here
/// refreshes the registry correctly.
fn collect_resync_events(
    control: &HerdrControl,
    panes: &HerdrPaneRegistry,
    last_status: &Mutex<HashMap<u32, AgentStatus>>,
) -> Result<Vec<AgentStatusChanged>> {
    let workspaces = control
        .list_workspaces()
        .context("resync: workspace.list")?;
    let mut input = Vec::new();
    for ws in &workspaces {
        let ws_panes = control
            .list_panes(&ws.workspace_id)
            .with_context(|| format!("resync: pane.list for {}", ws.workspace_id))?;
        for pane in ws_panes {
            input.push(ResyncPane {
                herdr_pane_id: pane.pane_id,
                terminal_id: pane.terminal_id,
                workspace_id: ws.workspace_id.clone(),
                workspace_label: ws.label.clone(),
                title: pane.title,
                status: pane.agent_status,
            });
        }
    }
    let mut table = lock(last_status);
    Ok(build_resync_events(&input, panes, &mut table))
}

/// One pane's state as gathered by the resync enumeration. Neutral of transport so
/// [`build_resync_events`] is a pure, fixture-testable function.
struct ResyncPane {
    herdr_pane_id: String,
    terminal_id: String,
    workspace_id: String,
    workspace_label: String,
    title: Option<String>,
    status: HerdrAgentStatus,
}

/// Pure dedup core: register each pane, update the last-known status table, and
/// emit a synthetic event only for a `Blocked`/`Done` pane whose status *changed*.
/// Panes absent from `input` are pruned from `last_status`.
fn build_resync_events(
    input: &[ResyncPane],
    reg: &HerdrPaneRegistry,
    last_status: &mut HashMap<u32, AgentStatus>,
) -> Vec<AgentStatusChanged> {
    let mut live: HashSet<u32> = HashSet::with_capacity(input.len());
    let mut events = Vec::new();
    for p in input {
        let id = reg.assign_or_get(&p.herdr_pane_id, &p.terminal_id);
        live.insert(id);
        let status = map_status(p.status);
        let changed = last_status.get(&id) != Some(&status);
        last_status.insert(id, status);
        if changed && matches!(status, AgentStatus::Blocked | AgentStatus::Done) {
            events.push(AgentStatusChanged {
                pane: id,
                workspace_id: p.workspace_id.clone(),
                workspace_name: Some(p.workspace_label.clone()),
                status,
                title: p.title.clone(),
                synthetic: true,
            });
        }
    }
    last_status.retain(|id, _| live.contains(id));
    events
}

/// One pane as the liveness probe sees it: its herdr id, its neutral id when the
/// registry already knows it (a **non-mutating** lookup — the probe is a
/// diagnostic and registers nothing), and its current status.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbePane {
    herdr_pane_id: String,
    pane: Option<u32>,
    status: AgentStatus,
}

/// What the probe concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Everything the control plane can see is already reflected in what the
    /// subscriptions delivered.
    Healthy,
    /// A pane exists that connection A never announced.
    LifecycleWedged,
    /// A subscribed pane's status moved without connection B pushing it.
    StatusWedged,
}

/// Run one control-plane liveness probe. `None` means inconclusive (the control
/// call itself failed) — never a strike.
async fn liveness_probe(kernel: &Kernel, subscribed: &HashSet<String>) -> Option<Verdict> {
    let control = Arc::clone(&kernel.control);
    let panes = Arc::clone(&kernel.panes);
    let observed =
        match tokio::task::spawn_blocking(move || collect_probe_panes(&control, &panes)).await {
            Ok(Ok(observed)) => observed,
            Ok(Err(e)) => {
                log::info!("herdr event kernel: liveness probe failed: {e:#}");
                return None;
            }
            Err(e) => {
                log::error!("herdr event kernel: liveness probe task panicked: {e}");
                return None;
            }
        };
    let table = lock(&kernel.last_status);
    Some(watchdog_verdict(&observed, subscribed, &table))
}

/// Blocking helper: enumerate panes for the liveness probe without touching the
/// registry.
fn collect_probe_panes(
    control: &HerdrControl,
    panes: &HerdrPaneRegistry,
) -> Result<Vec<ProbePane>> {
    let workspaces = control.list_workspaces().context("probe: workspace.list")?;
    let mut observed = Vec::new();
    for ws in &workspaces {
        let ws_panes = control
            .list_panes(&ws.workspace_id)
            .with_context(|| format!("probe: pane.list for {}", ws.workspace_id))?;
        for pane in ws_panes {
            observed.push(ProbePane {
                pane: panes.id_for_herdr_pane(&pane.pane_id),
                herdr_pane_id: pane.pane_id,
                status: map_status(pane.agent_status),
            });
        }
    }
    Ok(observed)
}

/// Pure watchdog core: decide whether the control plane can see something the
/// subscriptions should already have delivered.
///
/// A pane outside the subscribed set means connection A missed its creation, and
/// that verdict wins — rebuilding A rebuilds B as well. Otherwise a subscribed
/// pane whose status differs from the last value connection B delivered means B
/// missed a transition. A pane with no last-known status yet is not evidence of
/// anything (nothing to compare against), and a *missing* pane is deliberately
/// ignored: a dead pane's subscription is inert and owes no notification.
fn watchdog_verdict(
    observed: &[ProbePane],
    subscribed: &HashSet<String>,
    last_status: &HashMap<u32, AgentStatus>,
) -> Verdict {
    let mut status_wedged = false;
    for p in observed {
        if !subscribed.contains(&p.herdr_pane_id) {
            return Verdict::LifecycleWedged;
        }
        if let Some(known) = p.pane.and_then(|id| last_status.get(&id))
            && *known != p.status
        {
            status_wedged = true;
        }
    }
    if status_wedged {
        Verdict::StatusWedged
    } else {
        Verdict::Healthy
    }
}

// ── Parsing ─────────────────────────────────────────────────────────────────────

/// A classified pushed line.
enum Pushed {
    /// A live `pane.agent_status_changed` transition, ready to publish.
    AgentStatus(AgentStatusChanged),
    /// A `pane.created` push, carrying the herdr `pane_id` if one was parseable.
    /// herdr 0.9.0 nests the full pane object under `data.pane`.
    PaneCreated(Option<String>),
    /// A `pane.closed` / `pane.exited` push: the herdr id (for the subscribed-set
    /// prune) and the neutral id when the registry knows the pane (for the
    /// last-known-status prune).
    PaneGone {
        herdr_pane_id: Option<String>,
        pane: Option<u32>,
    },
    /// Anything not consumed (ack, unknown event, malformed) — ignored.
    Ignored,
}

/// A lifecycle-only view of a pushed line, used while buffering connection A
/// during the enumeration. Registry-free, so [`bootstrap_pane_set`] stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneChange {
    Created(Option<String>),
    Gone(Option<String>),
}

/// Split a pushed line into its canonical (underscored) event name and `data`
/// payload. Response envelopes (ack / error) carry no top-level `event` and are
/// rejected here.
fn pushed_envelope(line: &str) -> Option<(String, Option<Value>)> {
    if line.is_empty() {
        return None;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            log::trace!("herdr event kernel: ignoring non-JSON line");
            return None;
        }
    };
    let event = value.get("event").and_then(Value::as_str)?;
    // herdr's two push envelopes disagree: the lifecycle `EventKind` serialises
    // snake_case (`pane_created`) while `SubscriptionEventKind` is dotted
    // (`pane.agent_status_changed`). Normalise before matching.
    let canonical = event.replace('.', "_");
    let data = value.get("data").cloned();
    Some((canonical, data))
}

/// Classify one newline-stripped stream line.
fn classify_pushed(line: &str, panes: &HerdrPaneRegistry) -> Pushed {
    let Some((event, data)) = pushed_envelope(line) else {
        return Pushed::Ignored;
    };
    match event.as_str() {
        "pane_agent_status_changed" => parse_agent_status(data.as_ref(), panes),
        "pane_created" => Pushed::PaneCreated(pane_created_id(data.as_ref())),
        "pane_closed" | "pane_exited" => parse_pane_gone(data.as_ref(), panes),
        other => {
            log::trace!("herdr event kernel: ignoring event {other}");
            Pushed::Ignored
        }
    }
}

/// Classify one buffered connection-A line as a pane-set change, ignoring
/// everything else. Pure: it never consults or mutates the registry.
fn lifecycle_change(line: &str) -> Option<PaneChange> {
    let (event, data) = pushed_envelope(line)?;
    match event.as_str() {
        "pane_created" => Some(PaneChange::Created(pane_created_id(data.as_ref()))),
        "pane_closed" | "pane_exited" => Some(PaneChange::Gone(pane_gone_id(data.as_ref()))),
        _ => None,
    }
}

/// Parse a `pane.agent_status_changed` payload (`data`) into a neutral event.
///
/// herdr 0.9.0 carries `agent`, `title`, `display_agent` and `state_labels` here;
/// `title` is read so the push path no longer depends on resync-only enrichment.
/// The push still omits `terminal_id` and `workspace_name`, so those stay `None`,
/// and the remaining presentation fields have no home on the neutral event type.
fn parse_agent_status(data: Option<&Value>, panes: &HerdrPaneRegistry) -> Pushed {
    let Some(data) = data else {
        return Pushed::Ignored;
    };
    let (Some(pane_id), Some(workspace_id), Some(status)) = (
        data.get("pane_id").and_then(Value::as_str),
        data.get("workspace_id").and_then(Value::as_str),
        data.get("agent_status")
            .and_then(|v| serde_json::from_value::<HerdrAgentStatus>(v.clone()).ok()),
    ) else {
        log::debug!("herdr event kernel: malformed agent-status event ignored");
        return Pushed::Ignored;
    };
    // A pushed event never carries terminal_id → never clobber a live attach key.
    let pane = translate_pane(panes, pane_id, None);
    Pushed::AgentStatus(AgentStatusChanged {
        pane,
        workspace_id: workspace_id.to_string(),
        workspace_name: None,
        status: map_status(status),
        title: data
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        synthetic: false,
    })
}

/// Parse a `pane.closed` / `pane.exited` payload into both id forms.
fn parse_pane_gone(data: Option<&Value>, panes: &HerdrPaneRegistry) -> Pushed {
    let Some(herdr_pane_id) = pane_gone_id(data) else {
        return Pushed::Ignored;
    };
    let pane = panes.id_for_herdr_pane(&herdr_pane_id);
    Pushed::PaneGone {
        herdr_pane_id: Some(herdr_pane_id),
        pane,
    }
}

/// Extract the herdr `pane_id` from a `pane.closed` / `pane.exited` payload
/// (top-level in `data` per herdr 0.9.0, or nested under `data.pane.pane_id`).
fn pane_gone_id(data: Option<&Value>) -> Option<String> {
    let data = data?;
    data.get("pane_id")
        .and_then(Value::as_str)
        .or_else(|| {
            data.get("pane")
                .and_then(|p| p.get("pane_id"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

/// Extract the herdr `pane_id` from a `pane.created` payload. herdr 0.9.0 nests
/// the full pane object under `data.pane` (`data.pane.pane_id`); a top-level
/// `data.pane_id` is also accepted for tolerance.
fn pane_created_id(data: Option<&Value>) -> Option<String> {
    let data = data?;
    data.get("pane")
        .and_then(|p| p.get("pane_id"))
        .and_then(Value::as_str)
        .or_else(|| data.get("pane_id").and_then(Value::as_str))
        .map(str::to_string)
}

/// Build connection B's pane set: the enumeration, plus every pane connection A
/// announced while that enumeration was in flight, minus every pane A reported
/// gone in the same window.
///
/// This is the bootstrap-gap fix. herdr replays nothing, so a pane created during
/// the enumeration would otherwise be invisible to both halves of the old design:
/// absent from the snapshot, and its `pane.created` below the next watermark.
/// Dropping a pane that vanished in the same window matters just as much in the
/// other direction — herdr resolves every per-pane id at subscribe time, so one
/// stale id rejects the entire request.
fn bootstrap_pane_set(enumerated: Vec<String>, buffered: &[String]) -> Vec<String> {
    let mut ids = enumerated;
    let mut seen: HashSet<String> = ids.iter().cloned().collect();
    let mut gone: HashSet<String> = HashSet::new();
    for line in buffered {
        match lifecycle_change(line) {
            Some(PaneChange::Created(Some(id))) => {
                if seen.insert(id.clone()) {
                    ids.push(id);
                }
            }
            Some(PaneChange::Gone(Some(id))) => {
                gone.insert(id);
            }
            _ => {}
        }
    }
    ids.retain(|id| !gone.contains(id));
    ids
}

/// Decide whether a `pane.created` push must widen the agent-status subscription.
///
/// The test is membership of the set connection B is **currently subscribed to**,
/// which is mutable session state. That is deliberate and load-bearing: asking
/// instead whether the pane was in some connect-time snapshot is what produced
/// the bootstrap gap, and asking it the other way round (was it in a snapshot
/// taken *after* subscribing) reintroduces the identical hole, because a pane
/// created in that window appears in both the push and the snapshot and gets
/// judged already known.
///
/// A push with no parseable id cannot be matched, so err toward widening — a
/// pointless rebuild is recoverable, a permanently deaf pane is not.
fn pane_needs_widening(pane_id: Option<&str>, subscribed: &HashSet<String>) -> bool {
    match pane_id {
        Some(pid) => !subscribed.contains(pid),
        None => true,
    }
}

/// Is this line the `{"result":{"type":"subscription_started"}}` ack?
fn is_subscription_ack(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| {
            v.get("result")
                .and_then(|r| r.get("type"))
                .and_then(Value::as_str)
                .map(|t| t == "subscription_started")
        })
        .unwrap_or(false)
}

/// Extract a herdr error body from a response envelope line, if present.
fn subscribe_error(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    v.get("error").map(ToString::to_string)
}

/// Translate a herdr `pane_id` to a neutral `u32` **without clobbering** an
/// already-registered pane's `terminal_id`.
///
/// A pushed event may omit `terminal_id`; calling
/// [`HerdrPaneRegistry::assign_or_get`] with an empty string for a pane that is
/// already known (registered by a layout poll with its real `terminal_id`) would
/// overwrite that relay attach key. So we look up first (read-only) and only
/// assign — with whatever `terminal_id` the event carried — for a genuinely new
/// pane, where there is no prior value to lose.
fn translate_pane(
    panes: &HerdrPaneRegistry,
    herdr_pane_id: &str,
    terminal_id: Option<&str>,
) -> u32 {
    if let Some(id) = panes.id_for_herdr_pane(herdr_pane_id) {
        return id;
    }
    panes.assign_or_get(herdr_pane_id, terminal_id.unwrap_or_default())
}

/// Map herdr's `AgentStatus` to the neutral [`AgentStatus`].
fn map_status(status: HerdrAgentStatus) -> AgentStatus {
    match status {
        HerdrAgentStatus::Idle => AgentStatus::Idle,
        HerdrAgentStatus::Working => AgentStatus::Working,
        HerdrAgentStatus::Blocked => AgentStatus::Blocked,
        HerdrAgentStatus::Done => AgentStatus::Done,
        HerdrAgentStatus::Unknown => AgentStatus::Unknown,
    }
}

/// Connection A's request: the three *bare* lifecycle entries. None takes a pane
/// id, so this subscription is complete the moment it is acked and never needs to
/// widen — which is exactly why the lifecycle watcher can be the authority on new
/// panes.
fn lifecycle_request_line() -> Result<String> {
    let subscriptions = serde_json::json!([
        { "type": SUBSCRIBE_PANE_CREATED_TYPE },
        { "type": SUBSCRIBE_PANE_CLOSED_TYPE },
        { "type": SUBSCRIBE_PANE_EXITED_TYPE },
    ]);
    request_line(LIFECYCLE_REQUEST_ID, subscriptions)
}

/// Connection B's request: one `pane.agent_status_changed` entry per pane, each
/// carrying its explicit `pane_id`. herdr has no wildcard, no omitted form and no
/// sentinel — an absent id rejects the whole array — so the per-pane list is not
/// optional.
fn agent_status_request_line(pane_ids: &[String]) -> Result<String> {
    let subscriptions: Vec<Value> = pane_ids
        .iter()
        .map(|pane_id| {
            serde_json::json!({
                "type": SUBSCRIBE_AGENT_STATUS_TYPE,
                "pane_id": pane_id,
            })
        })
        .collect();
    request_line(AGENT_STATUS_REQUEST_ID, Value::Array(subscriptions))
}

/// Serialise one `events.subscribe` request line (JSON + trailing newline).
fn request_line(id: &str, subscriptions: Value) -> Result<String> {
    let params = serde_json::json!({ "subscriptions": subscriptions });
    let req = ApiRequest::new(id, "events.subscribe", params);
    let mut line = serde_json::to_string(&req).context("serialize events.subscribe request")?;
    line.push('\n');
    Ok(line)
}

/// Next backoff: double, capped at `max`.
fn next_backoff(prev: Duration, max: Duration) -> Duration {
    (prev * 2).min(max)
}

/// Record a widening at `now`, pruning entries older than `window` from
/// `history`, and report whether the circuit breaker should engage for *this*
/// reconnect (recording it pushes the window's count over `threshold`).
///
/// Pure and fixture-testable: `history` is the kernel's rolling widening
/// timestamp log, owned by [`run_tuned`] across reconnects so a storm that spans
/// several sessions is still detected.
fn record_grow_and_check_storm(
    history: &mut Vec<Instant>,
    now: Instant,
    window: Duration,
    threshold: usize,
) -> bool {
    history.retain(|&t| now.saturating_duration_since(t) < window);
    history.push(now);
    history.len() > threshold
}

/// Lock a mutex, recovering the guard if a previous holder panicked. The table
/// holds only a plain map, so a poisoned lock leaves consistent data.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Await the next shutdown signal. A dropped sender counts as shutdown: nobody is
/// left who could ever ask this task to stop, and returning `false` forever would
/// spin the surrounding `select!`.
async fn shutdown_requested(rx: &mut watch::Receiver<bool>) -> bool {
    match rx.changed().await {
        Ok(()) => *rx.borrow(),
        Err(_) => true,
    }
}

/// Sleep for `d` unless shutdown arrives first. Returns `true` when it did.
async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    let deadline = Instant::now() + d;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return false,
            stop = shutdown_requested(shutdown) => {
                if stop {
                    return true;
                }
            }
        }
    }
}

// ── Bounded line reader ──────────────────────────────────────────────────────────

/// A newline-delimited reader that is **cancel-safe**: partially read bytes live
/// in `pending`, which survives the future being dropped by a `select!` branch
/// that lost the race. Multiplexing two subscription sockets in one `select!`
/// makes that mandatory — a per-call accumulation buffer would silently truncate
/// an event whenever the other connection spoke first.
///
/// Lines are bounded at [`MAX_EVENT_LINE_BYTES`], mirroring the control plane's
/// [`MAX_RESPONSE_BYTES`](super::control::MAX_RESPONSE_BYTES) defence.
struct LineReader<R> {
    inner: R,
    pending: Vec<u8>,
    max_line: usize,
}

impl<R: AsyncBufReadExt + Unpin> LineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            max_line: MAX_EVENT_LINE_BYTES,
        }
    }

    /// Next `\n`-terminated line (newline stripped). `Ok(None)` is EOF with
    /// nothing buffered; a final unterminated line is returned once before that.
    /// Errors if the cap is reached without a newline, or on non-UTF-8 input.
    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            let chunk = self.inner.fill_buf().await?;
            if chunk.is_empty() {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return Some(take_utf8(&mut self.pending)).transpose();
            }
            match chunk.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    self.pending.extend_from_slice(&chunk[..pos]); // exclude the newline
                    self.inner.consume(pos + 1);
                    return Some(take_utf8(&mut self.pending)).transpose();
                }
                None => {
                    let n = chunk.len();
                    self.pending.extend_from_slice(chunk);
                    self.inner.consume(n);
                    if self.pending.len() > self.max_line {
                        self.pending.clear();
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "herdr event line exceeded size cap without a newline",
                        ));
                    }
                }
            }
        }
    }
}

/// Drain `pending` into an owned `String`, failing on non-UTF-8.
fn take_utf8(pending: &mut Vec<u8>) -> std::io::Result<String> {
    String::from_utf8(std::mem::take(pending))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use fake_herdr::{
        FakeHerdr, FakeState, RunningKernel, fast_probe_tuning, no_probe_tuning, pane,
    };
    use tokio::sync::broadcast;

    // ── pushed-event parsing (live `{"event","data"}` envelopes) ─────────────

    #[test]
    fn parses_agent_status_dot_envelope_including_the_title() {
        let panes = HerdrPaneRegistry::new();
        // herdr 0.9.0's PaneAgentStatusChangedEvent, untagged under `data`.
        let line = r#"{
            "event": "pane.agent_status_changed",
            "data": {
                "agent": "claude",
                "agent_status": "blocked",
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "title": "cargo test",
                "display_agent": "Claude Code",
                "state_labels": { "phase": "waiting" }
            }
        }"#;
        match classify_pushed(line, &panes) {
            Pushed::AgentStatus(ev) => {
                assert_eq!(ev.status, AgentStatus::Blocked);
                assert_eq!(ev.workspace_id, "w1");
                assert_eq!(
                    ev.title.as_deref(),
                    Some("cargo test"),
                    "0.9.0 carries the title on the push — read it"
                );
                // The push still carries no workspace name / terminal_id.
                assert!(ev.workspace_name.is_none());
                assert!(!ev.synthetic, "a pushed event is never synthetic");
                assert_eq!(panes.herdr_pane_id(ev.pane).as_deref(), Some("w1:p1"));
            }
            _ => panic!("expected an agent-status event"),
        }
    }

    #[test]
    fn agent_status_without_a_title_stays_none() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{
            "event": "pane.agent_status_changed",
            "data": { "agent_status": "done", "pane_id": "w9:p3", "workspace_id": "w9" }
        }"#;
        match classify_pushed(line, &panes) {
            Pushed::AgentStatus(ev) => assert!(ev.title.is_none()),
            _ => panic!("expected an agent-status event"),
        }
    }

    #[test]
    fn parses_agent_status_underscore_event_name_too() {
        // Tolerant name parsing: the underscore form must classify identically.
        let panes = HerdrPaneRegistry::new();
        let line = r#"{
            "event": "pane_agent_status_changed",
            "data": { "agent_status": "done", "pane_id": "w9:p3", "workspace_id": "w9" }
        }"#;
        match classify_pushed(line, &panes) {
            Pushed::AgentStatus(ev) => {
                assert_eq!(ev.status, AgentStatus::Done);
                assert_eq!(ev.workspace_id, "w9");
            }
            _ => panic!("expected an agent-status event"),
        }
    }

    #[test]
    fn tolerates_unknown_extra_data_fields() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{
            "event": "pane.agent_status_changed",
            "data": {
                "agent_status": "working",
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "display_agent": "claude-code",
                "custom_status": "thinking",
                "future_field": 42
            }
        }"#;
        match classify_pushed(line, &panes) {
            Pushed::AgentStatus(ev) => assert_eq!(ev.status, AgentStatus::Working),
            _ => panic!("unknown fields must be ignored, not fail the parse"),
        }
    }

    #[test]
    fn classifies_pane_created_underscore_form() {
        let panes = HerdrPaneRegistry::new();
        // herdr's lifecycle envelope: snake_case event name, tagged EventData
        // with the full pane object under `data.pane`.
        let line = r#"{
            "event": "pane_created",
            "data": {
                "type": "pane_created",
                "pane": { "pane_id": "w1:p2", "terminal_id": "t2", "workspace_id": "w1" }
            }
        }"#;
        match classify_pushed(line, &panes) {
            Pushed::PaneCreated(id) => assert_eq!(id.as_deref(), Some("w1:p2")),
            _ => panic!("expected PaneCreated"),
        }
    }

    #[test]
    fn classifies_pane_created_dot_form_too() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{"event":"pane.created","data":{}}"#;
        match classify_pushed(line, &panes) {
            // No parseable pane_id in this payload.
            Pushed::PaneCreated(id) => assert!(id.is_none()),
            _ => panic!("expected PaneCreated"),
        }
    }

    #[test]
    fn pane_created_id_accepts_top_level_pane_id_too() {
        let data = serde_json::json!({ "pane_id": "w2:p7" });
        assert_eq!(pane_created_id(Some(&data)).as_deref(), Some("w2:p7"));
        // Nested form wins / is found.
        let nested = serde_json::json!({ "pane": { "pane_id": "w3:p1" } });
        assert_eq!(pane_created_id(Some(&nested)).as_deref(), Some("w3:p1"));
        // Nothing parseable.
        assert!(pane_created_id(Some(&serde_json::json!({}))).is_none());
        assert!(pane_created_id(None).is_none());
    }

    // ── the widening rule ────────────────────────────────────────────────────

    #[test]
    fn pane_created_for_an_already_subscribed_pane_does_not_widen() {
        // RE-SPECIFIED against the live-only rule. The old premise — herdr
        // replays a pane_created for every existing pane after the ack — is gone
        // with 0.9.0's watermark; what remains true is that a pane.created can
        // legitimately arrive for a pane connection B *already covers*, because a
        // rebuild re-enumerates and a creation announced during that window lands
        // in the new subscription before its push is read. Widening again there
        // would be a rebuild loop.
        let subscribed: HashSet<String> =
            ["w1:p1", "w1:p2"].into_iter().map(str::to_string).collect();
        assert!(!pane_needs_widening(Some("w1:p1"), &subscribed));
    }

    #[test]
    fn pane_created_outside_the_subscribed_set_widens() {
        let subscribed: HashSet<String> = ["w1:p1"].into_iter().map(str::to_string).collect();
        assert!(pane_needs_widening(Some("w1:p2"), &subscribed));
    }

    #[test]
    fn pane_created_with_no_id_widens() {
        // Can't tell whether it is covered; herdr always carries the id, so this
        // path should not fire in practice — err toward widening.
        let subscribed: HashSet<String> = ["w1:p1"].into_iter().map(str::to_string).collect();
        assert!(pane_needs_widening(None, &subscribed));
    }

    #[test]
    fn an_empty_subscription_set_widens_for_the_first_pane() {
        assert!(pane_needs_widening(Some("w1:p1"), &HashSet::new()));
    }

    // ── the bootstrap gap (THE POINT) ────────────────────────────────────────

    fn created_line(pane_id: &str) -> String {
        format!(
            r#"{{"event":"pane_created","data":{{"type":"pane_created","pane":{{"pane_id":"{pane_id}","terminal_id":"t","workspace_id":"w1"}}}}}}"#
        )
    }

    fn closed_line(pane_id: &str) -> String {
        format!(
            r#"{{"event":"pane_closed","data":{{"type":"pane_closed","pane_id":"{pane_id}","workspace_id":"w1"}}}}"#
        )
    }

    #[test]
    fn a_pane_created_during_the_enumeration_still_gets_subscribed() {
        // The bootstrap gap: herdr replays nothing, so a pane that appears after
        // the enumeration snapshot is never re-announced. The lifecycle watcher
        // is already acked and buffering, so its pane.created lands in the very
        // request that would otherwise have missed the pane.
        let enumerated = vec!["w1:p1".to_string()];
        let buffered = vec![created_line("w1:p2")];
        let set = bootstrap_pane_set(enumerated, &buffered);
        assert_eq!(set, vec!["w1:p1".to_string(), "w1:p2".to_string()]);

        // …and it reaches the wire as an explicit per-pane entry.
        let line = agent_status_request_line(&set).expect("build request");
        let value: Value = serde_json::from_str(line.trim_end()).expect("valid JSON");
        let subs = value["params"]["subscriptions"].as_array().expect("array");
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[1]["type"], SUBSCRIBE_AGENT_STATUS_TYPE);
        assert_eq!(subs[1]["pane_id"], "w1:p2");
    }

    #[test]
    fn a_pane_closed_during_the_enumeration_is_dropped_from_the_set() {
        // A stale id aborts the ENTIRE subscribe request with an error and no
        // ack, so a pane that died inside the window must not be asked for.
        let enumerated = vec!["w1:p1".to_string(), "w1:p2".to_string()];
        let buffered = vec![closed_line("w1:p2")];
        assert_eq!(
            bootstrap_pane_set(enumerated, &buffered),
            vec!["w1:p1".to_string()]
        );
    }

    #[test]
    fn bootstrap_set_ignores_duplicates_and_unrelated_lines() {
        let enumerated = vec!["w1:p1".to_string()];
        let buffered = vec![
            created_line("w1:p1"), // already enumerated
            created_line("w1:p2"),
            created_line("w1:p2"), // duplicate announcement
            r#"{"event":"workspace.focused","data":{"workspace_id":"w1"}}"#.to_string(),
            "not json".to_string(),
        ];
        assert_eq!(
            bootstrap_pane_set(enumerated, &buffered),
            vec!["w1:p1".to_string(), "w1:p2".to_string()]
        );
    }

    #[test]
    fn a_pane_created_then_closed_inside_the_window_is_not_subscribed() {
        let buffered = vec![created_line("w1:p9"), closed_line("w1:p9")];
        assert!(bootstrap_pane_set(vec![], &buffered).is_empty());
    }

    // ── pane-gone classification ─────────────────────────────────────────────

    #[test]
    fn classifies_pane_closed_to_both_id_forms() {
        let panes = HerdrPaneRegistry::new();
        let id = panes.assign_or_get("w1:p1", "t1");
        let line = closed_line("w1:p1");
        match classify_pushed(&line, &panes) {
            Pushed::PaneGone {
                herdr_pane_id,
                pane,
            } => {
                assert_eq!(herdr_pane_id.as_deref(), Some("w1:p1"));
                assert_eq!(pane, Some(id));
            }
            _ => panic!("expected PaneGone"),
        }
    }

    #[test]
    fn classifies_pane_exited_nested_pane_id() {
        let panes = HerdrPaneRegistry::new();
        let id = panes.assign_or_get("w1:p5", "t5");
        let line = r#"{"event":"pane_exited","data":{"pane":{"pane_id":"w1:p5"}}}"#;
        match classify_pushed(line, &panes) {
            Pushed::PaneGone { pane, .. } => assert_eq!(pane, Some(id)),
            _ => panic!("expected PaneGone"),
        }
    }

    #[test]
    fn pane_closed_for_an_untracked_pane_still_yields_its_herdr_id() {
        // RE-SPECIFIED: the neutral id is unknown (nothing to prune from the
        // status table), but the herdr id is what prunes the subscribed set, so
        // the event is no longer discarded outright.
        let panes = HerdrPaneRegistry::new();
        let line = r#"{"event":"pane.closed","data":{"pane_id":"never-seen"}}"#;
        match classify_pushed(line, &panes) {
            Pushed::PaneGone {
                herdr_pane_id,
                pane,
            } => {
                assert_eq!(herdr_pane_id.as_deref(), Some("never-seen"));
                assert_eq!(pane, None);
            }
            _ => panic!("expected PaneGone"),
        }
    }

    #[test]
    fn pane_closed_with_no_id_is_ignored() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{"event":"pane.closed","data":{}}"#;
        assert!(matches!(classify_pushed(line, &panes), Pushed::Ignored));
    }

    // ── ignored lines ────────────────────────────────────────────────────────

    #[test]
    fn subscription_ack_is_recognised_and_not_a_pushed_event() {
        let ack = r#"{"id":"muxrd-events-lifecycle","result":{"type":"subscription_started"}}"#;
        assert!(is_subscription_ack(ack));
        // The ack has no top-level "event", so classify ignores it.
        let panes = HerdrPaneRegistry::new();
        assert!(matches!(classify_pushed(ack, &panes), Pushed::Ignored));
    }

    #[test]
    fn non_ack_response_is_not_an_ack() {
        assert!(!is_subscription_ack(r#"{"id":"x","result":{"type":"ok"}}"#));
        assert!(!is_subscription_ack(r#"{"id":"x","error":{"code":"bad"}}"#));
        assert!(!is_subscription_ack("not json"));
    }

    #[test]
    fn subscribe_error_extracts_error_body() {
        let line = r#"{"id":"x","error":{"code":"invalid_request","message":"missing field 'subscriptions'"}}"#;
        let err = subscribe_error(line).expect("error present");
        assert!(err.contains("invalid_request"));
        assert!(subscribe_error(r#"{"id":"x","result":{"type":"ok"}}"#).is_none());
    }

    #[test]
    fn ignores_unknown_event_type() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{"event":"workspace.focused","data":{"workspace_id":"w1"}}"#;
        assert!(matches!(classify_pushed(line, &panes), Pushed::Ignored));
        assert!(lifecycle_change(line).is_none());
    }

    #[test]
    fn ignores_malformed_and_empty_lines() {
        let panes = HerdrPaneRegistry::new();
        assert!(matches!(classify_pushed("", &panes), Pushed::Ignored));
        assert!(matches!(
            classify_pushed("not json at all", &panes),
            Pushed::Ignored
        ));
        assert!(matches!(
            classify_pushed("{ broken", &panes),
            Pushed::Ignored
        ));
        // Right event, but missing a required data field (workspace_id) → ignored.
        assert!(matches!(
            classify_pushed(
                r#"{"event":"pane.agent_status_changed","data":{"pane_id":"p","agent_status":"idle"}}"#,
                &panes
            ),
            Pushed::Ignored
        ));
    }

    // ── non-clobbering pane translation (the terminal_id-safety fix) ──────────

    #[test]
    fn push_does_not_clobber_existing_terminal_id() {
        let panes = HerdrPaneRegistry::new();
        // A layout poll registered this pane with its real terminal_id.
        let id = panes.assign_or_get("w1:p1", "term-real");
        // A pushed event for the same pane arrives WITHOUT a terminal_id.
        let line = r#"{
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "w1:p1", "workspace_id": "w1", "agent_status": "blocked" }
        }"#;
        match classify_pushed(line, &panes) {
            Pushed::AgentStatus(ev) => {
                assert_eq!(ev.pane, id, "same pane must resolve to the same neutral id");
                assert_eq!(
                    panes.terminal_id(id).as_deref(),
                    Some("term-real"),
                    "the live relay terminal_id must NOT be clobbered by the event"
                );
            }
            _ => panic!("expected an agent-status event"),
        }
    }

    #[test]
    fn translate_assigns_new_pane_from_event_terminal_id() {
        let panes = HerdrPaneRegistry::new();
        assert_eq!(translate_pane(&panes, "pane-new", Some("term-new")), 1);
        assert_eq!(panes.terminal_id(1).as_deref(), Some("term-new"));
        // Second, previously-unseen pane with no terminal_id still gets an id.
        let id2 = translate_pane(&panes, "pane-none", None);
        assert_eq!(id2, 2);
    }

    // ── status mapping ───────────────────────────────────────────────────────

    #[test]
    fn maps_every_agent_status() {
        assert_eq!(map_status(HerdrAgentStatus::Idle), AgentStatus::Idle);
        assert_eq!(map_status(HerdrAgentStatus::Working), AgentStatus::Working);
        assert_eq!(map_status(HerdrAgentStatus::Blocked), AgentStatus::Blocked);
        assert_eq!(map_status(HerdrAgentStatus::Done), AgentStatus::Done);
        assert_eq!(map_status(HerdrAgentStatus::Unknown), AgentStatus::Unknown);
    }

    // ── subscribe request shapes (the two connections) ───────────────────────

    #[test]
    fn lifecycle_request_is_three_bare_entries_and_never_carries_a_pane_id() {
        let line = lifecycle_request_line().expect("build lifecycle line");
        assert!(line.ends_with('\n'), "request must be newline-terminated");
        let value: Value = serde_json::from_str(line.trim_end()).expect("valid JSON");
        assert_eq!(value["method"], "events.subscribe");
        assert_eq!(value["id"], LIFECYCLE_REQUEST_ID);
        let subs = value["params"]["subscriptions"]
            .as_array()
            .expect("params.subscriptions must be an array");
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[0]["type"], SUBSCRIBE_PANE_CREATED_TYPE);
        assert_eq!(subs[1]["type"], SUBSCRIBE_PANE_CLOSED_TYPE);
        assert_eq!(
            subs[2]["type"], SUBSCRIBE_PANE_EXITED_TYPE,
            "pane.exited was parsed but never subscribed to before the split"
        );
        assert!(
            subs.iter().all(|s| s.get("pane_id").is_none()),
            "every lifecycle entry is bare — that is why A never widens"
        );
    }

    #[test]
    fn agent_status_request_carries_one_explicit_pane_id_per_pane() {
        let pane_ids = vec!["w1:p1".to_string(), "w1:p2".to_string()];
        let line = agent_status_request_line(&pane_ids).expect("build agent-status line");
        assert!(line.ends_with('\n'), "request must be newline-terminated");
        let value: Value = serde_json::from_str(line.trim_end()).expect("valid JSON");
        assert_eq!(value["method"], "events.subscribe");
        assert_eq!(value["id"], AGENT_STATUS_REQUEST_ID);
        let subs = value["params"]["subscriptions"].as_array().expect("array");
        assert_eq!(subs.len(), 2, "one entry per pane — no wildcard exists");
        assert_eq!(subs[0]["type"], SUBSCRIBE_AGENT_STATUS_TYPE);
        assert_eq!(subs[0]["pane_id"], "w1:p1");
        assert_eq!(subs[1]["pane_id"], "w1:p2");
        assert_eq!(SUBSCRIBE_AGENT_STATUS_TYPE, "pane.agent_status_changed");
    }

    #[test]
    fn agent_status_request_with_no_panes_is_empty_not_lifecycle_bearing() {
        // RE-SPECIFIED for the split: the lifecycle entries moved to connection
        // A, so B with no panes is a valid, inert subscription (herdr's schema
        // sets no minimum on the array).
        let line = agent_status_request_line(&[]).expect("build agent-status line");
        let value: Value = serde_json::from_str(line.trim_end()).expect("valid JSON");
        assert!(
            value["params"]["subscriptions"]
                .as_array()
                .expect("array")
                .is_empty()
        );
    }

    // ── synthetic resend suppression (dedup) ─────────────────────────────────

    fn resync_pane(pane_id: &str, status: HerdrAgentStatus) -> ResyncPane {
        ResyncPane {
            herdr_pane_id: pane_id.to_string(),
            terminal_id: format!("term-{pane_id}"),
            workspace_id: "w1".to_string(),
            workspace_label: "main".to_string(),
            title: None,
            status,
        }
    }

    #[test]
    fn fresh_start_emits_each_blocked_pane_once() {
        let reg = HerdrPaneRegistry::new();
        let mut last = HashMap::new();
        let input = vec![
            resync_pane("w1:p1", HerdrAgentStatus::Blocked),
            resync_pane("w1:p2", HerdrAgentStatus::Done),
            resync_pane("w1:p3", HerdrAgentStatus::Idle),
        ];
        let events = build_resync_events(&input, &reg, &mut last);
        assert_eq!(events.len(), 2, "only blocked + done emit");
        assert!(events.iter().all(|e| e.synthetic));
        // Second resync with identical state emits nothing.
        let again = build_resync_events(&input, &reg, &mut last);
        assert!(again.is_empty(), "unchanged state must not re-emit");
    }

    #[test]
    fn changed_status_re_emits_but_unchanged_does_not() {
        let reg = HerdrPaneRegistry::new();
        let mut last = HashMap::new();
        // First observation: blocked → emitted.
        let first = build_resync_events(
            &[resync_pane("w1:p1", HerdrAgentStatus::Blocked)],
            &reg,
            &mut last,
        );
        assert_eq!(first.len(), 1);
        // Goes idle (no emit — not blocked/done — but last-known updates).
        let idle = build_resync_events(
            &[resync_pane("w1:p1", HerdrAgentStatus::Idle)],
            &reg,
            &mut last,
        );
        assert!(idle.is_empty());
        // Blocked again — a genuine change from idle → re-emit.
        let reblocked = build_resync_events(
            &[resync_pane("w1:p1", HerdrAgentStatus::Blocked)],
            &reg,
            &mut last,
        );
        assert_eq!(reblocked.len(), 1, "idle→blocked is a real transition");
    }

    #[test]
    fn a_live_push_suppresses_the_next_resync_for_the_same_status() {
        let reg = HerdrPaneRegistry::new();
        let mut last = HashMap::new();
        // Simulate the push path having recorded this pane as blocked.
        let id = reg.assign_or_get("w1:p1", "t1");
        last.insert(id, AgentStatus::Blocked);
        // A resync observing the same blocked status must not re-ping.
        let events = build_resync_events(
            &[resync_pane("w1:p1", HerdrAgentStatus::Blocked)],
            &reg,
            &mut last,
        );
        assert!(events.is_empty(), "push already reported this block");
    }

    #[test]
    fn disappeared_panes_are_pruned_from_tracking() {
        let reg = HerdrPaneRegistry::new();
        let mut last = HashMap::new();
        build_resync_events(
            &[
                resync_pane("w1:p1", HerdrAgentStatus::Blocked),
                resync_pane("w1:p2", HerdrAgentStatus::Blocked),
            ],
            &reg,
            &mut last,
        );
        assert_eq!(last.len(), 2);
        // p2 vanished from the enumeration.
        build_resync_events(
            &[resync_pane("w1:p1", HerdrAgentStatus::Blocked)],
            &reg,
            &mut last,
        );
        assert_eq!(last.len(), 1, "p2 tracking pruned");
        let p1 = reg.id_for_herdr_pane("w1:p1").unwrap();
        assert!(last.contains_key(&p1));
    }

    // ── watchdog verdicts ────────────────────────────────────────────────────

    fn probe_pane(herdr_pane_id: &str, pane: Option<u32>, status: AgentStatus) -> ProbePane {
        ProbePane {
            herdr_pane_id: herdr_pane_id.to_string(),
            pane,
            status,
        }
    }

    #[test]
    fn a_quiet_system_is_healthy_and_never_churns() {
        // Nothing changed anywhere: the probe must return Healthy, which is what
        // keeps a legitimately silent herdr from being torn down on a timer.
        let subscribed: HashSet<String> = ["w1:p1"].into_iter().map(str::to_string).collect();
        let mut last = HashMap::new();
        last.insert(1, AgentStatus::Idle);
        let observed = vec![probe_pane("w1:p1", Some(1), AgentStatus::Idle)];
        assert_eq!(
            watchdog_verdict(&observed, &subscribed, &last),
            Verdict::Healthy
        );
        // Repeating it forever still never strikes.
        for _ in 0..100 {
            assert_eq!(
                watchdog_verdict(&observed, &subscribed, &last),
                Verdict::Healthy
            );
        }
    }

    #[test]
    fn a_status_that_moved_without_a_push_is_status_wedged() {
        let subscribed: HashSet<String> = ["w1:p1"].into_iter().map(str::to_string).collect();
        let mut last = HashMap::new();
        last.insert(1, AgentStatus::Working);
        let observed = vec![probe_pane("w1:p1", Some(1), AgentStatus::Blocked)];
        assert_eq!(
            watchdog_verdict(&observed, &subscribed, &last),
            Verdict::StatusWedged
        );
    }

    #[test]
    fn a_pane_the_lifecycle_watcher_never_announced_is_lifecycle_wedged() {
        let subscribed: HashSet<String> = ["w1:p1"].into_iter().map(str::to_string).collect();
        let mut last = HashMap::new();
        last.insert(1, AgentStatus::Idle);
        let observed = vec![
            probe_pane("w1:p1", Some(1), AgentStatus::Idle),
            probe_pane("w1:p2", None, AgentStatus::Blocked),
        ];
        assert_eq!(
            watchdog_verdict(&observed, &subscribed, &last),
            Verdict::LifecycleWedged,
            "an unannounced pane outranks a status divergence: rebuilding A rebuilds B"
        );
    }

    #[test]
    fn a_pane_with_no_last_known_status_is_not_evidence() {
        let subscribed: HashSet<String> = ["w1:p1"].into_iter().map(str::to_string).collect();
        let observed = vec![probe_pane("w1:p1", Some(1), AgentStatus::Blocked)];
        assert_eq!(
            watchdog_verdict(&observed, &subscribed, &HashMap::new()),
            Verdict::Healthy
        );
    }

    #[test]
    fn a_vanished_pane_is_not_evidence() {
        // A missed close leaves a subscribed id with no live pane. That is not
        // treated as a wedge: the dead subscription is inert and owes nothing.
        let subscribed: HashSet<String> =
            ["w1:p1", "w1:p2"].into_iter().map(str::to_string).collect();
        let mut last = HashMap::new();
        last.insert(1, AgentStatus::Idle);
        let observed = vec![probe_pane("w1:p1", Some(1), AgentStatus::Idle)];
        assert_eq!(
            watchdog_verdict(&observed, &subscribed, &last),
            Verdict::Healthy
        );
    }

    // ── backoff math ─────────────────────────────────────────────────────────

    #[test]
    fn backoff_doubles_then_caps_at_sixty() {
        let mut seq = Vec::new();
        let mut b = INITIAL_BACKOFF;
        for _ in 0..8 {
            seq.push(b.as_secs());
            b = next_backoff(b, MAX_BACKOFF);
        }
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn backoff_never_exceeds_cap() {
        let mut b = MAX_BACKOFF;
        for _ in 0..5 {
            b = next_backoff(b, MAX_BACKOFF);
            assert_eq!(b, MAX_BACKOFF);
        }
    }

    // ── widening storm breaker ───────────────────────────────────────────────

    fn record(history: &mut Vec<Instant>, now: Instant) -> bool {
        record_grow_and_check_storm(history, now, GROW_STORM_WINDOW, GROW_STORM_THRESHOLD)
    }

    #[test]
    fn below_threshold_widenings_stay_fast() {
        let mut history = Vec::new();
        let base = Instant::now();
        // Exactly GROW_STORM_THRESHOLD widenings in quick succession must never
        // trip the breaker — only exceeding it does.
        for i in 0..GROW_STORM_THRESHOLD {
            let now = base + Duration::from_millis(i as u64 * 10);
            assert!(
                !record(&mut history, now),
                "widening #{i} should stay under threshold"
            );
        }
    }

    #[test]
    fn a_coalesced_burst_of_pane_creations_cannot_trip_the_breaker() {
        // The re-reasoned threshold: GROW_COALESCE_WINDOW collapses a burst into
        // one widening, so even six separate bursts inside the window are fine.
        let mut history = Vec::new();
        let base = Instant::now();
        for i in 0..6 {
            let now = base + GROW_COALESCE_WINDOW * (i + 1);
            assert!(
                !record(&mut history, now),
                "six coalesced bursts must not look like a fault"
            );
        }
    }

    #[test]
    fn storm_engages_breaker() {
        let mut history = Vec::new();
        let base = Instant::now();
        for i in 0..GROW_STORM_THRESHOLD {
            let now = base + Duration::from_millis(i as u64 * 10);
            assert!(!record(&mut history, now));
        }
        // The (GROW_STORM_THRESHOLD + 1)th widening inside the window trips it.
        let tripped = base + Duration::from_millis(GROW_STORM_THRESHOLD as u64 * 10 + 10);
        assert!(record(&mut history, tripped));
        // Further reconnects while still inside the window stay engaged.
        let still_storming = tripped + Duration::from_millis(10);
        assert!(record(&mut history, still_storming));
    }

    #[test]
    fn breaker_disengages_after_quiet_window() {
        let mut history = Vec::new();
        let base = Instant::now();
        // Trip the breaker.
        for i in 0..=GROW_STORM_THRESHOLD {
            let now = base + Duration::from_millis(i as u64 * 10);
            record(&mut history, now);
        }
        let storming_at = base + Duration::from_millis(GROW_STORM_THRESHOLD as u64 * 10);
        assert!(
            record(&mut history, storming_at + Duration::from_millis(1)),
            "sanity: breaker is engaged right before the quiet window"
        );
        // Once GROW_STORM_WINDOW passes with no further widenings, the old
        // timestamps are pruned and a fresh one is back under threshold.
        let quiet = storming_at + GROW_STORM_WINDOW + Duration::from_millis(1);
        assert!(
            !record(&mut history, quiet),
            "breaker must disengage once the window clears"
        );
    }

    // ── bounded line reader ──────────────────────────────────────────────────

    #[tokio::test]
    async fn reads_newline_delimited_lines() {
        use std::io::Cursor;
        let mut reader = LineReader::new(BufReader::new(Cursor::new(b"first\nsecond\n".to_vec())));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("first"));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("second"));
        assert_eq!(reader.next_line().await.unwrap(), None, "EOF");
    }

    #[tokio::test]
    async fn returns_a_final_unterminated_line_before_eof() {
        use std::io::Cursor;
        let mut reader = LineReader::new(BufReader::new(Cursor::new(b"tail".to_vec())));
        assert_eq!(reader.next_line().await.unwrap().as_deref(), Some("tail"));
        assert_eq!(reader.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn read_rejects_oversize_line_without_newline() {
        use std::io::Cursor;
        let mut reader = LineReader::new(BufReader::new(Cursor::new(vec![
            b'x';
            MAX_EVENT_LINE_BYTES
                + 16
        ])));
        let err = reader
            .next_line()
            .await
            .expect_err("oversize line must error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // ── end-to-end against a fake herdr JSON-API socket ──────────────────────
    //
    // These drive the real state machine — both connections, the enumeration
    // window, the widening rule, the resync and the watchdog — over a Unix
    // socket served by a minimal stand-in that behaves the way herdr 0.9.0 does:
    // one request per connection, a `subscription_started` ack, and **no replay
    // of anything that happened before the request was accepted**.

    mod fake_herdr {
        use super::*;
        use crate::multiplexer::events::EVENT_BUS_CAPACITY;
        use crate::multiplexer::herdr::registry::HerdrTabRegistry;
        use std::sync::atomic::{AtomicU64, Ordering};
        use tokio::net::UnixListener;
        use tokio::sync::{broadcast, mpsc};

        /// A pane as the fake reports it from `pane.list`.
        #[derive(Debug, Clone)]
        pub(super) struct FakePane {
            pub pane_id: String,
            pub status: HerdrAgentStatus,
        }

        pub(super) fn pane(pane_id: &str, status: HerdrAgentStatus) -> FakePane {
            FakePane {
                pane_id: pane_id.to_string(),
                status,
            }
        }

        #[derive(Default)]
        pub(super) struct FakeState {
            /// What `pane.list` answers. Tests mutate this mid-run.
            pub panes: Vec<FakePane>,
            /// Pane-id lists from every agent-status subscribe received, in order.
            pub status_subscribes: Vec<Vec<String>>,
            /// How many lifecycle subscribes were received.
            pub lifecycle_subscribes: usize,
            /// Lines pushed on the live lifecycle connection when the FIRST
            /// `pane.list` arrives — i.e. squarely inside the enumeration window.
            pub inject_on_first_pane_list: Vec<String>,
            /// How long the first `pane.list` stalls after injecting, so the
            /// kernel demonstrably reads the push while enumerating.
            pub pane_list_delay: Duration,
            pub pane_list_calls: usize,
            /// Reject the first agent-status subscribe the way herdr rejects a
            /// stale pane id: an error response, then close.
            pub reject_first_status_subscribe: bool,
            pub rejected_once: bool,
            /// Sender into the live lifecycle connection.
            pub lifecycle_tx: Option<mpsc::UnboundedSender<String>>,
        }

        pub(super) struct FakeHerdr {
            pub socket: PathBuf,
            pub state: Arc<Mutex<FakeState>>,
            server: tokio::task::JoinHandle<()>,
        }

        impl Drop for FakeHerdr {
            fn drop(&mut self) {
                self.server.abort();
                let _ = std::fs::remove_file(&self.socket);
            }
        }

        impl FakeHerdr {
            pub(super) fn start(state: FakeState) -> Self {
                // Keep the path SHORT: `sun_path` caps at ~104 bytes and CI's
                // temp dir is deep (mirrors control.rs's fake-socket tests).
                static SEQ: AtomicU64 = AtomicU64::new(0);
                let socket = std::env::temp_dir().join(format!(
                    "mxk{}_{}.sock",
                    std::process::id(),
                    SEQ.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_file(&socket);
                let listener = UnixListener::bind(&socket).expect("bind fake herdr socket");
                let state = Arc::new(Mutex::new(state));
                let server = tokio::spawn(serve(listener, Arc::clone(&state)));
                Self {
                    socket,
                    state,
                    server,
                }
            }

            pub(super) fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
                f(&mut lock(&self.state))
            }

            /// Block until `cond` holds, or fail the test.
            pub(super) async fn wait_for(&self, what: &str, cond: impl Fn(&FakeState) -> bool) {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if cond(&lock(&self.state)) {
                        return;
                    }
                    assert!(Instant::now() < deadline, "timed out waiting for {what}");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }

        async fn serve(listener: UnixListener, state: Arc<Mutex<FakeState>>) {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(handle(stream, Arc::clone(&state)));
            }
        }

        async fn handle(stream: UnixStream, state: Arc<Mutex<FakeState>>) {
            let (read_half, mut write) = stream.into_split();
            let mut reader = LineReader::new(BufReader::new(read_half));
            let Ok(Some(line)) = reader.next_line().await else {
                return;
            };
            let Ok(req) = serde_json::from_str::<Value>(&line) else {
                return;
            };
            let id = req["id"].as_str().unwrap_or_default().to_string();
            match req["method"].as_str().unwrap_or_default() {
                "workspace.list" => {
                    let body = serde_json::json!({
                        "id": id,
                        "result": { "type": "workspace_list", "workspaces": [{
                            "workspace_id": "w1", "number": 1, "label": "main", "focused": true,
                            "pane_count": 1, "tab_count": 1, "active_tab_id": "w1:t1",
                            "agent_status": "idle"
                        }]}
                    });
                    let _ = write.write_all(format!("{body}\n").as_bytes()).await;
                }
                "pane.list" => {
                    let (inject, delay) = {
                        let mut s = lock(&state);
                        s.pane_list_calls += 1;
                        if s.pane_list_calls == 1 {
                            (
                                std::mem::take(&mut s.inject_on_first_pane_list),
                                s.pane_list_delay,
                            )
                        } else {
                            (Vec::new(), Duration::ZERO)
                        }
                    };
                    if !inject.is_empty() {
                        let tx = lock(&state).lifecycle_tx.clone();
                        if let Some(tx) = tx {
                            for line in inject {
                                let _ = tx.send(line);
                            }
                        }
                        // Give the kernel time to read the push while this
                        // enumeration call is still outstanding.
                        tokio::time::sleep(delay).await;
                    }
                    let panes: Vec<Value> = lock(&state)
                        .panes
                        .iter()
                        .map(|p| {
                            serde_json::json!({
                                "pane_id": p.pane_id,
                                "terminal_id": format!("t-{}", p.pane_id),
                                "workspace_id": "w1",
                                "tab_id": "w1:t1",
                                "focused": false,
                                "agent_status": p.status,
                                "revision": 1
                            })
                        })
                        .collect();
                    let body = serde_json::json!({
                        "id": id,
                        "result": { "type": "pane_list", "panes": panes }
                    });
                    let _ = write.write_all(format!("{body}\n").as_bytes()).await;
                }
                "events.subscribe" => {
                    let subs = req["params"]["subscriptions"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    if id == LIFECYCLE_REQUEST_ID {
                        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
                        {
                            let mut s = lock(&state);
                            s.lifecycle_subscribes += 1;
                            s.lifecycle_tx = Some(tx);
                        }
                        let _ = write.write_all(ack_line(&id).as_bytes()).await;
                        while let Some(line) = rx.recv().await {
                            if write
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        return;
                    }
                    let pane_ids: Vec<String> = subs
                        .iter()
                        .filter_map(|s| s["pane_id"].as_str().map(str::to_string))
                        .collect();
                    let reject = {
                        let mut s = lock(&state);
                        s.status_subscribes.push(pane_ids);
                        if s.reject_first_status_subscribe && !s.rejected_once {
                            s.rejected_once = true;
                            true
                        } else {
                            false
                        }
                    };
                    if reject {
                        // herdr resolves every per-pane entry at subscribe time;
                        // one that no longer exists rejects the whole request and
                        // the connection closes with no ack.
                        let body = serde_json::json!({
                            "id": id,
                            "error": { "code": "not_found", "message": "pane not found" }
                        });
                        let _ = write.write_all(format!("{body}\n").as_bytes()).await;
                        return;
                    }
                    let _ = write.write_all(ack_line(&id).as_bytes()).await;
                    // Acked and then permanently silent — the wedged-but-open
                    // subscription the watchdog exists for.
                    std::future::pending::<()>().await;
                }
                _ => {}
            }
        }

        fn ack_line(id: &str) -> String {
            format!(r#"{{"id":"{id}","result":{{"type":"subscription_started"}}}}"#) + "\n"
        }

        /// A running kernel wired to one fake herdr.
        pub(super) struct RunningKernel {
            pub events: broadcast::Receiver<MuxEvent>,
            shutdown: watch::Sender<bool>,
            task: tokio::task::JoinHandle<()>,
        }

        impl RunningKernel {
            pub(super) fn start(fake: &FakeHerdr, tuning: Tuning) -> Self {
                let panes = Arc::new(HerdrPaneRegistry::new());
                let tabs = Arc::new(HerdrTabRegistry::new());
                let control = Arc::new(HerdrControl::new(
                    fake.socket.clone(),
                    Arc::clone(&panes),
                    tabs,
                ));
                let (bus, events) = broadcast::channel(EVENT_BUS_CAPACITY);
                let (shutdown, rx) = watch::channel(false);
                let task = tokio::spawn(run_tuned(
                    fake.socket.clone(),
                    panes,
                    control,
                    bus,
                    rx,
                    tuning,
                ));
                Self {
                    events,
                    shutdown,
                    task,
                }
            }

            pub(super) async fn stop(self) {
                let _ = self.shutdown.send(true);
                let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
            }
        }

        /// Tuning that keeps the watchdog out of the way.
        pub(super) fn no_probe_tuning() -> Tuning {
            Tuning {
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(50),
                ack_timeout: Duration::from_secs(5),
                grow_coalesce_window: Duration::from_millis(20),
                probe_interval: Duration::from_secs(3600),
                ..Tuning::default()
            }
        }

        /// Tuning that exercises the watchdog at millisecond scale.
        pub(super) fn fast_probe_tuning() -> Tuning {
            Tuning {
                probe_interval: Duration::from_millis(25),
                probe_strikes: LIVENESS_STRIKES,
                ..no_probe_tuning()
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pane_created_during_the_enumeration_reaches_the_agent_status_subscription() {
        // THE POINT. herdr 0.9.0 replays nothing, so a pane that appears after
        // the enumeration snapshot is announced exactly once, on the lifecycle
        // connection, while the enumeration is still in flight. The fake never
        // lists w1:p2 from `pane.list` at all, so the ONLY way it can reach an
        // agent-status subscription is the buffered lifecycle push.
        let fake = FakeHerdr::start(FakeState {
            panes: vec![pane("w1:p1", HerdrAgentStatus::Idle)],
            inject_on_first_pane_list: vec![created_line("w1:p2")],
            pane_list_delay: Duration::from_millis(150),
            ..FakeState::default()
        });
        let kernel = RunningKernel::start(&fake, no_probe_tuning());

        fake.wait_for("the agent-status subscription", |s| {
            !s.status_subscribes.is_empty()
        })
        .await;

        let first = fake.with(|s| s.status_subscribes[0].clone());
        assert_eq!(
            first,
            vec!["w1:p1".to_string(), "w1:p2".to_string()],
            "the pane created during the enumeration must be in the FIRST \
             agent-status subscription — nothing ever re-announces it"
        );
        kernel.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_quiet_herdr_never_reconnects_either_subscription() {
        // The other half of the watchdog contract: a system with no agent
        // activity and no pane churn is legitimately silent on both connections
        // for as long as it likes, and must not be torn down on a timer.
        let fake = FakeHerdr::start(FakeState {
            panes: vec![pane("w1:p1", HerdrAgentStatus::Idle)],
            ..FakeState::default()
        });
        let kernel = RunningKernel::start(&fake, fast_probe_tuning());

        fake.wait_for("both subscriptions", |s| !s.status_subscribes.is_empty())
            .await;
        // ~20 probe intervals of complete silence.
        tokio::time::sleep(Duration::from_millis(500)).await;

        fake.with(|s| {
            assert_eq!(
                s.status_subscribes.len(),
                1,
                "a quiet system must not churn the agent-status subscription"
            );
            assert_eq!(s.lifecycle_subscribes, 1, "nor the lifecycle subscription");
        });
        kernel.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_watchdog_rebuilds_an_agent_status_subscription_that_stops_delivering() {
        // The fake acks connection B and then never pushes anything — the
        // acknowledged-but-silent failure mode. The pane goes `blocked` with no
        // push, so the control-plane probe sees a transition the subscription
        // owed us and, after LIVENESS_STRIKES consecutive probes, rebuilds it.
        let fake = FakeHerdr::start(FakeState {
            panes: vec![pane("w1:p1", HerdrAgentStatus::Idle)],
            ..FakeState::default()
        });
        let mut kernel = RunningKernel::start(&fake, fast_probe_tuning());

        fake.wait_for("the first agent-status subscription", |s| {
            !s.status_subscribes.is_empty()
        })
        .await;
        // Let the first resync record the pane as idle before it moves.
        tokio::time::sleep(Duration::from_millis(60)).await;
        fake.with(|s| s.panes[0].status = HerdrAgentStatus::Blocked);

        fake.wait_for(
            "the watchdog to rebuild the agent-status subscription",
            |s| s.status_subscribes.len() >= 2,
        )
        .await;
        fake.with(|s| {
            assert_eq!(
                s.lifecycle_subscribes, 1,
                "rebuilding connection B must never take connection A down"
            );
        });

        // …and the resync that follows the rebuild republishes the transition
        // the wedged subscription swallowed.
        let blocked = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match kernel.events.recv().await {
                    Ok(MuxEvent::AgentStatusChanged(ev))
                        if ev.status == AgentStatus::Blocked && ev.synthetic =>
                    {
                        return ev;
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => panic!("bus closed"),
                }
            }
        })
        .await;
        assert!(
            blocked.is_ok(),
            "the post-rebuild resync must re-report the missed transition"
        );
        kernel.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rejected_agent_status_subscription_leaves_the_lifecycle_watcher_up() {
        // A pane id that vanished between enumeration and subscribe aborts the
        // ENTIRE request with an error and no ack. Under the split that kills
        // connection B only: it backs off, re-enumerates and retries, while
        // connection A keeps watching pane lifecycle throughout.
        let fake = FakeHerdr::start(FakeState {
            panes: vec![pane("w1:p1", HerdrAgentStatus::Idle)],
            reject_first_status_subscribe: true,
            ..FakeState::default()
        });
        let kernel = RunningKernel::start(&fake, no_probe_tuning());

        fake.wait_for("the agent-status retry", |s| s.status_subscribes.len() >= 2)
            .await;
        fake.with(|s| {
            assert_eq!(
                s.lifecycle_subscribes, 1,
                "connection B failing must not take connection A down"
            );
            assert!(
                s.pane_list_calls >= 2,
                "each B attempt must re-enumerate, or a stale id would fail forever"
            );
        });
        kernel.stop().await;
    }

    #[tokio::test]
    async fn partial_lines_survive_cancellation() {
        // Multiplexing two sockets in one select! drops the losing branch's
        // future mid-read; a reader that buffered inside the future would eat the
        // bytes it had already consumed.
        let (mut client, server) = tokio::net::UnixStream::pair().expect("socketpair");
        let mut reader = LineReader::new(BufReader::new(server));
        client.write_all(b"{\"event\":").await.expect("partial");
        // Force at least one poll that reads the partial chunk, then cancel it.
        let cancelled = tokio::time::timeout(Duration::from_millis(50), reader.next_line()).await;
        assert!(
            cancelled.is_err(),
            "no newline yet — the read must not finish"
        );
        client
            .write_all(b"\"pane.created\"}\n")
            .await
            .expect("rest");
        let line = reader
            .next_line()
            .await
            .expect("read")
            .expect("a whole line");
        assert_eq!(
            line, r#"{"event":"pane.created"}"#,
            "the cancelled read must not have lost its buffered prefix"
        );
    }
}
