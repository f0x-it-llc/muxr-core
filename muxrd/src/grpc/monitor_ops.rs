//! Host-vitals RPCs over [`crate::monitor::MonitorHandle`]: `WatchVitals` and `GetVitalsHistory`,
//! the model → proto mapping, and `WatchProcesses`/`SignalProcess` declared ahead of their card.
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
    self, DEFAULT_FULL_INTERVAL, MAX_FULL_INTERVAL, MIN_FULL_INTERVAL, MonitorHandle, Tier,
    VitalsItem, VitalsSubscription,
};
use crate::proto::{
    ActionAck as ProtoAck, HistoryRange, HistorySeries, ProcessSnapshot, SeriesKind,
    SignalProcessRequest, VitalsDetail, VitalsEvent, VitalsHistory, VitalsHistoryRequest,
    WatchProcessesRequest, WatchVitalsRequest,
};
use crate::relay::TOKEN_RECHECK_INTERVAL;

use super::MuxrService;

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

    /// Declared on the wire; implemented by the next card.
    pub(super) async fn watch_processes_impl(
        &self,
        _request: Request<WatchProcessesRequest>,
    ) -> Result<Response<ProcessSnapshotStream>, Status> {
        Err(Status::unimplemented("next card"))
    }

    /// Declared on the wire; implemented by the next card.
    pub(super) async fn signal_process_impl(
        &self,
        _request: Request<SignalProcessRequest>,
    ) -> Result<Response<ProtoAck>, Status> {
        Err(Status::unimplemented("next card"))
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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::watch;
    use tokio::time::Instant;
    use tokio_stream::StreamExt;

    use crate::auth::SessionReadOnly;
    use crate::monitor::{
        DemandLevel, DiskReading, HostSource, InterfaceReading, LoadAverage, Memory, RawReading,
        ReadSpec, SourceFactory,
    };

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

    #[tokio::test]
    async fn the_process_rpcs_are_declared_but_unimplemented() {
        let service = MuxrService::new();
        let err = service
            .watch_processes_impl(Request::new(WatchProcessesRequest::default()))
            .await
            .err()
            .expect("unimplemented");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
        let err = service
            .signal_process_impl(Request::new(SignalProcessRequest::default()))
            .await
            .expect_err("unimplemented");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
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
}
