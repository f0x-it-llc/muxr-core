//! model — the monitor's plain data: host facts, one vitals sample, and history views.
//!
//! No proto types and no behaviour beyond small accessors; the gRPC layer maps these onto the wire.

use std::time::Duration;

// ─── Host facts ───────────────────────────────────────────────────────────────

/// Facts about the host that do not change while the daemon runs. Read once each time
/// monitoring starts; a field the platform does not report is empty, zero or `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostInfo {
    /// Host name.
    pub hostname: String,
    /// Operating system name (`Arch Linux`, `Darwin`).
    pub os_name: String,
    /// Operating system version.
    pub os_version: String,
    /// Kernel version.
    pub kernel: String,
    /// CPU architecture (`x86_64`, `arm64`).
    pub arch: String,
    /// CPU brand string of the first core.
    pub cpu_brand: String,
    /// Logical cores (hardware threads).
    pub logical_cores: u32,
    /// Physical cores, when the platform reports them.
    pub physical_cores: Option<u32>,
    /// Boot time, seconds since the Unix epoch (0 when unknown).
    pub boot_time_unix_s: u64,
    /// Installed memory in bytes.
    pub total_memory_bytes: u64,
}

// ─── One sample ───────────────────────────────────────────────────────────────

/// Load averages over 1, 5 and 15 minutes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// Physical memory, partitioned so that `used + cache + free == total`. `available` is
/// reported beside the partition, not inside it: it overlaps `cache` and `free`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Memory {
    pub total: u64,
    pub used: u64,
    pub cache: u64,
    pub free: u64,
    pub available: u64,
}

impl Memory {
    /// The partition from `total` and `available` alone, for a platform that reports no
    /// cached figure: `used = total - available`, `cache = 0`, `free` the rest.
    pub fn without_cache(total: u64, available: u64) -> Self {
        let available = available.min(total);
        let used = total - available;
        Self {
            total,
            used,
            cache: 0,
            free: total - used,
            available,
        }
    }

    /// `used / total`, 0 when `total` is 0.
    pub fn used_fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.used as f64 / self.total as f64) as f32
        }
    }
}

/// One real filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSpace {
    /// Mount point.
    pub mount: String,
    /// Filesystem type (`ext4`, `apfs`, `overlay`).
    pub fs: String,
    /// Size in bytes.
    pub total: u64,
    /// Bytes in use (`total - available`).
    pub used: u64,
}

/// The primary network interface and its traffic.
#[derive(Debug, Clone, PartialEq)]
pub struct NetInterface {
    /// Interface name (`enp5s0`, `en0`).
    pub name: String,
    /// The interface's preferred address (IPv4 first), when it has one.
    pub address: Option<String>,
    /// Receive rate in bytes per second; `None` on the first tick after a pick or a counter reset.
    pub rx_bps: Option<u64>,
    /// Transmit rate in bytes per second; `None` as for `rx_bps`.
    pub tx_bps: Option<u64>,
    /// Bytes received as the kernel counts them, plus every value seen before a counter reset,
    /// so the total never goes backwards.
    pub rx_total: u64,
    /// Bytes transmitted, as for `rx_total`.
    pub tx_total: u64,
}

/// One temperature sensor reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Temperature {
    pub label: String,
    pub celsius: f32,
}

/// One sample of host vitals. Plain data; the sampler fills it, subscribers read it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VitalsSample {
    /// Wall-clock time of the sample, milliseconds since the Unix epoch.
    pub unix_ms: u64,
    /// +1 per sample, monotonic for the daemon's lifetime (across disable and re-enable).
    pub revision: u64,
    /// Total cpu busy percentage, 0..=100.
    pub cpu_percent: f32,
    /// User share of cpu time (Linux `/proc/stat`; `None` elsewhere).
    pub cpu_user_percent: Option<f32>,
    /// System share of cpu time (Linux only).
    pub cpu_system_percent: Option<f32>,
    /// I/O wait share of cpu time (Linux only).
    pub cpu_iowait_percent: Option<f32>,
    /// Per logical core busy percentage.
    pub per_core_percent: Vec<f32>,
    /// Load averages, when the platform reports them.
    pub load: Option<LoadAverage>,
    /// Mean core frequency in MHz, refreshed at most once a minute.
    pub cpu_freq_mhz: Option<u64>,
    pub memory: Memory,
    pub swap_used: u64,
    pub swap_total: u64,
    /// Real filesystems only (see `sampler::keep_real_disks`).
    pub disks: Vec<DiskSpace>,
    /// Bytes read per second across the reported disks.
    pub disk_read_bps: Option<u64>,
    /// Bytes written per second across the reported disks.
    pub disk_write_bps: Option<u64>,
    /// The primary network interface, when there is any non-loopback interface.
    pub network: Option<NetInterface>,
    /// Package sensor first, then the rest by label. Filled only for `Full` subscribers.
    pub temperatures: Vec<Temperature>,
    /// Number of processes; filled by the process sampler, `None` here.
    pub process_count: Option<u32>,
    /// Number of running processes; as for `process_count`.
    pub running_count: Option<u32>,
}

// ─── Demand tiers ─────────────────────────────────────────────────────────────

/// What a subscriber asks the sampler for. The effective interval is the smallest any live
/// subscriber asks for (see `demand`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// A summary card: a sample every `SUMMARY_INTERVAL`.
    Summary,
    /// A full vitals view at `interval`, clamped to `[MIN_FULL_INTERVAL, MAX_FULL_INTERVAL]`.
    Full { interval: Duration },
}

// ─── History ──────────────────────────────────────────────────────────────────

/// A history window: a fixed number of fixed-width slots ending now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRange {
    /// 30 slots of 2 s.
    OneMinute,
    /// 60 slots of 5 s.
    FiveMinutes,
    /// 60 slots of 15 s.
    FifteenMinutes,
    /// 60 slots of 60 s.
    OneHour,
}

impl HistoryRange {
    /// Width of one slot in milliseconds.
    pub fn step_ms(self) -> u64 {
        match self {
            Self::OneMinute => 2_000,
            Self::FiveMinutes => 5_000,
            Self::FifteenMinutes => 15_000,
            Self::OneHour => 60_000,
        }
    }

    /// Number of slots.
    pub fn slots(self) -> usize {
        match self {
            Self::OneMinute => 30,
            Self::FiveMinutes | Self::FifteenMinutes | Self::OneHour => 60,
        }
    }
}

/// The series history keeps, each `slots` long, oldest first. `f32::NAN` marks a slot with no
/// data at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistorySeries {
    /// Total cpu percentage.
    pub cpu_total: Vec<f32>,
    /// System cpu percentage (all NaN where the platform has no split).
    pub cpu_system: Vec<f32>,
    /// Memory used as a fraction of total, 0..=1.
    pub memory_used_fraction: Vec<f32>,
    pub net_rx_bps: Vec<f32>,
    pub net_tx_bps: Vec<f32>,
    pub disk_read_bps: Vec<f32>,
    pub disk_write_bps: Vec<f32>,
}

/// A fixed-step history window. Slot `i` (0-based, of `n`) covers
/// `[end_unix_ms - (n - i) * step_ms, end_unix_ms - (n - i - 1) * step_ms)`; the newest slot,
/// last, is the one holding "now" and may be partial.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryView {
    pub range: HistoryRange,
    pub step_ms: u64,
    /// Exclusive end of the newest slot, a multiple of `step_ms`.
    pub end_unix_ms: u64,
    /// Revision of the newest sample recorded (0 before any sample).
    pub revision: u64,
    pub series: HistorySeries,
}

// ─── Processes ────────────────────────────────────────────────────────────────

/// Floor of a process subscription's interval; the spike measured a full cpu + memory + cmd +
/// user refresh of ~490 processes at ~6.5 ms of cpu.
pub const MIN_PROCESS_INTERVAL: Duration = Duration::from_secs(3);

/// Ceiling of a process subscription's interval.
pub const MAX_PROCESS_INTERVAL: Duration = Duration::from_secs(10);

/// Rows a [`ProcessQuery`] gets by default.
pub const DEFAULT_PROCESS_LIMIT: u32 = 50;

/// Most rows a [`ProcessQuery`] can ask for.
pub const MAX_PROCESS_LIMIT: u32 = 200;

/// The column a process list is sorted by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProcessSort {
    /// Cpu percentage, highest first by default.
    #[default]
    Cpu,
    /// Resident memory, largest first by default.
    Memory,
    /// Pid, ascending by default.
    Pid,
    /// Name (case-insensitive), ascending by default.
    Name,
}

/// What one process subscriber wants to see. Each subscription gets a view built for its own query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessQuery {
    pub sort: ProcessSort,
    /// Flip the column's default direction (ties stay by pid ascending).
    pub reverse: bool,
    /// Case-insensitive substring of the name, the user or the decimal pid. Never the command
    /// line. Empty matches everything.
    pub filter: String,
    /// Rows to return, clamped to `1..=`[`MAX_PROCESS_LIMIT`].
    pub limit: u32,
    /// A process (pid, start time in Unix seconds) to append when it fell outside the cut; it
    /// also carries its thread count.
    pub pinned: Option<(u32, i64)>,
    /// Refresh interval asked for, clamped to `[MIN_PROCESS_INTERVAL, MAX_PROCESS_INTERVAL]`.
    pub interval: Duration,
}

impl Default for ProcessQuery {
    fn default() -> Self {
        Self {
            sort: ProcessSort::default(),
            reverse: false,
            filter: String::new(),
            limit: DEFAULT_PROCESS_LIMIT,
            pinned: None,
            interval: MIN_PROCESS_INTERVAL,
        }
    }
}

impl ProcessQuery {
    /// The row limit after clamping.
    pub fn effective_limit(&self) -> usize {
        self.limit.clamp(1, MAX_PROCESS_LIMIT) as usize
    }

    /// The refresh interval after clamping.
    pub fn effective_interval(&self) -> Duration {
        self.interval
            .clamp(MIN_PROCESS_INTERVAL, MAX_PROCESS_INTERVAL)
    }
}

/// A process's scheduler state, mapped from the host's own status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    Sleeping,
    /// Uninterruptible sleep (Linux `D`).
    DiskWait,
    Zombie,
    Stopped,
    Idle,
    Dead,
    Unknown,
}

/// One process. Plain data; the identity of a process is `(pid, start_unix_s)`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessRow {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    /// Never empty: the host's name, else the executable's file name, else `<unknown>`.
    pub name: String,
    /// The full command line joined by spaces; empty when the host does not show it.
    pub command: String,
    /// User name, falling back to the numeric uid; empty when the host reports neither.
    pub user: String,
    /// Percent of the whole machine's cpu capacity, 0..=100.
    pub cpu_percent: f32,
    /// Resident memory; `None` (never 0) when the host refuses it.
    pub memory_bytes: Option<u64>,
    /// Thread count; only the pinned process's row carries it (Linux only).
    pub threads: Option<u32>,
    /// Start time, seconds since the Unix epoch.
    pub start_unix_s: i64,
    /// Accumulated cpu time in milliseconds.
    pub cpu_time_ms: u64,
    pub state: ProcessState,
}

/// One subscriber's view of the process table.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessSnapshot {
    /// +1 per process refresh, monotonic for the daemon's lifetime.
    pub revision: u64,
    /// Wall-clock time of the refresh, milliseconds since the Unix epoch.
    pub unix_ms: u64,
    /// Every process on the host, before the filter. Kernel threads count.
    pub total: u32,
    /// Processes in [`ProcessState::Running`], before the filter.
    pub running: u32,
    /// The filtered, sorted, truncated rows, then the pinned row when it fell outside the cut.
    pub rows: Vec<ProcessRow>,
}
