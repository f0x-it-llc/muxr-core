//! monitor_probe — measures what host monitoring costs with `sysinfo` and which fields this
//! host provides. A hand-run measurement for sizing the server monitor's sampling intervals; it
//! is not a test and changes nothing in muxrd.
//!
//! Usage:
//!     cargo run -p muxrd --example monitor_probe -- [--iterations N] [--interval-ms N]
//!
//! Defaults: 30 iterations, 2000 ms apart. Output is plain text, one section per measurement.

use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use sysinfo::{
    Components, CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, Networks, Pid,
    ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind, Users,
};

struct Args {
    iterations: usize,
    interval: Duration,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        iterations: 30,
        interval: Duration::from_millis(2000),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))?;
        match flag.as_str() {
            "--iterations" => args.iterations = value.parse()?,
            "--interval-ms" => args.interval = Duration::from_millis(value.parse()?),
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    anyhow::ensure!(args.iterations >= 2, "--iterations must be at least 2");
    Ok(args)
}

/// User + system cpu time this process has used so far.
fn own_cpu_time() -> Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes one `rusage` into the pointer we pass, which points at a
    // zero-initialised, correctly sized and aligned local; RUSAGE_SELF is always valid.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return Duration::ZERO;
    }
    // SAFETY: getrusage returned 0, so it filled the struct.
    let usage = unsafe { usage.assume_init() };
    let tv = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// Runs `refresh` `iterations` times, `interval` apart, and prints median/p95 wall time and the
/// probe's own cpu time over the block.
fn measure(label: &str, args: &Args, mut refresh: impl FnMut()) {
    let cpu_before = own_cpu_time();
    let started = Instant::now();
    let mut walls = Vec::with_capacity(args.iterations);
    for i in 0..args.iterations {
        let t = Instant::now();
        refresh();
        walls.push(t.elapsed());
        if i + 1 < args.iterations {
            std::thread::sleep(args.interval);
        }
    }
    let run = started.elapsed();
    let cpu = own_cpu_time().saturating_sub(cpu_before);
    walls.sort();
    let pick = |q: f64| walls[((walls.len() - 1) as f64 * q).round() as usize];
    println!("== {label}");
    println!("   iterations      {}", walls.len());
    println!("   median refresh  {:.3} ms", ms(pick(0.5)));
    println!("   p95 refresh     {:.3} ms", ms(pick(0.95)));
    println!(
        "   max refresh     {:.3} ms",
        ms(*walls.last().unwrap_or(&Duration::ZERO))
    );
    println!(
        "   probe cpu time  {:.1} ms over {:.1} s ({:.3} % of one core)",
        ms(cpu),
        run.as_secs_f64(),
        100.0 * cpu.as_secs_f64() / run.as_secs_f64()
    );
    println!("   cpu per refresh {:.3} ms", ms(cpu) / walls.len() as f64);
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn host_totals() -> RefreshKind {
    RefreshKind::nothing()
        .with_cpu(CpuRefreshKind::nothing().with_usage())
        .with_memory(MemoryRefreshKind::everything())
}

fn process_kind(with_cmd_user: bool) -> ProcessRefreshKind {
    let kind = ProcessRefreshKind::nothing().with_cpu().with_memory();
    let kind = if with_cmd_user {
        kind.with_cmd(UpdateKind::OnlyIfNotSet)
            .with_user(UpdateKind::OnlyIfNotSet)
    } else {
        kind
    };
    // `without_tasks` matters on Linux only: it skips enumerating every thread as a process.
    kind.without_tasks()
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    println!(
        "monitor_probe: sysinfo 0.40, {} iterations, {} ms apart, target {} {}",
        args.iterations,
        args.interval.as_millis(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "MINIMUM_CPU_UPDATE_INTERVAL = {:?}",
        sysinfo::MINIMUM_CPU_UPDATE_INTERVAL
    );
    println!();

    let mut sys = System::new()?;
    let mut disks = Disks::new_with_refreshed_list()?;
    let mut networks = Networks::new_with_refreshed_list()?;
    let mut components = Components::new_with_refreshed_list()?;

    // Prime cpu usage so the first measured refresh computes a real delta.
    sys.refresh_specifics(host_totals());
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);

    let disk_kind = DiskRefreshKind::nothing().with_storage().with_io_usage();
    measure(
        "a. host totals (cpu, memory, swap, load, network, disks)",
        &args,
        || {
            sys.refresh_specifics(host_totals());
            let _ = System::load_average();
            networks.refresh(true);
            disks.refresh_specifics(true, disk_kind);
        },
    );
    println!();

    measure("b. host totals + cpu frequency", &args, || {
        sys.refresh_specifics(host_totals());
        sys.refresh_cpu_frequency();
        let _ = System::load_average();
        networks.refresh(true);
        disks.refresh_specifics(true, disk_kind);
    });
    println!();

    measure("c. processes: cpu + memory", &args, || {
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_kind(false));
    });
    println!("   process count   {}", sys.processes().len());
    println!();

    measure(
        "d. processes: cpu + memory + cmd + user (OnlyIfNotSet)",
        &args,
        || {
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_kind(true));
        },
    );
    println!("   process count   {}", sys.processes().len());
    println!();

    components.refresh(true);
    print_fields(&mut sys, &disks, &networks, &components)?;
    println!();
    probe_pid_reuse(&mut sys)?;
    Ok(())
}

fn print_fields(
    sys: &mut System,
    disks: &Disks,
    networks: &Networks,
    components: &Components,
) -> anyhow::Result<()> {
    println!("== field availability");
    sys.refresh_cpu_all();
    let cpus = sys.cpus();
    println!("-- cpu");
    println!("   logical cores       {}", cpus.len());
    println!(
        "   physical cores      {:?}",
        System::physical_core_count().ok()
    );
    println!(
        "   brand               {:?}",
        cpus.first().map(|c| c.brand())
    );
    println!(
        "   per-core usage      {}",
        cpus.iter()
            .map(|c| format!("{:.0}", c.usage()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "   frequency MHz       {}",
        cpus.iter()
            .map(|c| c.frequency().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!("   global usage        {:.1} %", sys.global_cpu_usage());
    println!(
        "   user/system split   not exposed by sysinfo (Cpu has usage() and frequency() only)"
    );
    print_proc_stat_presence();
    match System::load_average() {
        Ok(l) => println!(
            "   load average        {:.2} {:.2} {:.2}",
            l.one, l.five, l.fifteen
        ),
        Err(e) => println!("   load average        unavailable: {e}"),
    }

    println!("-- host");
    println!("   hostname            {:?}", System::host_name().ok());
    println!(
        "   os                  {:?} {:?}",
        System::name().ok(),
        System::os_version().ok()
    );
    println!("   kernel              {:?}", System::kernel_version().ok());
    println!("   arch                {}", System::cpu_arch());
    println!("   uptime s            {:?}", System::uptime().ok());
    println!("   boot time           {:?}", System::boot_time().ok());

    println!("-- memory (sysinfo)");
    println!("   total               {}", sys.total_memory());
    println!("   free                {}", sys.free_memory());
    println!("   available           {}", sys.available_memory());
    println!("   used                {}", sys.used_memory());
    println!(
        "   swap total/used     {} / {}",
        sys.total_swap(),
        sys.used_swap()
    );
    println!(
        "   cgroup limits       {:?}",
        sys.cgroup_limits().map(|c| (c.total_memory, c.free_memory))
    );
    print_os_memory();

    println!("-- disks (all {} listed by sysinfo)", disks.len());
    for d in disks.iter() {
        let u = d.usage();
        println!(
            "   {:<28} fs={:<10} dev={:<22} total={:<14} avail={:<14} ro={} read={}/{} written={}/{}",
            d.mount_point().display(),
            d.file_system().to_string_lossy(),
            d.name().to_string_lossy(),
            d.total_space(),
            d.available_space(),
            d.is_read_only(),
            u.read_bytes,
            u.total_read_bytes,
            u.written_bytes,
            u.total_written_bytes,
        );
    }
    println!(
        "   io counters: DiskUsage carries both since-last-refresh (read_bytes/written_bytes) and \
         cumulative (total_*) figures; compare the pairs above"
    );
    let kept = filter_disks(disks);
    println!(
        "-- disks after the real-filesystem filter ({} kept)",
        kept.len()
    );
    for d in &kept {
        println!("   {:<28} fs={}", d.0, d.1);
    }

    println!("-- network ({} interfaces)", networks.len());
    let mut names: Vec<_> = networks.keys().collect();
    names.sort();
    for name in names {
        let n = &networks[name];
        let name = name.to_string_lossy();
        println!(
            "   {:<16} rx={}/{} tx={}/{} addrs={:?} {}",
            name,
            n.received(),
            n.total_received(),
            n.transmitted(),
            n.total_transmitted(),
            n.ip_networks().iter().map(|ip| ip.addr).collect::<Vec<_>>(),
            interface_kind(&name)
        );
    }

    println!("-- temperatures ({} components)", components.len());
    for c in components.iter() {
        println!(
            "   {:<32} {:?} C (max {:?}, critical {:?})",
            c.label(),
            c.temperature(),
            c.max(),
            c.critical()
        );
    }
    #[cfg(target_os = "macos")]
    println!(
        "   source: sysinfo's macOS component feature builds on objc2-io-kit's hidsystem \
         (IOHID sensors), per its Cargo features"
    );

    println!("-- processes");
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything().without_environ(),
    );
    let users = Users::new_with_refreshed_list()?;
    let me = sysinfo::get_current_pid().map_err(|e| anyhow::anyhow!(e))?;
    for pid in [me, Pid::from_u32(1)] {
        let Some(p) = sys.process(pid) else {
            println!("   pid {pid}: not visible");
            continue;
        };
        let user = p
            .user_id()
            .and_then(|uid| users.get_user_by_id(uid).map(|u| u.name().to_string()));
        println!("   pid {pid}");
        println!("     name              {:?}", p.name());
        println!("     cmd               {:?}", p.cmd());
        println!(
            "     user              {user:?} (uid {:?})",
            p.user_id().map(|u| u.to_string())
        );
        println!("     parent            {:?}", p.parent());
        println!("     start_time        {}", p.start_time());
        println!("     run_time s        {}", p.run_time());
        println!("     accumulated cpu   {} ms", p.accumulated_cpu_time());
        println!("     memory resident   {}", p.memory());
        println!("     threads (tasks)   {:?}", p.tasks().map(HashSet::len));
        println!("     status            {:?}", p.status());
    }
    println!(
        "   nice                not exposed by sysinfo (no nice/priority accessor on Process)"
    );
    println!(
        "   thread count        only through tasks(), which needs with_tasks() and is Linux-only"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn print_os_memory() {
    println!("-- memory (/proc/meminfo)");
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        println!("   unreadable");
        return;
    };
    for line in text.lines() {
        let key = line.split(':').next().unwrap_or("");
        if matches!(
            key,
            "MemTotal"
                | "MemFree"
                | "MemAvailable"
                | "Buffers"
                | "Cached"
                | "SReclaimable"
                | "Shmem"
        ) {
            println!("   {line}");
        }
    }
}

#[cfg(target_os = "macos")]
fn print_os_memory() {
    println!("-- memory (vm_stat)");
    match Command::new("vm_stat").output() {
        Ok(out) => {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                println!("   {line}");
            }
        }
        Err(e) => println!("   vm_stat failed: {e}"),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn print_os_memory() {}

#[cfg(target_os = "linux")]
fn print_proc_stat_presence() {
    println!(
        "   /proc/stat present  {}",
        Path::new("/proc/stat").exists()
    );
}

#[cfg(not(target_os = "linux"))]
fn print_proc_stat_presence() {
    println!("   /proc/stat present  n/a (not Linux)");
}

/// Linux: an interface with a backing device under /sys/class/net is physical.
#[cfg(target_os = "linux")]
fn interface_kind(name: &str) -> &'static str {
    if Path::new("/sys/class/net")
        .join(name)
        .join("device")
        .exists()
    {
        "physical"
    } else {
        "virtual"
    }
}

#[cfg(not(target_os = "linux"))]
fn interface_kind(_name: &str) -> &'static str {
    ""
}

/// The real-filesystem rule the monitor will use: (mount point, fs type) of the kept rows.
fn filter_disks(disks: &Disks) -> Vec<(String, String)> {
    let real = RealFilesystems::load();
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for d in disks.iter() {
        let fs = d.file_system().to_string_lossy().to_string();
        if d.total_space() == 0 || !real.keeps(&fs, d.is_read_only()) {
            continue;
        }
        if !seen.insert(d.name().to_os_string()) {
            continue;
        }
        kept.push((d.mount_point().display().to_string(), fs));
    }
    kept
}

/// Linux: fs types in /proc/filesystems without `nodev`, minus squashfs/nullfs; zfs is kept.
/// An unreadable /proc/filesystems keeps every row (the probe reports, it does not hide).
#[cfg(target_os = "linux")]
struct RealFilesystems(Option<HashSet<String>>);

#[cfg(target_os = "linux")]
impl RealFilesystems {
    fn load() -> Self {
        let set = std::fs::read_to_string("/proc/filesystems")
            .ok()
            .map(|text| {
                text.lines()
                    .filter(|l| !l.starts_with("nodev"))
                    .map(|l| l.trim().to_string())
                    .filter(|fs| !fs.is_empty() && fs != "squashfs" && fs != "nullfs")
                    .collect()
            });
        Self(set)
    }

    fn keeps(&self, fs: &str, _read_only: bool) -> bool {
        self.0
            .as_ref()
            .is_none_or(|set| set.contains(fs) || fs == "zfs")
    }
}

/// Elsewhere: on macOS drop autofs and read-only system-snapshot mounts; other targets keep all.
#[cfg(not(target_os = "linux"))]
struct RealFilesystems;

#[cfg(not(target_os = "linux"))]
impl RealFilesystems {
    fn load() -> Self {
        Self
    }

    #[cfg(target_os = "macos")]
    fn keeps(&self, fs: &str, read_only: bool) -> bool {
        fs != "autofs" && !read_only
    }

    #[cfg(not(target_os = "macos"))]
    fn keeps(&self, _fs: &str, _read_only: bool) -> bool {
        true
    }
}

/// Spawns short-lived children until one reuses the first child's pid (or 200 attempts pass),
/// then reports whether sysinfo's `OnlyIfNotSet` refresh re-read the new process's name and cmd.
fn probe_pid_reuse(sys: &mut System) -> anyhow::Result<()> {
    println!("== pid reuse");
    let kind = process_kind(true);
    let snapshot = |sys: &System, pid: Pid| {
        sys.process(pid).map(|p| {
            (
                p.start_time(),
                p.name().to_string_lossy().to_string(),
                p.cmd().to_vec(),
            )
        })
    };
    let mut first = Command::new("sleep").arg("1").spawn()?;
    let pid = Pid::from_u32(first.id());
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
    let before = snapshot(sys, pid);
    first.wait()?;
    let mut reused = None;
    for _ in 0..200 {
        let mut child = Command::new("sleep").arg("2").spawn()?;
        if child.id() == pid.as_u32() {
            reused = Some(child);
            break;
        }
        child.kill()?;
        child.wait()?;
    }
    let Some(mut child) = reused else {
        println!("   not observed (no pid reused in 200 spawns; before = {before:?})");
        return Ok(());
    };
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), false, kind);
    let after = snapshot(sys, pid);
    let re_read = match (&before, &after) {
        (Some(b), Some(a)) => a.0 != b.0 && a.2 != b.2,
        _ => false,
    };
    println!("   pid {pid} reused; before = {before:?}, after = {after:?}");
    println!(
        "   name/cmd re-read for the new process: {}",
        if re_read {
            "yes"
        } else {
            "no (stale or missing)"
        }
    );
    child.kill()?;
    child.wait()?;
    Ok(())
}
