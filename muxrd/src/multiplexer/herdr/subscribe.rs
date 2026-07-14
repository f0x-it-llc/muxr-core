//! herdr **event kernel** — a persistent consumer of herdr's JSON-API
//! `events.subscribe` stream, republishing agent-status transitions onto the
//! internal [`EventBus`](crate::multiplexer::events::EventBus).
//!
//! ## Why a new connection style
//!
//! [`HerdrControl`](super::control::HerdrControl) is **one connection per
//! request** (fresh [`std::os::unix::net::UnixStream`] per call, bounded read
//! timeout). `events.subscribe` is the opposite: subscribe **once** on a
//! long-lived socket, then read newline-delimited pushed events indefinitely.
//! This task therefore owns its own [`tokio::net::UnixStream`] reader — the
//! blocking `spawn_blocking` idiom applies to the *short* control calls, not to
//! this streaming reader. The only blocking work it does (pre-connect pane
//! enumeration and the post-connect resync) is routed through `HerdrControl` on
//! the blocking pool.
//!
//! ## Wire schema (live-verified against herdr 0.7.1 / protocol 14)
//!
//! - **Request** — the subscription set is fixed at connect time and sent in a
//!   single request whose params carry a `subscriptions` array:
//!   `{"id":…,"method":"events.subscribe","params":{"subscriptions":[…]}}`.
//!   `pane.agent_status_changed` is **per-pane** and requires a `pane_id`
//!   (`"w1:p1"` form) — no wildcard and no omission — so the kernel enumerates
//!   the current panes first and emits one entry per pane, plus bare
//!   `{"type":"pane.created"}` / `{"type":"pane.closed"}` entries. A **second**
//!   `events.subscribe` on the same socket resets the connection, so growing the
//!   set means reconnecting.
//! - **Ack** — `{"id":…,"result":{"type":"subscription_started"}}`. The kernel
//!   logs "subscribed" only once this ack is observed, then resyncs.
//! - **Pushed event** — top-level key `event` (dot form for agent status,
//!   inconsistently underscore for `pane_created`) with the payload under `data`:
//!   `{"event":"pane.agent_status_changed","data":{"agent_status":"blocked",
//!   "pane_id":"w1:p1","workspace_id":"w1"}}`. The push carries no
//!   `terminal_id` / `workspace_name` / `title` for the agent-status event, so
//!   those stay `None` on the push path (the resync path still supplies the
//!   workspace label). The event name is parsed tolerantly (dot **and**
//!   underscore forms accepted).
//!
//! ## Reconnect & loss
//!
//! herdr documents no reconnect semantics for the subscription, so the kernel
//! owns its own exponential backoff ([`INITIAL_BACKOFF`] → [`MAX_BACKOFF`], reset
//! after a stable connection ≥ [`STABLE_THRESHOLD`]) and treats the subscription
//! as **lossy**: after every (re)connect it *resyncs* current pane agent states
//! via `HerdrControl` and emits synthetic [`AgentStatus::Blocked`]/[`AgentStatus::Done`]
//! events (`synthetic: true`) so a consumer that missed a transition during the
//! gap still learns which panes currently need attention. A `pane.created` push
//! for a pane **not** already subscribed is a **non-error reconnect** (widen the
//! subscription set): backoff is reset and a short [`GROW_RECONNECT_DELAY`]
//! applied rather than the failure backoff.
//!
//! ## `pane_created` snapshot replay (herdr 0.7.1)
//!
//! herdr replays a `pane_created` for **every** already-existing pane immediately
//! after the `subscription_started` ack (snapshot semantics, wire-confirmed
//! 2026-07-15). Treating each as a widening would Grow-reconnect, whose fresh
//! subscription replays them again — an infinite loop. The kernel therefore
//! Grow-reconnects **only** for a `pane_id` absent from the connect-time set; a
//! replayed/known id is ignored (the id lives at `data.pane.pane_id`).
//!
//! ## Synthetic resend suppression
//!
//! The kernel tracks each pane's last-known [`AgentStatus`]. During a resync a
//! synthetic event is emitted **only** when a pane's `Blocked`/`Done` status
//! *differs* from its last-known value — so a connection that flaps repeatedly no
//! longer re-pings a consumer for the same still-stuck agent. On a fresh start the
//! table is empty, so every currently-blocked pane emits exactly once. Tracking is
//! pruned for panes that disappear from the enumeration and updated by live pushes.
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
/// next drop resets backoff to [`INITIAL_BACKOFF`] rather than continuing to
/// escalate (so a herdr that flaps once an hour never sits at the 60 s cap).
const STABLE_THRESHOLD: Duration = Duration::from_secs(60);

/// Delay before a `pane.created`-driven reconnect. A new pane must be added to the
/// (connect-time-fixed) subscription set by reconnecting; this is an expected
/// widening, not a failure, so backoff is reset and only this short pause applied.
const GROW_RECONNECT_DELAY: Duration = Duration::from_millis(250);

/// Hard ceiling on a single pushed-event line, mirroring the control plane's
/// [`MAX_RESPONSE_BYTES`](super::control::MAX_RESPONSE_BYTES) defence: a peer
/// streaming bytes without a newline must not grow the line buffer unbounded.
/// Agent-status events are tiny; 1 MiB is generous while still bounding a hostile
/// stream.
const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;

/// Per-pane agent-status subscription type (dot form, in the request).
const SUBSCRIBE_AGENT_STATUS_TYPE: &str = "pane.agent_status_changed";

/// Pane-created subscription type (bare, no `pane_id`).
const SUBSCRIBE_PANE_CREATED_TYPE: &str = "pane.created";

/// Pane-closed subscription type (bare, no `pane_id`).
const SUBSCRIBE_PANE_CLOSED_TYPE: &str = "pane.closed";

// ── Public entry point ──────────────────────────────────────────────────────────

/// Spawn the herdr event kernel as a background Tokio task.
///
/// Publishes [`MuxEvent`]s onto `bus`; exits cleanly when `shutdown` is set to
/// `true`. Shares the backend's [`HerdrPaneRegistry`] / [`HerdrTabRegistry`]
/// `Arc`s so translated pane ids stay identical to those handed out by layout
/// polls, and builds its own long-lived subscription socket (plus a per-request
/// `HerdrControl` over the same socket path for pane enumeration and resync).
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

// ── Subscription lifecycle ──────────────────────────────────────────────────────

/// Shared last-known agent status per neutral pane id, used to suppress duplicate
/// synthetic resync emissions. Lives for the whole kernel lifetime (across
/// reconnects) so a flapping connection does not re-ping unchanged blocked panes.
type StatusTable = Arc<Mutex<HashMap<u32, AgentStatus>>>;

/// Outcome of a single subscription session (one connected socket lifetime).
enum SessionOutcome {
    /// Shutdown was requested — the outer loop must exit.
    Shutdown,
    /// The session ended (connect failure, EOF, or read error) — reconnect with backoff.
    Ended,
    /// A `pane.created` push arrived — reconnect promptly to widen the subscription
    /// set (not a failure; backoff is reset).
    Grow,
}

/// The reconnect loop: (re)subscribe forever until shutdown, backing off between
/// failures.
async fn run(
    api_socket: PathBuf,
    panes: Arc<HerdrPaneRegistry>,
    control: Arc<HerdrControl>,
    bus: EventBus,
    mut shutdown: watch::Receiver<bool>,
) {
    log::info!(
        "herdr event kernel: starting (socket {})",
        api_socket.display()
    );
    let last_status: StatusTable = Arc::new(Mutex::new(HashMap::new()));
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let started = Instant::now();
        match run_session(
            &api_socket,
            &panes,
            &control,
            &last_status,
            &bus,
            &mut shutdown,
        )
        .await
        {
            SessionOutcome::Shutdown => break,
            SessionOutcome::Grow => {
                // Expected widening of the subscription set — reset backoff and
                // reconnect promptly rather than treating it as a failure.
                backoff = INITIAL_BACKOFF;
                tokio::select! {
                    _ = tokio::time::sleep(GROW_RECONNECT_DELAY) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
            }
            SessionOutcome::Ended => {
                // A connection that lasted long enough is "stable": reset backoff.
                if started.elapsed() >= STABLE_THRESHOLD {
                    backoff = INITIAL_BACKOFF;
                }
                log::info!(
                    "herdr event kernel: disconnected — retrying in {}s",
                    backoff.as_secs()
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                }
                backoff = next_backoff(backoff);
            }
        }
    }
    log::info!("herdr event kernel: stopped");
}

/// One subscription session: enumerate panes, connect, subscribe, await the ack,
/// resync, then read pushed events until EOF / error / shutdown / a `pane.created`
/// widening.
async fn run_session(
    api_socket: &Path,
    panes: &Arc<HerdrPaneRegistry>,
    control: &Arc<HerdrControl>,
    last_status: &StatusTable,
    bus: &EventBus,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionOutcome {
    // 1. Enumerate the current panes BEFORE connecting: the subscription set is
    //    fixed at connect time and `pane.agent_status_changed` is per-pane.
    let pane_ids = match enumerate_pane_ids(Arc::clone(control)).await {
        Ok(ids) => ids,
        Err(e) => {
            log::info!("herdr event kernel: pane enumeration failed: {e:#}");
            return SessionOutcome::Ended;
        }
    };
    // The connect-time subscription set, for O(1) replay detection: herdr replays
    // a `pane_created` for every one of these right after the ack (snapshot), and
    // only an id absent from this set is a genuinely new pane worth growing for.
    let known_pane_ids: HashSet<&str> = pane_ids.iter().map(String::as_str).collect();

    // 2. Connect the long-lived subscription socket.
    let stream = match UnixStream::connect(api_socket).await {
        Ok(s) => s,
        Err(e) => {
            log::info!(
                "herdr event kernel: connect to {} failed: {e}",
                api_socket.display()
            );
            return SessionOutcome::Ended;
        }
    };
    let (read_half, mut write_half) = stream.into_split();

    // 3. Build and send the single subscribe request.
    let line = match subscribe_request_line(&pane_ids) {
        Ok(l) => l,
        Err(e) => {
            log::error!("herdr event kernel: could not build subscribe request: {e:#}");
            return SessionOutcome::Ended;
        }
    };
    if let Err(e) = write_half.write_all(line.as_bytes()).await {
        log::info!("herdr event kernel: subscribe write failed: {e}");
        return SessionOutcome::Ended;
    }

    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();

    // 4. Wait for the `subscription_started` ack before doing anything else.
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return SessionOutcome::Shutdown;
                }
            }
            read = read_event_line(&mut reader, &mut buf) => {
                match read {
                    Ok(0) => {
                        log::info!("herdr event kernel: stream closed before subscribe ack");
                        return SessionOutcome::Ended;
                    }
                    Ok(_) => {
                        let l = buf.trim_end();
                        if is_subscription_ack(l) {
                            log::info!(
                                "herdr event kernel: subscribed ({} pane subscription(s) + pane.created/closed)",
                                pane_ids.len()
                            );
                            break;
                        }
                        if let Some(err) = subscribe_error(l) {
                            log::warn!("herdr event kernel: subscribe rejected: {err}");
                            return SessionOutcome::Ended;
                        }
                        log::trace!("herdr event kernel: ignoring pre-ack line");
                    }
                    Err(e) => {
                        log::info!("herdr event kernel: read error before ack: {e}");
                        return SessionOutcome::Ended;
                    }
                }
            }
        }
    }

    // 5. Lossy subscription: re-observe current state (deduped) on every (re)connect.
    let resynced = resync(
        Arc::clone(control),
        Arc::clone(panes),
        Arc::clone(last_status),
        bus.clone(),
    )
    .await;
    log::info!("herdr event kernel: resync emitted {resynced} synthetic event(s)");

    // 6. Read pushed events until EOF / error / shutdown / a widening reconnect.
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return SessionOutcome::Shutdown;
                }
            }
            read = read_event_line(&mut reader, &mut buf) => {
                match read {
                    Ok(0) => {
                        log::info!("herdr event kernel: subscription stream closed (EOF)");
                        return SessionOutcome::Ended;
                    }
                    Ok(_) => match classify_pushed(buf.trim_end(), panes) {
                        Pushed::AgentStatus(ev) => {
                            lock(last_status).insert(ev.pane, ev.status);
                            log::debug!(
                                "herdr event kernel: {:?} pane={} ws={}",
                                ev.status,
                                ev.pane,
                                ev.workspace_id
                            );
                            // No receivers yet is fine (broadcast → recoverable Err).
                            let _ = bus.send(MuxEvent::AgentStatusChanged(ev));
                        }
                        Pushed::PaneCreated(pane_id) => {
                            if pane_created_grows(pane_id.as_deref(), &known_pane_ids) {
                                log::info!(
                                    "herdr event kernel: new pane created ({}) — reconnecting to widen subscription set",
                                    pane_id.as_deref().unwrap_or("<no id>")
                                );
                                return SessionOutcome::Grow;
                            }
                            log::debug!(
                                "herdr event kernel: ignoring replayed pane_created for already-subscribed pane {}",
                                pane_id.as_deref().unwrap_or("<no id>")
                            );
                        }
                        Pushed::PaneGone(id) => {
                            // A dead subscription is harmless; just drop tracking so
                            // a future pane reusing the id does not inherit its status.
                            lock(last_status).remove(&id);
                        }
                        Pushed::Ignored => {}
                    },
                    Err(e) => {
                        log::info!("herdr event kernel: read error: {e}");
                        return SessionOutcome::Ended;
                    }
                }
            }
        }
    }
}

/// Enumerate the herdr `pane_id`s of every pane across all workspaces, on the
/// blocking pool (connection-per-request control transport).
async fn enumerate_pane_ids(control: Arc<HerdrControl>) -> Result<Vec<String>> {
    tokio::task::spawn_blocking(move || collect_pane_ids(&control))
        .await
        .context("herdr pane enumeration task panicked")?
}

/// Blocking helper: list every workspace's panes and collect their herdr `pane_id`s.
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

// ── Parsing ─────────────────────────────────────────────────────────────────────

/// A classified pushed line.
enum Pushed {
    /// A live `pane.agent_status_changed` transition, ready to publish.
    AgentStatus(AgentStatusChanged),
    /// A `pane_created` push, carrying the herdr `pane_id` if one was parseable
    /// (`data.pane.pane_id`). herdr replays a `pane_created` for **every**
    /// already-subscribed pane right after the ack (snapshot semantics), so the
    /// caller must Grow-reconnect **only** for an id absent from the connect-time
    /// set — a replayed/known id is ignored.
    PaneCreated(Option<String>),
    /// A `pane_closed` / `pane_exited` push for a pane we track (neutral id).
    PaneGone(u32),
    /// Anything not consumed (ack, unknown event, malformed, unknown pane) — ignored.
    Ignored,
}

/// Classify one newline-stripped stream line. The event name is matched
/// tolerantly: dot and underscore forms of a given event are treated alike
/// (herdr pushes `pane.agent_status_changed` with a dot but `pane_created` with an
/// underscore). Unknown events, response envelopes, and malformed lines are ignored.
fn classify_pushed(line: &str, panes: &HerdrPaneRegistry) -> Pushed {
    if line.is_empty() {
        return Pushed::Ignored;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            log::trace!("herdr event kernel: ignoring non-JSON line");
            return Pushed::Ignored;
        }
    };
    // Pushed events carry a top-level `event`; a response envelope (ack/error) does
    // not, so it falls through to Ignored here.
    let Some(event) = value.get("event").and_then(Value::as_str) else {
        return Pushed::Ignored;
    };
    let canonical = event.replace('.', "_");
    let data = value.get("data");
    match canonical.as_str() {
        "pane_agent_status_changed" => parse_agent_status(data, panes),
        "pane_created" => Pushed::PaneCreated(pane_created_id(data)),
        "pane_closed" | "pane_exited" => parse_pane_gone(data, panes),
        other => {
            log::trace!("herdr event kernel: ignoring event {other}");
            Pushed::Ignored
        }
    }
}

/// Parse a `pane.agent_status_changed` payload (`data`) into a neutral event. The
/// push omits `terminal_id`, `workspace_name`, and `title`, so those stay `None`.
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
        title: None,
        synthetic: false,
    })
}

/// Parse a `pane_closed` / `pane_exited` payload for the herdr `pane_id` (either
/// top-level in `data` or nested under `data.pane.pane_id`) and resolve it to a
/// tracked neutral id. A pane we never registered is [`Pushed::Ignored`].
fn parse_pane_gone(data: Option<&Value>, panes: &HerdrPaneRegistry) -> Pushed {
    let Some(data) = data else {
        return Pushed::Ignored;
    };
    let herdr_pane_id = data.get("pane_id").and_then(Value::as_str).or_else(|| {
        data.get("pane")
            .and_then(|p| p.get("pane_id"))
            .and_then(Value::as_str)
    });
    match herdr_pane_id.and_then(|pid| panes.id_for_herdr_pane(pid)) {
        Some(id) => Pushed::PaneGone(id),
        None => Pushed::Ignored,
    }
}

/// Extract the herdr `pane_id` from a `pane_created` payload. The wire capture
/// nests the full pane object under `data.pane` (`data.pane.pane_id`); a
/// top-level `data.pane_id` is also accepted for tolerance. `None` when no id is
/// parseable (genuine new panes always carry one per the wire capture).
fn pane_created_id(data: Option<&Value>) -> Option<String> {
    let data = data?;
    data.get("pane")
        .and_then(|p| p.get("pane_id"))
        .and_then(Value::as_str)
        .or_else(|| data.get("pane_id").and_then(Value::as_str))
        .map(str::to_string)
}

/// Decide whether a `pane_created` push should trigger a Grow reconnect.
///
/// herdr 0.7.1 replays a `pane_created` for **every** already-subscribed pane
/// immediately after the `subscription_started` ack (snapshot semantics,
/// wire-confirmed 2026-07-15). Those carry a `pane_id` already in the
/// connect-time set and must be ignored — otherwise each replay triggers a Grow
/// reconnect, whose fresh subscription replays them again: an infinite loop.
/// Only an id **absent** from `known` is a genuine new pane. A push with no
/// parseable id can't be matched, so err toward Grow (safe: genuine new panes
/// always carry the id, so this path should not fire in practice).
fn pane_created_grows(pane_id: Option<&str>, known: &HashSet<&str>) -> bool {
    match pane_id {
        Some(pid) => !known.contains(pid),
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

/// Build the `events.subscribe` request line (JSON + trailing newline).
///
/// The set is fixed at connect time and carried in a single `subscriptions` array:
/// one per-pane `pane.agent_status_changed` entry (each with its `pane_id`) plus
/// bare `pane.created` / `pane.closed` entries.
fn subscribe_request_line(pane_ids: &[String]) -> Result<String> {
    let mut subscriptions: Vec<Value> = Vec::with_capacity(pane_ids.len() + 2);
    for pane_id in pane_ids {
        subscriptions.push(serde_json::json!({
            "type": SUBSCRIBE_AGENT_STATUS_TYPE,
            "pane_id": pane_id,
        }));
    }
    subscriptions.push(serde_json::json!({ "type": SUBSCRIBE_PANE_CREATED_TYPE }));
    subscriptions.push(serde_json::json!({ "type": SUBSCRIBE_PANE_CLOSED_TYPE }));

    let params = serde_json::json!({ "subscriptions": subscriptions });
    let req = ApiRequest::new("muxrd-events-subscribe", "events.subscribe", params);
    let mut line = serde_json::to_string(&req).context("serialize events.subscribe request")?;
    line.push('\n');
    Ok(line)
}

/// Next backoff: double, capped at [`MAX_BACKOFF`].
fn next_backoff(prev: Duration) -> Duration {
    (prev * 2).min(MAX_BACKOFF)
}

/// Lock a mutex, recovering the guard if a previous holder panicked. The table
/// holds only a plain map, so a poisoned lock leaves consistent data.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── Bounded line reader ──────────────────────────────────────────────────────────

/// Read one `\n`-terminated line into `out` (newline stripped), bounded at
/// [`MAX_EVENT_LINE_BYTES`]. Returns the number of bytes placed in `out`; `Ok(0)`
/// signals EOF with nothing buffered. Errors if the cap is reached without a
/// newline (hostile / malformed stream) or on non-UTF-8 input.
async fn read_event_line<R>(reader: &mut R, out: &mut String) -> std::io::Result<usize>
where
    R: AsyncBufReadExt + Unpin,
{
    out.clear();
    let mut raw: Vec<u8> = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            break; // EOF
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                raw.extend_from_slice(&chunk[..pos]); // exclude the newline
                reader.consume(pos + 1);
                break;
            }
            None => {
                let n = chunk.len();
                raw.extend_from_slice(chunk);
                reader.consume(n);
                if raw.len() > MAX_EVENT_LINE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "herdr event line exceeded size cap without a newline",
                    ));
                }
            }
        }
    }
    if raw.is_empty() {
        return Ok(0);
    }
    match String::from_utf8(raw) {
        Ok(s) => {
            *out = s;
            Ok(out.len())
        }
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── pushed-event parsing (live `{"event","data"}` envelope) ──────────────

    #[test]
    fn parses_agent_status_dot_envelope() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{
            "event": "pane.agent_status_changed",
            "data": {
                "agent": "claude",
                "agent_status": "blocked",
                "pane_id": "w1:p1",
                "workspace_id": "w1"
            }
        }"#;
        match classify_pushed(line, &panes) {
            Pushed::AgentStatus(ev) => {
                assert_eq!(ev.status, AgentStatus::Blocked);
                assert_eq!(ev.workspace_id, "w1");
                // The push carries no name/title/terminal_id.
                assert!(ev.workspace_name.is_none());
                assert!(ev.title.is_none());
                assert!(!ev.synthetic, "a pushed event is never synthetic");
                assert_eq!(panes.herdr_pane_id(ev.pane).as_deref(), Some("w1:p1"));
            }
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
        // herdr pushes pane_created with the full pane object under data.pane.
        let line = r#"{
            "event": "pane_created",
            "data": { "pane": { "pane_id": "w1:p2", "terminal_id": "t2", "workspace_id": "w1" } }
        }"#;
        match classify_pushed(line, &panes) {
            // The nested pane_id must be extracted for replay detection.
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

    // ── pane_created replay suppression (the Grow-loop fix) ───────────────────

    #[test]
    fn replayed_pane_created_for_known_pane_does_not_grow() {
        // herdr replays a pane_created for every already-subscribed pane after the
        // ack; a known id must NOT trigger a Grow reconnect.
        let known: HashSet<&str> = ["w1:p1", "w1:p2"].into_iter().collect();
        assert!(!pane_created_grows(Some("w1:p1"), &known));
    }

    #[test]
    fn genuinely_new_pane_created_grows() {
        let known: HashSet<&str> = ["w1:p1"].into_iter().collect();
        assert!(pane_created_grows(Some("w1:p2"), &known));
    }

    #[test]
    fn pane_created_with_no_id_grows() {
        // Can't tell whether it's a replay; genuine new panes always carry an id,
        // so this path should not fire in practice — err toward Grow.
        let known: HashSet<&str> = ["w1:p1"].into_iter().collect();
        assert!(pane_created_grows(None, &known));
    }

    #[test]
    fn classifies_pane_closed_to_tracked_neutral_id() {
        let panes = HerdrPaneRegistry::new();
        let id = panes.assign_or_get("w1:p1", "t1");
        let line = r#"{"event":"pane.closed","data":{"pane_id":"w1:p1"}}"#;
        match classify_pushed(line, &panes) {
            Pushed::PaneGone(gone) => assert_eq!(gone, id),
            _ => panic!("expected PaneGone"),
        }
    }

    #[test]
    fn classifies_pane_exited_nested_pane_id() {
        let panes = HerdrPaneRegistry::new();
        let id = panes.assign_or_get("w1:p5", "t5");
        let line = r#"{"event":"pane_exited","data":{"pane":{"pane_id":"w1:p5"}}}"#;
        match classify_pushed(line, &panes) {
            Pushed::PaneGone(gone) => assert_eq!(gone, id),
            _ => panic!("expected PaneGone"),
        }
    }

    #[test]
    fn pane_closed_for_untracked_pane_is_ignored() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{"event":"pane.closed","data":{"pane_id":"never-seen"}}"#;
        assert!(matches!(classify_pushed(line, &panes), Pushed::Ignored));
    }

    // ── ignored lines ────────────────────────────────────────────────────────

    #[test]
    fn subscription_ack_is_recognised_and_not_a_pushed_event() {
        let ack = r#"{"id":"muxrd-events-subscribe","result":{"type":"subscription_started"}}"#;
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

    // ── subscribe request shape (live `subscriptions` schema) ─────────────────

    #[test]
    fn subscribe_request_uses_subscriptions_array_with_per_pane_entries() {
        let pane_ids = vec!["w1:p1".to_string(), "w1:p2".to_string()];
        let line = subscribe_request_line(&pane_ids).expect("build subscribe line");
        assert!(line.ends_with('\n'), "request must be newline-terminated");
        let value: Value =
            serde_json::from_str(line.trim_end()).expect("subscribe line is valid JSON");
        assert_eq!(value["method"], "events.subscribe");

        let subs = value["params"]["subscriptions"]
            .as_array()
            .expect("params.subscriptions must be an array");
        // Two per-pane agent-status entries + pane.created + pane.closed.
        assert_eq!(subs.len(), 4);
        assert_eq!(subs[0]["type"], SUBSCRIBE_AGENT_STATUS_TYPE);
        assert_eq!(subs[0]["pane_id"], "w1:p1");
        assert_eq!(subs[1]["type"], SUBSCRIBE_AGENT_STATUS_TYPE);
        assert_eq!(subs[1]["pane_id"], "w1:p2");
        assert_eq!(subs[2]["type"], SUBSCRIBE_PANE_CREATED_TYPE);
        assert!(subs[2].get("pane_id").is_none(), "pane.created is bare");
        assert_eq!(subs[3]["type"], SUBSCRIBE_PANE_CLOSED_TYPE);
        assert_eq!(SUBSCRIBE_AGENT_STATUS_TYPE, "pane.agent_status_changed");
    }

    #[test]
    fn subscribe_request_with_no_panes_still_subscribes_to_lifecycle() {
        let line = subscribe_request_line(&[]).expect("build subscribe line");
        let value: Value = serde_json::from_str(line.trim_end()).expect("valid JSON");
        let subs = value["params"]["subscriptions"].as_array().unwrap();
        assert_eq!(subs.len(), 2, "just pane.created + pane.closed");
        assert_eq!(subs[0]["type"], SUBSCRIBE_PANE_CREATED_TYPE);
        assert_eq!(subs[1]["type"], SUBSCRIBE_PANE_CLOSED_TYPE);
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

    // ── backoff math ─────────────────────────────────────────────────────────

    #[test]
    fn backoff_doubles_then_caps_at_sixty() {
        let mut seq = Vec::new();
        let mut b = INITIAL_BACKOFF;
        for _ in 0..8 {
            seq.push(b.as_secs());
            b = next_backoff(b);
        }
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn backoff_never_exceeds_cap() {
        let mut b = MAX_BACKOFF;
        for _ in 0..5 {
            b = next_backoff(b);
            assert_eq!(b, MAX_BACKOFF);
        }
    }

    // ── bounded line reader ──────────────────────────────────────────────────

    #[tokio::test]
    async fn reads_newline_delimited_lines() {
        use std::io::Cursor;
        let data = Cursor::new(b"first\nsecond\n".to_vec());
        let mut reader = BufReader::new(data);
        let mut buf = String::new();
        assert_eq!(read_event_line(&mut reader, &mut buf).await.unwrap(), 5);
        assert_eq!(buf, "first");
        assert_eq!(read_event_line(&mut reader, &mut buf).await.unwrap(), 6);
        assert_eq!(buf, "second");
        // EOF.
        assert_eq!(read_event_line(&mut reader, &mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn read_rejects_oversize_line_without_newline() {
        use std::io::Cursor;
        let data = Cursor::new(vec![b'x'; MAX_EVENT_LINE_BYTES + 16]);
        let mut reader = BufReader::new(data);
        let mut buf = String::new();
        let err = read_event_line(&mut reader, &mut buf)
            .await
            .expect_err("oversize line must error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
