//! sampler — the host source, the pure readers and filters behind it, and the sampling task.
//!
//! Every sysinfo refresh and /proc read blocks and runs on `spawn_blocking`; tests script a fake.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use sysinfo::{
    Components, CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, Networks, RefreshKind,
    System,
};
use tokio::sync::watch;
use tokio::time::Instant;

use super::Shared;
use super::model::{
    DiskSpace, HostInfo, LoadAverage, Memory, NetInterface, Temperature, VitalsSample,
};

/// Delay between the priming refresh and the first emitted sample. A cpu figure needs two
/// refreshes at least `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` apart; a second gives a steadier one.
pub(crate) const PRIME_DELAY: Duration = Duration::from_secs(1);

/// The cpu frequency is re-read at most this often.
pub(crate) const FREQUENCY_REFRESH: Duration = Duration::from_secs(60);

// ─── Raw readings ─────────────────────────────────────────────────────────────

/// Cumulative cpu time counters from `/proc/stat`'s first line, in clock ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

/// One mounted filesystem as the source sees it, before or after [`keep_real_disks`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskReading {
    pub mount: String,
    pub fs: String,
    /// Backing device name; one row per device survives the filter.
    pub device: String,
    pub total: u64,
    pub used: u64,
    /// The mount point is a directory (containers bind-mount single files).
    pub is_dir: bool,
    pub read_only: bool,
    /// Cumulative bytes read from the device.
    pub read_bytes: u64,
    /// Cumulative bytes written to the device.
    pub written_bytes: u64,
}

/// One network interface's cumulative counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterfaceReading {
    pub name: String,
    pub address: Option<String>,
    /// Cumulative bytes received, as the kernel counts them (may reset).
    pub rx_total: u64,
    /// Cumulative bytes transmitted, as for `rx_total`.
    pub tx_total: u64,
    pub loopback: bool,
    /// Backed by a device (Linux: `/sys/class/net/<if>/device` exists; elsewhere every
    /// non-loopback interface counts as physical).
    pub physical: bool,
}

/// What the sampler wants from one read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadSpec {
    /// Refresh the temperature sensors (a `Full` subscriber exists).
    pub temperatures: bool,
    /// Re-read the cpu frequency; otherwise the last figure is repeated.
    pub cpu_frequency: bool,
}

/// One raw reading: counters, not rates. [`Deriver`] turns consecutive readings into samples.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawReading {
    pub cpu_percent: f32,
    pub per_core_percent: Vec<f32>,
    /// Linux only; `None` elsewhere (sysinfo has no user/system split).
    pub cpu_times: Option<CpuTimes>,
    pub load: Option<LoadAverage>,
    pub cpu_freq_mhz: Option<u64>,
    pub memory: Memory,
    pub swap_used: u64,
    pub swap_total: u64,
    /// Real filesystems only.
    pub disks: Vec<DiskReading>,
    pub interfaces: Vec<InterfaceReading>,
    /// Sensor label and reading; empty unless [`ReadSpec::temperatures`].
    pub temperatures: Vec<(String, Option<f32>)>,
}

/// Where host vitals come from. Abstracted so the sampling task runs on a scripted fake in tests.
/// Both calls block and are only ever made from `spawn_blocking`.
pub trait HostSource: Send + 'static {
    /// The host's static facts.
    fn host_info(&mut self) -> anyhow::Result<HostInfo>;
    /// Refresh and return one reading.
    fn read(&mut self, spec: ReadSpec) -> anyhow::Result<RawReading>;
}

// ─── Pure readers ─────────────────────────────────────────────────────────────

/// The memory partition from `/proc/meminfo` text (values in kB).
///
/// `available = MemAvailable`, `used = total - available`,
/// `cache = min(Cached + SReclaimable, available - MemFree)`, `free = total - used - cache`, so
/// `used + cache + free == total`. The ZFS ARC is not in `Cached` and is not counted: on a ZFS
/// host it shows as used. `None` when MemTotal, MemFree or MemAvailable is missing.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn memory_from_meminfo(text: &str) -> Option<Memory> {
    let mut fields: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse().ok()) {
            fields.insert(key.trim(), kb);
        }
    }
    let bytes = |key: &str| fields.get(key).map(|kb| kb.saturating_mul(1024));
    let total = bytes("MemTotal")?;
    let available = bytes("MemAvailable")?.min(total);
    let mem_free = bytes("MemFree")?.min(available);
    let cached = bytes("Cached")
        .unwrap_or(0)
        .saturating_add(bytes("SReclaimable").unwrap_or(0));
    let used = total - available;
    let cache = cached.min(available - mem_free);
    Some(Memory {
        total,
        used,
        cache,
        free: total - used - cache,
        available,
    })
}

/// The aggregate cpu counters from `/proc/stat` text. Missing trailing fields (old kernels) read
/// as 0; `None` when the `cpu ` line is absent or has fewer than four numbers.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn cpu_times_from_proc_stat(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let numbers: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map_while(|v| v.parse().ok())
        .collect();
    if numbers.len() < 4 {
        return None;
    }
    let at = |i: usize| numbers.get(i).copied().unwrap_or(0);
    Some(CpuTimes {
        user: at(0),
        nice: at(1),
        system: at(2),
        idle: at(3),
        iowait: at(4),
        irq: at(5),
        softirq: at(6),
        steal: at(7),
    })
}

/// The fs types `/proc/filesystems` lists without `nodev`, minus squashfs and nullfs.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn real_filesystems(text: &str) -> HashSet<String> {
    text.lines()
        .filter(|l| !l.starts_with("nodev"))
        .map(|l| l.trim().to_owned())
        .filter(|fs| !fs.is_empty() && fs != "squashfs" && fs != "nullfs")
        .collect()
}

/// Which mounts count as real filesystems on this host.
#[derive(Debug, Clone, Default)]
pub(crate) struct DiskRule {
    /// Linux: fs types backed by a device (see [`real_filesystems`]); `None` keeps every type.
    pub(crate) real_fs: Option<HashSet<String>>,
    /// macOS: drop autofs and read-only (sealed system snapshot) mounts.
    pub(crate) drop_autofs_and_read_only: bool,
}

impl DiskRule {
    /// The rule for the running platform. An unreadable `/proc/filesystems` keeps every type.
    fn for_host() -> Self {
        #[cfg(target_os = "linux")]
        let rule = Self {
            real_fs: std::fs::read_to_string("/proc/filesystems")
                .ok()
                .map(|text| real_filesystems(&text)),
            drop_autofs_and_read_only: false,
        };
        #[cfg(target_os = "macos")]
        let rule = Self {
            real_fs: None,
            drop_autofs_and_read_only: true,
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let rule = Self::default();
        rule
    }

    fn keeps(&self, disk: &DiskReading) -> bool {
        if self.drop_autofs_and_read_only && (disk.fs == "autofs" || disk.read_only) {
            return false;
        }
        self.real_fs.as_ref().is_none_or(|set| {
            // zfs is real; an overlay at `/` is a container's root disk.
            set.contains(&disk.fs)
                || disk.fs == "zfs"
                || (disk.fs == "overlay" && disk.mount == "/")
        })
    }
}

/// Real filesystems only: a kept fs type, a non-zero size, a directory mount point, and one row
/// per device (the first mount seen).
pub(crate) fn keep_real_disks(disks: Vec<DiskReading>, rule: &DiskRule) -> Vec<DiskReading> {
    let mut devices = HashSet::new();
    disks
        .into_iter()
        .filter(|d| d.total > 0 && d.is_dir && rule.keeps(d))
        .filter(|d| devices.insert(d.device.clone()))
        .collect()
}

/// `lo`, `lo0`, or an interface whose every address is a loopback address.
pub(crate) fn is_loopback(name: &str, addrs: &[IpAddr]) -> bool {
    let named = name
        .strip_prefix("lo")
        .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()));
    named || (!addrs.is_empty() && addrs.iter().all(IpAddr::is_loopback))
}

/// The address to show: the first routable IPv4, else the first routable IPv6.
pub(crate) fn preferred_address(addrs: &[IpAddr]) -> Option<String> {
    let routable = |a: &&IpAddr| match a {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            !v6.is_loopback() && !v6.is_unspecified() && (v6.segments()[0] & 0xffc0) != 0xfe80
        }
    };
    addrs
        .iter()
        .filter(routable)
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.iter().filter(routable).find(|a| a.is_ipv6()))
        .map(IpAddr::to_string)
}

/// Linux: an interface with a backing device. veth, bridges, docker, WireGuard and tailscale
/// interfaces have none.
#[cfg(target_os = "linux")]
fn is_physical(name: &str) -> bool {
    std::path::Path::new("/sys/class/net")
        .join(name)
        .join("device")
        .exists()
}

/// Elsewhere there is no cheap test; every non-loopback interface is a candidate.
#[cfg(not(target_os = "linux"))]
fn is_physical(_name: &str) -> bool {
    true
}

/// The interface to report. Loopback never; physical interfaces when any exists, else the
/// non-loopback rest (inside a container every interface is virtual). The current pick is kept
/// while it is present and still eligible; otherwise the highest cumulative total wins.
pub(crate) fn pick_interface<'a>(
    current: Option<&str>,
    interfaces: &'a [InterfaceReading],
) -> Option<&'a InterfaceReading> {
    let any_physical = interfaces.iter().any(|i| !i.loopback && i.physical);
    let eligible = |i: &&InterfaceReading| !i.loopback && (i.physical || !any_physical);
    if let Some(kept) = current
        .and_then(|name| interfaces.iter().find(|i| i.name == name))
        .filter(eligible)
    {
        return Some(kept);
    }
    interfaces.iter().filter(eligible).max_by(|a, b| {
        let total = |i: &InterfaceReading| i.rx_total.saturating_add(i.tx_total);
        total(a)
            .cmp(&total(b))
            // On a tie the alphabetically first name wins.
            .then_with(|| b.name.cmp(&a.name))
    })
}

/// Drop `None` and exactly-0.0 readings, then order: the cpu package sensor first ("Package id"
/// for coretemp, "Tctl" for k10temp, else the hottest "Core"/"CPU" label), the rest by label.
pub(crate) fn order_temperatures(raw: Vec<(String, Option<f32>)>) -> Vec<Temperature> {
    let mut temps: Vec<Temperature> = raw
        .into_iter()
        .filter_map(|(label, celsius)| {
            celsius
                .filter(|c| *c != 0.0 && c.is_finite())
                .map(|celsius| Temperature { label, celsius })
        })
        .collect();
    temps.sort_by(|a, b| a.label.cmp(&b.label));
    let package = temps
        .iter()
        .position(|t| t.label.contains("Package id"))
        .or_else(|| temps.iter().position(|t| t.label.contains("Tctl")))
        .or_else(|| {
            temps
                .iter()
                .enumerate()
                .filter(|(_, t)| t.label.contains("Core") || t.label.contains("CPU"))
                .max_by(|(_, a), (_, b)| a.celsius.total_cmp(&b.celsius))
                .map(|(i, _)| i)
        });
    if let Some(i) = package {
        let first = temps.remove(i);
        temps.insert(0, first);
    }
    temps
}

/// User, system and iowait percentages from two `/proc/stat` readings; `None` when a counter
/// went backwards or no time passed.
fn cpu_split(prev: &CpuTimes, cur: &CpuTimes) -> Option<(f32, f32, f32)> {
    let d = |a: u64, b: u64| b.checked_sub(a);
    let user = d(prev.user, cur.user)? + d(prev.nice, cur.nice)?;
    let system =
        d(prev.system, cur.system)? + d(prev.irq, cur.irq)? + d(prev.softirq, cur.softirq)?;
    let iowait = d(prev.iowait, cur.iowait)?;
    let total = user + system + iowait + d(prev.idle, cur.idle)? + d(prev.steal, cur.steal)?;
    if total == 0 {
        return None;
    }
    let pct = |v: u64| (v as f64 * 100.0 / total as f64) as f32;
    Some((pct(user), pct(system), pct(iowait)))
}

// ─── Deriver ──────────────────────────────────────────────────────────────────

/// The picked interface's counter state.
#[derive(Debug)]
struct NetState {
    name: String,
    last_rx: u64,
    last_tx: u64,
    /// Sum of the counter values seen just before each reset, so totals stay monotonic.
    carried_rx: u64,
    carried_tx: u64,
}

/// One direction of a counter: the rate since the last reading, or `None` when there is no
/// elapsed time or the counter went backwards (a reset or wrap, whose pre-reset value is carried).
fn counter_step(
    last: &mut u64,
    carried: &mut u64,
    raw: u64,
    elapsed_s: Option<f64>,
) -> Option<u64> {
    let rate = if raw >= *last {
        elapsed_s.map(|s| ((raw - *last) as f64 / s).round() as u64)
    } else {
        *carried = carried.saturating_add(*last);
        None
    };
    *last = raw;
    rate
}

/// Turns consecutive raw readings into samples: rates from counter deltas over the real elapsed
/// time between readings (never the nominal interval), the sticky interface pick, the cpu split.
#[derive(Debug, Default)]
pub(crate) struct Deriver {
    prev_at_ms: Option<u64>,
    prev_cpu: Option<CpuTimes>,
    prev_disks: HashMap<String, (u64, u64)>,
    net: Option<NetState>,
}

impl Deriver {
    /// Derive a sample from `raw`, taken at monotonic time `at_ms`. `unix_ms` and `revision` are
    /// left at 0 for the caller.
    pub(crate) fn derive(&mut self, raw: RawReading, at_ms: u64) -> VitalsSample {
        let elapsed_s = self
            .prev_at_ms
            .and_then(|prev| at_ms.checked_sub(prev))
            .filter(|ms| *ms > 0)
            .map(|ms| ms as f64 / 1000.0);
        self.prev_at_ms = Some(at_ms);

        let split = match (self.prev_cpu, raw.cpu_times) {
            (Some(prev), Some(cur)) => cpu_split(&prev, &cur),
            _ => None,
        };
        self.prev_cpu = raw.cpu_times;

        // Disk io: per-device deltas clamp at zero; no previous reading, no rate.
        let mut read = 0u64;
        let mut written = 0u64;
        let mut any_prev = false;
        let mut prev_disks = HashMap::with_capacity(raw.disks.len());
        for disk in &raw.disks {
            if let Some((r, w)) = self.prev_disks.get(&disk.device) {
                any_prev = true;
                read = read.saturating_add(disk.read_bytes.saturating_sub(*r));
                written = written.saturating_add(disk.written_bytes.saturating_sub(*w));
            }
            prev_disks.insert(disk.device.clone(), (disk.read_bytes, disk.written_bytes));
        }
        self.prev_disks = prev_disks;
        let disk_rate = |bytes: u64| {
            elapsed_s
                .filter(|_| any_prev)
                .map(|s| (bytes as f64 / s).round() as u64)
        };

        let network = self.network(&raw.interfaces, elapsed_s);

        VitalsSample {
            unix_ms: 0,
            revision: 0,
            cpu_percent: raw.cpu_percent.clamp(0.0, 100.0),
            cpu_user_percent: split.map(|s| s.0),
            cpu_system_percent: split.map(|s| s.1),
            cpu_iowait_percent: split.map(|s| s.2),
            per_core_percent: raw
                .per_core_percent
                .iter()
                .map(|p| p.clamp(0.0, 100.0))
                .collect(),
            load: raw.load,
            cpu_freq_mhz: raw.cpu_freq_mhz,
            memory: raw.memory,
            swap_used: raw.swap_used.min(raw.swap_total),
            swap_total: raw.swap_total,
            disks: raw
                .disks
                .iter()
                .map(|d| DiskSpace {
                    mount: d.mount.clone(),
                    fs: d.fs.clone(),
                    total: d.total,
                    used: d.used.min(d.total),
                })
                .collect(),
            disk_read_bps: disk_rate(read),
            disk_write_bps: disk_rate(written),
            network,
            temperatures: order_temperatures(raw.temperatures),
            process_count: None,
            running_count: None,
        }
    }

    fn network(
        &mut self,
        interfaces: &[InterfaceReading],
        elapsed_s: Option<f64>,
    ) -> Option<NetInterface> {
        let current = self.net.as_ref().map(|n| n.name.as_str());
        let Some(picked) = pick_interface(current, interfaces) else {
            self.net = None;
            return None;
        };
        let state = match &mut self.net {
            Some(state) if state.name == picked.name => state,
            slot => {
                // A new pick: no rate until its second reading.
                *slot = Some(NetState {
                    name: picked.name.clone(),
                    last_rx: picked.rx_total,
                    last_tx: picked.tx_total,
                    carried_rx: 0,
                    carried_tx: 0,
                });
                let state = slot.as_mut().expect("just set");
                return Some(NetInterface {
                    name: picked.name.clone(),
                    address: picked.address.clone(),
                    rx_bps: None,
                    tx_bps: None,
                    rx_total: state.last_rx,
                    tx_total: state.last_tx,
                });
            }
        };
        let rx_bps = counter_step(
            &mut state.last_rx,
            &mut state.carried_rx,
            picked.rx_total,
            elapsed_s,
        );
        let tx_bps = counter_step(
            &mut state.last_tx,
            &mut state.carried_tx,
            picked.tx_total,
            elapsed_s,
        );
        Some(NetInterface {
            name: picked.name.clone(),
            address: picked.address.clone(),
            rx_bps,
            tx_bps,
            rx_total: state.carried_rx.saturating_add(picked.rx_total),
            tx_total: state.carried_tx.saturating_add(picked.tx_total),
        })
    }
}

// ─── The sysinfo source ───────────────────────────────────────────────────────

/// The production [`HostSource`]: sysinfo plus, on Linux, `/proc/meminfo` and `/proc/stat`.
/// Created when monitoring starts and dropped when it stops.
pub struct SysinfoSource {
    sys: System,
    disks: Disks,
    networks: Networks,
    /// Opened on the first read that wants temperatures.
    components: Option<Components>,
    disk_rule: DiskRule,
    cpu_freq_mhz: Option<u64>,
}

impl SysinfoSource {
    /// Open the sysinfo handles. Blocking.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            sys: System::new().context("open sysinfo's system handle")?,
            disks: Disks::new().context("open sysinfo's disk list")?,
            networks: Networks::new().context("open sysinfo's network list")?,
            components: None,
            disk_rule: DiskRule::for_host(),
            cpu_freq_mhz: None,
        })
    }

    /// Linux: the `/proc/meminfo` partition, or sysinfo's figures without a cache split when
    /// that file cannot be read.
    #[cfg(target_os = "linux")]
    fn memory(&self) -> Memory {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| memory_from_meminfo(&text))
            .unwrap_or_else(|| {
                Memory::without_cache(self.sys.total_memory(), self.sys.available_memory())
            })
    }

    /// macOS: sysinfo's used figure is app (internal - purgeable) + wired + compressed pages, so
    /// available = total - used. The file-backed (cache) page count is not exposed by sysinfo
    /// and reading it needs host_statistics64 (unsafe), so cache = 0 and free is the rest.
    #[cfg(not(target_os = "linux"))]
    fn memory(&self) -> Memory {
        let total = self.sys.total_memory();
        Memory::without_cache(total, total.saturating_sub(self.sys.used_memory()))
    }

    /// Linux: the aggregate counters of `/proc/stat`.
    #[cfg(target_os = "linux")]
    fn cpu_times() -> Option<CpuTimes> {
        std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| cpu_times_from_proc_stat(&text))
    }

    /// Elsewhere sysinfo has no user/system split.
    #[cfg(not(target_os = "linux"))]
    fn cpu_times() -> Option<CpuTimes> {
        None
    }

    fn disks(&mut self) -> Vec<DiskReading> {
        self.disks.refresh_specifics(
            true,
            DiskRefreshKind::nothing().with_storage().with_io_usage(),
        );
        let readings = self
            .disks
            .iter()
            .map(|d| {
                let usage = d.usage();
                DiskReading {
                    mount: d.mount_point().display().to_string(),
                    fs: d.file_system().to_string_lossy().into_owned(),
                    device: d.name().to_string_lossy().into_owned(),
                    total: d.total_space(),
                    used: d.total_space().saturating_sub(d.available_space()),
                    is_dir: d.mount_point().is_dir(),
                    read_only: d.is_read_only(),
                    read_bytes: usage.total_read_bytes,
                    written_bytes: usage.total_written_bytes,
                }
            })
            .collect();
        keep_real_disks(readings, &self.disk_rule)
    }

    fn interfaces(&mut self) -> Vec<InterfaceReading> {
        self.networks.refresh(true);
        let mut list: Vec<InterfaceReading> = self
            .networks
            .iter()
            .map(|(name, data)| {
                let name = name.to_string_lossy().into_owned();
                let addrs: Vec<IpAddr> = data.ip_networks().iter().map(|n| n.addr).collect();
                let loopback = is_loopback(&name, &addrs);
                InterfaceReading {
                    physical: !loopback && is_physical(&name),
                    address: preferred_address(&addrs),
                    rx_total: data.total_received(),
                    tx_total: data.total_transmitted(),
                    loopback,
                    name,
                }
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    fn temperatures(&mut self) -> anyhow::Result<Vec<(String, Option<f32>)>> {
        let components = match &mut self.components {
            Some(components) => {
                components.refresh(true);
                components
            }
            slot => slot.insert(
                Components::new_with_refreshed_list().context("open sysinfo's sensor list")?,
            ),
        };
        Ok(components
            .iter()
            .map(|c| (c.label().to_owned(), c.temperature()))
            .collect())
    }
}

impl HostSource for SysinfoSource {
    fn host_info(&mut self) -> anyhow::Result<HostInfo> {
        self.sys.refresh_cpu_all();
        self.sys
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let cpus = self.sys.cpus();
        Ok(HostInfo {
            hostname: System::host_name().unwrap_or_default(),
            os_name: System::name().unwrap_or_default(),
            os_version: System::os_version().unwrap_or_default(),
            kernel: System::kernel_version().unwrap_or_default(),
            arch: System::cpu_arch(),
            cpu_brand: cpus
                .first()
                .map(|c| c.brand().trim().to_owned())
                .unwrap_or_default(),
            logical_cores: u32::try_from(cpus.len()).unwrap_or(u32::MAX),
            physical_cores: System::physical_core_count()
                .ok()
                .and_then(|n| u32::try_from(n).ok()),
            boot_time_unix_s: System::boot_time().unwrap_or(0),
            total_memory_bytes: self.sys.total_memory(),
        })
    }

    fn read(&mut self, spec: ReadSpec) -> anyhow::Result<RawReading> {
        // Host totals only: no process table here, and no cpu frequency unless asked.
        self.sys.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_usage())
                .with_memory(MemoryRefreshKind::everything()),
        );
        if spec.cpu_frequency {
            self.sys.refresh_cpu_frequency();
            let cpus = self.sys.cpus();
            let sum: u64 = cpus.iter().map(|c| c.frequency()).sum();
            self.cpu_freq_mhz = (sum > 0).then(|| sum / cpus.len() as u64);
        }
        let temperatures = if spec.temperatures {
            self.temperatures()?
        } else {
            Vec::new()
        };
        Ok(RawReading {
            cpu_percent: self.sys.global_cpu_usage(),
            per_core_percent: self.sys.cpus().iter().map(|c| c.usage()).collect(),
            cpu_times: Self::cpu_times(),
            load: System::load_average().ok().map(|l| LoadAverage {
                one: l.one,
                five: l.five,
                fifteen: l.fifteen,
            }),
            cpu_freq_mhz: self.cpu_freq_mhz,
            memory: self.memory(),
            swap_used: self.sys.used_swap(),
            swap_total: self.sys.total_swap(),
            disks: self.disks(),
            interfaces: self.interfaces(),
            temperatures,
        })
    }
}

// ─── The sampling task ────────────────────────────────────────────────────────

/// Resolve once `flag` is `true` or its sender is gone; immediately when that is already so.
async fn flagged(flag: &mut watch::Receiver<bool>) {
    loop {
        if *flag.borrow() {
            return;
        }
        if flag.changed().await.is_err() {
            return;
        }
    }
}

/// Why the task stopped.
enum End {
    /// The handle disabled monitoring (its run was dropped); nothing left to tidy.
    Disabled,
    /// The daemon is shutting down: end the run so subscribers see `Disabled`.
    Shutdown,
}

async fn ended(stop: &mut watch::Receiver<bool>, shutdown: &mut watch::Receiver<bool>) -> End {
    tokio::select! {
        () = flagged(stop) => End::Disabled,
        () = flagged(shutdown) => End::Shutdown,
    }
}

fn finish(shared: &Shared, generation: u64, end: End) {
    match end {
        End::Disabled => log::debug!("monitor: sampler stopped (disabled)"),
        End::Shutdown => {
            log::debug!("monitor: sampler stopped (shutdown)");
            shared.end_run(generation);
        }
    }
}

/// The sampling task for run `generation`: open the source, prime it, then sample at the demand
/// level's interval until the run is dropped, the daemon shuts down, or the source fails hard.
pub(super) async fn run(shared: Arc<Shared>, generation: u64, mut stop: watch::Receiver<bool>) {
    let mut shutdown = shared.shutdown.clone();
    let mut demand = shared.demand.watch();
    let origin = Instant::now();
    let mono_ms = || u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Open, read the host facts and take the priming reading, all off the runtime.
    let factory = Arc::clone(&shared.factory);
    let open = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mut source = factory()?;
        let info = source.host_info().context("read the host facts")?;
        let prime = source
            .read(ReadSpec {
                temperatures: false,
                cpu_frequency: true,
            })
            .context("take the priming reading")?;
        Ok((source, info, prime))
    });
    let opened = tokio::select! {
        biased;
        end = ended(&mut stop, &mut shutdown) => return finish(&shared, generation, end),
        opened = open => opened,
    };
    let (mut source, info, prime) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(err)) => {
            log::warn!("monitor: could not start sampling: {err:#}");
            return shared.end_run(generation);
        }
        Err(err) => {
            log::error!("monitor: sampler start-up panicked: {err}");
            return shared.end_run(generation);
        }
    };
    shared.set_host_info(generation, info);
    let mut deriver = Deriver::default();
    let _ = deriver.derive(prime, mono_ms());

    let mut last_read = Instant::now();
    let mut freq_read_at = last_read;
    let mut next = last_read + PRIME_DELAY;
    let mut primed = false;
    let mut warned = false;
    log::debug!("monitor: sampler started");

    loop {
        tokio::select! {
            biased;
            end = ended(&mut stop, &mut shutdown) => return finish(&shared, generation, end),
            changed = demand.changed() => {
                // The registry outlives every run (it lives in `Shared`), so this never errs.
                if changed.is_ok() && primed {
                    next = last_read + demand.borrow_and_update().interval;
                }
                continue;
            }
            () = tokio::time::sleep_until(next) => {}
        }

        let level = *demand.borrow_and_update();
        let started = Instant::now();
        let spec = ReadSpec {
            temperatures: level.full,
            cpu_frequency: started.duration_since(freq_read_at) >= FREQUENCY_REFRESH,
        };
        let job = tokio::task::spawn_blocking(move || {
            let result = source.read(spec);
            (source, result)
        });
        let joined = tokio::select! {
            biased;
            end = ended(&mut stop, &mut shutdown) => return finish(&shared, generation, end),
            joined = job => joined,
        };
        let result = match joined {
            Ok((back, result)) => {
                source = back;
                result
            }
            Err(err) => {
                log::error!("monitor: sampler read panicked: {err}");
                return shared.end_run(generation);
            }
        };
        last_read = Instant::now();
        let at_ms = mono_ms();
        // A subscriber that arrived during the read counts from this reading.
        let level_now = *demand.borrow_and_update();
        next = last_read + level_now.interval;
        primed = true;

        match result {
            Ok(raw) => {
                if spec.cpu_frequency {
                    freq_read_at = started;
                }
                warned = false;
                let mut sample = deriver.derive(raw, at_ms);
                sample.unix_ms = (shared.clock)();
                if !shared.publish(generation, sample, level.full) {
                    // The run was replaced while this reading was in flight.
                    return;
                }
            }
            // One warn per run of failures; the next tick retries.
            Err(err) if !warned => {
                warned = true;
                log::warn!("monitor: sampling failed: {err:#}");
            }
            Err(err) => log::debug!("monitor: sampling failed: {err:#}"),
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // ── memory ───────────────────────────────────────────────────────────────

    const MEMINFO: &str = "MemTotal:       16000000 kB\n\
        MemFree:         2000000 kB\n\
        MemAvailable:    9000000 kB\n\
        Buffers:          300000 kB\n\
        Cached:          5000000 kB\n\
        SwapCached:            0 kB\n\
        SReclaimable:     600000 kB\n\
        Shmem:            400000 kB\n";

    #[test]
    fn meminfo_partitions_memory_into_used_cache_and_free() {
        let m = memory_from_meminfo(MEMINFO).unwrap();
        let kb = 1024u64;
        assert_eq!(m.total, 16_000_000 * kb);
        assert_eq!(m.available, 9_000_000 * kb);
        assert_eq!(m.used, 7_000_000 * kb);
        assert_eq!(m.cache, 5_600_000 * kb, "Cached + SReclaimable");
        assert_eq!(m.free, 3_400_000 * kb);
        assert_eq!(m.used + m.cache + m.free, m.total);
    }

    #[test]
    fn meminfo_cache_is_capped_so_the_partition_still_sums() {
        // Cached + SReclaimable exceeds available - MemFree.
        let text = "MemTotal: 1000 kB\nMemFree: 300 kB\nMemAvailable: 400 kB\n\
                    Cached: 900 kB\nSReclaimable: 50 kB\n";
        let m = memory_from_meminfo(text).unwrap();
        assert_eq!(m.cache, 100 * 1024);
        assert_eq!(m.free, 300 * 1024);
        assert_eq!(m.used + m.cache + m.free, m.total);
        assert_eq!(m.available, 400 * 1024);
    }

    #[test]
    fn meminfo_without_the_required_keys_is_none() {
        assert_eq!(
            memory_from_meminfo("MemTotal: 10 kB\nMemFree: 5 kB\n"),
            None
        );
        assert_eq!(memory_from_meminfo(""), None);
    }

    #[test]
    fn a_cacheless_partition_still_sums() {
        let m = Memory::without_cache(100, 30);
        assert_eq!((m.used, m.cache, m.free, m.available), (70, 0, 30, 30));
        let clamped = Memory::without_cache(100, 500);
        assert_eq!(clamped.used + clamped.cache + clamped.free, 100);
    }

    // ── cpu split ────────────────────────────────────────────────────────────

    #[test]
    fn proc_stat_first_line_parses_and_splits_from_deltas() {
        let a = cpu_times_from_proc_stat(
            "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4 5 6 7 8\nintr 1\n",
        )
        .unwrap();
        let b = cpu_times_from_proc_stat("cpu  130 10 70 900 70 5 5 0 0 0\n").unwrap();
        let (user, system, iowait) = cpu_split(&a, &b).unwrap();
        // Deltas: user 30 + nice 10, system 20 + irq 5 + softirq 5, iowait 20, idle 100: total 190.
        assert!((user - 40.0 * 100.0 / 190.0).abs() < 1e-3, "{user}");
        assert!((system - 30.0 * 100.0 / 190.0).abs() < 1e-3, "{system}");
        assert!((iowait - 20.0 * 100.0 / 190.0).abs() < 1e-3, "{iowait}");
        assert_eq!(cpu_split(&b, &a), None, "counters going backwards");
        assert_eq!(cpu_split(&a, &a), None, "no time passed");
        assert_eq!(cpu_times_from_proc_stat("cpu0 1 2 3 4\n"), None);
    }

    // ── disks ────────────────────────────────────────────────────────────────

    const PROC_FILESYSTEMS: &str = "nodev\tsysfs\nnodev\ttmpfs\nnodev\tproc\nnodev\toverlay\n\
        \text4\n\tsquashfs\n\tvfat\n\txfs\n";

    fn mount(mount: &str, fs: &str, device: &str, total: u64, is_dir: bool) -> DiskReading {
        DiskReading {
            mount: mount.into(),
            fs: fs.into(),
            device: device.into(),
            total,
            used: total / 2,
            is_dir,
            ..DiskReading::default()
        }
    }

    fn linux_rule() -> DiskRule {
        DiskRule {
            real_fs: Some(real_filesystems(PROC_FILESYSTEMS)),
            drop_autofs_and_read_only: false,
        }
    }

    fn mounts_of(disks: &[DiskReading]) -> Vec<&str> {
        disks.iter().map(|d| d.mount.as_str()).collect()
    }

    #[test]
    fn a_host_mount_list_keeps_only_the_real_root() {
        let disks = vec![
            mount("/", "ext4", "/dev/nvme0n1p2", 500, true),
            mount("/run", "tmpfs", "tmpfs", 100, true),
            mount(
                "/var/lib/docker/overlay2/x/merged",
                "overlay",
                "overlay",
                500,
                true,
            ),
            mount("/snap/core/1", "squashfs", "/dev/loop0", 60, true),
            mount("/mnt/empty", "ext4", "/dev/sdb1", 0, true),
            mount("/srv/bind", "ext4", "/dev/nvme0n1p2", 500, true),
        ];
        assert_eq!(mounts_of(&keep_real_disks(disks, &linux_rule())), ["/"]);
    }

    #[test]
    fn a_container_mount_list_keeps_only_the_overlay_root() {
        let disks = vec![
            mount("/", "overlay", "overlay", 500, true),
            mount("/etc/resolv.conf", "ext4", "/dev/sda1", 500, false),
            mount("/etc/hosts", "ext4", "/dev/sda1", 500, false),
            mount("/dev/shm", "tmpfs", "shm", 64, true),
        ];
        assert_eq!(mounts_of(&keep_real_disks(disks, &linux_rule())), ["/"]);
    }

    #[test]
    fn zfs_is_kept_and_squashfs_never_listed_as_real() {
        let set = real_filesystems(PROC_FILESYSTEMS);
        assert!(set.contains("ext4") && set.contains("xfs"));
        assert!(!set.contains("squashfs") && !set.contains("tmpfs"));
        let kept = keep_real_disks(
            vec![mount("/tank", "zfs", "tank", 900, true)],
            &linux_rule(),
        );
        assert_eq!(mounts_of(&kept), ["/tank"]);
    }

    #[test]
    fn the_macos_rule_drops_autofs_and_the_read_only_snapshot() {
        let rule = DiskRule {
            real_fs: None,
            drop_autofs_and_read_only: true,
        };
        let mut snapshot = mount("/", "apfs", "disk3s1s1", 500, true);
        snapshot.read_only = true;
        let disks = vec![
            snapshot,
            mount("/System/Volumes/Data", "apfs", "disk3s5", 500, true),
            mount(
                "/System/Volumes/Data/home",
                "autofs",
                "map auto_home",
                1,
                true,
            ),
        ];
        assert_eq!(
            mounts_of(&keep_real_disks(disks, &rule)),
            ["/System/Volumes/Data"]
        );
    }

    // ── network ──────────────────────────────────────────────────────────────

    fn iface(name: &str, total: u64, loopback: bool, physical: bool) -> InterfaceReading {
        InterfaceReading {
            name: name.into(),
            address: None,
            rx_total: total,
            tx_total: 0,
            loopback,
            physical,
        }
    }

    #[test]
    fn loopback_and_virtual_interfaces_lose_to_a_physical_one() {
        let list = vec![
            iface("lo", 9_000, true, false),
            iface("docker0", 5_000, false, false),
            iface("enp5s0", 100, false, true),
        ];
        assert_eq!(pick_interface(None, &list).unwrap().name, "enp5s0");
        // Even a sticky virtual pick yields once a physical interface exists.
        assert_eq!(
            pick_interface(Some("docker0"), &list).unwrap().name,
            "enp5s0"
        );
    }

    #[test]
    fn a_container_picks_its_virtual_eth0() {
        let list = vec![
            iface("eth0", 10, false, false),
            iface("lo", 9_000, true, false),
        ];
        assert_eq!(pick_interface(None, &list).unwrap().name, "eth0");
        assert!(pick_interface(None, &[iface("lo", 1, true, false)]).is_none());
    }

    #[test]
    fn the_pick_is_sticky_while_the_interface_exists() {
        let mut list = vec![
            iface("enp5s0", 100, false, true),
            iface("wlan0", 50, false, true),
        ];
        assert_eq!(pick_interface(None, &list).unwrap().name, "enp5s0");
        list[1].rx_total = 10_000;
        assert_eq!(
            pick_interface(Some("enp5s0"), &list).unwrap().name,
            "enp5s0"
        );
        list.remove(0);
        assert_eq!(pick_interface(Some("enp5s0"), &list).unwrap().name, "wlan0");
    }

    #[test]
    fn loopback_and_address_detection() {
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 5));
        let link = IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap());
        let v6 = IpAddr::V6("2001:db8::5".parse::<Ipv6Addr>().unwrap());
        assert!(is_loopback("lo", &[]));
        assert!(is_loopback("lo0", &[]));
        assert!(is_loopback("x", &[IpAddr::V4(Ipv4Addr::LOCALHOST)]));
        assert!(!is_loopback("lowpan0x", &[v4]));
        assert!(!is_loopback("eth0", &[]));
        assert_eq!(preferred_address(&[link, v6, v4]), Some(v4.to_string()));
        assert_eq!(preferred_address(&[link, v6]), Some(v6.to_string()));
        assert_eq!(preferred_address(&[link]), None);
    }

    // ── temperatures ─────────────────────────────────────────────────────────

    #[test]
    fn temperatures_drop_zero_readings_and_lead_with_the_package() {
        let raw = vec![
            ("acpitz temp1".to_owned(), Some(27.8)),
            ("coretemp Core 0".to_owned(), Some(41.0)),
            ("nvme Composite".to_owned(), Some(0.0)),
            ("coretemp Package id 0".to_owned(), Some(45.0)),
            ("iwlwifi_1 temp1".to_owned(), None),
        ];
        let labels: Vec<String> = order_temperatures(raw)
            .into_iter()
            .map(|t| t.label)
            .collect();
        assert_eq!(
            labels,
            ["coretemp Package id 0", "acpitz temp1", "coretemp Core 0"]
        );
    }

    #[test]
    fn tctl_then_the_hottest_core_stand_in_for_the_package() {
        let amd = vec![
            ("amdgpu edge".to_owned(), Some(50.0)),
            ("k10temp Tctl".to_owned(), Some(60.0)),
        ];
        assert_eq!(order_temperatures(amd)[0].label, "k10temp Tctl");
        let cores = vec![
            ("A sensor".to_owned(), Some(30.0)),
            ("CPU core 1".to_owned(), Some(55.0)),
            ("CPU core 0".to_owned(), Some(62.0)),
        ];
        let ordered = order_temperatures(cores);
        assert_eq!(ordered[0].label, "CPU core 0");
        assert_eq!(ordered[1].label, "A sensor");
    }

    // ── deriver ──────────────────────────────────────────────────────────────

    fn reading(rx: u64, tx: u64, read: u64) -> RawReading {
        RawReading {
            cpu_percent: 12.5,
            interfaces: vec![InterfaceReading {
                name: "enp5s0".into(),
                address: Some("192.168.0.5".into()),
                rx_total: rx,
                tx_total: tx,
                loopback: false,
                physical: true,
            }],
            disks: vec![DiskReading {
                device: "/dev/nvme0n1p2".into(),
                read_bytes: read,
                ..mount("/", "ext4", "/dev/nvme0n1p2", 500, true)
            }],
            ..RawReading::default()
        }
    }

    #[test]
    fn rates_use_the_real_elapsed_time_not_the_nominal_interval() {
        let mut deriver = Deriver::default();
        let first = deriver.derive(reading(1_000, 0, 0), 10_000);
        let net = first.network.unwrap();
        assert_eq!(
            (net.rx_bps, net.tx_bps),
            (None, None),
            "no rate on the first reading"
        );
        assert_eq!(first.disk_read_bps, None);
        // The nominal interval is 2 s, but the second reading came 4 s later.
        let second = deriver.derive(reading(9_000, 400, 8_000), 14_000);
        let net = second.network.unwrap();
        assert_eq!(net.rx_bps, Some(2_000));
        assert_eq!(net.tx_bps, Some(100));
        assert_eq!(second.disk_read_bps, Some(2_000));
        assert_eq!(second.disk_write_bps, Some(0));
    }

    #[test]
    fn a_counter_going_backwards_gives_no_rate_and_keeps_the_total_monotonic() {
        let mut deriver = Deriver::default();
        deriver.derive(reading(5_000, 100, 9_000), 0);
        let reset = deriver.derive(reading(200, 300, 1_000), 2_000);
        let net = reset.network.unwrap();
        assert_eq!(net.rx_bps, None, "rx reset");
        assert_eq!(net.tx_bps, Some(100), "tx kept counting");
        assert_eq!(net.rx_total, 5_200, "the pre-reset value is carried");
        assert_eq!(reset.disk_read_bps, Some(0), "disk deltas clamp at zero");
        let after = deriver.derive(reading(1_200, 500, 2_000), 4_000);
        let net = after.network.unwrap();
        assert_eq!(net.rx_bps, Some(500));
        assert_eq!(net.rx_total, 6_200);
        assert!(net.rx_total >= 5_200);
    }

    #[test]
    fn the_cpu_split_needs_two_readings() {
        let mut deriver = Deriver::default();
        let times = |user, idle| RawReading {
            cpu_times: Some(CpuTimes {
                user,
                idle,
                ..CpuTimes::default()
            }),
            ..RawReading::default()
        };
        assert_eq!(deriver.derive(times(0, 0), 0).cpu_user_percent, None);
        let s = deriver.derive(times(25, 75), 2_000);
        assert_eq!(s.cpu_user_percent, Some(25.0));
        assert_eq!(s.cpu_system_percent, Some(0.0));
        assert_eq!(s.cpu_iowait_percent, Some(0.0));
    }

    // ── the real host ────────────────────────────────────────────────────────

    #[test]
    fn two_real_samples_from_this_host_are_sane() {
        let mut source = SysinfoSource::new().expect("open sysinfo");
        let info = source.host_info().expect("host facts");
        assert!(info.logical_cores >= 1);
        assert!(info.total_memory_bytes > 0);
        let mut deriver = Deriver::default();
        let spec = ReadSpec {
            temperatures: true,
            cpu_frequency: true,
        };
        deriver.derive(source.read(spec).expect("first reading"), 0);
        std::thread::sleep(Duration::from_millis(300));
        let sample = deriver.derive(source.read(spec).expect("second reading"), 300);
        assert!(
            (0.0..=100.0).contains(&sample.cpu_percent),
            "{}",
            sample.cpu_percent
        );
        assert!(
            sample
                .per_core_percent
                .iter()
                .all(|p| (0.0..=100.0).contains(p))
        );
        let m = sample.memory;
        assert!(m.total > 0);
        assert!(m.used <= m.total);
        assert_eq!(m.used + m.cache + m.free, m.total);
        assert!(m.available <= m.total);
        assert!(sample.swap_used <= sample.swap_total);
        assert!(
            sample
                .disks
                .iter()
                .all(|d| d.used <= d.total && d.total > 0)
        );
        if let Some((user, system)) = sample.cpu_user_percent.zip(sample.cpu_system_percent) {
            assert!((0.0..=100.0).contains(&(user + system)));
        }
        assert!(sample.temperatures.iter().all(|t| t.celsius != 0.0));
    }
}
