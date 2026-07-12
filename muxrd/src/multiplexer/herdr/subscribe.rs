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
//! this streaming reader. The only blocking work it does (the post-reconnect
//! resync) is routed through `HerdrControl` on the blocking pool.
//!
//! ## Reconnect & loss
//!
//! herdr documents no reconnect semantics for the subscription, so the kernel
//! owns its own exponential backoff ([`INITIAL_BACKOFF`] → [`MAX_BACKOFF`], reset
//! after a stable connection ≥ [`STABLE_THRESHOLD`]) and treats the subscription
//! as **lossy**: after every (re)connect it *resyncs* current pane agent states
//! via `HerdrControl` and emits synthetic [`AgentStatus::Blocked`]/[`AgentStatus::Done`]
//! events (`synthetic: true`) so a consumer that missed a transition during the
//! gap still learns which panes currently need attention.
//!
//! ## Read-only
//!
//! This task never *mutates* herdr — no `workspace.focus`, no lifecycle calls. It
//! subscribes and lists. Pane-id translation goes through the shared
//! [`HerdrPaneRegistry`] using a **non-mutating** lookup
//! ([`HerdrPaneRegistry::id_for_herdr_pane`]) so a pushed event that omits
//! `terminal_id` can never clobber a live pane's relay attach key.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
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

/// Hard ceiling on a single pushed-event line, mirroring the control plane's
/// [`MAX_RESPONSE_BYTES`](super::control::MAX_RESPONSE_BYTES) defence: a peer
/// streaming bytes without a newline must not grow the line buffer unbounded.
/// Agent-status events are tiny; 1 MiB is generous while still bounding a hostile
/// stream.
const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;

/// herdr event type in the **subscription request** (dot form).
const SUBSCRIBE_EVENT_TYPE: &str = "pane.agent_status_changed";

/// herdr event type on **pushed events** (underscore form — the shape code must
/// match; verified against herdr's public schema).
const PUSHED_EVENT_TYPE: &str = "pane_agent_status_changed";

// ── Public entry point ──────────────────────────────────────────────────────────

/// Spawn the herdr event kernel as a background Tokio task.
///
/// Publishes [`MuxEvent`]s onto `bus`; exits cleanly when `shutdown` is set to
/// `true`. Shares the backend's [`HerdrPaneRegistry`] / [`HerdrTabRegistry`]
/// `Arc`s so translated pane ids stay identical to those handed out by layout
/// polls, and builds its own long-lived subscription socket (plus a per-request
/// `HerdrControl` over the same socket path for resync).
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

/// Outcome of a single subscription session (one connected socket lifetime).
enum SessionOutcome {
    /// Shutdown was requested — the outer loop must exit.
    Shutdown,
    /// The session ended (connect failure, EOF, or read error) — reconnect.
    Ended,
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
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let started = Instant::now();
        match run_session(&api_socket, &panes, &control, &bus, &mut shutdown).await {
            SessionOutcome::Shutdown => break,
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

/// One subscription session: connect, subscribe, resync, then read pushed events
/// until EOF / error / shutdown.
async fn run_session(
    api_socket: &Path,
    panes: &Arc<HerdrPaneRegistry>,
    control: &Arc<HerdrControl>,
    bus: &EventBus,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionOutcome {
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

    let line = match subscribe_request_line() {
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
    log::info!("herdr event kernel: subscribed to {SUBSCRIBE_EVENT_TYPE}");

    // Lossy subscription: re-observe current state on every (re)connect.
    let resynced = resync(Arc::clone(control), Arc::clone(panes), bus.clone()).await;
    log::info!("herdr event kernel: resync emitted {resynced} synthetic event(s)");

    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();
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
                    Ok(_) => {
                        if let Some(ev) = parse_pushed_event(buf.trim_end(), panes) {
                            log::debug!(
                                "herdr event kernel: {:?} pane={} ws={}",
                                ev.status,
                                ev.pane,
                                ev.workspace_id
                            );
                            // No receivers yet (task 05 is the first consumer) is fine.
                            let _ = bus.send(MuxEvent::AgentStatusChanged(ev));
                        }
                    }
                    Err(e) => {
                        log::info!("herdr event kernel: read error: {e}");
                        return SessionOutcome::Ended;
                    }
                }
            }
        }
    }
}

/// Re-observe current pane agent states after a (re)connect and publish synthetic
/// `Blocked`/`Done` events. Returns the number of synthetic events emitted.
///
/// Runs the blocking `HerdrControl` list calls on the blocking pool (they use the
/// synchronous connection-per-request transport, per the `spawn_blocking`
/// discipline for short herdr calls).
async fn resync(control: Arc<HerdrControl>, panes: Arc<HerdrPaneRegistry>, bus: EventBus) -> usize {
    let collected =
        tokio::task::spawn_blocking(move || collect_resync_events(&control, &panes)).await;
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
/// those currently `Blocked`/`Done`. `list_panes` carries each pane's real
/// `terminal_id`, so `assign_or_get` here refreshes the registry correctly.
fn collect_resync_events(
    control: &HerdrControl,
    panes: &HerdrPaneRegistry,
) -> Result<Vec<AgentStatusChanged>> {
    let workspaces = control
        .list_workspaces()
        .context("resync: workspace.list")?;
    let mut events = Vec::new();
    for ws in &workspaces {
        let ws_panes = control
            .list_panes(&ws.workspace_id)
            .with_context(|| format!("resync: pane.list for {}", ws.workspace_id))?;
        for pane in ws_panes {
            if matches!(
                pane.agent_status,
                HerdrAgentStatus::Blocked | HerdrAgentStatus::Done
            ) {
                let id = panes.assign_or_get(&pane.pane_id, &pane.terminal_id);
                events.push(AgentStatusChanged {
                    pane: id,
                    workspace_id: ws.workspace_id.clone(),
                    workspace_name: Some(ws.label.clone()),
                    status: map_status(pane.agent_status),
                    title: pane.title.clone(),
                    synthetic: true,
                });
            }
        }
    }
    Ok(events)
}

// ── Parsing ─────────────────────────────────────────────────────────────────────

/// A pushed `pane_agent_status_changed` event. Defensive: unknown fields are
/// ignored (no `deny_unknown_fields`) and every non-required field is optional so
/// a herdr schema drift degrades to "some fields missing", never a hard failure.
#[derive(Debug, Deserialize)]
struct RawPushedEvent {
    pane_id: String,
    #[serde(default)]
    terminal_id: Option<String>,
    workspace_id: String,
    #[serde(default)]
    workspace_name: Option<String>,
    agent_status: HerdrAgentStatus,
    #[serde(default)]
    title: Option<String>,
}

/// Parse one newline-stripped stream line into a neutral [`AgentStatusChanged`],
/// or `None` for anything that is not a pushed agent-status event (the subscribe
/// ack, the dot-form request schema, unknown event types, or malformed lines) —
/// all ignored without error.
fn parse_pushed_event(line: &str, panes: &HerdrPaneRegistry) -> Option<AgentStatusChanged> {
    if line.is_empty() {
        return None;
    }
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            log::trace!("herdr event kernel: ignoring non-JSON line");
            return None;
        }
    };
    let ty = value.get("type").and_then(serde_json::Value::as_str)?;
    if ty != PUSHED_EVENT_TYPE {
        // Subscribe ack, dot-form `pane.agent_status_changed`, or an event family
        // we do not consume — silently ignored.
        log::trace!("herdr event kernel: ignoring line of type {ty}");
        return None;
    }
    let raw: RawPushedEvent = match serde_json::from_value(value) {
        Ok(r) => r,
        Err(e) => {
            log::debug!("herdr event kernel: malformed {PUSHED_EVENT_TYPE} event ignored: {e}");
            return None;
        }
    };
    let pane = translate_pane(panes, &raw.pane_id, raw.terminal_id.as_deref());
    Some(AgentStatusChanged {
        pane,
        workspace_id: raw.workspace_id,
        workspace_name: raw.workspace_name,
        status: map_status(raw.agent_status),
        title: raw.title,
        synthetic: false,
    })
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

/// Build the `events.subscribe` request line (JSON + trailing newline) that
/// subscribes to `pane.agent_status_changed` (dot form in the request).
fn subscribe_request_line() -> Result<String> {
    let params = serde_json::json!({
        "events": [ { "type": SUBSCRIBE_EVENT_TYPE } ],
    });
    let req = ApiRequest::new("muxrd-events-subscribe", "events.subscribe", params);
    let mut line = serde_json::to_string(&req).context("serialize events.subscribe request")?;
    line.push('\n');
    Ok(line)
}

/// Next backoff: double, capped at [`MAX_BACKOFF`].
fn next_backoff(prev: Duration) -> Duration {
    (prev * 2).min(MAX_BACKOFF)
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

    // ── pushed-event parsing (acceptance 2a/2b) ──────────────────────────────

    #[test]
    fn parses_full_underscore_event() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{
            "type": "pane_agent_status_changed",
            "pane_id": "pane-abc",
            "terminal_id": "term-xyz",
            "workspace_id": "ws-1",
            "workspace_name": "main",
            "agent_status": "blocked",
            "title": "claude"
        }"#;
        let ev = parse_pushed_event(line, &panes).expect("a full event must parse");
        assert_eq!(ev.status, AgentStatus::Blocked);
        assert_eq!(ev.workspace_id, "ws-1");
        assert_eq!(ev.workspace_name.as_deref(), Some("main"));
        assert_eq!(ev.title.as_deref(), Some("claude"));
        assert!(!ev.synthetic, "a pushed event is never synthetic");
        // pane id was registered and round-trips to the herdr id + terminal id.
        assert_eq!(panes.herdr_pane_id(ev.pane).as_deref(), Some("pane-abc"));
        assert_eq!(panes.terminal_id(ev.pane).as_deref(), Some("term-xyz"));
    }

    #[test]
    fn parses_minimal_underscore_event_without_optional_fields() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{
            "type": "pane_agent_status_changed",
            "pane_id": "pane-1",
            "workspace_id": "ws-9",
            "agent_status": "done"
        }"#;
        let ev = parse_pushed_event(line, &panes).expect("required-only event must parse");
        assert_eq!(ev.status, AgentStatus::Done);
        assert_eq!(ev.workspace_id, "ws-9");
        assert!(ev.workspace_name.is_none());
        assert!(ev.title.is_none());
        assert!(ev.pane >= 1, "a neutral id was assigned");
    }

    #[test]
    fn tolerates_unknown_extra_fields() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{
            "type": "pane_agent_status_changed",
            "pane_id": "p",
            "workspace_id": "w",
            "agent_status": "working",
            "display_agent": "claude-code",
            "custom_status": "thinking",
            "state_labels": {"k":"v"},
            "future_field": 42
        }"#;
        let ev = parse_pushed_event(line, &panes).expect("unknown fields must be ignored");
        assert_eq!(ev.status, AgentStatus::Working);
    }

    // ── ignored lines (acceptance 2b) ────────────────────────────────────────

    #[test]
    fn ignores_dot_form_request_schema() {
        let panes = HerdrPaneRegistry::new();
        // The dot form is the *request* type — it must NEVER be treated as a
        // pushed event.
        let line = r#"{"type":"pane.agent_status_changed","pane_id":"p","agent_status":"blocked"}"#;
        assert!(parse_pushed_event(line, &panes).is_none());
    }

    #[test]
    fn ignores_unknown_event_type() {
        let panes = HerdrPaneRegistry::new();
        let line = r#"{"type":"workspace_focused","workspace_id":"ws-1"}"#;
        assert!(parse_pushed_event(line, &panes).is_none());
    }

    #[test]
    fn ignores_subscribe_ack_and_error_responses() {
        let panes = HerdrPaneRegistry::new();
        // A JSON-API response envelope (no top-level "type") — e.g. the subscribe ack.
        assert!(
            parse_pushed_event(
                r#"{"id":"muxrd-events-subscribe","result":{"type":"ok"}}"#,
                &panes
            )
            .is_none()
        );
        assert!(
            parse_pushed_event(
                r#"{"id":"x","error":{"code":"bad","message":"no"}}"#,
                &panes
            )
            .is_none()
        );
    }

    #[test]
    fn ignores_malformed_and_empty_lines() {
        let panes = HerdrPaneRegistry::new();
        assert!(parse_pushed_event("", &panes).is_none());
        assert!(parse_pushed_event("not json at all", &panes).is_none());
        assert!(parse_pushed_event("{ broken", &panes).is_none());
        // Right type but missing a required field (workspace_id) → ignored.
        assert!(
            parse_pushed_event(
                r#"{"type":"pane_agent_status_changed","pane_id":"p","agent_status":"idle"}"#,
                &panes
            )
            .is_none()
        );
    }

    // ── non-clobbering pane translation (the terminal_id-safety fix) ──────────

    #[test]
    fn translate_does_not_clobber_existing_terminal_id() {
        let panes = HerdrPaneRegistry::new();
        // A layout poll registered this pane with its real terminal_id.
        let id = panes.assign_or_get("pane-x", "term-real");
        // A pushed event for the same pane arrives WITHOUT a terminal_id.
        let line = r#"{
            "type": "pane_agent_status_changed",
            "pane_id": "pane-x",
            "workspace_id": "ws-1",
            "agent_status": "blocked"
        }"#;
        let ev = parse_pushed_event(line, &panes).expect("event must parse");
        assert_eq!(ev.pane, id, "same pane must resolve to the same neutral id");
        assert_eq!(
            panes.terminal_id(id).as_deref(),
            Some("term-real"),
            "the live relay terminal_id must NOT be clobbered by the event"
        );
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

    // ── subscribe request shape ──────────────────────────────────────────────

    #[test]
    fn subscribe_request_targets_events_subscribe_with_dot_type() {
        let line = subscribe_request_line().expect("build subscribe line");
        assert!(line.ends_with('\n'), "request must be newline-terminated");
        let value: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("subscribe line is valid JSON");
        assert_eq!(value["method"], "events.subscribe");
        assert_eq!(value["params"]["events"][0]["type"], SUBSCRIBE_EVENT_TYPE);
        assert_eq!(SUBSCRIBE_EVENT_TYPE, "pane.agent_status_changed");
    }

    // ── backoff math (acceptance 2c) ─────────────────────────────────────────

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
