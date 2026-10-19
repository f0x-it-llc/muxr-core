//! The TUI event loop.
//!
//! Mirrors fdemon's proven **poll + drain** runner (NOT `tokio::select!`), with
//! the ratcn runtime wired in as the input front end:
//!
//! 1. Stamp `state.ui.now` from a process-start `Instant` and prune expired
//!    toasts against it, so the reducer and the toaster share one clock.
//! 2. Drain all pending [`Message`]s from the mpsc channel via `try_recv` and
//!    feed each to [`update`], collecting [`UpdateAction`]s.
//! 3. Apply runner-side effects: `Quit` exits the loop; async actions are
//!    dispatched onto `tokio::task::spawn_blocking` tasks that post their
//!    results back over a cloned `tx`.
//! 4. `terminal.draw(...)` **unconditionally** every iteration — ratatui's
//!    double-buffer diff suppresses redundant terminal writes. Drawing is one
//!    [`Ratcn::render`] pass (see [`crate::tui::views::render`]).
//! 5. Route **at most one** input event per iteration from a persistent
//!    pending queue ([`InputPump`]). It goes through [`Ratcn::handle_event`]
//!    with the current state: an emitted message becomes `Message::Ui`, a
//!    consumed event becomes nothing, and only a key the component surface
//!    ignored reaches the reducer as `Message::Key`. Key presses, mouse events
//!    and bracketed pastes are kept; a run of consecutive pointer moves
//!    collapses to the last one. Why one per frame: routing a whole drain
//!    against one `&AppState` made N typed characters each edit the same stale
//!    field value (a paste left only its last character) and let later events
//!    hit a surface the earlier ones had already changed; reducing and
//!    redrawing between events keeps every event seeing what the previous one
//!    produced. With a non-empty queue the loop does not block, so a burst
//!    costs fast iterations, not one tick each.
//! 6. The heartbeat is a wall-clock `Message::Tick`, one per [`TICK`] of
//!    elapsed time whatever the input rate (the reducer's pollers count Ticks
//!    as 50 ms each).
//!
//! The loop exits as soon as `AppState.should_quit` is set.

use std::time::{Duration, Instant};

use anyhow::Result;
use std::collections::VecDeque;

use crossterm::event::{self, Event as CtEvent, KeyEventKind, MouseEventKind};
use ratatui::DefaultTerminal;
use ratcn::runtime::{Event, EventResult, KeyEvent, Ratcn, TabWrap};
use tokio::sync::mpsc;

use crate::app::{AppState, Message, UiMsg, UpdateAction, update};

/// Poll cadence / tick interval.
const TICK: Duration = Duration::from_millis(50);

/// Channel capacity for the message bus. Cloned senders feed async task results.
const CHANNEL_CAPACITY: usize = 256;

/// Most input events read in one [`InputPump::fill`].
const MAX_EVENTS_PER_TICK: usize = 256;

/// The runtime, wired to the app's focus and modal stack.
///
/// Also used by the view tests, so what they drive is the real surface the
/// binary runs.
pub(crate) fn build_ratcn() -> Ratcn<AppState, UiMsg> {
    Ratcn::new()
        .focus(|state: &AppState| &state.ui.focus, UiMsg::Focus)
        .modals(|state: &AppState| &state.ui.modals)
        .tab_wrap(TabWrap::Wrap)
}

/// Run the TUI to completion. Returns when the user quits.
///
/// Owns the message channel: the receiver is drained here; the sender is cloned
/// into each async [`UpdateAction`] task so results post back into the loop.
pub fn run(terminal: &mut DefaultTerminal, state: &mut AppState) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<Message>(CHANNEL_CAPACITY);
    let mut ratcn = build_ratcn();
    let theme = crate::tui::theme::muxr();
    let started = Instant::now();
    let mut pump = InputPump::new(started);
    let mut source = CrosstermSource;

    // Restore persisted advertise_trust before the first render so the
    // operator's last-set value survives restarts.
    {
        let persisted = crate::server::load_advertise_trust();
        state.cert.advertise_trust =
            crate::app::state::cert::AdvertiseTrust::from_persist_str(persisted);
    }

    // Seed the dashboard: every section loads once before the first key.
    state.server.loading = true;
    state.config.loading = true;
    state.tokens.loading = true;
    state.cert.loading = true;
    state.devices.loading = true;
    apply_actions(
        state,
        vec![
            UpdateAction::RefreshStatus,
            UpdateAction::LoadConfig,
            UpdateAction::LoadTokens,
            UpdateAction::LoadCertInfo,
            UpdateAction::LoadDevices,
            UpdateAction::LoadCertMode,
        ],
        tx.clone(),
    );

    while !state.should_quit {
        // (1) One clock for the reducer's toasts and the toaster's expiry.
        state.ui.now = started.elapsed();
        let _ = state.ui.toasts.prune_expired(state.ui.now);

        // (2) Drain all pending messages and run the update cycle.
        while let Ok(message) = rx.try_recv() {
            let actions = update(state, message);
            apply_actions(state, actions, tx.clone());
        }

        if state.should_quit {
            break;
        }

        // (3) Render unconditionally.
        terminal.draw(|frame| crate::tui::views::render(frame, &mut ratcn, state, &theme))?;

        // (4) Route at most one event with the current state; the next
        // iteration reduces and redraws before the following one is routed.
        pump.fill(&mut source)?;
        if let Some(event) = pump.next()
            && let Some(message) = route_event(&mut ratcn, state, event)
        {
            // Best-effort send; the channel is only saturated under pathological
            // backpressure, which the per-fill read bound keeps out of reach.
            let _ = tx.try_send(message);
        }

        // (5) Wall-clock heartbeat.
        if pump.heartbeat_due(Instant::now()) {
            let _ = tx.try_send(Message::Tick);
        }
    }

    Ok(())
}

/// Route one terminal event through the component surface with `state` and map
/// the outcome to the message it deserves.
///
/// Events ratcn cannot represent yield nothing.
pub(crate) fn route_event(
    ratcn: &mut Ratcn<AppState, UiMsg>,
    state: &AppState,
    event: CtEvent,
) -> Option<Message> {
    let event = Event::try_from(event).ok()?;
    let key: Option<KeyEvent> = match &event {
        Event::Key(key) => Some(*key),
        _ => None,
    };
    message_for(ratcn.handle_event(event, state), key)
}

/// Map one routed event's outcome (with its key, when it was one) to a message.
///
/// A key the declared surface emits a message for becomes [`Message::Ui`]; one
/// it consumed internally (a focus move, a character typed into a field) adds
/// nothing; only a key nothing handled reaches the reducer as
/// [`Message::Key`], which is what makes the dashboard's single-letter
/// shortcuts safe while a text field has focus. The heartbeat is not produced
/// here: see [`InputPump::heartbeat_due`].
pub(crate) fn message_for(result: EventResult<UiMsg>, key: Option<KeyEvent>) -> Option<Message> {
    match result {
        EventResult::Emit(msg) => Some(Message::Ui(msg)),
        EventResult::Consumed => None,
        EventResult::Ignored => key.map(Message::Key),
    }
}

/// Where terminal events come from; the seam that lets tests script input.
pub(crate) trait EventSource {
    /// Whether an event is ready within `timeout`.
    fn poll(&mut self, timeout: Duration) -> Result<bool>;
    /// Read the ready event.
    fn read(&mut self) -> Result<CtEvent>;
}

/// The real source: crossterm's global event queue.
struct CrosstermSource;

impl EventSource for CrosstermSource {
    fn poll(&mut self, timeout: Duration) -> Result<bool> {
        Ok(event::poll(timeout)?)
    }

    fn read(&mut self) -> Result<CtEvent> {
        Ok(event::read()?)
    }
}

/// Input held across loop iterations: events read but not yet routed, and the
/// time of the last heartbeat.
///
/// Persisting the queue is what lets the loop route one event per frame; a
/// drain-and-route-all design reintroduces stale-state edits.
pub(crate) struct InputPump {
    pending: VecDeque<CtEvent>,
    last_tick: Instant,
}

impl InputPump {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            pending: VecDeque::new(),
            last_tick: now,
        }
    }

    /// Refill the queue, only when it is empty.
    ///
    /// Waits at most until the next heartbeat is due (so an idle loop wakes
    /// exactly on it), then reads whatever else is already queued, bounded by
    /// [`MAX_EVENTS_PER_TICK`] so a flood (a trackpad sweeping the pointer over
    /// the window emits hundreds of moves) cannot starve the render. Resize and
    /// focus events are dropped.
    pub(crate) fn fill(&mut self, source: &mut impl EventSource) -> Result<()> {
        if !self.pending.is_empty() {
            return Ok(());
        }
        let remaining = TICK.saturating_sub(self.last_tick.elapsed());
        if !source.poll(remaining)? {
            return Ok(());
        }
        let mut raw = vec![source.read()?];
        while raw.len() < MAX_EVENTS_PER_TICK && source.poll(Duration::ZERO)? {
            raw.push(source.read()?);
        }
        self.pending
            .extend(coalesce_moves(raw.into_iter().filter(keep_event).collect()));
        Ok(())
    }

    /// Pop the next event to route. A pointer move directly followed by another
    /// is superseded and never routed; hover only needs the latest position.
    pub(crate) fn next(&mut self) -> Option<CtEvent> {
        let mut event = self.pending.pop_front()?;
        while is_move(&event) && self.pending.front().is_some_and(is_move) {
            event = self.pending.pop_front()?;
        }
        Some(event)
    }

    /// Whether a heartbeat is due at `now`; if so it is consumed. One per
    /// [`TICK`] of wall time at most, and a long stall yields one, not a
    /// backlog.
    pub(crate) fn heartbeat_due(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_tick) >= TICK {
            self.last_tick = now;
            true
        } else {
            false
        }
    }
}

/// Whether an event is routed at all: key **presses** only (so terminals that
/// report key release / repeat via crossterm "kitty" enhanced reporting cannot
/// double-fire), every mouse event, and bracketed pastes (one atomic edit).
fn keep_event(event: &CtEvent) -> bool {
    match event {
        CtEvent::Key(key) => key.kind == KeyEventKind::Press,
        CtEvent::Mouse(_) | CtEvent::Paste(_) => true,
        _ => false,
    }
}

fn is_move(event: &CtEvent) -> bool {
    matches!(event, CtEvent::Mouse(m) if m.kind == MouseEventKind::Moved)
}

/// Collapse each run of consecutive pointer moves to its last event. Hover
/// only needs the latest position; nothing else is merged or reordered, so a
/// click's Down/Up keep their order relative to the moves around them.
fn coalesce_moves(events: Vec<CtEvent>) -> Vec<CtEvent> {
    let mut out: Vec<CtEvent> = Vec::with_capacity(events.len());
    for event in events {
        if is_move(&event) && out.last().is_some_and(is_move) {
            out.pop();
        }
        out.push(event);
    }
    out
}

/// Apply runner-side effects from an update cycle.
///
/// `Quit` is handled synchronously via `state.should_quit` (already set by
/// `update`). All async actions are dispatched onto `tokio::task::spawn_blocking`
/// tasks; results are sent back over a clone of `tx`.
fn apply_actions(state: &mut AppState, actions: Vec<UpdateAction>, tx: mpsc::Sender<Message>) {
    for action in actions {
        match action {
            UpdateAction::Quit => {
                state.should_quit = true;
            }
            UpdateAction::RefreshStatus => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let state = crate::server::status();
                    let msg = Message::StatusLoaded(state);
                    let _ = tx.blocking_send(msg);
                });
            }
            UpdateAction::StartServer => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match crate::server::start_daemon() {
                        Ok(()) => {
                            // Brief wait so the daemon has a chance to start.
                            std::thread::sleep(Duration::from_millis(500));
                            // Re-query status so the UI reflects the new state.
                            let st = crate::server::status();
                            Message::StatusLoaded(st)
                        }
                        Err(e) => Message::ActionFailed(e.to_string()),
                    };
                    let _ = tx.blocking_send(msg);
                });
            }
            UpdateAction::StopServer => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match crate::server::stop() {
                        Ok(()) => Message::StatusLoaded(None),
                        Err(e) => Message::ActionFailed(e.to_string()),
                    };
                    let _ = tx.blocking_send(msg);
                });
            }
            UpdateAction::LoadConfig => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = load_config_snapshot();
                    let _ = tx.blocking_send(msg);
                });
            }
            UpdateAction::SaveBind(addr) => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match crate::server::set_bind_addr(&addr) {
                        Ok(()) => Message::ActionOk(format!("Saved bind address: {addr}")),
                        Err(e) => Message::ActionFailed(e.to_string()),
                    };
                    let _ = tx.blocking_send(msg);
                });
            }
            UpdateAction::EnsureCert(sans) => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = ensure_cert_task(&sans);
                    let _ = tx.blocking_send(msg);
                });
            }

            // ── Token management ──────────────────────────────────────────────
            UpdateAction::LoadTokens => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match crate::server::tokens::list() {
                        Ok(tokens) => Message::TokensLoaded(tokens),
                        Err(e) => Message::ActionFailed(format!("list tokens failed: {e}")),
                    };
                    let _ = tx.blocking_send(msg);
                });
            }
            UpdateAction::CreateToken {
                name,
                read_only,
                expiry_secs,
            } => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match crate::server::tokens::create(name, read_only) {
                        Ok((token, name)) => {
                            // Record the opt-in muxr-side expiry keyed by the token
                            // hash. A failure here is non-fatal — the token is still
                            // usable, just without the time box — so surface it as a
                            // status note rather than failing the create.
                            if let Some(ttl) = expiry_secs
                                && let Err(e) = muxrd::token_expiry::set_expiry_in(&token, ttl)
                            {
                                log::warn!("failed to record token expiry: {e:#}");
                            }
                            Message::TokenCreated {
                                token,
                                name,
                                read_only,
                            }
                        }
                        Err(e) => Message::ActionFailed(format!("create token failed: {e}")),
                    };
                    let _ = tx.blocking_send(msg);
                });
            }
            UpdateAction::RevokeToken(name) => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match crate::server::tokens::revoke(&name) {
                        Ok(_) => Message::TokensChanged,
                        Err(e) => Message::ActionFailed(format!("revoke token failed: {e}")),
                    };
                    let _ = tx.blocking_send(msg);
                });
            }

            // ── Device management ─────────────────────────────────────────────
            UpdateAction::LoadDevices => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match crate::server::devices::list() {
                        Ok(devices) => Message::DevicesLoaded {
                            devices,
                            relay_url: crate::server::devices::relay_url(),
                        },
                        Err(e) => Message::ActionFailed(format!("list devices failed: {e}")),
                    };
                    let _ = tx.blocking_send(msg);
                });
            }
            UpdateAction::RemoveDevice(name) => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = match crate::server::devices::remove(&name) {
                        Ok(_) => Message::DevicesChanged,
                        Err(e) => Message::ActionFailed(format!("remove device failed: {e}")),
                    };
                    let _ = tx.blocking_send(msg);
                });
            }

            // ── Dashboard ─────────────────────────────────────────────────────
            UpdateAction::LoadCertInfo => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = load_cert_info_task();
                    let _ = tx.blocking_send(msg);
                });
            }

            UpdateAction::LoadCertMode => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg =
                        Message::CertModeLoaded(crate::server::server_cert_mode().map(tls_mode));
                    let _ = tx.blocking_send(msg);
                });
            }

            // ── ctl-local state persistence ───────────────────────────────────
            // Handled synchronously — the file write is tiny and non-blocking in
            // practice; no async task needed, no message posted back.
            UpdateAction::SaveAdvertiseTrust(trust) => {
                crate::server::save_advertise_trust(trust.persist_str());
            }

            // ── Token QR overlay ──────────────────────────────────────────────
            UpdateAction::ShowTokenQr {
                token,
                read_only,
                seq,
                advertise_trust,
            } => {
                let tx = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let msg = build_token_qr_task(token, read_only, seq, advertise_trust);
                    let _ = tx.blocking_send(msg);
                });
            }
        }
    }
}

/// Load config + reachable IPs and return the appropriate `Message`.
fn load_config_snapshot() -> Message {
    use crate::app::message::ConfigSnapshot;

    let cfg = match crate::server::effective_config() {
        Ok(c) => c,
        Err(e) => return Message::ActionFailed(format!("Config load failed: {e}")),
    };
    let reachable_ips = crate::pairing::net::reachable_ipv4();

    Message::ConfigLoaded(ConfigSnapshot {
        bind_addr: cfg.bind_addr,
        cert_dir: cfg.cert_dir.display().to_string(),
        reachable_ips,
        advertise_sans: crate::server::env_extra_sans(),
    })
}

/// Read the persisted cert fingerprint + SAN sidecar **without** generating anything.
///
/// Calls only the read-only facade functions `current_cert_fingerprint()` and
/// `persisted_extra_sans()`.  Never calls `ensure_cert` / `load_or_generate_identity`.
fn load_cert_info_task() -> Message {
    let fingerprint = match crate::server::current_cert_fingerprint() {
        Ok(fp) => fp,
        Err(e) => return Message::ActionFailed(format!("cert info: {e}")),
    };
    let sans = crate::server::persisted_extra_sans();
    Message::CertInfoLoaded { fingerprint, sans }
}

/// Ensure (or regenerate) the TLS cert and return the appropriate `Message`.
fn ensure_cert_task(sans: &[crate::app::state::San]) -> Message {
    let cert_pem = match crate::server::ensure_cert(sans) {
        Ok(p) => p,
        Err(e) => return Message::ActionFailed(format!("Cert generation failed: {e}")),
    };
    let fingerprint = match crate::server::cert_fingerprint(&cert_pem) {
        Ok(fp) => fp,
        Err(e) => return Message::ActionFailed(format!("Fingerprint failed: {e}")),
    };
    // Collect active SANs as display strings.
    let active_sans: Vec<String> = sans.iter().map(|s| s.value().to_string()).collect();

    Message::CertEnsured {
        fingerprint,
        sans: active_sans,
    }
}

/// Build a pairing QR URI for an **existing** plaintext token and return the
/// appropriate `Message`.
///
/// This is the old `start_pairing_task` minus the mint/revoke: the `token` is the
/// real user token the caller already created (the one whose plaintext we still
/// hold). It is NEVER minted or revoked here.
///
/// Steps (all in `spawn_blocking`):
/// 1. Guard: the server must be **running** — else `TokenQrFailed`.
/// 2. Read host + port from `effective_config().bind_addr`.
/// 3. Pick the advertise host: prefer the configured bind host when it is a
///    concrete (non-loopback, non-unspecified) address; otherwise fall back to
///    the first reachable non-loopback IPv4.
/// 4. Resolve the pairing trust (Layer-1 + Layer-2 + heuristics):
///    - Layer-2 override (`advertise_trust != Auto`) takes precedence.
///    - Layer-1: query the running server's `cert_mode` via the control socket.
///    - Heuristic: a DNS-name advertise host nudges toward `Ca` in Auto mode.
///    - For `Pin` paths: read the persisted cert fingerprint without regenerating.
///    - For `Ca` / h2c paths: no fingerprint required.
/// 5. Build `PairingParams{ token, trust, .. }.to_uri()` from the passed plaintext
///    token; capture `client_count` as baseline.
fn build_token_qr_task(
    token: String,
    read_only: bool,
    seq: u64,
    advertise_trust: crate::app::state::cert::AdvertiseTrust,
) -> Message {
    use crate::app::state::cert::AdvertiseTrust;
    use crate::pairing::payload::{PairingParams, PairingTrust};

    // 1. Guard: the server must be running.
    if !crate::server::is_running() {
        return Message::TokenQrFailed {
            err: "Start the server first.".to_string(),
            seq,
        };
    }

    // 2. Effective config (for bind addr).
    let cfg = match crate::server::effective_config() {
        Ok(c) => c,
        Err(e) => {
            return Message::TokenQrFailed {
                err: format!("config: {e}"),
                seq,
            };
        }
    };

    // 3. Parse host + port from bind_addr.
    let (bind_host, port) = {
        let addr = &cfg.bind_addr;
        if let Some(colon) = addr.rfind(':') {
            let h = addr[..colon].to_string();
            let p: u16 = addr[colon + 1..].parse().unwrap_or(50051);
            (h, p)
        } else {
            (addr.clone(), 50051)
        }
    };

    // 4a. Pick the advertise host (prefer the configured bind IP, then an explicit
    //    MUXRD_SAN advertise address, then a discovered interface IP).
    let advertise_host = choose_advertise_host(&bind_host, &crate::server::env_extra_sans());

    // 4b. Resolve the pairing trust.
    //
    // Priority:
    //   - Layer-2 explicit override (advertise_trust = Ca | Pin) → use it directly.
    //   - Layer-2 Auto + Layer-1 cert_mode query:
    //       - External | H2c → Ca (no fp)
    //       - SelfSigned     → Pin (fp required)
    //   - Heuristic (Auto + SelfSigned only): a DNS-name advertise host nudges
    //     toward Ca — the server is likely behind a CA-fronted proxy even if ctl
    //     sees a self-signed local cert (e.g. Recipe B: Dokploy + insecureSkipVerify).
    //     The operator should set advertise_trust=Ca to override cleanly; this is
    //     documented as a nudge hint, not a security decision.
    //   - Fallback (server down / socket error in Auto): use Pin if a cert exists,
    //     Ca if no local cert found (h2c or proxy scenario without a local cert).
    let trust = match advertise_trust {
        // Layer-2: operator forced CA → no fingerprint needed.
        AdvertiseTrust::Ca => PairingTrust::Ca,

        // Layer-2: operator forced Pin → fingerprint required.
        AdvertiseTrust::Pin => match crate::server::current_cert_fingerprint() {
            Ok(Some(fp)) => PairingTrust::Pin { fingerprint: fp },
            Ok(None) => {
                return Message::TokenQrFailed {
                    err:
                        "No certificate yet — open the Certificate dialog (e) and regenerate first."
                            .to_string(),
                    seq,
                };
            }
            Err(e) => {
                return Message::TokenQrFailed {
                    err: format!("cert fingerprint: {e}"),
                    seq,
                };
            }
        },

        // Layer-2 Auto: consult Layer-1 cert_mode via the shared pure helper.
        AdvertiseTrust::Auto => {
            // Layer-1: query the running server's cert_mode.
            let cert_mode = crate::server::server_cert_mode();

            // resolve_auto_trust: true → Pin (SelfSigned/None); false → Ca (External/H2c).
            if resolve_auto_trust(cert_mode) {
                // Self-signed cert (or server not running / old server without
                // cert_mode field, which defaults to SelfSigned).
                //
                // Finding 1 fix: always emit Pin whenever a local cert fingerprint
                // exists, regardless of the advertise-host shape (IP vs DNS).  The
                // previous code silently downgraded DNS-named hosts to Ca, which is
                // wrong for direct / LAN self-signed servers (e.g. `localhost` or a
                // hostname) — those produce a QR that fails on-device because no
                // CA-valid cert exists.
                //
                // If the operator is actually behind a CA-fronted proxy AND the
                // server reports SelfSigned (Recipe B), they should override via
                // the trust cycle to force `Ca` explicitly.  The Certificate dialog
                // shows an advisory hint when the host looks like a DNS name + SelfSigned
                // (Finding 2), nudging the operator toward that override.
                match crate::server::current_cert_fingerprint() {
                    Ok(Some(fp)) => PairingTrust::Pin { fingerprint: fp },
                    Ok(None) => {
                        return Message::TokenQrFailed {
                            err: "No certificate yet — open the Certificate dialog (e) \
                                  and regenerate first."
                                .to_string(),
                            seq,
                        };
                    }
                    Err(e) => {
                        return Message::TokenQrFailed {
                            err: format!("cert fingerprint: {e}"),
                            seq,
                        };
                    }
                }
            } else {
                // External cert or h2c → system-CA trust; no fingerprint needed.
                // These modes mean the client sees either a CA-signed cert or the
                // proxy's edge cert — pinning the local self-signed cert would fail.
                PairingTrust::Ca
            }
        }
    };

    // 5. Build the URI from the PASSED plaintext token (no mint, no revoke).
    let trust_is_ca = matches!(trust, PairingTrust::Ca);
    let fingerprint_display = match &trust {
        PairingTrust::Pin { fingerprint } => fingerprint.clone(),
        PairingTrust::Ca => String::new(),
    };

    let uri = PairingParams {
        host: advertise_host.clone(),
        port,
        trust,
        token,
        read_only,
        label: "muxrd".to_string(),
    }
    .to_uri();

    // 6. Capture current client count as baseline.
    let baseline_clients = crate::server::client_count().unwrap_or(0);

    // Build a short display string for the info panel below the QR.
    let fingerprint_short = if trust_is_ca {
        "CA trust (no pin)".to_string()
    } else if fingerprint_display.len() > 16 {
        format!("{}…", &fingerprint_display[..16])
    } else {
        fingerprint_display
    };

    Message::TokenQrReady {
        uri,
        host: advertise_host,
        port,
        fingerprint_short,
        baseline_clients,
        seq,
    }
}

/// Resolve the Auto-mode pairing trust from the server's reported cert mode.
///
/// Returns `true` if the mode requires a fingerprint Pin, `false` if Ca trust
/// (no fingerprint) is appropriate.
///
/// Mapping:
/// - `External` | `H2c` → `false` (Ca): the client sees a CA-signed or
///   proxy-edge cert; pinning the server's local self-signed cert would fail.
/// - `SelfSigned` | `None` → `true` (Pin): no CA-valid cert exists, so the
///   client must pin the fingerprint.  `None` is the conservative fallback for
///   an old server that predates the `cert_mode` field.
///
/// This is a pure function with no I/O.  The `Auto` branch of
/// `build_token_qr_task` delegates to it so both production code and unit
/// tests exercise the same mapping.
pub(crate) fn resolve_auto_trust(cert_mode: Option<muxrd::config::CertMode>) -> bool {
    use muxrd::config::CertMode;
    match cert_mode {
        Some(CertMode::External) | Some(CertMode::H2c) => false,
        Some(CertMode::SelfSigned) | None => true,
    }
}

/// Choose the host to advertise in the pairing QR.
///
/// Priority:
/// 1. The configured bind host, when it is a concrete reachable address (not
///    loopback, not the unspecified `0.0.0.0` / `::` wildcard) — the user bound
///    to a specific address deliberately.
/// 2. An explicit advertise SAN from `MUXRD_SAN` (`advertise_sans`) — the
///    operator's externally-reachable address. This is essential in a container
///    bound to `0.0.0.0`, where the externally-reachable IP (e.g. a tailnet IP
///    reached via host-side NAT) is NOT a local interface and so would never be
///    discovered by interface enumeration. The pairing QR must point the phone
///    at *this* address, not the container-internal bridge IP.
/// 3. The first reachable non-loopback IPv4 discovered from local interfaces
///    (the native LAN case).
/// 4. The bind host as a last resort.
fn choose_advertise_host(bind_host: &str, advertise_sans: &[String]) -> String {
    if is_concrete_advertise_host(bind_host) {
        return bind_host.to_string();
    }
    if let Some(adv) = advertise_sans
        .iter()
        .find(|s| is_concrete_advertise_host(s))
    {
        return adv.trim().to_string();
    }
    crate::pairing::net::reachable_ipv4()
        .into_iter()
        .next()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| bind_host.to_string())
}

/// Whether `host` is a concrete address worth advertising directly — i.e. it
/// parses as an IP that is neither loopback nor unspecified. Non-IP hosts (DNS
/// names) are treated as concrete (the user typed something specific).
fn is_concrete_advertise_host(host: &str) -> bool {
    let h = host.trim();
    if h.is_empty() {
        return false;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => !ip.is_loopback() && !ip.is_unspecified(),
        // A DNS name (e.g. "server.local") is a deliberate choice — keep it.
        Err(_) => true,
    }
}

/// Convert the daemon's reported cert mode into the app-layer mirror.
///
/// `tui/runner.rs` and `server/` are the only modules that may name `muxrd`
/// types; this is where the conversion for `LoadCertMode` lives.
fn tls_mode(mode: muxrd::config::CertMode) -> crate::app::state::cert::TlsMode {
    use crate::app::state::cert::TlsMode;
    use muxrd::config::CertMode;
    match mode {
        CertMode::SelfSigned => TlsMode::SelfSigned,
        CertMode::External => TlsMode::External,
        CertMode::H2c => TlsMode::H2c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod heartbeat {
        use super::super::{EventSource, InputPump, MAX_EVENTS_PER_TICK, TICK};
        use anyhow::Result;
        use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
        use std::collections::VecDeque;
        use std::time::{Duration, Instant};

        struct Scripted(VecDeque<CtEvent>);

        impl EventSource for Scripted {
            fn poll(&mut self, _timeout: Duration) -> Result<bool> {
                Ok(!self.0.is_empty())
            }
            fn read(&mut self) -> Result<CtEvent> {
                Ok(self.0.pop_front().expect("polled ready"))
            }
        }

        fn press(c: char) -> CtEvent {
            CtEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
        }

        #[test]
        fn one_tick_per_tick_interval_of_wall_time() {
            let t0 = Instant::now();
            let mut pump = InputPump::new(t0 - TICK);
            assert!(pump.heartbeat_due(t0));
            assert!(!pump.heartbeat_due(t0 + Duration::from_millis(10)));
            assert!(pump.heartbeat_due(t0 + Duration::from_millis(50)));
        }

        #[test]
        fn a_long_stall_yields_a_single_tick() {
            let t0 = Instant::now();
            let mut pump = InputPump::new(t0);
            let later = t0 + Duration::from_millis(300);
            assert!(pump.heartbeat_due(later));
            assert!(!pump.heartbeat_due(later));
        }

        #[test]
        fn nothing_ready_yields_no_event() {
            let mut pump = InputPump::new(Instant::now());
            pump.fill(&mut Scripted(VecDeque::new())).unwrap();
            assert!(pump.next().is_none());
        }

        #[test]
        fn fill_stops_at_the_bound_and_leaves_the_rest() {
            let mut source = Scripted((0..MAX_EVENTS_PER_TICK + 10).map(|_| press('a')).collect());
            let mut pump = InputPump::new(Instant::now());
            pump.fill(&mut source).unwrap();
            assert_eq!(source.0.len(), 10);
            assert_eq!(pump.pending.len(), MAX_EVENTS_PER_TICK);
            // A non-empty queue is not refilled.
            pump.fill(&mut source).unwrap();
            assert_eq!(source.0.len(), 10);
            while pump.next().is_some() {}
            pump.fill(&mut source).unwrap();
            assert!(source.0.is_empty());
            assert_eq!(pump.pending.len(), 10);
        }

        #[test]
        fn message_for_maps_each_outcome() {
            use super::super::message_for;
            use crate::app::state::DialogId;
            use crate::app::{Message, UiMsg};
            use ratcn::runtime::{EventResult, KeyCode, KeyEvent};
            let key = KeyEvent::new(KeyCode::Char('q'));
            assert!(matches!(
                message_for(EventResult::Emit(UiMsg::Open(DialogId::Tokens)), None),
                Some(Message::Ui(_))
            ));
            assert!(message_for(EventResult::Consumed, Some(key)).is_none());
            assert!(matches!(
                message_for(EventResult::Ignored, Some(key)),
                Some(Message::Key(_))
            ));
            assert!(message_for(EventResult::Ignored, None).is_none());
        }
    }

    mod input {
        use super::super::{InputPump, coalesce_moves, keep_event};
        use crossterm::event::{
            Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
            MouseEvent, MouseEventKind,
        };

        fn mouse(kind: MouseEventKind, column: u16, row: u16) -> CtEvent {
            CtEvent::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        }

        fn key(kind: KeyEventKind) -> CtEvent {
            CtEvent::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                kind,
            ))
        }

        #[test]
        fn a_run_of_moves_before_a_key_collapses_to_the_last() {
            let events = vec![
                mouse(MouseEventKind::Moved, 1, 1),
                mouse(MouseEventKind::Moved, 2, 2),
                mouse(MouseEventKind::Moved, 3, 3),
                key(KeyEventKind::Press),
            ];
            assert_eq!(
                coalesce_moves(events),
                vec![mouse(MouseEventKind::Moved, 3, 3), key(KeyEventKind::Press)]
            );
        }

        #[test]
        fn moves_around_a_press_keep_their_order() {
            let down = MouseEventKind::Down(MouseButton::Left);
            let up = MouseEventKind::Up(MouseButton::Left);
            let events = vec![
                mouse(MouseEventKind::Moved, 1, 1),
                mouse(down, 1, 1),
                mouse(MouseEventKind::Moved, 2, 2),
                mouse(MouseEventKind::Moved, 3, 3),
                mouse(up, 3, 3),
            ];
            assert_eq!(
                coalesce_moves(events),
                vec![
                    mouse(MouseEventKind::Moved, 1, 1),
                    mouse(down, 1, 1),
                    mouse(MouseEventKind::Moved, 3, 3),
                    mouse(up, 3, 3),
                ]
            );
        }

        #[test]
        fn only_presses_and_mouse_events_are_kept() {
            assert!(keep_event(&key(KeyEventKind::Press)));
            assert!(!keep_event(&key(KeyEventKind::Release)));
            assert!(!keep_event(&key(KeyEventKind::Repeat)));
            assert!(keep_event(&mouse(MouseEventKind::Moved, 0, 0)));
            assert!(!keep_event(&CtEvent::FocusGained));
            assert!(!keep_event(&CtEvent::Resize(80, 24)));
            assert!(keep_event(&CtEvent::Paste("x".into())));
        }

        #[test]
        fn next_drops_a_superseded_move_but_routes_one_before_a_press() {
            let down = MouseEventKind::Down(MouseButton::Left);
            let mut pump = InputPump::new(std::time::Instant::now());
            pump.pending.extend([
                mouse(MouseEventKind::Moved, 1, 1),
                mouse(MouseEventKind::Moved, 2, 2),
                mouse(down, 2, 2),
            ]);
            assert_eq!(pump.next(), Some(mouse(MouseEventKind::Moved, 2, 2)));
            assert_eq!(pump.next(), Some(mouse(down, 2, 2)));
            assert_eq!(pump.next(), None);
        }
    }

    #[test]
    fn concrete_host_keeps_configured_bind_ip() {
        // A user-configured LAN IP must be advertised as-is (review Major #5).
        assert!(is_concrete_advertise_host("192.168.1.50"));
        assert_eq!(choose_advertise_host("192.168.1.50", &[]), "192.168.1.50");
    }

    #[test]
    fn dns_bind_host_is_treated_as_concrete() {
        assert!(is_concrete_advertise_host("server.local"));
        assert_eq!(choose_advertise_host("server.local", &[]), "server.local");
    }

    #[test]
    fn wildcard_bind_prefers_advertise_san_over_reachable() {
        // The tailnet/container case: bind 0.0.0.0, MUXRD_SAN advertises the
        // externally-reachable IP. The pairing QR host must be that advertise IP —
        // NOT a discovered container-internal interface IP — so the phone can dial
        // it. An unspecified/loopback advertise entry is skipped.
        let adv = vec!["0.0.0.0".to_string(), "100.64.0.7".to_string()];
        assert_eq!(choose_advertise_host("0.0.0.0", &adv), "100.64.0.7");
    }

    #[test]
    fn loopback_and_unspecified_are_not_concrete() {
        // These force the fall-back to a reachable interface IP.
        assert!(!is_concrete_advertise_host("127.0.0.1"));
        assert!(!is_concrete_advertise_host("0.0.0.0"));
        assert!(!is_concrete_advertise_host("::"));
        assert!(!is_concrete_advertise_host("::1"));
        assert!(!is_concrete_advertise_host(""));
    }

    #[test]
    fn unspecified_bind_falls_back_to_reachable_or_self() {
        // With an unspecified bind host, choose_advertise_host returns either a
        // discovered reachable IP or (if none) the original host — never panics,
        // and never returns the wildcard when a reachable IP exists.
        let chosen = choose_advertise_host("0.0.0.0", &[]);
        if let Some(ip) = crate::pairing::net::reachable_ipv4().into_iter().next() {
            assert_eq!(chosen, ip.to_string());
        } else {
            assert_eq!(chosen, "0.0.0.0");
        }
    }

    // ── Finding 1: Auto + SelfSigned trust resolution tests ───────────────────
    //
    // These tests verify the corrected Auto + SelfSigned logic: the DNS-shape of
    // the advertise host must NO LONGER silently downgrade to Ca.  Auto + SelfSigned
    // must always attempt Pin (regardless of host shape), and Auto + External/H2c
    // must always yield Ca.
    //
    // Tests call `resolve_auto_trust` — the real pure fn used by
    // `build_token_qr_task` — so any future divergence between tests and the
    // production path will be caught at compile time.

    #[test]
    fn auto_self_signed_dns_host_wants_pin() {
        // Finding 1 regression test: Auto + SelfSigned must resolve to Pin even
        // when the advertise host is a DNS name (e.g. "server.local", "localhost").
        // The old code emitted Ca here; the fix always emits Pin for SelfSigned.
        use muxrd::config::CertMode;
        assert!(
            resolve_auto_trust(Some(CertMode::SelfSigned)),
            "Auto + SelfSigned must want Pin regardless of host shape"
        );
    }

    #[test]
    fn auto_self_signed_loopback_host_wants_pin() {
        // "localhost" is non-IP → was previously treated as `host_is_dns=true` →
        // silently emitted Ca.  Post-fix, SelfSigned always wants Pin.
        use muxrd::config::CertMode;
        // The loopback-exclusion is now only used by the Cert-screen hint
        // (host_looks_like_dns), not by the QR trust resolution.
        assert!(
            resolve_auto_trust(Some(CertMode::SelfSigned)),
            "Auto + SelfSigned must want Pin for localhost host"
        );
    }

    #[test]
    fn auto_external_yields_ca() {
        // Auto + External cert → Ca (no fingerprint needed).
        use muxrd::config::CertMode;
        assert!(
            !resolve_auto_trust(Some(CertMode::External)),
            "Auto + External must resolve to Ca"
        );
    }

    #[test]
    fn auto_h2c_yields_ca() {
        // Auto + H2c → Ca (no fingerprint needed — no cert at all).
        use muxrd::config::CertMode;
        assert!(
            !resolve_auto_trust(Some(CertMode::H2c)),
            "Auto + H2c must resolve to Ca"
        );
    }

    #[test]
    fn auto_none_cert_mode_wants_pin() {
        // None (server down or pre-cert_mode server) → conservative fallback: Pin.
        assert!(
            resolve_auto_trust(None),
            "Auto + None cert_mode must want Pin (conservative fallback)"
        );
    }
}
