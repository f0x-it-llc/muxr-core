//! Host-vitals RPCs over [`crate::monitor::MonitorHandle`]: `WatchVitals`, `GetVitalsHistory`,
//! `WatchProcesses` (sorted, filtered and cut on the server; command lines redacted for a
//! read-only token), the guarded `SignalProcess`, and the model → proto mapping.
//! Streams mirror `WatchAgents` (bounded channel, token re-check, fatal lane); see `agent_ops`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::{Interval, MissedTickBehavior};
use tonic::{Request, Response, Status};

use crate::monitor::{
    self, DEFAULT_FULL_INTERVAL, DEFAULT_PROCESS_LIMIT, MAX_FULL_INTERVAL, MAX_PROCESS_LIMIT,
    MIN_FULL_INTERVAL, MIN_PROCESS_INTERVAL, MonitorHandle, ProcessQuery, ProcessSubscription,
    SignalRefusal, Tier, VitalsItem, VitalsSubscription,
};
use crate::proto::{
    ActionAck as ProtoAck, HistoryRange, HistorySeries, ProcessSnapshot, SeriesKind,
    SignalProcessRequest, VitalsDetail, VitalsEvent, VitalsHistory, VitalsHistoryRequest,
    WatchProcessesRequest, WatchVitalsRequest,
};
use crate::relay::TOKEN_RECHECK_INTERVAL;

use super::MuxrService;
use super::helpers;

/// Longest `WatchProcesses` filter, in bytes.
const MAX_PROCESS_FILTER_BYTES: usize = 128;

/// Bound on one `WatchVitals` response channel, as `WatchAgents`' own. A client slower than the
/// sampler stalls the stream task, whose subscription then lags (reported in `lagged`) instead
/// of anything buffering without bound.
const WATCH_CHANNEL_BOUND: usize = 16;

/// The `WatchVitals` response stream type.
pub(super) type VitalsEventStream =
    Pin<Box<dyn futures::Stream<Item = Result<VitalsEvent, Status>> + Send>>;

/// The `WatchProcesses` response stream type.
pub(super) type ProcessSnapshotStream =
    Pin<Box<dyn futures::Stream<Item = Result<ProcessSnapshot, Status>> + Send>>;

/// A blocking session-token check (`ipc::validate_session_token` in production). Always called
/// on the blocking pool.
type TokenValidator = Arc<dyn Fn(&str) -> anyhow::Result<bool> + Send + Sync>;

/// The answer to every host-vitals call made while monitoring is off.
fn monitoring_disabled(rpc: &str) -> Status {
    log::info!("{rpc}: monitoring is disabled → failed_precondition");
    Status::failed_precondition("monitoring is disabled")
}

impl MuxrService {
    /// The monitor handle, when there is one and it is enabled; otherwise `failed_precondition`.
    fn enabled_monitor(&self, rpc: &str) -> Result<&MonitorHandle, Status> {
        match self.monitor() {
            Some(handle) if handle.is_enabled() => Ok(handle),
            _ => Err(monitoring_disabled(rpc)),
        }
    }

    // ── WatchVitals ───────────────────────────────────────────────────────────

    /// Stream host vitals at the requested detail. A read: permitted for read-only tokens.
    pub(super) async fn watch_vitals_impl(
        &self,
        request: Request<WatchVitalsRequest>,
    ) -> Result<Response<VitalsEventStream>, Status> {
        let token = request
            .extensions()
            .get::<crate::auth::SessionToken>()
            .map(|t| t.0.clone());
        let tier = vitals_tier(request.get_ref())?;
        let handle = self.enabled_monitor("WatchVitals")?;
        // Disabled between the check and here: the first item is `Disabled`, so the stream
        // sends its terminal event and ends OK.
        let subscription = handle.subscribe(tier);
        log::info!("WatchVitals: opened ({tier:?})");
        let watch = VitalsWatch {
            handle: handle.clone(),
            tier,
            auth: TokenCheck {
                token,
                validate: Arc::new(|t: &str| crate::ipc::validate_session_token(t)),
            },
            token_recheck: TOKEN_RECHECK_INTERVAL,
        };
        Ok(Response::new(Box::pin(start_vitals_watch(
            watch,
            subscription,
        ))))
    }

    // ── GetVitalsHistory ──────────────────────────────────────────────────────

    /// One fixed-step history window. A read: permitted for read-only tokens.
    pub(super) async fn get_vitals_history_impl(
        &self,
        request: Request<VitalsHistoryRequest>,
    ) -> Result<Response<VitalsHistory>, Status> {
        let range = history_range(request.get_ref().range)?;
        let handle = self.enabled_monitor("GetVitalsHistory")?;
        // A short in-memory read under the monitor's lock; nothing here blocks.
        let view = handle.history(range);
        log::debug!("GetVitalsHistory: {range:?} at revision {}", view.revision);
        Ok(Response::new(history_msg(&view)))
    }

    // ── WatchProcesses / SignalProcess ───────────────────────────────────────

    /// Stream the process table through the request's query. A read: permitted for read-only
    /// tokens, whose rows never carry a command line.
    pub(super) async fn watch_processes_impl(
        &self,
        request: Request<WatchProcessesRequest>,
    ) -> Result<Response<ProcessSnapshotStream>, Status> {
        const RPC: &str = "WatchProcesses";
        let token = request
            .extensions()
            .get::<crate::auth::SessionToken>()
            .map(|t| t.0.clone());
        let redact = redact_commands(&request, RPC);
        let query = process_query(request.get_ref())?;
        let handle = self.enabled_monitor(RPC)?;
        // Disabled between the check and here: the subscription yields nothing, so the stream
        // sends its terminal snapshot and ends OK.
        let subscription = handle.watch_processes(query.clone());
        log::info!(
            "{RPC}: opened (sort {:?}, reverse {}, limit {}, pinned {}, redacted {redact})",
            query.sort,
            query.reverse,
            query.limit,
            query.pinned.is_some(),
        );
        let watch = ProcessWatch {
            redact,
            auth: TokenCheck {
                token,
                validate: Arc::new(|t: &str| crate::ipc::validate_session_token(t)),
            },
            token_recheck: TOKEN_RECHECK_INTERVAL,
        };
        Ok(Response::new(Box::pin(start_process_watch(
            watch,
            subscription,
        ))))
    }

    /// Signal one process named by `(pid, start time)`. MUTATING: read-only tokens are
    /// refused. A refusal by the monitor is an `ok: false` ack, never a transport error.
    pub(super) async fn signal_process_impl(
        &self,
        request: Request<SignalProcessRequest>,
    ) -> Result<Response<ProtoAck>, Status> {
        helpers::reject_if_read_only(&request, "SignalProcess")?;
        const RPC: &str = "SignalProcess";
        let req = request.into_inner();
        let signal = process_signal(req.signal)?;
        if req.pid == 0 {
            log::info!("{RPC}: pid 0 → invalid_argument");
            return Err(Status::invalid_argument("pid: must not be 0"));
        }
        let handle = self.enabled_monitor(RPC)?;
        // `None`: switched off since the check (or the blocking lookup panicked, logged there).
        let Some(result) = handle
            .signal_process(req.pid, req.start_unix_s, signal)
            .await
        else {
            return Err(monitoring_disabled(RPC));
        };
        let pid = req.pid;
        Ok(Response::new(match result {
            Ok(()) => {
                log::info!("{RPC}: sent {signal:?} to pid {pid}");
                ProtoAck {
                    ok: true,
                    ..ProtoAck::default()
                }
            }
            Err(refusal) => {
                log::info!("{RPC}: {signal:?} to pid {pid} refused: {refusal}");
                ProtoAck {
                    ok: false,
                    error: refusal_text(refusal).to_owned(),
                    info: String::new(),
                }
            }
        }))
    }
}

// ─── Request decoding ─────────────────────────────────────────────────────────

/// The demand tier a `WatchVitals` request asks for. Unspecified detail is a summary; a `FULL`
/// interval of 0 is the default, anything else is clamped (never rejected). An unknown detail
/// value is `invalid_argument`.
fn vitals_tier(req: &WatchVitalsRequest) -> Result<Tier, Status> {
    match VitalsDetail::try_from(req.detail) {
        Ok(VitalsDetail::Unspecified | VitalsDetail::Summary) => Ok(Tier::Summary),
        Ok(VitalsDetail::Full) => {
            let interval = match req.interval_ms {
                0 => DEFAULT_FULL_INTERVAL,
                ms => {
                    Duration::from_millis(u64::from(ms)).clamp(MIN_FULL_INTERVAL, MAX_FULL_INTERVAL)
                }
            };
            Ok(Tier::Full { interval })
        }
        Err(_) => {
            log::info!(
                "WatchVitals: unknown detail {} → invalid_argument",
                req.detail
            );
            Err(Status::invalid_argument("detail: unknown value"))
        }
    }
}

/// The model's range for a wire value; unspecified or unknown is `invalid_argument`.
fn history_range(raw: i32) -> Result<monitor::HistoryRange, Status> {
    match HistoryRange::try_from(raw) {
        Ok(HistoryRange::HistoryRange1m) => Ok(monitor::HistoryRange::OneMinute),
        Ok(HistoryRange::HistoryRange5m) => Ok(monitor::HistoryRange::FiveMinutes),
        Ok(HistoryRange::HistoryRange15m) => Ok(monitor::HistoryRange::FifteenMinutes),
        Ok(HistoryRange::HistoryRange1h) => Ok(monitor::HistoryRange::OneHour),
        Ok(HistoryRange::Unspecified) | Err(_) => {
            log::info!("GetVitalsHistory: range {raw} → invalid_argument");
            Err(Status::invalid_argument("range: unspecified or unknown"))
        }
    }
}

/// The monitor query a `WatchProcesses` request asks for. Limit 0 is the default and anything
/// above the ceiling is clamped; an unspecified sort is cpu; pinned pid 0 is none; interval 0 is
/// the floor, anything else is left for the monitor to clamp. An unknown sort, an oversized
/// filter or one with control characters is `invalid_argument`.
fn process_query(req: &WatchProcessesRequest) -> Result<ProcessQuery, Status> {
    use crate::proto::ProcessSort as Wire;
    let sort = match Wire::try_from(req.sort) {
        Ok(Wire::Unspecified | Wire::Cpu) => monitor::ProcessSort::Cpu,
        Ok(Wire::Memory) => monitor::ProcessSort::Memory,
        Ok(Wire::Pid) => monitor::ProcessSort::Pid,
        Ok(Wire::Name) => monitor::ProcessSort::Name,
        Err(_) => {
            log::info!(
                "WatchProcesses: unknown sort {} → invalid_argument",
                req.sort
            );
            return Err(Status::invalid_argument("sort: unknown value"));
        }
    };
    if req.filter.len() > MAX_PROCESS_FILTER_BYTES {
        log::info!(
            "WatchProcesses: filter of {} bytes → invalid_argument",
            req.filter.len()
        );
        return Err(Status::invalid_argument(format!(
            "filter: at most {MAX_PROCESS_FILTER_BYTES} bytes"
        )));
    }
    if req.filter.chars().any(char::is_control) {
        log::info!("WatchProcesses: filter with control characters → invalid_argument");
        return Err(Status::invalid_argument(
            "filter: control characters are not allowed",
        ));
    }
    let limit = match req.limit {
        0 => DEFAULT_PROCESS_LIMIT,
        n => n.min(MAX_PROCESS_LIMIT),
    };
    let pinned = (req.pinned_pid != 0).then_some((req.pinned_pid, req.pinned_start_unix_s));
    let interval = match req.interval_ms {
        0 => MIN_PROCESS_INTERVAL,
        ms => Duration::from_millis(u64::from(ms)),
    };
    Ok(ProcessQuery {
        sort,
        reverse: req.reverse,
        filter: req.filter.clone(),
        limit,
        pinned,
        interval,
    })
}

/// Whether a stream's rows lose their command line: a read-only token, or no read-only flag at
/// all (fail closed — the auth layer always sets it, so its absence is that layer's bug).
fn redact_commands<T>(request: &Request<T>, rpc: &str) -> bool {
    if request
        .extensions()
        .get::<crate::auth::SessionReadOnly>()
        .is_none()
    {
        log::warn!(
            "{rpc}: SessionReadOnly extension absent (auth layer bug) → redacting command lines"
        );
    }
    helpers::session_is_read_only(request)
}

/// The monitor's signal for a wire value; unspecified or unknown is `invalid_argument`.
fn process_signal(raw: i32) -> Result<monitor::ProcessSignal, Status> {
    use crate::proto::ProcessSignal as Wire;
    match Wire::try_from(raw) {
        Ok(Wire::Term) => Ok(monitor::ProcessSignal::Term),
        Ok(Wire::Kill) => Ok(monitor::ProcessSignal::Kill),
        Ok(Wire::Int) => Ok(monitor::ProcessSignal::Int),
        Ok(Wire::Hup) => Ok(monitor::ProcessSignal::Hup),
        Ok(Wire::Unspecified) | Err(_) => {
            log::info!("SignalProcess: signal {raw} → invalid_argument");
            Err(Status::invalid_argument("signal: unspecified or unknown"))
        }
    }
}

/// The caller's short, generic text for a refusal; the log keeps the detail.
fn refusal_text(refusal: SignalRefusal) -> &'static str {
    match refusal {
        SignalRefusal::NotFound => "process not found",
        SignalRefusal::Recycled => "process changed since it was listed",
        SignalRefusal::Forbidden => "not allowed",
        SignalRefusal::NotPermitted(_) => "permission denied by the operating system",
    }
}

// ─── Model → proto ────────────────────────────────────────────────────────────

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn host_msg(info: &monitor::HostInfo) -> crate::proto::HostInfo {
    crate::proto::HostInfo {
        hostname: info.hostname.clone(),
        os_name: info.os_name.clone(),
        os_version: info.os_version.clone(),
        kernel_version: info.kernel.clone(),
        arch: info.arch.clone(),
        cpu_brand: info.cpu_brand.clone(),
        logical_cores: info.logical_cores,
        physical_cores: info.physical_cores,
        memory_total_bytes: info.total_memory_bytes,
        boot_unix_s: to_i64(info.boot_time_unix_s),
    }
}

fn disk_msg(disk: &monitor::DiskSpace) -> crate::proto::DiskVolume {
    crate::proto::DiskVolume {
        mount: disk.mount.clone(),
        fs: disk.fs.clone(),
        total_bytes: disk.total,
        used_bytes: disk.used,
    }
}

/// The fullest volume by `used / total` (exact, by cross-multiplication); the first on a tie.
/// A zero-sized volume ranks as empty.
fn fullest(disks: &[monitor::DiskSpace]) -> Option<&monitor::DiskSpace> {
    let ratio = |d: &monitor::DiskSpace| {
        if d.total == 0 {
            (0, 1)
        } else {
            (u128::from(d.used), u128::from(d.total))
        }
    };
    disks.iter().fold(None, |best, disk| match best {
        Some(b) => {
            let (bu, bt) = ratio(b);
            let (du, dt) = ratio(disk);
            if du * bt > bu * dt {
                Some(disk)
            } else {
                Some(b)
            }
        }
        None => Some(disk),
    })
}

/// One sample on the wire. A [`Tier::Summary`] stream carries no per-core figures, no
/// temperatures, and only the fullest disk.
fn sample_msg(s: &monitor::VitalsSample, tier: Tier) -> crate::proto::VitalsSample {
    let summary = matches!(tier, Tier::Summary);
    let disks = if summary {
        fullest(&s.disks).map(disk_msg).into_iter().collect()
    } else {
        s.disks.iter().map(disk_msg).collect()
    };
    crate::proto::VitalsSample {
        unix_ms: to_i64(s.unix_ms),
        cpu_percent: s.cpu_percent,
        cpu_user_percent: s.cpu_user_percent,
        cpu_system_percent: s.cpu_system_percent,
        cpu_iowait_percent: s.cpu_iowait_percent,
        core_percent: if summary {
            Vec::new()
        } else {
            s.per_core_percent.clone()
        },
        load: s.load.map(|l| crate::proto::LoadAverage {
            load_1: l.one,
            load_5: l.five,
            load_15: l.fifteen,
        }),
        cpu_freq_mhz: s.cpu_freq_mhz.map(|f| u32::try_from(f).unwrap_or(u32::MAX)),
        memory: Some(crate::proto::Memory {
            total_bytes: s.memory.total,
            used_bytes: s.memory.used,
            cache_bytes: s.memory.cache,
            free_bytes: s.memory.free,
            available_bytes: s.memory.available,
        }),
        swap_used_bytes: s.swap_used,
        swap_total_bytes: s.swap_total,
        disks,
        disk_read_bps: s.disk_read_bps,
        disk_write_bps: s.disk_write_bps,
        net: s.network.as_ref().map(|n| crate::proto::NetInterface {
            name: n.name.clone(),
            address: n.address.clone(),
            rx_bps: n.rx_bps,
            tx_bps: n.tx_bps,
            rx_total_bytes: n.rx_total,
            tx_total_bytes: n.tx_total,
        }),
        temperatures: if summary {
            Vec::new()
        } else {
            s.temperatures
                .iter()
                .map(|t| crate::proto::Temperature {
                    label: t.label.clone(),
                    celsius: t.celsius,
                })
                .collect()
        },
        process_count: s.process_count,
        running_count: s.running_count,
    }
}

/// A history window on the wire. A series with no data anywhere in the window (empty, or NaN
/// in every slot — e.g. `cpu_system` on a host without a cpu split) is omitted.
fn history_msg(view: &monitor::HistoryView) -> VitalsHistory {
    let s = &view.series;
    let all = [
        (SeriesKind::CpuTotal, &s.cpu_total),
        (SeriesKind::CpuSystem, &s.cpu_system),
        (SeriesKind::MemoryUsedFraction, &s.memory_used_fraction),
        (SeriesKind::NetRxBps, &s.net_rx_bps),
        (SeriesKind::NetTxBps, &s.net_tx_bps),
        (SeriesKind::DiskReadBps, &s.disk_read_bps),
        (SeriesKind::DiskWriteBps, &s.disk_write_bps),
    ];
    VitalsHistory {
        step_ms: u32::try_from(view.step_ms).unwrap_or(u32::MAX),
        end_unix_ms: to_i64(view.end_unix_ms),
        revision: view.revision,
        series: all
            .into_iter()
            .filter(|(_, values)| values.iter().any(|v| !v.is_nan()))
            .map(|(kind, values)| HistorySeries {
                kind: kind as i32,
                values: values.clone(),
            })
            .collect(),
    }
}

fn process_state_msg(state: monitor::ProcessState) -> crate::proto::ProcessState {
    use crate::proto::ProcessState as Wire;
    match state {
        monitor::ProcessState::Running => Wire::Running,
        monitor::ProcessState::Sleeping => Wire::Sleeping,
        monitor::ProcessState::DiskWait => Wire::DiskWait,
        monitor::ProcessState::Zombie => Wire::Zombie,
        monitor::ProcessState::Stopped => Wire::Stopped,
        monitor::ProcessState::Idle => Wire::Idle,
        monitor::ProcessState::Dead => Wire::Dead,
        monitor::ProcessState::Unknown => Wire::Unknown,
    }
}

/// One process on the wire; `redact` empties its command line.
fn process_row_msg(row: &monitor::ProcessRow, redact: bool) -> crate::proto::ProcessRow {
    crate::proto::ProcessRow {
        pid: row.pid,
        parent_pid: row.parent_pid,
        name: row.name.clone(),
        command: if redact {
            String::new()
        } else {
            row.command.clone()
        },
        user: row.user.clone(),
        cpu_percent: row.cpu_percent,
        memory_bytes: row.memory_bytes,
        threads: row.threads,
        start_unix_s: row.start_unix_s,
        cpu_time_ms: row.cpu_time_ms,
        state: process_state_msg(row.state) as i32,
    }
}

/// One process view on the wire; `redact` empties every row's command line.
fn process_snapshot_msg(snap: &monitor::ProcessSnapshot, redact: bool) -> ProcessSnapshot {
    ProcessSnapshot {
        revision: snap.revision,
        unix_ms: to_i64(snap.unix_ms),
        total: snap.total,
        running: snap.running,
        rows: snap
            .rows
            .iter()
            .map(|row| process_row_msg(row, redact))
            .collect(),
        disabled: false,
    }
}

// ─── Token re-check ───────────────────────────────────────────────────────────

/// The bearer token a stream was opened with and how to re-validate it.
struct TokenCheck {
    /// `None` fails closed on the first re-check, as in `WatchAgents` and the relay.
    token: Option<String>,
    validate: TokenValidator,
}

/// Re-validate on the blocking pool. `true` only for a token that is still valid; absence,
/// invalidity or any error fails closed. Mirrors `agent_ops::token_still_valid`.
async fn token_still_valid(rpc: &str, auth: &TokenCheck) -> bool {
    let Some(token) = auth.token.clone() else {
        log::warn!("{rpc}: no token to re-validate → failing closed");
        return false;
    };
    let validate = Arc::clone(&auth.validate);
    match tokio::task::spawn_blocking(move || validate(&token)).await {
        Ok(Ok(valid)) => valid,
        Ok(Err(e)) => {
            log::warn!("{rpc}: token re-validation DB error (failing closed): {e}");
            false
        }
        Err(e) => {
            log::warn!("{rpc}: token re-validation task panicked (failing closed): {e}");
            false
        }
    }
}

// ─── Watch stream ─────────────────────────────────────────────────────────────

/// A watch response stream: the loop's bounded channel, plus a one-shot fatal status that
/// overtakes it, so a revoked token ends the stream even ahead of messages a stalled client
/// never read. The same shape as `agent_ops::WatchStream`, generic over the message.
struct WatchStream<T> {
    rx: mpsc::Receiver<T>,
    /// `None` once it resolved or its sender went away without a status.
    fatal: Option<oneshot::Receiver<Status>>,
    /// The fatal status was delivered: the stream is over.
    done: bool,
}

impl<T> futures::Stream for WatchStream<T> {
    type Item = Result<T, Status>;

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
        this.rx.poll_recv(cx).map(|item| item.map(Ok))
    }
}

/// How a wait inside a watch loop ended.
enum Flow {
    Continue,
    /// The client went away.
    Gone,
    /// The token re-check failed.
    Revoked,
    /// Monitoring was switched off; the terminal message was delivered.
    Disabled,
}

/// Hand one message to the channel, waiting for room inside a `select!` beside the token
/// re-check and the client's departure, so a client that stops reading can neither suspend
/// the re-check nor hold the task forever.
async fn deliver<T>(
    rpc: &str,
    auth: &TokenCheck,
    tx: &mpsc::Sender<T>,
    recheck: &mut Interval,
    msg: T,
) -> Flow {
    loop {
        tokio::select! {
            () = tx.closed() => return Flow::Gone,
            _ = recheck.tick() => {
                if !token_still_valid(rpc, auth).await {
                    return Flow::Revoked;
                }
            }
            permit = tx.reserve() => {
                return match permit {
                    Ok(permit) => {
                        permit.send(msg);
                        Flow::Continue
                    }
                    Err(_) => Flow::Gone,
                };
            }
        }
    }
}

/// A token re-check interval whose immediate first tick is already consumed: the auth layer
/// validated the token at open.
async fn recheck_interval(period: Duration) -> Interval {
    let mut recheck = tokio::time::interval(period);
    recheck.set_missed_tick_behavior(MissedTickBehavior::Delay);
    recheck.tick().await;
    recheck
}

// ─── WatchVitals stream ───────────────────────────────────────────────────────

/// Everything one `WatchVitals` stream needs besides its subscription and channels.
struct VitalsWatch {
    /// For the host facts the first event carries.
    handle: MonitorHandle,
    tier: Tier,
    auth: TokenCheck,
    token_recheck: Duration,
}

/// Spawn the loop that forwards `subscription` to a fresh stream.
fn start_vitals_watch(
    w: VitalsWatch,
    subscription: VitalsSubscription,
) -> WatchStream<VitalsEvent> {
    let (tx, rx) = mpsc::channel(WATCH_CHANNEL_BOUND);
    let (fatal_tx, fatal_rx) = oneshot::channel();
    tokio::spawn(vitals_loop(w, subscription, tx, fatal_tx));
    WatchStream {
        rx,
        fatal: Some(fatal_rx),
        done: false,
    }
}

/// Forward samples until the client goes, the token is revoked, or monitoring is switched off
/// (one `disabled` event, then the stream ends OK). A lag is folded into the next sample's
/// event as `lagged`. The subscription, and with it its demand, is dropped when this returns.
async fn vitals_loop(
    w: VitalsWatch,
    mut subscription: VitalsSubscription,
    tx: mpsc::Sender<VitalsEvent>,
    fatal: oneshot::Sender<Status>,
) {
    const RPC: &str = "WatchVitals";
    let mut recheck = recheck_interval(w.token_recheck).await;
    let mut first = true;
    let mut lagged = 0u64;

    let flow = loop {
        let item = tokio::select! {
            () = tx.closed() => break Flow::Gone,
            _ = recheck.tick() => {
                if !token_still_valid(RPC, &w.auth).await {
                    break Flow::Revoked;
                }
                continue;
            }
            item = subscription.recv() => item,
        };
        let event = match item {
            VitalsItem::Sample(sample) => {
                let host = std::mem::take(&mut first).then(|| host_msg(&w.handle.host_info()));
                VitalsEvent {
                    revision: sample.revision,
                    host,
                    sample: Some(sample_msg(&sample, w.tier)),
                    disabled: false,
                    lagged: std::mem::take(&mut lagged),
                }
            }
            VitalsItem::Lagged(missed) => {
                log::debug!("{RPC}: client fell {missed} sample(s) behind");
                lagged = lagged.saturating_add(missed);
                continue;
            }
            VitalsItem::Disabled => {
                let terminal = VitalsEvent {
                    disabled: true,
                    ..VitalsEvent::default()
                };
                match deliver(RPC, &w.auth, &tx, &mut recheck, terminal).await {
                    Flow::Continue => break Flow::Disabled,
                    ended => break ended,
                }
            }
        };
        match deliver(RPC, &w.auth, &tx, &mut recheck, event).await {
            Flow::Continue => {}
            ended => break ended,
        }
    };
    drop(subscription);
    match flow {
        Flow::Revoked => {
            log::warn!("{RPC}: token no longer valid — ending stream");
            let _ = fatal.send(Status::unauthenticated("session token no longer valid"));
        }
        Flow::Gone => log::debug!("{RPC}: client went away — ending stream"),
        Flow::Disabled => log::info!("{RPC}: monitoring switched off — stream ended"),
        Flow::Continue => {}
    }
}

// ─── WatchProcesses stream ────────────────────────────────────────────────────

/// Everything one `WatchProcesses` stream needs besides its subscription and channels.
struct ProcessWatch {
    /// Decided once at open: every row's command line is emptied.
    redact: bool,
    auth: TokenCheck,
    token_recheck: Duration,
}

/// Spawn the loop that forwards `subscription` to a fresh stream.
fn start_process_watch(
    w: ProcessWatch,
    subscription: ProcessSubscription,
) -> WatchStream<ProcessSnapshot> {
    let (tx, rx) = mpsc::channel(WATCH_CHANNEL_BOUND);
    let (fatal_tx, fatal_rx) = oneshot::channel();
    tokio::spawn(process_loop(w, subscription, tx, fatal_tx));
    WatchStream {
        rx,
        fatal: Some(fatal_rx),
        done: false,
    }
}

/// Forward snapshots until the client goes, the token is revoked, or monitoring is switched off
/// (one `disabled` snapshot, then the stream ends OK). A snapshot the client was too slow for is
/// replaced by the newest one inside the subscription. The subscription, and with it its
/// demand, is dropped when this returns.
async fn process_loop(
    w: ProcessWatch,
    mut subscription: ProcessSubscription,
    tx: mpsc::Sender<ProcessSnapshot>,
    fatal: oneshot::Sender<Status>,
) {
    const RPC: &str = "WatchProcesses";
    let mut recheck = recheck_interval(w.token_recheck).await;

    let flow = loop {
        let item = tokio::select! {
            () = tx.closed() => break Flow::Gone,
            _ = recheck.tick() => {
                if !token_still_valid(RPC, &w.auth).await {
                    break Flow::Revoked;
                }
                continue;
            }
            item = subscription.recv() => item,
        };
        let Some(snapshot) = item else {
            let terminal = ProcessSnapshot {
                disabled: true,
                ..ProcessSnapshot::default()
            };
            match deliver(RPC, &w.auth, &tx, &mut recheck, terminal).await {
                Flow::Continue => break Flow::Disabled,
                ended => break ended,
            }
        };
        let msg = process_snapshot_msg(&snapshot, w.redact);
        match deliver(RPC, &w.auth, &tx, &mut recheck, msg).await {
            Flow::Continue => {}
            ended => break ended,
        }
    };
    drop(subscription);
    match flow {
        Flow::Revoked => {
            log::warn!("{RPC}: token no longer valid — ending stream");
            let _ = fatal.send(Status::unauthenticated("session token no longer valid"));
        }
        Flow::Gone => log::debug!("{RPC}: client went away — ending stream"),
        Flow::Disabled => log::info!("{RPC}: monitoring switched off — stream ended"),
        Flow::Continue => {}
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::watch;
    use tokio::time::Instant;
    use tokio_stream::StreamExt;

    use crate::auth::SessionReadOnly;
    use crate::monitor::{
        DemandLevel, DiskReading, HostSource, InterfaceReading, LoadAverage, Memory, RawProcess,
        RawReading, ReadSpec, SourceFactory,
    };
    use crate::proto::ProcessSignal as WireSignal;
    use crate::proto::ProcessSort as WireSort;

    use super::*;

    /// Per-message wait in paused time: past a full token re-check interval, so a missing
    /// message is a timeout rather than a hang.
    const WAIT: Duration = Duration::from_secs(120);

    /// A scripted host: four cores, two disks (`/data` the fullest), two sensors, one NIC.
    struct FakeHost {
        reads: u64,
    }

    impl HostSource for FakeHost {
        fn host_info(&mut self) -> anyhow::Result<monitor::HostInfo> {
            Ok(monitor::HostInfo {
                hostname: "fake-host".into(),
                os_name: "FakeOS".into(),
                kernel: "6.0".into(),
                logical_cores: 4,
                physical_cores: Some(2),
                boot_time_unix_s: 1_700_000_000,
                total_memory_bytes: 1_000,
                ..monitor::HostInfo::default()
            })
        }

        fn read(&mut self, _spec: ReadSpec) -> anyhow::Result<RawReading> {
            self.reads += 1;
            let disk = |mount: &str, device: &str, used: u64| DiskReading {
                mount: mount.into(),
                fs: "ext4".into(),
                device: device.into(),
                total: 100,
                used,
                is_dir: true,
                ..DiskReading::default()
            };
            Ok(RawReading {
                cpu_percent: 25.0,
                per_core_percent: vec![10.0, 20.0, 30.0, 40.0],
                load: Some(LoadAverage {
                    one: 1.0,
                    five: 0.5,
                    fifteen: 0.25,
                }),
                memory: Memory::without_cache(1_000, 600),
                disks: vec![
                    disk("/", "sda1", 50),
                    disk("/data", "sdb1", 90),
                    disk("/boot", "sda2", 10),
                ],
                interfaces: vec![InterfaceReading {
                    name: "eth0".into(),
                    rx_total: self.reads * 1_000,
                    physical: true,
                    ..InterfaceReading::default()
                }],
                // Returned whatever the spec asks: the handler trims for SUMMARY itself.
                temperatures: vec![
                    ("nvme".into(), Some(40.0)),
                    ("Package id 0".into(), Some(55.0)),
                ],
                ..RawReading::default()
            })
        }
    }

    /// An enabled monitor over [`FakeHost`]; keep the shutdown sender alive for the test.
    fn enabled_monitor() -> (MonitorHandle, watch::Sender<bool>) {
        let factory: SourceFactory =
            Arc::new(|| Ok(Box::new(FakeHost { reads: 0 }) as Box<dyn HostSource>));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let handle = MonitorHandle::with_source(factory, shutdown_rx);
        handle.set_enabled(true);
        (handle, shutdown)
    }

    fn service(handle: &MonitorHandle) -> MuxrService {
        MuxrService::new().with_monitor(handle.clone())
    }

    fn vitals_req(detail: VitalsDetail, interval_ms: u32) -> Request<WatchVitalsRequest> {
        Request::new(WatchVitalsRequest {
            detail: detail as i32,
            interval_ms,
        })
    }

    fn history_req(range: HistoryRange) -> Request<VitalsHistoryRequest> {
        Request::new(VitalsHistoryRequest {
            range: range as i32,
        })
    }

    async fn recv<S>(rx: &mut S) -> Option<Result<VitalsEvent, Status>>
    where
        S: futures::Stream<Item = Result<VitalsEvent, Status>> + Unpin,
    {
        tokio::time::timeout(WAIT, rx.next())
            .await
            .expect("the stream must answer in time")
    }

    async fn event<S>(rx: &mut S) -> VitalsEvent
    where
        S: futures::Stream<Item = Result<VitalsEvent, Status>> + Unpin,
    {
        recv(rx)
            .await
            .expect("an event, not the end")
            .expect("an event, not an error")
    }

    async fn open(service: &MuxrService, detail: VitalsDetail, ms: u32) -> VitalsEventStream {
        service
            .watch_vitals_impl(vitals_req(detail, ms))
            .await
            .expect("the watch opens")
            .into_inner()
    }

    /// A stream over `handle` whose token stays valid exactly as long as `valid` holds.
    fn revocable(
        handle: &MonitorHandle,
        tier: Tier,
        valid: Arc<AtomicBool>,
        recheck: Duration,
    ) -> WatchStream<VitalsEvent> {
        let watch = VitalsWatch {
            handle: handle.clone(),
            tier,
            auth: TokenCheck {
                token: Some("tok".into()),
                validate: Arc::new(move |_| Ok(valid.load(Ordering::SeqCst))),
            },
            token_recheck: recheck,
        };
        start_vitals_watch(watch, handle.subscribe(tier))
    }

    async fn idle_demand(handle: &MonitorHandle) {
        tokio::time::timeout(WAIT, async {
            while handle.demand() != DemandLevel::IDLE {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the stream task must drop its subscription");
    }

    // ── Preconditions ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn both_rpcs_fail_precondition_without_a_monitor_handle() {
        let service = MuxrService::new();
        let err = service
            .watch_vitals_impl(vitals_req(VitalsDetail::Summary, 0))
            .await
            .err()
            .expect("refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let err = service
            .get_vitals_history_impl(history_req(HistoryRange::HistoryRange1m))
            .await
            .expect_err("refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn both_rpcs_fail_precondition_while_monitoring_is_off() {
        let (handle, _shutdown) = enabled_monitor();
        handle.set_enabled(false);
        let service = service(&handle);
        let err = service
            .watch_vitals_impl(vitals_req(VitalsDetail::Full, 0))
            .await
            .err()
            .expect("refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let err = service
            .get_vitals_history_impl(history_req(HistoryRange::HistoryRange5m))
            .await
            .expect_err("refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    // ── WatchVitals ───────────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn the_first_event_carries_the_host_and_revisions_increase() {
        let (handle, _shutdown) = enabled_monitor();
        let mut rx = open(&service(&handle), VitalsDetail::Full, 2_000).await;

        let first = event(&mut rx).await;
        let host = first.host.expect("the first event carries the host");
        assert_eq!(host.hostname, "fake-host");
        assert_eq!(host.kernel_version, "6.0");
        assert_eq!(host.physical_cores, Some(2));
        assert_eq!(host.boot_unix_s, 1_700_000_000);
        assert!(first.sample.is_some());
        assert!(!first.disabled);
        assert_eq!(first.lagged, 0);

        let mut revision = first.revision;
        assert!(revision > 0);
        for _ in 0..3 {
            let next = event(&mut rx).await;
            assert!(next.host.is_none(), "only the first event carries the host");
            assert!(next.sample.is_some());
            assert!(
                next.revision > revision,
                "{} after {revision}",
                next.revision
            );
            revision = next.revision;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn summary_trims_cores_temperatures_and_all_but_the_fullest_disk() {
        let (handle, _shutdown) = enabled_monitor();
        let mut rx = open(&service(&handle), VitalsDetail::Unspecified, 0).await;
        let sample = event(&mut rx).await.sample.expect("a sample");
        assert!(sample.core_percent.is_empty());
        assert!(sample.temperatures.is_empty());
        let mounts: Vec<_> = sample.disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, ["/data"]);
        assert_eq!(sample.cpu_percent, 25.0);
        assert!(sample.memory.is_some() && sample.load.is_some() && sample.net.is_some());
        assert_eq!(handle.demand().interval, monitor::SUMMARY_INTERVAL);
        assert!(!handle.demand().full);
    }

    #[tokio::test(start_paused = true)]
    async fn full_carries_every_core_temperature_and_disk() {
        let (handle, _shutdown) = enabled_monitor();
        let mut rx = open(&service(&handle), VitalsDetail::Full, 0).await;
        let sample = event(&mut rx).await.sample.expect("a sample");
        assert_eq!(sample.core_percent, [10.0, 20.0, 30.0, 40.0]);
        let labels: Vec<_> = sample
            .temperatures
            .iter()
            .map(|t| t.label.as_str())
            .collect();
        assert_eq!(labels, ["Package id 0", "nvme"], "package sensor first");
        assert_eq!(sample.disks.len(), 3);
        let memory = sample.memory.expect("memory");
        assert_eq!(
            memory.used_bytes + memory.cache_bytes + memory.free_bytes,
            memory.total_bytes
        );
        assert_eq!(handle.demand().interval, DEFAULT_FULL_INTERVAL);
        assert!(handle.demand().full);
    }

    #[test]
    fn the_full_interval_defaults_and_is_clamped_never_rejected() {
        let tier = |detail: VitalsDetail, ms| {
            vitals_tier(&vitals_req(detail, ms).into_inner()).expect("a known detail")
        };
        let full = |ms: u64| Tier::Full {
            interval: Duration::from_millis(ms),
        };
        assert_eq!(tier(VitalsDetail::Full, 0), full(3_000), "0 = the default");
        assert_eq!(
            tier(VitalsDetail::Full, 500),
            full(2_000),
            "up to the floor"
        );
        assert_eq!(tier(VitalsDetail::Full, 4_000), full(4_000));
        assert_eq!(
            tier(VitalsDetail::Full, 600_000),
            full(10_000),
            "down to the ceiling"
        );
        assert_eq!(
            tier(VitalsDetail::Summary, 500),
            Tier::Summary,
            "interval ignored"
        );
        assert_eq!(tier(VitalsDetail::Unspecified, 0), Tier::Summary);
        let unknown = vitals_tier(&WatchVitalsRequest {
            detail: 99,
            interval_ms: 0,
        })
        .expect_err("unknown detail");
        assert_eq!(unknown.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test(start_paused = true)]
    async fn a_clamped_interval_reaches_the_sampler() {
        let (handle, _shutdown) = enabled_monitor();
        let mut rx = open(&service(&handle), VitalsDetail::Full, 100).await;
        let _ = event(&mut rx).await;
        assert_eq!(handle.demand().interval, MIN_FULL_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_only_token_may_call_both() {
        let (handle, _shutdown) = enabled_monitor();
        let service = service(&handle);
        let mut req = vitals_req(VitalsDetail::Summary, 0);
        req.extensions_mut().insert(SessionReadOnly(true));
        let mut rx = service
            .watch_vitals_impl(req)
            .await
            .expect("permitted")
            .into_inner();
        assert!(event(&mut rx).await.sample.is_some());

        let mut req = history_req(HistoryRange::HistoryRange1m);
        req.extensions_mut().insert(SessionReadOnly(true));
        service
            .get_vitals_history_impl(req)
            .await
            .expect("permitted");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_without_auth_extensions_ends_unauthenticated_at_the_first_recheck() {
        let (handle, _shutdown) = enabled_monitor();
        let opened = Instant::now();
        let mut rx = open(&service(&handle), VitalsDetail::Summary, 0).await;
        let err = loop {
            match recv(&mut rx).await.expect("an error, not a silent end") {
                Ok(ev) => assert!(ev.sample.is_some()),
                Err(status) => break status,
            }
        };
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        let after = Instant::now() - opened;
        assert!(
            after >= TOKEN_RECHECK_INTERVAL && after < TOKEN_RECHECK_INTERVAL * 2,
            "ended at the first re-check, {after:?} after open"
        );
        assert!(recv(&mut rx).await.is_none(), "the stream ends after it");
        idle_demand(&handle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_revoked_token_ends_the_stream_unauthenticated() {
        let (handle, _shutdown) = enabled_monitor();
        let valid = Arc::new(AtomicBool::new(true));
        let mut rx = revocable(
            &handle,
            Tier::Summary,
            Arc::clone(&valid),
            Duration::from_secs(1),
        );
        let _ = event(&mut rx).await;
        let _ = event(&mut rx).await;

        valid.store(false, Ordering::SeqCst);
        let err = loop {
            match recv(&mut rx).await.expect("an error, not a silent end") {
                Ok(_) => {}
                Err(status) => break status,
            }
        };
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert!(recv(&mut rx).await.is_none(), "the stream ends after it");
        idle_demand(&handle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn disabling_mid_stream_sends_one_disabled_event_then_ends_ok() {
        let (handle, _shutdown) = enabled_monitor();
        let mut rx = open(&service(&handle), VitalsDetail::Full, 2_000).await;
        let _ = event(&mut rx).await;

        handle.set_enabled(false);
        let terminal = loop {
            let ev = event(&mut rx).await;
            if ev.disabled {
                break ev;
            }
            assert!(ev.sample.is_some(), "a sample published before the switch");
        };
        assert!(terminal.sample.is_none() && terminal.host.is_none());
        assert_eq!(terminal.revision, 0);
        assert!(recv(&mut rx).await.is_none(), "ends without an error");
    }

    #[tokio::test(start_paused = true)]
    async fn the_subscription_is_released_when_the_client_goes_away() {
        let (handle, _shutdown) = enabled_monitor();
        let mut rx = open(&service(&handle), VitalsDetail::Full, 2_000).await;
        let _ = event(&mut rx).await;
        assert!(handle.demand().full);
        drop(rx);
        idle_demand(&handle).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_falls_behind_is_told_how_many_samples_it_skipped() {
        let (handle, _shutdown) = enabled_monitor();
        let mut rx = revocable(
            &handle,
            Tier::Full {
                interval: MIN_FULL_INTERVAL,
            },
            Arc::new(AtomicBool::new(true)),
            Duration::from_secs(3600),
        );
        let first = event(&mut rx).await;
        // Stop reading long enough to fill this stream's channel and overrun the
        // subscription behind it.
        let stall = (WATCH_CHANNEL_BOUND + monitor::VITALS_CHANNEL_CAPACITY + 8) as u32;
        tokio::time::sleep(MIN_FULL_INTERVAL * stall).await;

        let mut previous = first.revision;
        for _ in 0..WATCH_CHANNEL_BOUND + 4 {
            let ev = event(&mut rx).await;
            assert!(ev.sample.is_some(), "a lag never travels alone");
            assert_eq!(
                ev.revision,
                previous + ev.lagged + 1,
                "the gap is exactly the lag"
            );
            if ev.lagged > 0 {
                return;
            }
            previous = ev.revision;
        }
        panic!("no event reported the lag");
    }

    // ── GetVitalsHistory ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn history_rejects_an_unspecified_or_unknown_range() {
        let (handle, _shutdown) = enabled_monitor();
        let service = service(&handle);
        for range in [HistoryRange::Unspecified as i32, 99] {
            let err = service
                .get_vitals_history_impl(Request::new(VitalsHistoryRequest { range }))
                .await
                .expect_err("rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "range {range}");
        }
    }

    #[test]
    fn history_series_share_one_length_keep_nan_and_omit_the_unavailable() {
        let nan = f32::NAN;
        let view = monitor::HistoryView {
            range: monitor::HistoryRange::OneMinute,
            step_ms: 2_000,
            end_unix_ms: 1_800_000_000_000,
            revision: 7,
            series: monitor::HistorySeries {
                cpu_total: vec![nan, 10.0, 20.0],
                cpu_system: Vec::new(),
                memory_used_fraction: vec![0.5, nan, 0.25],
                net_rx_bps: vec![nan, nan, nan],
                net_tx_bps: vec![1.0, 2.0, 3.0],
                disk_read_bps: vec![nan, nan, 4.0],
                disk_write_bps: vec![nan, 5.0, nan],
            },
        };
        let msg = history_msg(&view);
        assert_eq!(msg.step_ms, 2_000);
        assert_eq!(msg.end_unix_ms, 1_800_000_000_000);
        assert_eq!(msg.revision, 7);
        let kinds: Vec<_> = msg.series.iter().map(HistorySeries::kind).collect();
        assert_eq!(
            kinds,
            [
                SeriesKind::CpuTotal,
                SeriesKind::MemoryUsedFraction,
                SeriesKind::NetTxBps,
                SeriesKind::DiskReadBps,
                SeriesKind::DiskWriteBps,
            ],
            "the empty and the all-NaN series are absent"
        );
        assert!(msg.series.iter().all(|s| s.values.len() == 3));
        assert!(msg.series[0].values[0].is_nan(), "NaN passes through");
        assert_eq!(msg.series[0].values[1..], [10.0, 20.0]);

        // An all-NaN cpu_system (a platform without a split) is absent too.
        let mut split_less = view.clone();
        split_less.series.cpu_system = vec![nan; 3];
        assert!(
            history_msg(&split_less)
                .series
                .iter()
                .all(|s| s.kind() != SeriesKind::CpuSystem)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn history_reads_the_monitors_window() {
        let (handle, _shutdown) = enabled_monitor();
        let mut sub = handle.subscribe(Tier::Full {
            interval: MIN_FULL_INTERVAL,
        });
        for _ in 0..2 {
            assert!(matches!(sub.recv().await, VitalsItem::Sample(_)));
        }
        let history = service(&handle)
            .get_vitals_history_impl(history_req(HistoryRange::HistoryRange1m))
            .await
            .expect("history")
            .into_inner();
        assert_eq!(history.step_ms, 2_000);
        assert_eq!(history.end_unix_ms % 2_000, 0);
        assert!(history.revision >= 2);
        let kinds: Vec<_> = history.series.iter().map(HistorySeries::kind).collect();
        assert!(kinds.contains(&SeriesKind::CpuTotal));
        assert!(kinds.contains(&SeriesKind::MemoryUsedFraction));
        assert!(
            !kinds.contains(&SeriesKind::CpuSystem),
            "the fake host has no cpu split"
        );
        let cpu = &history.series[0].values;
        assert!(history.series.iter().all(|s| s.values.len() == 30));
        assert!(
            cpu.iter().any(|v| v.is_nan()),
            "slots before monitoring are NaN"
        );
        assert!(cpu.iter().any(|v| (v - 25.0).abs() < f32::EPSILON));
    }

    // ── WatchProcesses ────────────────────────────────────────────────────────

    /// The uid the process fake reports as muxrd's own.
    const OWN_UID: u32 = 1000;

    /// A scripted process table: pid 2 is ours (its command line shows), pid 3 is root's.
    struct FakeProcesses;

    impl HostSource for FakeProcesses {
        fn host_info(&mut self) -> anyhow::Result<monitor::HostInfo> {
            Ok(monitor::HostInfo::default())
        }

        fn read(&mut self, _spec: ReadSpec) -> anyhow::Result<RawReading> {
            Ok(RawReading::default())
        }

        fn read_processes(&mut self) -> anyhow::Result<Vec<RawProcess>> {
            Ok(vec![
                RawProcess {
                    pid: 2,
                    parent_pid: Some(1),
                    start_unix_s: 100,
                    name: "mine".into(),
                    cmd: vec!["/usr/bin/mine".into(), "--secret-flag".into()],
                    uid: Some(OWN_UID),
                    user: "alice".into(),
                    cpu_percent: 30.0,
                    memory_bytes: Some(4_096),
                    cpu_time_ms: 1_500,
                    state: monitor::ProcessState::Running,
                },
                RawProcess {
                    pid: 3,
                    parent_pid: Some(1),
                    start_unix_s: 200,
                    name: "sshd".into(),
                    cmd: vec!["/usr/sbin/sshd".into(), "-D".into()],
                    uid: Some(0),
                    user: "root".into(),
                    cpu_percent: 10.0,
                    memory_bytes: None,
                    cpu_time_ms: 20,
                    state: monitor::ProcessState::Sleeping,
                },
            ])
        }

        fn own_uid(&mut self) -> Option<u32> {
            Some(OWN_UID)
        }

        fn read_proc_status(&mut self, pid: u32) -> Option<String> {
            (pid == 3).then(|| "Name:\tsshd\nThreads:\t4\n".into())
        }
    }

    /// An enabled monitor over [`FakeProcesses`]; keep the shutdown sender alive for the test.
    fn process_monitor() -> (MonitorHandle, watch::Sender<bool>) {
        let factory: SourceFactory =
            Arc::new(|| Ok(Box::new(FakeProcesses) as Box<dyn HostSource>));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let handle = MonitorHandle::with_source(factory, shutdown_rx);
        handle.set_enabled(true);
        (handle, shutdown)
    }

    fn by_pid() -> WatchProcessesRequest {
        WatchProcessesRequest {
            sort: WireSort::Pid as i32,
            ..WatchProcessesRequest::default()
        }
    }

    /// `req` with the read-only flag set to `read_only`, or with no flag at all.
    fn with_flag<T>(req: T, read_only: Option<bool>) -> Request<T> {
        let mut req = Request::new(req);
        if let Some(ro) = read_only {
            req.extensions_mut().insert(SessionReadOnly(ro));
        }
        req
    }

    async fn open_processes(
        service: &MuxrService,
        req: Request<WatchProcessesRequest>,
    ) -> ProcessSnapshotStream {
        service
            .watch_processes_impl(req)
            .await
            .expect("the watch opens")
            .into_inner()
    }

    async fn recv_snapshot<S>(rx: &mut S) -> Option<Result<ProcessSnapshot, Status>>
    where
        S: futures::Stream<Item = Result<ProcessSnapshot, Status>> + Unpin,
    {
        tokio::time::timeout(WAIT, rx.next())
            .await
            .expect("the stream must answer in time")
    }

    async fn snapshot<S>(rx: &mut S) -> ProcessSnapshot
    where
        S: futures::Stream<Item = Result<ProcessSnapshot, Status>> + Unpin,
    {
        recv_snapshot(rx)
            .await
            .expect("a snapshot, not the end")
            .expect("a snapshot, not an error")
    }

    fn query(req: WatchProcessesRequest) -> Result<ProcessQuery, Status> {
        process_query(&req)
    }

    #[tokio::test]
    async fn the_process_rpcs_fail_precondition_without_a_monitor_handle() {
        let service = MuxrService::new();
        let err = service
            .watch_processes_impl(with_flag(by_pid(), Some(false)))
            .await
            .err()
            .expect("refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let err = service
            .signal_process_impl(with_flag(
                signal_req(4242, 0, WireSignal::Term),
                Some(false),
            ))
            .await
            .expect_err("refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn the_process_rpcs_fail_precondition_while_monitoring_is_off() {
        let (handle, _shutdown) = process_monitor();
        handle.set_enabled(false);
        let service = service(&handle);
        let err = service
            .watch_processes_impl(with_flag(by_pid(), Some(false)))
            .await
            .err()
            .expect("refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let err = service
            .signal_process_impl(with_flag(
                signal_req(4242, 0, WireSignal::Term),
                Some(false),
            ))
            .await
            .expect_err("refused");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn the_process_query_defaults_and_clamps() {
        let defaults = query(WatchProcessesRequest::default()).expect("valid");
        assert_eq!(
            defaults,
            ProcessQuery {
                sort: monitor::ProcessSort::Cpu,
                reverse: false,
                filter: String::new(),
                limit: DEFAULT_PROCESS_LIMIT,
                pinned: None,
                interval: MIN_PROCESS_INTERVAL,
            },
            "limit 0 = 50, unspecified sort = cpu, pinned pid 0 = none, interval 0 = the floor"
        );
        assert_eq!(DEFAULT_PROCESS_LIMIT, 50);

        let clamped = query(WatchProcessesRequest {
            limit: 1_000,
            ..WatchProcessesRequest::default()
        })
        .expect("valid");
        assert_eq!(clamped.limit, MAX_PROCESS_LIMIT);
        assert_eq!(MAX_PROCESS_LIMIT, 200);
        let kept = query(WatchProcessesRequest {
            limit: 7,
            ..WatchProcessesRequest::default()
        })
        .expect("valid");
        assert_eq!(kept.limit, 7);

        let explicit = query(WatchProcessesRequest {
            sort: WireSort::Name as i32,
            reverse: true,
            filter: "ssh".into(),
            pinned_pid: 3,
            pinned_start_unix_s: 200,
            interval_ms: 5_000,
            ..WatchProcessesRequest::default()
        })
        .expect("valid");
        assert_eq!(explicit.sort, monitor::ProcessSort::Name);
        assert!(explicit.reverse);
        assert_eq!(explicit.filter, "ssh");
        assert_eq!(explicit.pinned, Some((3, 200)), "pinned passed through");
        assert_eq!(explicit.interval, Duration::from_secs(5));

        for (wire, model) in [
            (WireSort::Cpu, monitor::ProcessSort::Cpu),
            (WireSort::Memory, monitor::ProcessSort::Memory),
            (WireSort::Pid, monitor::ProcessSort::Pid),
        ] {
            let q = query(WatchProcessesRequest {
                sort: wire as i32,
                ..WatchProcessesRequest::default()
            })
            .expect("valid");
            assert_eq!(q.sort, model);
        }

        let fast = query(WatchProcessesRequest {
            interval_ms: 100,
            ..WatchProcessesRequest::default()
        })
        .expect("valid");
        assert_eq!(
            fast.effective_interval(),
            MIN_PROCESS_INTERVAL,
            "left for the monitor to clamp"
        );
    }

    #[test]
    fn oversized_or_control_character_filters_and_unknown_sorts_are_rejected() {
        let filter = |f: &str| {
            query(WatchProcessesRequest {
                filter: f.into(),
                ..WatchProcessesRequest::default()
            })
        };
        assert!(filter(&"a".repeat(MAX_PROCESS_FILTER_BYTES)).is_ok());
        // 43 three-byte characters: 129 bytes, under 128 chars.
        for bad in [
            "a".repeat(MAX_PROCESS_FILTER_BYTES + 1),
            "€".repeat(43),
            "ss\nh".into(),
            "\u{1b}[31m".into(),
            "a\u{7f}".into(),
            "\u{9b}".into(),
        ] {
            let err = filter(&bad).expect_err("rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{bad:?}");
        }
        let err = query(WatchProcessesRequest {
            sort: 99,
            ..WatchProcessesRequest::default()
        })
        .expect_err("unknown sort");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn an_invalid_request_is_rejected_before_the_stream_opens() {
        let (handle, _shutdown) = process_monitor();
        let service = service(&handle);
        for req in [
            WatchProcessesRequest {
                sort: 42,
                ..WatchProcessesRequest::default()
            },
            WatchProcessesRequest {
                filter: "x\ty".into(),
                ..WatchProcessesRequest::default()
            },
        ] {
            let err = service
                .watch_processes_impl(with_flag(req, Some(false)))
                .await
                .err()
                .expect("rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_token_sees_the_command_line_of_its_own_uids_rows_only() {
        let (handle, _shutdown) = process_monitor();
        let mut rx = open_processes(&service(&handle), with_flag(by_pid(), Some(false))).await;
        let snap = snapshot(&mut rx).await;
        assert!(!snap.disabled);
        assert!(snap.revision > 0);
        assert!(snap.unix_ms > 0);
        assert_eq!((snap.total, snap.running), (2, 1));
        let pids: Vec<_> = snap.rows.iter().map(|r| r.pid).collect();
        assert_eq!(pids, [2, 3]);
        assert_eq!(snap.rows[0].command, "/usr/bin/mine --secret-flag");
        assert_eq!(snap.rows[1].command, "", "another uid's row has none");
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_only_token_sees_no_command_line_but_everything_else() {
        let (handle, _shutdown) = process_monitor();
        let mut rx = open_processes(&service(&handle), with_flag(by_pid(), Some(true))).await;
        let snap = snapshot(&mut rx).await;
        assert!(snap.rows.iter().all(|r| r.command.is_empty()));
        let mine = &snap.rows[0];
        assert_eq!(
            (
                mine.pid,
                mine.parent_pid,
                mine.name.as_str(),
                mine.user.as_str()
            ),
            (2, Some(1), "mine", "alice")
        );
        assert_eq!(mine.cpu_percent, 30.0);
        assert_eq!(mine.memory_bytes, Some(4_096));
        assert_eq!(mine.threads, None, "only the pinned row carries threads");
        assert_eq!((mine.start_unix_s, mine.cpu_time_ms), (100, 1_500));
        assert_eq!(mine.state(), crate::proto::ProcessState::Running);
        let theirs = &snap.rows[1];
        assert_eq!(
            (theirs.name.as_str(), theirs.user.as_str()),
            ("sshd", "root")
        );
        assert_eq!(theirs.memory_bytes, None, "a refused figure stays unset");
        assert_eq!(theirs.state(), crate::proto::ProcessState::Sleeping);
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_read_only_flag_redacts_every_command_line() {
        let (handle, _shutdown) = process_monitor();
        let mut rx = open_processes(&service(&handle), with_flag(by_pid(), None)).await;
        let snap = snapshot(&mut rx).await;
        assert_eq!(snap.rows.len(), 2);
        assert!(snap.rows.iter().all(|r| r.command.is_empty()));
        assert_eq!(snap.rows[0].name, "mine");
    }

    #[tokio::test(start_paused = true)]
    async fn the_limit_and_the_pinned_process_reach_the_monitor() {
        let (handle, _shutdown) = process_monitor();
        let service = service(&handle);
        let pinned = WatchProcessesRequest {
            limit: 1,
            pinned_pid: 3,
            pinned_start_unix_s: 200,
            ..by_pid()
        };
        let mut rx = open_processes(&service, with_flag(pinned, Some(false))).await;
        let snap = snapshot(&mut rx).await;
        let pids: Vec<_> = snap.rows.iter().map(|r| r.pid).collect();
        assert_eq!(pids, [2, 3], "the cut, then the pinned row");
        assert_eq!(snap.rows[1].threads, Some(4));

        let stale = WatchProcessesRequest {
            limit: 1,
            pinned_pid: 3,
            pinned_start_unix_s: 999,
            ..by_pid()
        };
        let mut rx = open_processes(&service, with_flag(stale, Some(false))).await;
        let pids: Vec<_> = snapshot(&mut rx).await.rows.iter().map(|r| r.pid).collect();
        assert_eq!(pids, [2], "another start time is another process");

        let cpu = WatchProcessesRequest {
            limit: 1,
            ..WatchProcessesRequest::default()
        };
        let mut rx = open_processes(&service, with_flag(cpu, Some(false))).await;
        let pids: Vec<_> = snapshot(&mut rx).await.rows.iter().map(|r| r.pid).collect();
        assert_eq!(pids, [2], "the default sort is cpu, highest first");
    }

    #[tokio::test(start_paused = true)]
    async fn a_process_stream_without_auth_extensions_ends_unauthenticated_at_the_first_recheck() {
        let (handle, _shutdown) = process_monitor();
        let opened = Instant::now();
        let mut rx = open_processes(&service(&handle), Request::new(by_pid())).await;
        let err = loop {
            match recv_snapshot(&mut rx)
                .await
                .expect("an error, not a silent end")
            {
                Ok(snap) => assert!(!snap.rows.is_empty()),
                Err(status) => break status,
            }
        };
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        let after = Instant::now() - opened;
        assert!(
            after >= TOKEN_RECHECK_INTERVAL && after < TOKEN_RECHECK_INTERVAL * 2,
            "ended at the first re-check, {after:?} after open"
        );
        assert!(
            recv_snapshot(&mut rx).await.is_none(),
            "the stream ends after it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_revoked_token_ends_the_process_stream_unauthenticated() {
        let (handle, _shutdown) = process_monitor();
        let valid = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&valid);
        let watch = ProcessWatch {
            redact: false,
            auth: TokenCheck {
                token: Some("tok".into()),
                validate: Arc::new(move |_| Ok(flag.load(Ordering::SeqCst))),
            },
            token_recheck: Duration::from_secs(1),
        };
        let subscription = handle.watch_processes(ProcessQuery::default());
        let mut rx = start_process_watch(watch, subscription);
        let _ = snapshot(&mut rx).await;
        let _ = snapshot(&mut rx).await;

        valid.store(false, Ordering::SeqCst);
        let err = loop {
            match recv_snapshot(&mut rx)
                .await
                .expect("an error, not a silent end")
            {
                Ok(_) => {}
                Err(status) => break status,
            }
        };
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert!(
            recv_snapshot(&mut rx).await.is_none(),
            "the stream ends after it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn disabling_mid_stream_sends_one_disabled_snapshot_then_ends_ok() {
        let (handle, _shutdown) = process_monitor();
        let mut rx = open_processes(&service(&handle), with_flag(by_pid(), Some(false))).await;
        let _ = snapshot(&mut rx).await;

        handle.set_enabled(false);
        let terminal = loop {
            let snap = snapshot(&mut rx).await;
            if snap.disabled {
                break snap;
            }
            assert!(
                !snap.rows.is_empty(),
                "a snapshot published before the switch"
            );
        };
        assert!(terminal.rows.is_empty());
        assert_eq!((terminal.revision, terminal.total), (0, 0));
        assert!(
            recv_snapshot(&mut rx).await.is_none(),
            "ends without an error"
        );
    }

    // ── SignalProcess ─────────────────────────────────────────────────────────

    /// Kills and reaps the child if a test fails before it does.
    struct Sleeper(std::process::Child);

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Sleeper {
        fn spawn() -> Self {
            Self(
                std::process::Command::new("sleep")
                    .arg("60")
                    .spawn()
                    .expect("spawn sleep"),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }

        fn alive(&mut self) -> bool {
            self.0.try_wait().expect("try_wait").is_none()
        }

        /// Waits (without blocking the runtime) until the child has exited.
        async fn exited(&mut self) -> std::process::ExitStatus {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(status) = self.0.try_wait().expect("try_wait") {
                        return status;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the child must exit")
        }
    }

    /// The start time sysinfo reports for `pid`, as `ProcessRow.start_unix_s` carries it.
    fn start_time_of(pid: u32) -> i64 {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let mut system = System::new().expect("sysinfo");
        let pid = Pid::from_u32(pid);
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        let process = system.process(pid).expect("the child is visible");
        i64::try_from(process.start_time()).expect("a start time")
    }

    fn signal_req(pid: u32, start_unix_s: i64, signal: WireSignal) -> SignalProcessRequest {
        SignalProcessRequest {
            pid,
            start_unix_s,
            signal: signal as i32,
        }
    }

    async fn signal(
        service: &MuxrService,
        req: SignalProcessRequest,
        read_only: Option<bool>,
    ) -> Result<ProtoAck, Status> {
        service
            .signal_process_impl(with_flag(req, read_only))
            .await
            .map(Response::into_inner)
    }

    #[tokio::test]
    async fn a_read_only_or_flagless_token_may_not_signal() {
        let (handle, _shutdown) = process_monitor();
        let service = service(&handle);
        let mut child = Sleeper::spawn();
        let start = start_time_of(child.pid());
        for read_only in [Some(true), None] {
            let err = signal(
                &service,
                signal_req(child.pid(), start, WireSignal::Kill),
                read_only,
            )
            .await
            .expect_err("denied");
            assert_eq!(err.code(), tonic::Code::PermissionDenied, "{read_only:?}");
        }
        assert!(child.alive(), "nothing was sent");
    }

    #[tokio::test]
    async fn a_wrong_start_time_is_refused_and_the_child_lives() {
        let (handle, _shutdown) = process_monitor();
        let mut child = Sleeper::spawn();
        let start = start_time_of(child.pid());
        let ack = signal(
            &service(&handle),
            signal_req(child.pid(), start + 1, WireSignal::Term),
            Some(false),
        )
        .await
        .expect("an ack");
        assert!(!ack.ok);
        assert_eq!(ack.error, "process changed since it was listed");
        assert!(ack.info.is_empty());
        assert!(child.alive());
    }

    #[tokio::test]
    async fn the_right_start_time_terminates_the_child() {
        let (handle, _shutdown) = process_monitor();
        let mut child = Sleeper::spawn();
        let start = start_time_of(child.pid());
        let ack = signal(
            &service(&handle),
            signal_req(child.pid(), start, WireSignal::Term),
            Some(false),
        )
        .await
        .expect("an ack");
        assert!(ack.ok, "{ack:?}");
        assert!(ack.error.is_empty() && ack.info.is_empty());
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(child.exited().await.signal(), Some(libc::SIGTERM));
    }

    #[tokio::test]
    async fn pid_one_is_not_allowed() {
        let (handle, _shutdown) = process_monitor();
        let ack = signal(
            &service(&handle),
            signal_req(1, 0, WireSignal::Term),
            Some(false),
        )
        .await
        .expect("an ack");
        assert!(!ack.ok);
        assert_eq!(ack.error, "not allowed");
    }

    #[tokio::test]
    async fn a_missing_pid_is_not_found() {
        let (handle, _shutdown) = process_monitor();
        let ack = signal(
            &service(&handle),
            signal_req(2_000_000_000, 0, WireSignal::Term),
            Some(false),
        )
        .await
        .expect("an ack");
        assert!(!ack.ok);
        assert_eq!(ack.error, "process not found");
    }

    #[tokio::test]
    async fn an_unspecified_or_unknown_signal_and_pid_zero_are_invalid() {
        let (handle, _shutdown) = process_monitor();
        let service = service(&handle);
        let mut child = Sleeper::spawn();
        let start = start_time_of(child.pid());
        for raw in [WireSignal::Unspecified as i32, 99] {
            let req = SignalProcessRequest {
                pid: child.pid(),
                start_unix_s: start,
                signal: raw,
            };
            let err = signal(&service, req, Some(false))
                .await
                .expect_err("rejected");
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "signal {raw}");
        }
        let err = signal(&service, signal_req(0, 0, WireSignal::Term), Some(false))
            .await
            .expect_err("rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(child.alive());
    }

    #[test]
    fn every_refusal_has_a_short_generic_text() {
        assert_eq!(refusal_text(SignalRefusal::NotFound), "process not found");
        assert_eq!(
            refusal_text(SignalRefusal::Recycled),
            "process changed since it was listed"
        );
        assert_eq!(refusal_text(SignalRefusal::Forbidden), "not allowed");
        assert_eq!(
            refusal_text(SignalRefusal::NotPermitted(libc::EPERM)),
            "permission denied by the operating system"
        );
    }
}
