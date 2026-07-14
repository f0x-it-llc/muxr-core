//! sender — the outbound push notifier (muxrd's first-ever outbound HTTP call).
//!
//! ## What this is
//!
//! A background task that consumes the internal event bus
//! ([`events`](crate::multiplexer::events)), filters herdr agent
//! `Blocked`/`Done` transitions, debounces repeats, and POSTs a *minimal*
//! payload to the configured push relay (`{relay}/v1/notify`) for every
//! registered device. It prunes devices the relay reports gone (404/410).
//!
//! ## Privacy stance (see `RESEARCH.md` §3)
//!
//! Notification content transits our relay VM **and** Google/Apple, so payloads
//! carry only minimal text — a workspace label at most (`normal` verbosity) or a
//! fixed "Attention needed" string (`generic`). Pane titles / terminal content
//! are **never** included; the app fetches real detail from muxrd over gRPC on
//! tap. This is also strictly **opt-in**: when zero devices are registered the
//! task performs *no outbound traffic at all* (the zero-devices short-circuit).
//!
//! ## Testability
//!
//! All relay I/O goes through the [`RelayTransport`] trait, so the debounce /
//! payload / fan-out / prune logic is unit-tested against an in-memory mock with
//! no network. The concrete [`ReqwestRelay`] is the only piece that touches the
//! wire.

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;

use crate::multiplexer::events::{AgentStatus, AgentStatusChanged, EventBus, MuxEvent};
use crate::notify::devices::{HANDLE_PREFIX_LEN, PushDeviceStore};

/// Per-(pane, kind) debounce window: suppress an identical trigger within this
/// span. A `Blocked`→`Done` transition uses a *different* key and is never
/// suppressed by a recent `Blocked`.
const DEBOUNCE_WINDOW: Duration = Duration::from_secs(30);

/// Per-request timeout for a single relay POST.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Delay before the single retry on a transient (5xx / network) failure.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Relay path appended to the configured base URL.
const NOTIFY_PATH: &str = "/v1/notify";

/// The two agent transitions that trigger a notification. A neutral local enum
/// (rather than reusing [`AgentStatus`], which carries non-trigger variants and
/// is deliberately not `Hash`) so it can key the debounce map directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TriggerKind {
    Blocked,
    Done,
}

impl TriggerKind {
    /// The relay `kind` discriminator carried in the payload (stable wire string).
    fn wire(self) -> &'static str {
        match self {
            TriggerKind::Blocked => "agent_blocked",
            TriggerKind::Done => "agent_done",
        }
    }
}

/// Map an [`AgentStatus`] to its notification trigger, if any. Only `Blocked`
/// and `Done` trigger a push; every other status is a no-op.
fn trigger_kind(status: AgentStatus) -> Option<TriggerKind> {
    match status {
        AgentStatus::Blocked => Some(TriggerKind::Blocked),
        AgentStatus::Done => Some(TriggerKind::Done),
        AgentStatus::Idle | AgentStatus::Working | AgentStatus::Unknown => None,
    }
}

/// The minimal, privacy-preserving notification content for one event. The
/// per-device `push_handle` is *not* part of this — it is supplied by the
/// transport at send time so one built `NotifyContent` fans out to every device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyContent {
    /// Relay `kind` discriminator (`"agent_blocked"` / `"agent_done"`).
    pub kind: String,
    /// Short notification title (always `"Muxr"` today).
    pub title: Option<String>,
    /// Notification body — a workspace label (`normal`) or a fixed generic string.
    pub body: Option<String>,
}

/// The outcome of a single relay POST for one device handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Relay accepted the notify (`204`).
    Ok,
    /// Relay reports the handle unknown/pruned (`404`/`410`) — prune the device.
    Gone,
    /// Relay rate-limited this send (`429`) — drop it, do not retry.
    RateLimited,
    /// Transient failure (`5xx` / network / timeout / out-of-contract status) —
    /// eligible for one retry, then dropped.
    Transient,
}

/// The relay transport seam. Abstracted so the notifier's logic is testable
/// without network access (see the module-level tests).
pub trait RelayTransport: Send + Sync + 'static {
    /// POST one notify to the relay for a single device `handle`.
    ///
    /// Returns an explicit [`SendOutcome`] rather than a `Result` so every wire
    /// status the notifier cares about (accept / gone / rate-limited /
    /// transient) is modelled as data — there is no error to bubble up, only a
    /// per-device disposition.
    fn send(
        &self,
        handle: &str,
        content: &NotifyContent,
    ) -> impl Future<Output = SendOutcome> + Send;
}

// ─── Concrete reqwest transport ────────────────────────────────────────────────

/// The production [`RelayTransport`]: a single reused `reqwest` client (rustls)
/// POSTing JSON to `{relay}/v1/notify`. No proxy configuration is applied — the
/// relay URL is contacted directly.
pub struct ReqwestRelay {
    client: reqwest::Client,
    /// Fully-joined notify endpoint (`{relay}{NOTIFY_PATH}`).
    notify_url: String,
}

impl ReqwestRelay {
    /// Build the client once. `relay_base` is the operator-configured relay URL
    /// (already validated as an `http`/`https` URL by `config::resolve`).
    pub fn new(relay_base: &str) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .no_proxy()
            .build()?;
        let notify_url = format!("{}{NOTIFY_PATH}", relay_base.trim_end_matches('/'));
        Ok(Self { client, notify_url })
    }
}

/// Wire body for `POST /v1/notify` — borrows so a built [`NotifyContent`] fans
/// out to many handles without cloning. `title`/`body` are omitted when absent.
#[derive(serde::Serialize)]
struct NotifyRequest<'a> {
    push_handle: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
}

impl RelayTransport for ReqwestRelay {
    fn send(
        &self,
        handle: &str,
        content: &NotifyContent,
    ) -> impl Future<Output = SendOutcome> + Send {
        let req = NotifyRequest {
            push_handle: handle,
            kind: &content.kind,
            title: content.title.as_deref(),
            body: content.body.as_deref(),
        };
        let fut = self.client.post(&self.notify_url).json(&req).send();
        async move {
            match fut.await {
                Ok(resp) => match resp.status().as_u16() {
                    204 => SendOutcome::Ok,
                    404 | 410 => SendOutcome::Gone,
                    429 => SendOutcome::RateLimited,
                    other => {
                        // 5xx and any out-of-contract status: log + treat as
                        // transient (bounded single retry, never queued).
                        log::warn!("notify: relay returned unexpected status {other}");
                        SendOutcome::Transient
                    }
                },
                Err(e) => {
                    log::warn!("notify: relay POST failed: {e}");
                    SendOutcome::Transient
                }
            }
        }
    }
}

// ─── Debounce ──────────────────────────────────────────────────────────────────

/// Per-(pane, kind) monotonic-clock debouncer. Opportunistically pruned on every
/// decision so the map never grows unbounded for churny panes.
struct Debouncer {
    last_sent: HashMap<(u32, TriggerKind), Instant>,
    window: Duration,
}

impl Debouncer {
    fn new(window: Duration) -> Self {
        Self {
            last_sent: HashMap::new(),
            window,
        }
    }

    /// Decide whether a `(pane, kind)` trigger should send `now`. Records the
    /// send time when it returns `true`. Repeats inside `window` are suppressed;
    /// a different `kind` (the `Blocked`→`Done` transition) is never suppressed
    /// by a recent send of the other kind.
    fn should_send(&mut self, pane: u32, kind: TriggerKind, now: Instant) -> bool {
        // Opportunistic prune: drop entries older than the window.
        self.last_sent
            .retain(|_, &mut t| now.duration_since(t) < self.window);
        match self.last_sent.get(&(pane, kind)) {
            Some(&t) if now.duration_since(t) < self.window => false,
            _ => {
                self.last_sent.insert((pane, kind), now);
                true
            }
        }
    }
}

// ─── Payload building ──────────────────────────────────────────────────────────

/// Build the notification content for `kind`. `normal` verbosity carries a
/// workspace label (name when known, else the raw id); `generic` carries a fixed
/// string. The pane/agent `title` on the event is **never** used — see the
/// module privacy note.
fn content_for(kind: TriggerKind, ev: &AgentStatusChanged, verbosity: &str) -> NotifyContent {
    let body = if verbosity == "generic" {
        "Attention needed".to_owned()
    } else {
        let label = ev
            .workspace_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&ev.workspace_id);
        match kind {
            TriggerKind::Blocked => format!("Agent blocked in {label}"),
            TriggerKind::Done => format!("Agent finished in {label}"),
        }
    };
    NotifyContent {
        kind: kind.wire().to_owned(),
        title: Some("Muxr".to_owned()),
        body: Some(body),
    }
}

/// Pure trigger-filter + payload builder — the single tested entrypoint that
/// mirrors the production filter/skip policy `handle_event` applies inline
/// (which is split so the zero-device check can run *before* any content is
/// built). Returns `None` for non-trigger statuses and for **synthetic `Done`**
/// events: a post-reconnect resync re-observes current state, and replaying a
/// `Done` there risks a duplicate "finished" notification for an agent that
/// finished before the client ever disconnected. Synthetic `Blocked` *is* sent —
/// a still-blocked agent genuinely still needs the user.
#[cfg(test)]
fn build_content(ev: &AgentStatusChanged, verbosity: &str) -> Option<NotifyContent> {
    let kind = trigger_kind(ev.status)?;
    if ev.synthetic && kind == TriggerKind::Done {
        return None;
    }
    Some(content_for(kind, ev, verbosity))
}

// ─── Send fan-out ──────────────────────────────────────────────────────────────

/// Leading chars of a handle safe to log (never the full bearer capability).
fn handle_prefix(handle: &str) -> &str {
    &handle[..handle.len().min(HANDLE_PREFIX_LEN)]
}

/// Send to one handle, retrying **once** after [`RETRY_DELAY`] on a transient
/// failure. Never queues beyond this single retry.
async fn send_with_retry<T: RelayTransport>(
    transport: &T,
    handle: &str,
    content: &NotifyContent,
) -> SendOutcome {
    match transport.send(handle, content).await {
        SendOutcome::Transient => {
            tokio::time::sleep(RETRY_DELAY).await;
            transport.send(handle, content).await
        }
        other => other,
    }
}

/// Handle one bus event: filter → zero-device short-circuit → debounce → build →
/// fan-out. Reads the device store **fresh** every time (muxrctl may mutate the
/// file concurrently — no cache).
async fn handle_event<T: RelayTransport>(
    ev: &AgentStatusChanged,
    verbosity: &str,
    debouncer: &mut Debouncer,
    store: &PushDeviceStore,
    transport: &T,
) {
    // 1. Trigger filter (+ synthetic-Done skip). Cheap, no I/O.
    let kind = match trigger_kind(ev.status) {
        Some(k) => k,
        None => return,
    };
    if ev.synthetic && kind == TriggerKind::Done {
        log::debug!("notify: skipping synthetic Done for pane {}", ev.pane);
        return;
    }

    // 2. Zero-devices short-circuit — BEFORE building or sending anything, so an
    //    empty registry produces zero outbound traffic (the opt-in guarantee).
    //    The store is plain synchronous `std::fs` (+ an advisory file lock), so
    //    read it on the blocking pool — never on the async runtime — matching
    //    every other caller (grpc::push_ops, muxrctl runner).
    let devices = {
        let store = store.clone();
        match tokio::task::spawn_blocking(move || store.list()).await {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => {
                log::warn!("notify: failed to read push-device registry: {e:#}");
                return;
            }
            Err(e) => {
                log::warn!("notify: push-device list task panicked: {e}");
                return;
            }
        }
    };
    if devices.is_empty() {
        return;
    }

    // 3. Debounce (only after we know there is a real device to notify, so the
    //    debounce state reflects actual send decisions).
    if !debouncer.should_send(ev.pane, kind, Instant::now()) {
        log::debug!(
            "notify: debounced {} for pane {} (within {}s)",
            kind.wire(),
            ev.pane,
            DEBOUNCE_WINDOW.as_secs()
        );
        return;
    }

    // 4. Build once, fan out sequentially (device counts are tiny).
    let content = content_for(kind, ev, verbosity);
    log::info!(
        "notify: sending {} for pane {} to {} device(s)",
        kind.wire(),
        ev.pane,
        devices.len()
    );
    for device in &devices {
        match send_with_retry(transport, &device.push_handle, &content).await {
            SendOutcome::Ok => {
                log::debug!(
                    "notify: delivered to {}… ({})",
                    handle_prefix(&device.push_handle),
                    device.device_name
                );
            }
            SendOutcome::Gone => {
                log::info!(
                    "notify: relay reports handle {}… gone — pruning device {}",
                    handle_prefix(&device.push_handle),
                    device.device_name
                );
                // Prune on the blocking pool (synchronous std::fs + file lock).
                let store = store.clone();
                let handle = device.push_handle.clone();
                match tokio::task::spawn_blocking(move || store.remove_by_handle(&handle)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => log::warn!("notify: failed to prune gone device: {e:#}"),
                    Err(e) => log::warn!("notify: prune task panicked: {e}"),
                }
            }
            SendOutcome::RateLimited => {
                log::warn!(
                    "notify: relay rate-limited handle {}… — dropping this send",
                    handle_prefix(&device.push_handle)
                );
            }
            SendOutcome::Transient => {
                log::warn!(
                    "notify: transient failure sending to {}… after retry — dropping",
                    handle_prefix(&device.push_handle)
                );
            }
        }
    }
}

// ─── Task loop + spawn ─────────────────────────────────────────────────────────

/// The notifier task: consume the event bus until shutdown. Tolerates
/// [`RecvError::Lagged`] (agent status is a level, not an edge — the next event
/// or a resync re-establishes truth).
async fn run_notifier<T: RelayTransport>(
    mut bus_rx: broadcast::Receiver<MuxEvent>,
    mut shutdown: watch::Receiver<bool>,
    store: PushDeviceStore,
    transport: T,
    verbosity: String,
) {
    log::info!("notify: outbound notifier started (verbosity={verbosity})");
    let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                // Sender dropped, or shutdown flag flipped → stop.
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            recv = bus_rx.recv() => {
                match recv {
                    Ok(MuxEvent::AgentStatusChanged(ev)) => {
                        handle_event(&ev, &verbosity, &mut debouncer, &store, &transport).await;
                    }
                    Err(RecvError::Lagged(n)) => {
                        log::warn!("notify: event bus lagged, dropped {n} event(s)");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
    log::info!("notify: outbound notifier stopped");
}

/// Spawn the outbound notifier onto the current runtime.
///
/// Constructs the concrete [`ReqwestRelay`] (fallible — reqwest client build)
/// and subscribes a receiver from `bus`. The caller passes a `shutdown` receiver
/// derived from the same watch channel that stops the herdr event kernel, so the
/// notifier winds down with the rest of the daemon.
///
/// Only call this when a relay URL is configured **and** the event bus exists
/// (herdr detected) — see `bin/muxrd.rs::serve`.
pub fn spawn_notifier(
    bus: &EventBus,
    shutdown: watch::Receiver<bool>,
    store: PushDeviceStore,
    relay_url: &str,
    verbosity: String,
) -> anyhow::Result<()> {
    let transport = ReqwestRelay::new(relay_url)?;
    let bus_rx = bus.subscribe();
    tokio::spawn(run_notifier(bus_rx, shutdown, store, transport, verbosity));
    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::devices::{MIN_HANDLE_LEN, PushDevice};
    use std::sync::{Arc, Mutex};

    /// In-memory transport: records every send and returns a fixed outcome.
    struct MockTransport {
        outcome: SendOutcome,
        calls: Arc<Mutex<Vec<(String, NotifyContent)>>>,
    }

    impl MockTransport {
        fn new(outcome: SendOutcome) -> Self {
            Self {
                outcome,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn calls(&self) -> Arc<Mutex<Vec<(String, NotifyContent)>>> {
            Arc::clone(&self.calls)
        }
    }

    impl RelayTransport for MockTransport {
        fn send(
            &self,
            handle: &str,
            content: &NotifyContent,
        ) -> impl Future<Output = SendOutcome> + Send {
            self.calls
                .lock()
                .unwrap()
                .push((handle.to_owned(), content.clone()));
            let outcome = self.outcome;
            async move { outcome }
        }
    }

    fn ev(status: AgentStatus, synthetic: bool) -> AgentStatusChanged {
        AgentStatusChanged {
            pane: 7,
            workspace_id: "ws-9".into(),
            workspace_name: Some("main".into()),
            status,
            title: Some("claude".into()),
            synthetic,
        }
    }

    fn temp_store_path(tag: &str) -> std::path::PathBuf {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "muxrd-notify-sender-{tag}-{}-{secs}.json",
            std::process::id()
        ))
    }

    fn device(name: &str, handle_char: char) -> PushDevice {
        PushDevice {
            device_name: name.to_owned(),
            push_handle: handle_char.to_string().repeat(MIN_HANDLE_LEN),
            platform: "android".to_owned(),
            registered_at: 1_700_000_000,
        }
    }

    // ── Debounce ────────────────────────────────────────────────────────────

    #[test]
    fn debounce_suppresses_repeat_within_window() {
        let mut d = Debouncer::new(DEBOUNCE_WINDOW);
        let t0 = Instant::now();
        assert!(d.should_send(1, TriggerKind::Blocked, t0), "first send");
        assert!(
            !d.should_send(1, TriggerKind::Blocked, t0),
            "immediate repeat suppressed"
        );
    }

    #[test]
    fn debounce_does_not_suppress_transition_to_other_kind() {
        let mut d = Debouncer::new(DEBOUNCE_WINDOW);
        let t0 = Instant::now();
        assert!(d.should_send(1, TriggerKind::Blocked, t0));
        assert!(
            d.should_send(1, TriggerKind::Done, t0),
            "a Done after a recent Blocked is a different key — not suppressed"
        );
    }

    #[test]
    fn debounce_expires_after_window() {
        let mut d = Debouncer::new(DEBOUNCE_WINDOW);
        let t0 = Instant::now();
        assert!(d.should_send(1, TriggerKind::Blocked, t0));
        let later = t0 + DEBOUNCE_WINDOW + Duration::from_secs(1);
        assert!(
            d.should_send(1, TriggerKind::Blocked, later),
            "after the window the same trigger sends again"
        );
    }

    // ── Payload building ──────────────────────────────────────────────────────

    #[test]
    fn payload_normal_blocked_uses_workspace_name() {
        let c = build_content(&ev(AgentStatus::Blocked, false), "normal").unwrap();
        assert_eq!(c.kind, "agent_blocked");
        assert_eq!(c.title.as_deref(), Some("Muxr"));
        assert_eq!(c.body.as_deref(), Some("Agent blocked in main"));
    }

    #[test]
    fn payload_normal_done_uses_finished_verb() {
        let c = build_content(&ev(AgentStatus::Done, false), "normal").unwrap();
        assert_eq!(c.kind, "agent_done");
        assert_eq!(c.body.as_deref(), Some("Agent finished in main"));
    }

    #[test]
    fn payload_normal_falls_back_to_workspace_id_when_no_name() {
        let mut e = ev(AgentStatus::Blocked, false);
        e.workspace_name = None;
        let c = build_content(&e, "normal").unwrap();
        assert_eq!(c.body.as_deref(), Some("Agent blocked in ws-9"));
    }

    #[test]
    fn payload_generic_is_fixed_text_but_keeps_kind() {
        let c = build_content(&ev(AgentStatus::Blocked, false), "generic").unwrap();
        assert_eq!(c.kind, "agent_blocked", "kind is still actionable");
        assert_eq!(
            c.body.as_deref(),
            Some("Attention needed"),
            "generic body never leaks the workspace label"
        );
    }

    #[test]
    fn non_trigger_status_builds_nothing() {
        assert!(build_content(&ev(AgentStatus::Working, false), "normal").is_none());
        assert!(build_content(&ev(AgentStatus::Idle, false), "normal").is_none());
    }

    #[test]
    fn synthetic_done_is_skipped_but_synthetic_blocked_is_sent() {
        assert!(
            build_content(&ev(AgentStatus::Done, true), "normal").is_none(),
            "synthetic Done must not replay a finished notification"
        );
        assert!(
            build_content(&ev(AgentStatus::Blocked, true), "normal").is_some(),
            "synthetic Blocked still needs the user"
        );
    }

    // ── Fan-out / short-circuit / prune ───────────────────────────────────────

    #[tokio::test]
    async fn zero_devices_short_circuits_before_any_send() {
        let path = temp_store_path("zero");
        let store = PushDeviceStore::at_path(path); // no file → empty registry
        let transport = MockTransport::new(SendOutcome::Ok);
        let calls = transport.calls();
        let mut d = Debouncer::new(DEBOUNCE_WINDOW);

        handle_event(
            &ev(AgentStatus::Blocked, false),
            "normal",
            &mut d,
            &store,
            &transport,
        )
        .await;

        assert!(
            calls.lock().unwrap().is_empty(),
            "an empty registry must produce zero outbound sends"
        );
    }

    #[tokio::test]
    async fn happy_path_sends_to_every_device() {
        let path = temp_store_path("happy");
        let store = PushDeviceStore::at_path(path.clone());
        store.upsert(device("phone-a", 'a')).unwrap();
        store.upsert(device("phone-b", 'b')).unwrap();

        let transport = MockTransport::new(SendOutcome::Ok);
        let calls = transport.calls();
        let mut d = Debouncer::new(DEBOUNCE_WINDOW);

        handle_event(
            &ev(AgentStatus::Blocked, false),
            "normal",
            &mut d,
            &store,
            &transport,
        )
        .await;

        assert_eq!(calls.lock().unwrap().len(), 2, "both devices notified");
        assert_eq!(store.count().unwrap(), 2, "no pruning on 204");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn gone_response_prunes_the_device() {
        let path = temp_store_path("prune");
        let store = PushDeviceStore::at_path(path.clone());
        store.upsert(device("phone-a", 'a')).unwrap();

        let transport = MockTransport::new(SendOutcome::Gone);
        let calls = transport.calls();
        let mut d = Debouncer::new(DEBOUNCE_WINDOW);

        handle_event(
            &ev(AgentStatus::Blocked, false),
            "normal",
            &mut d,
            &store,
            &transport,
        )
        .await;

        assert_eq!(calls.lock().unwrap().len(), 1, "one send attempted");
        assert_eq!(
            store.count().unwrap(),
            0,
            "a 404/410 must prune the device from the registry"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn debounce_suppresses_second_event_end_to_end() {
        let path = temp_store_path("e2e-debounce");
        let store = PushDeviceStore::at_path(path.clone());
        store.upsert(device("phone-a", 'a')).unwrap();

        let transport = MockTransport::new(SendOutcome::Ok);
        let calls = transport.calls();
        let mut d = Debouncer::new(DEBOUNCE_WINDOW);

        let e = ev(AgentStatus::Blocked, false);
        handle_event(&e, "normal", &mut d, &store, &transport).await;
        handle_event(&e, "normal", &mut d, &store, &transport).await;

        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "the second identical event within the window is debounced"
        );
        let _ = std::fs::remove_file(&path);
    }
}
