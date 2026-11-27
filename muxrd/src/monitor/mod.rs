//! monitor — host vitals sampled at a demand-driven rate, fanned out, and kept as history.
//!
//! [`MonitorHandle`] is the switch and the read side; the sampler task runs only while enabled.

mod demand;
mod history;
mod model;
mod sampler;

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{broadcast, watch};

pub use demand::{
    DEFAULT_FULL_INTERVAL, DemandLevel, IDLE_INTERVAL, MAX_FULL_INTERVAL, MIN_FULL_INTERVAL,
    SUMMARY_INTERVAL,
};
pub use model::{
    DiskSpace, HistoryRange, HistorySeries, HistoryView, HostInfo, LoadAverage, Memory,
    NetInterface, Temperature, Tier, VitalsSample,
};
pub use sampler::{
    CpuTimes, DiskReading, HostSource, InterfaceReading, RawReading, ReadSpec, SysinfoSource,
};

use demand::{Demand, DemandGuard};
use history::History;

/// Samples buffered per subscriber before the slowest one starts losing the oldest. At the 2 s
/// floor that is half a minute of slack; a lagging subscriber sees [`VitalsItem::Lagged`].
pub const VITALS_CHANNEL_CAPACITY: usize = 16;

/// Opens a fresh [`HostSource`] each time monitoring starts. Called on a blocking thread.
pub type SourceFactory = Arc<dyn Fn() -> anyhow::Result<Box<dyn HostSource>> + Send + Sync>;

/// Wall-clock milliseconds since the Unix epoch.
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ─── Shared state ─────────────────────────────────────────────────────────────

/// One enabled period. Dropping it stops its task (the `stop` sender goes away) and ends every
/// subscription (the only broadcast sender goes away).
struct Run {
    generation: u64,
    tx: broadcast::Sender<Arc<VitalsSample>>,
    _stop: watch::Sender<bool>,
}

struct Inner {
    run: Option<Run>,
    /// Bumped per enable, so a task outliving its run can tell.
    generation: u64,
    /// Revision of the newest sample; never reset.
    revision: u64,
    history: History,
    host_info: HostInfo,
}

/// State shared by every handle clone and the sampler task.
pub(crate) struct Shared {
    inner: Mutex<Inner>,
    demand: Demand,
    factory: SourceFactory,
    clock: Clock,
    shutdown: watch::Receiver<bool>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock still holds consistent state: no write spans a panic point.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Stamp, record and fan out one sample of run `generation`. `false` when that run is over.
    fn publish(&self, generation: u64, mut sample: VitalsSample, fine: bool) -> bool {
        let mut inner = self.lock();
        if inner
            .run
            .as_ref()
            .is_none_or(|r| r.generation != generation)
        {
            return false;
        }
        inner.revision += 1;
        sample.revision = inner.revision;
        inner.history.record(&sample, fine);
        if let Some(run) = &inner.run {
            // No receiver is fine: history still records the sample.
            let _ = run.tx.send(Arc::new(sample));
        }
        true
    }

    fn set_host_info(&self, generation: u64, info: HostInfo) {
        let mut inner = self.lock();
        if inner
            .run
            .as_ref()
            .is_some_and(|r| r.generation == generation)
        {
            inner.host_info = info;
        }
    }

    /// End run `generation` from inside (shutdown or a failed source): as a disable.
    fn end_run(&self, generation: u64) {
        let mut inner = self.lock();
        if inner
            .run
            .as_ref()
            .is_some_and(|r| r.generation == generation)
        {
            Self::stop(&mut inner, &self.demand);
            log::info!("monitor: disabled");
        }
    }

    fn stop(inner: &mut Inner, demand: &Demand) {
        inner.run = None;
        inner.history = History::default();
        demand.clear();
    }
}

// ─── Handle ───────────────────────────────────────────────────────────────────

/// The monitor's switch and read side. Cheap to clone; every clone drives one monitor.
///
/// While disabled no task runs and no sysinfo handle is open. Enabling spawns the sampler on
/// the current tokio runtime; disabling (or the shutdown signal) stops it, drops the history
/// rings and ends every subscription with [`VitalsItem::Disabled`].
#[derive(Clone)]
pub struct MonitorHandle {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for MonitorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitorHandle")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl MonitorHandle {
    /// A disabled monitor over the real host. `shutdown` is the daemon's shutdown watch: once it
    /// reads `true` (or its sender is gone) the sampler stops and monitoring stays off.
    pub fn new(shutdown: watch::Receiver<bool>) -> Self {
        let factory: SourceFactory =
            Arc::new(|| Ok(Box::new(SysinfoSource::new()?) as Box<dyn HostSource>));
        Self::with_source(factory, shutdown)
    }

    /// A disabled monitor over `factory`'s source.
    pub fn with_source(factory: SourceFactory, shutdown: watch::Receiver<bool>) -> Self {
        Self::with_parts(factory, Arc::new(system_clock), shutdown)
    }

    fn with_parts(factory: SourceFactory, clock: Clock, shutdown: watch::Receiver<bool>) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    run: None,
                    generation: 0,
                    revision: 0,
                    history: History::default(),
                    host_info: HostInfo::default(),
                }),
                demand: Demand::default(),
                factory,
                clock,
                shutdown,
            }),
        }
    }

    /// Whether the sampler is running (or starting).
    pub fn is_enabled(&self) -> bool {
        self.shared.lock().run.is_some()
    }

    /// Start or stop sampling; repeating the current state is a no-op. Enabling must be called
    /// inside a tokio runtime, and is refused (logged) once the shutdown signal has fired.
    pub fn set_enabled(&self, enabled: bool) {
        let mut inner = self.shared.lock();
        match (enabled, inner.run.is_some()) {
            (true, false) => {
                if *self.shared.shutdown.borrow() {
                    log::debug!("monitor: not enabling during shutdown");
                    return;
                }
                inner.generation += 1;
                let generation = inner.generation;
                let (tx, _) = broadcast::channel(VITALS_CHANNEL_CAPACITY);
                let (stop_tx, stop_rx) = watch::channel(false);
                inner.run = Some(Run {
                    generation,
                    tx,
                    _stop: stop_tx,
                });
                drop(inner);
                tokio::spawn(sampler::run(Arc::clone(&self.shared), generation, stop_rx));
                log::info!("monitor: enabled");
            }
            (false, true) => {
                Shared::stop(&mut inner, &self.shared.demand);
                log::info!("monitor: disabled");
            }
            _ => {}
        }
    }

    /// Subscribe to samples at `tier`. The demand lasts until the subscription drops or
    /// monitoring is disabled. On a disabled monitor the first item is [`VitalsItem::Disabled`].
    pub fn subscribe(&self, tier: Tier) -> VitalsSubscription {
        let inner = self.shared.lock();
        match &inner.run {
            Some(run) => VitalsSubscription {
                rx: Some(run.tx.subscribe()),
                demand: Some(self.shared.demand.register(tier)),
            },
            None => VitalsSubscription {
                rx: None,
                demand: None,
            },
        }
    }

    /// The `range` window ending now; all NaN while disabled.
    pub fn history(&self, range: HistoryRange) -> HistoryView {
        let inner = self.shared.lock();
        inner
            .history
            .view(range, (self.shared.clock)(), inner.revision)
    }

    /// The host's static facts, read when sampling last started (defaults before that).
    pub fn host_info(&self) -> HostInfo {
        self.shared.lock().host_info.clone()
    }

    /// The level the sampler is currently asked for.
    pub fn demand(&self) -> DemandLevel {
        self.shared.demand.level()
    }
}

// ─── Subscription ─────────────────────────────────────────────────────────────

/// One item of a [`VitalsSubscription`].
#[derive(Debug, Clone, PartialEq)]
pub enum VitalsItem {
    /// The next sample.
    Sample(Arc<VitalsSample>),
    /// This subscriber fell behind and lost this many samples; the next item is current again.
    Lagged(u64),
    /// Monitoring is off. Terminal: every later call answers this again.
    Disabled,
}

/// A live feed of samples. Dropping it releases its demand.
#[derive(Debug)]
pub struct VitalsSubscription {
    rx: Option<broadcast::Receiver<Arc<VitalsSample>>>,
    demand: Option<DemandGuard>,
}

impl VitalsSubscription {
    /// The next item. Never blocks the sampler: a slow subscriber lags instead.
    pub async fn recv(&mut self) -> VitalsItem {
        let Some(rx) = &mut self.rx else {
            return VitalsItem::Disabled;
        };
        match rx.recv().await {
            Ok(sample) => VitalsItem::Sample(sample),
            Err(broadcast::error::RecvError::Lagged(missed)) => VitalsItem::Lagged(missed),
            Err(broadcast::error::RecvError::Closed) => {
                self.rx = None;
                self.demand = None;
                VitalsItem::Disabled
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    /// Wall-clock origin for the paused clock (aligned to every history step).
    const BASE_MS: u64 = 1_800_000_000_000;

    /// Counts sources opened and alive, and reports every read.
    #[derive(Clone)]
    struct Probe {
        opened: Arc<AtomicUsize>,
        alive: Arc<AtomicUsize>,
        reads: mpsc::UnboundedSender<ReadSpec>,
    }

    struct FakeHost {
        probe: Probe,
        reads: u64,
    }

    impl Drop for FakeHost {
        fn drop(&mut self) {
            self.probe.alive.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl HostSource for FakeHost {
        fn host_info(&mut self) -> anyhow::Result<HostInfo> {
            Ok(HostInfo {
                hostname: "fake".into(),
                logical_cores: 4,
                ..HostInfo::default()
            })
        }

        fn read(&mut self, spec: ReadSpec) -> anyhow::Result<RawReading> {
            self.reads += 1;
            let _ = self.probe.reads.send(spec);
            Ok(RawReading {
                cpu_percent: 10.0,
                memory: Memory::without_cache(1_000, 600),
                interfaces: vec![InterfaceReading {
                    name: "eth0".into(),
                    rx_total: self.reads * 1_000,
                    physical: true,
                    ..InterfaceReading::default()
                }],
                ..RawReading::default()
            })
        }
    }

    struct Rig {
        handle: MonitorHandle,
        probe: Probe,
        reads: mpsc::UnboundedReceiver<ReadSpec>,
        shutdown: watch::Sender<bool>,
    }

    fn rig() -> Rig {
        let (reads_tx, reads) = mpsc::unbounded_channel();
        let probe = Probe {
            opened: Arc::new(AtomicUsize::new(0)),
            alive: Arc::new(AtomicUsize::new(0)),
            reads: reads_tx,
        };
        let factory_probe = probe.clone();
        let factory: SourceFactory = Arc::new(move || {
            factory_probe.opened.fetch_add(1, Ordering::SeqCst);
            factory_probe.alive.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeHost {
                probe: factory_probe.clone(),
                reads: 0,
            }) as Box<dyn HostSource>)
        });
        let origin = Instant::now();
        let clock: Clock =
            Arc::new(move || BASE_MS + u64::try_from(origin.elapsed().as_millis()).unwrap());
        let (shutdown, shutdown_rx) = watch::channel(false);
        Rig {
            handle: MonitorHandle::with_parts(factory, clock, shutdown_rx),
            probe,
            reads,
            shutdown,
        }
    }

    impl Rig {
        /// Wait for the next read; returns its spec and paused-clock time.
        async fn next_read(&mut self) -> (ReadSpec, Instant) {
            let spec = self.reads.recv().await.expect("reads channel open");
            (spec, Instant::now())
        }

        /// Wait for the priming read and the first emitted read.
        async fn started(&mut self) -> Instant {
            let (_, primed) = self.next_read().await;
            let (_, first) = self.next_read().await;
            assert_eq!(first - primed, sampler::PRIME_DELAY);
            first
        }

        /// Wait for the next read and return the gap since `last`, moving `last` on.
        async fn gap(&mut self, last: &mut Instant) -> Duration {
            let (_, at) = self.next_read().await;
            let gap = at - *last;
            *last = at;
            gap
        }

        /// Until every opened source has been dropped.
        async fn source_dropped(&self) {
            while self.probe.alive.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    async fn next_sample(sub: &mut VitalsSubscription) -> Arc<VitalsSample> {
        match sub.recv().await {
            VitalsItem::Sample(sample) => sample,
            other => panic!("expected a sample, got {other:?}"),
        }
    }

    fn full(ms: u64) -> Tier {
        Tier::Full {
            interval: Duration::from_millis(ms),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_disabled_handle_spawns_nothing_and_answers_disabled() {
        let rig = rig();
        assert!(!rig.handle.is_enabled());
        let mut sub = rig.handle.subscribe(Tier::Summary);
        assert_eq!(sub.recv().await, VitalsItem::Disabled);
        assert_eq!(sub.recv().await, VitalsItem::Disabled, "terminal");
        assert_eq!(
            rig.handle.demand(),
            DemandLevel::IDLE,
            "no demand registered"
        );
        let view = rig.handle.history(HistoryRange::OneMinute);
        assert!(view.series.cpu_total.iter().all(|v| v.is_nan()));
        assert_eq!(view.revision, 0);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(
            rig.probe.opened.load(Ordering::SeqCst),
            0,
            "no source opened"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_interval_follows_the_most_demanding_live_subscriber() {
        let mut rig = rig();
        rig.handle.set_enabled(true);
        let mut last = rig.started().await;
        assert_eq!(rig.gap(&mut last).await, IDLE_INTERVAL);

        let summary = rig.handle.subscribe(Tier::Summary);
        assert_eq!(rig.gap(&mut last).await, SUMMARY_INTERVAL);
        assert_eq!(rig.gap(&mut last).await, SUMMARY_INTERVAL);

        let three = rig.handle.subscribe(full(3_000));
        assert_eq!(rig.gap(&mut last).await, Duration::from_secs(3));

        let clamped = rig.handle.subscribe(full(500));
        assert_eq!(
            rig.gap(&mut last).await,
            MIN_FULL_INTERVAL,
            "clamped to the floor"
        );

        drop(clamped);
        assert_eq!(rig.gap(&mut last).await, Duration::from_secs(3));
        drop(three);
        assert_eq!(rig.gap(&mut last).await, SUMMARY_INTERVAL);
        drop(summary);
        assert_eq!(rig.gap(&mut last).await, IDLE_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_subscriber_shortens_a_long_idle_wait() {
        let mut rig = rig();
        rig.handle.set_enabled(true);
        let mut last = rig.started().await;
        // Six seconds into a 15 s idle wait, a Full{2 s} subscriber arrives: the next read is
        // due 2 s after the last one, which has already passed, so it runs now.
        tokio::time::sleep(Duration::from_secs(6)).await;
        let _full = rig.handle.subscribe(full(2_000));
        assert_eq!(rig.gap(&mut last).await, Duration::from_secs(6));
        assert_eq!(rig.gap(&mut last).await, MIN_FULL_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn the_shutdown_signal_stops_the_task() {
        let mut rig = rig();
        rig.handle.set_enabled(true);
        rig.started().await;
        let mut sub = rig.handle.subscribe(Tier::Summary);
        rig.shutdown.send(true).unwrap();
        // Drain anything already published, then the terminal item.
        loop {
            match sub.recv().await {
                VitalsItem::Disabled => break,
                VitalsItem::Sample(_) | VitalsItem::Lagged(_) => {}
            }
        }
        rig.source_dropped().await;
        assert!(!rig.handle.is_enabled());
        rig.handle.set_enabled(true);
        assert!(!rig.handle.is_enabled(), "no restart after shutdown");
        assert_eq!(rig.probe.opened.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn two_subscribers_both_receive_every_sample() {
        let rig = rig();
        rig.handle.set_enabled(true);
        let mut a = rig.handle.subscribe(full(2_000));
        let mut b = rig.handle.subscribe(Tier::Summary);
        for _ in 0..5 {
            let from_a = next_sample(&mut a).await;
            let from_b = next_sample(&mut b).await;
            assert_eq!(from_a.revision, from_b.revision);
            assert!(Arc::ptr_eq(&from_a, &from_b), "one sample, shared");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_subscriber_lags_without_holding_back_the_others() {
        let rig = rig();
        rig.handle.set_enabled(true);
        let mut fast = rig.handle.subscribe(full(2_000));
        let mut slow = rig.handle.subscribe(full(2_000));
        let total = VITALS_CHANNEL_CAPACITY as u64 + 4;
        let first = next_sample(&mut fast).await.revision;
        for k in 1..total {
            assert_eq!(next_sample(&mut fast).await.revision, first + k);
        }
        assert_eq!(slow.recv().await, VitalsItem::Lagged(4));
        assert_eq!(next_sample(&mut slow).await.revision, first + 4);
    }

    #[tokio::test(start_paused = true)]
    async fn revisions_step_by_one_and_survive_disable_and_re_enable() {
        let rig = rig();
        rig.handle.set_enabled(true);
        let mut sub = rig.handle.subscribe(full(2_000));
        let mut revision = next_sample(&mut sub).await.revision;
        assert_eq!(revision, 1);
        for _ in 0..3 {
            let next = next_sample(&mut sub).await.revision;
            assert_eq!(next, revision + 1);
            revision = next;
        }
        assert_eq!(
            rig.handle.history(HistoryRange::OneMinute).revision,
            revision
        );

        rig.handle.set_enabled(false);
        assert_eq!(sub.recv().await, VitalsItem::Disabled);
        assert_eq!(sub.recv().await, VitalsItem::Disabled);
        rig.source_dropped().await;
        let view = rig.handle.history(HistoryRange::OneMinute);
        assert!(
            view.series.cpu_total.iter().all(|v| v.is_nan()),
            "rings dropped"
        );

        rig.handle.set_enabled(true);
        let mut sub = rig.handle.subscribe(Tier::Summary);
        assert_eq!(next_sample(&mut sub).await.revision, revision + 1);
        assert_eq!(rig.probe.opened.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn disable_ends_every_subscription_and_releases_the_source() {
        let mut rig = rig();
        rig.handle.set_enabled(true);
        rig.started().await;
        let mut a = rig.handle.subscribe(Tier::Summary);
        let mut b = rig.handle.subscribe(full(3_000));
        assert!(rig.handle.demand().full);
        rig.handle.set_enabled(false);
        assert!(!rig.handle.is_enabled());
        assert_eq!(a.recv().await, VitalsItem::Disabled);
        assert_eq!(b.recv().await, VitalsItem::Disabled);
        assert_eq!(rig.handle.demand(), DemandLevel::IDLE);
        rig.source_dropped().await;
        drop((a, b));
        assert_eq!(rig.handle.demand(), DemandLevel::IDLE);
    }

    #[tokio::test(start_paused = true)]
    async fn the_fine_ring_and_temperatures_follow_full_subscribers() {
        let mut rig = rig();
        rig.handle.set_enabled(true);
        rig.started().await;
        let mut summary = rig.handle.subscribe(Tier::Summary);
        next_sample(&mut summary).await;
        next_sample(&mut summary).await;
        assert!(!rig.handle.shared.lock().history.has_fine_data());
        while let Ok(spec) = rig.reads.try_recv() {
            assert!(!spec.temperatures, "no Full subscriber yet");
        }

        let mut full = rig.handle.subscribe(full(2_000));
        next_sample(&mut full).await;
        next_sample(&mut full).await;
        assert!(rig.handle.shared.lock().history.has_fine_data());
        let mut specs = Vec::new();
        while let Ok(spec) = rig.reads.try_recv() {
            specs.push(spec);
        }
        assert!(!specs.is_empty() && specs.iter().all(|s| s.temperatures));

        let view = rig.handle.history(HistoryRange::OneMinute);
        assert_eq!(view.end_unix_ms % view.step_ms, 0);
        let real = view.series.cpu_total.iter().filter(|v| !v.is_nan()).count();
        assert!(real >= 2, "{:?}", view.series.cpu_total);
        drop(summary);
    }

    #[tokio::test(start_paused = true)]
    async fn host_info_is_read_when_sampling_starts() {
        let mut rig = rig();
        assert_eq!(rig.handle.host_info(), HostInfo::default());
        rig.handle.set_enabled(true);
        rig.started().await;
        assert_eq!(rig.handle.host_info().hostname, "fake");
        assert_eq!(rig.handle.host_info().logical_cores, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn the_cpu_frequency_is_re_read_at_most_once_a_minute() {
        let mut rig = rig();
        rig.handle.set_enabled(true);
        let (prime, _) = rig.next_read().await;
        assert!(prime.cpu_frequency);
        let _full = rig.handle.subscribe(full(2_000));
        let mut frequency_reads = 0;
        // 1 s prime delay, then 2 s apart: 60 reads span ~2 minutes.
        for _ in 0..60 {
            if rig.next_read().await.0.cpu_frequency {
                frequency_reads += 1;
            }
        }
        assert_eq!(
            frequency_reads, 1,
            "~119 s after the prime: one re-read at 60 s"
        );
    }
}
