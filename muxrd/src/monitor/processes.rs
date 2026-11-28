//! processes — on-demand process sampling: the registry of watchers, the identity cache, and
//! the per-query view (filter, sort, top-N, pinned row).
//!
//! The expensive refresh runs only while a [`ProcessSubscription`] is alive, on the sampler's
//! `spawn_blocking` path; everything here is pure bookkeeping over what the host source returns.
//!
//! Command lines are shown only for processes running as muxrd's own uid; every other row
//! carries an empty one (the host source does not even read them, and [`ProcessTracker`] drops
//! any that arrive). The filter never matches command lines either way.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;

use super::model::{ProcessQuery, ProcessRow, ProcessSnapshot, ProcessSort, ProcessState};
use super::sampler::HostSource;

// ─── What the host source returns ─────────────────────────────────────────────

/// One process as the host source sees it. The source resolves the user name; the identity cache
/// decides whether to trust these identity fields (see [`ProcessTracker`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RawProcess {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    /// Unix seconds; with `pid` it identifies the process.
    pub start_unix_s: i64,
    /// May be empty (the refresh raced the exec, or the host hides it).
    pub name: String,
    /// The command line's arguments; empty when not (yet) readable, and for any uid but
    /// muxrd's own.
    pub cmd: Vec<String>,
    /// The owning (real) uid; `None` when unknown. Decides whether `cmd` may be shown.
    pub uid: Option<u32>,
    /// User name, else the numeric uid, else empty.
    pub user: String,
    /// Percent of the whole machine's cpu capacity.
    pub cpu_percent: f32,
    /// `None` when the host refuses the figure.
    pub memory_bytes: Option<u64>,
    pub cpu_time_ms: u64,
    pub state: ProcessState,
}

/// One refresh: the process table, the `/proc/<pid>/status` text of each pinned pid, and
/// muxrd's own uid (whose rows alone keep their command line).
pub(super) struct ProcessReading {
    pub(super) processes: anyhow::Result<Vec<RawProcess>>,
    pub(super) statuses: Vec<(u32, Option<String>)>,
    pub(super) own_uid: Option<u32>,
}

/// Refresh the process table and read the status of each of `pinned`. Blocking.
pub(super) fn collect(source: &mut dyn HostSource, pinned: &[u32]) -> ProcessReading {
    let processes = source.read_processes();
    let statuses = pinned
        .iter()
        .map(|pid| (*pid, source.read_proc_status(*pid)))
        .collect();
    ProcessReading {
        processes,
        statuses,
        own_uid: source.own_uid(),
    }
}

/// The `Threads:` figure of `/proc/<pid>/status` text.
pub(crate) fn threads_from_status(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|rest| rest.trim().parse().ok())
}

// ─── Identity ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Identity {
    name: String,
    command: String,
    user: String,
}

impl Identity {
    fn from_raw(raw: &RawProcess) -> Self {
        Self {
            name: display_name(raw),
            command: raw.cmd.join(" "),
            user: raw.user.clone(),
        }
    }
}

/// The host's name, else the executable's file name (from the command line), else `<unknown>`.
fn display_name(raw: &RawProcess) -> String {
    let name = raw.name.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    raw.cmd
        .first()
        .and_then(|exe| exe.rsplit('/').next())
        .filter(|n| !n.is_empty())
        .map_or_else(|| "<unknown>".to_owned(), str::to_owned)
}

/// The host reports a name for the process, and not the cached one. An empty report (the
/// refresh raced the exec) never replaces a known identity.
fn renamed(known: &Identity, process: &RawProcess) -> bool {
    let reported = process.name.trim();
    !reported.is_empty() && reported != known.name
}

/// The refreshed table in display form, before any query is applied.
#[derive(Debug, Default)]
pub(crate) struct ProcessTable {
    pub(crate) rows: Vec<ProcessRow>,
    pub(crate) total: u32,
    pub(crate) running: u32,
}

/// Keeps name, command line and user per `(pid, start time)`, so a recycled pid never shows its
/// predecessor's identity. The cached identity is replaced when its command line was empty and
/// is now readable, or when the host reports a different name for the same `(pid, start time)`
/// (the process exec'd: a child seen between fork and exec first carries its parent's name).
/// Lives in the sampler task; cleared when the last process subscription goes.
#[derive(Debug, Default)]
pub(crate) struct ProcessTracker {
    cache: HashMap<(u32, i64), Identity>,
}

impl ProcessTracker {
    pub(crate) fn clear(&mut self) {
        self.cache.clear();
    }

    /// Fold one refresh into the cache and return the table. Entries of vanished processes go.
    /// Rows whose uid is not `own_uid` (muxrd's) lose their command line; with no `own_uid`,
    /// every row does.
    pub(crate) fn ingest(&mut self, raw: Vec<RawProcess>, own_uid: Option<u32>) -> ProcessTable {
        let mut live: HashMap<(u32, i64), Identity> = HashMap::with_capacity(raw.len());
        let mut rows = Vec::with_capacity(raw.len());
        let mut running = 0;
        for mut process in raw {
            if own_uid.is_none() || process.uid != own_uid {
                process.cmd.clear();
            }
            let key = (process.pid, process.start_unix_s);
            let identity = match self.cache.remove(&key) {
                // Known process whose command line was empty and is now readable, or that
                // exec'd (the host names it differently now): take the new identity whole.
                Some(known)
                    if (known.command.is_empty() && !process.cmd.is_empty())
                        || renamed(&known, &process) =>
                {
                    Identity::from_raw(&process)
                }
                Some(known) => known,
                None => Identity::from_raw(&process),
            };
            if process.state == ProcessState::Running {
                running += 1;
            }
            rows.push(ProcessRow {
                pid: process.pid,
                parent_pid: process.parent_pid,
                name: identity.name.clone(),
                command: identity.command.clone(),
                user: identity.user.clone(),
                cpu_percent: process.cpu_percent.clamp(0.0, 100.0),
                memory_bytes: process.memory_bytes,
                threads: None,
                start_unix_s: process.start_unix_s,
                cpu_time_ms: process.cpu_time_ms,
                state: process.state,
            });
            live.insert(key, identity);
        }
        self.cache = live;
        ProcessTable {
            total: u32::try_from(rows.len()).unwrap_or(u32::MAX),
            running,
            rows,
        }
    }
}

// ─── The view ─────────────────────────────────────────────────────────────────

fn matches_filter(row: &ProcessRow, needle: &str) -> bool {
    needle.is_empty()
        || row.name.to_lowercase().contains(needle)
        || row.user.to_lowercase().contains(needle)
        || row.pid.to_string().contains(needle)
}

/// The snapshot `query` sees of `table`. `threads` holds the thread counts read for pinned pids.
pub(crate) fn view(
    table: &ProcessTable,
    query: &ProcessQuery,
    threads: &HashMap<u32, u32>,
    revision: u64,
    unix_ms: u64,
) -> ProcessSnapshot {
    let needle = query.filter.trim().to_lowercase();
    let mut selected: Vec<&ProcessRow> = table
        .rows
        .iter()
        .filter(|row| matches_filter(row, &needle))
        .collect();
    selected.sort_by(|a, b| {
        let primary = match query.sort {
            ProcessSort::Cpu => b.cpu_percent.total_cmp(&a.cpu_percent),
            ProcessSort::Memory => b.memory_bytes.cmp(&a.memory_bytes),
            ProcessSort::Pid => a.pid.cmp(&b.pid),
            ProcessSort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        };
        let primary = if query.reverse {
            primary.reverse()
        } else {
            primary
        };
        primary.then_with(|| a.pid.cmp(&b.pid))
    });
    selected.truncate(query.effective_limit());

    let mut rows: Vec<ProcessRow> = selected.into_iter().cloned().collect();
    if let Some((pid, start)) = query.pinned {
        let pinned_threads = threads.get(&pid).copied();
        match rows
            .iter_mut()
            .find(|r| r.pid == pid && r.start_unix_s == start)
        {
            Some(row) => row.threads = pinned_threads,
            None => {
                if let Some(row) = table
                    .rows
                    .iter()
                    .find(|r| r.pid == pid && r.start_unix_s == start)
                {
                    rows.push(ProcessRow {
                        threads: pinned_threads,
                        ..row.clone()
                    });
                }
            }
        }
    }
    ProcessSnapshot {
        revision,
        unix_ms,
        total: table.total,
        running: table.running,
        rows,
    }
}

// ─── Subscriptions ────────────────────────────────────────────────────────────

type SnapshotTx = watch::Sender<Option<Arc<ProcessSnapshot>>>;

struct Entry {
    query: ProcessQuery,
    tx: SnapshotTx,
}

#[derive(Default)]
struct Subs {
    next_id: u64,
    revision: u64,
    entries: HashMap<u64, Entry>,
}

struct RegistryInner {
    subs: Mutex<Subs>,
    /// The smallest interval any live subscription asks for; `None` while there is none.
    interval: watch::Sender<Option<Duration>>,
}

/// The live process subscriptions. Cheap to clone; every clone shares one registry.
#[derive(Clone)]
pub(crate) struct ProcessRegistry {
    inner: Arc<RegistryInner>,
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                subs: Mutex::new(Subs::default()),
                interval: watch::Sender::new(None),
            }),
        }
    }
}

impl ProcessRegistry {
    fn subs(&self) -> MutexGuard<'_, Subs> {
        // A poisoned lock still holds a valid map: every write is one insert, remove or replace.
        self.inner
            .subs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish_interval(&self, subs: &Subs) {
        let interval = subs
            .entries
            .values()
            .map(|e| e.query.effective_interval())
            .min();
        self.inner.interval.send_if_modified(|current| {
            let changed = *current != interval;
            *current = interval;
            changed
        });
    }

    /// Register a watcher; its demand lasts until the subscription drops or [`Self::clear`].
    pub(crate) fn register(&self, query: ProcessQuery) -> ProcessSubscription {
        let (tx, rx) = watch::channel(None);
        let mut subs = self.subs();
        let id = subs.next_id;
        subs.next_id += 1;
        subs.entries.insert(id, Entry { query, tx });
        self.publish_interval(&subs);
        ProcessSubscription {
            rx,
            guard: Some(ProcessGuard {
                registry: self.clone(),
                id,
            }),
        }
    }

    /// Forget every watcher (monitoring was disabled): their feeds end.
    pub(crate) fn clear(&self) {
        let mut subs = self.subs();
        subs.entries.clear();
        self.publish_interval(&subs);
    }

    /// The smallest requested interval, `None` when nobody is watching processes.
    pub(crate) fn interval(&self) -> Option<Duration> {
        *self.inner.interval.borrow()
    }

    /// A receiver that observes every change of [`Self::interval`].
    pub(crate) fn watch(&self) -> watch::Receiver<Option<Duration>> {
        self.inner.interval.subscribe()
    }

    /// The distinct pinned pids of the live watchers, or `None` when there is no watcher.
    pub(crate) fn pinned_pids(&self) -> Option<Vec<u32>> {
        let subs = self.subs();
        if subs.entries.is_empty() {
            return None;
        }
        let mut pids: Vec<u32> = subs
            .entries
            .values()
            .filter_map(|e| e.query.pinned.map(|(pid, _)| pid))
            .collect();
        pids.sort_unstable();
        pids.dedup();
        Some(pids)
    }

    /// Hand every watcher its own view of `table`.
    pub(crate) fn publish(&self, table: &ProcessTable, threads: &HashMap<u32, u32>, unix_ms: u64) {
        let mut subs = self.subs();
        subs.revision += 1;
        let revision = subs.revision;
        for entry in subs.entries.values() {
            let snapshot = view(table, &entry.query, threads, revision, unix_ms);
            entry.tx.send_replace(Some(Arc::new(snapshot)));
        }
    }

    fn release(&self, id: u64) {
        let mut subs = self.subs();
        if subs.entries.remove(&id).is_some() {
            self.publish_interval(&subs);
        }
    }
}

struct ProcessGuard {
    registry: ProcessRegistry,
    id: u64,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.registry.release(self.id);
    }
}

/// A live feed of one query's process snapshots. Dropping it releases its demand; once the last
/// one is gone the sampler stops refreshing processes and forgets the table.
pub struct ProcessSubscription {
    rx: watch::Receiver<Option<Arc<ProcessSnapshot>>>,
    guard: Option<ProcessGuard>,
}

impl std::fmt::Debug for ProcessSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessSubscription")
            .field("live", &self.guard.is_some())
            .finish_non_exhaustive()
    }
}

impl ProcessSubscription {
    /// A subscription that never yields: monitoring is off.
    pub(super) fn closed() -> Self {
        let (_, rx) = watch::channel(None);
        Self { rx, guard: None }
    }

    /// The next snapshot, skipping any this subscriber was too slow to take (the newest wins).
    /// `None` once monitoring is disabled, and on every later call.
    pub async fn recv(&mut self) -> Option<Arc<ProcessSnapshot>> {
        loop {
            if self.rx.changed().await.is_err() {
                self.guard = None;
                return None;
            }
            if let Some(snapshot) = self.rx.borrow_and_update().clone() {
                return Some(snapshot);
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{
        HostInfo, MIN_PROCESS_INTERVAL, MonitorHandle, RawReading, ReadSpec, SourceFactory, Tier,
        VitalsItem,
    };
    use tokio::sync::watch as tokio_watch;

    /// The uid the fakes report as muxrd's own.
    const OWN_UID: u32 = 1000;

    fn raw(pid: u32, start: i64, name: &str) -> RawProcess {
        RawProcess {
            pid,
            parent_pid: Some(1),
            start_unix_s: start,
            name: name.into(),
            cmd: vec![format!("/usr/bin/{name}"), "--secret-flag".into()],
            uid: Some(OWN_UID),
            user: "alice".into(),
            cpu_percent: 0.0,
            memory_bytes: Some(0),
            cpu_time_ms: 0,
            state: ProcessState::Sleeping,
        }
    }

    fn with(mut p: RawProcess, cpu: f32, mem: Option<u64>) -> RawProcess {
        p.cpu_percent = cpu;
        p.memory_bytes = mem;
        p
    }

    fn table_of(procs: Vec<RawProcess>) -> ProcessTable {
        ProcessTracker::default().ingest(procs, Some(OWN_UID))
    }

    fn pids(snap: &ProcessSnapshot) -> Vec<u32> {
        snap.rows.iter().map(|r| r.pid).collect()
    }

    fn sample_table() -> ProcessTable {
        table_of(vec![
            with(raw(10, 100, "Zed"), 5.0, Some(300)),
            with(raw(20, 100, "alpha"), 50.0, Some(100)),
            with(raw(30, 100, "beta"), 5.0, Some(200)),
            with(raw(40, 100, "gamma"), 20.0, None),
        ])
    }

    fn run_view(table: &ProcessTable, query: &ProcessQuery) -> ProcessSnapshot {
        view(table, query, &HashMap::new(), 1, 0)
    }

    #[test]
    fn every_sort_column_and_direction_with_pid_tie_break() {
        let table = sample_table();
        let q = |sort, reverse| ProcessQuery {
            sort,
            reverse,
            ..ProcessQuery::default()
        };
        // Cpu descending; 10 and 30 tie at 5.0 and stay pid-ascending.
        assert_eq!(
            pids(&run_view(&table, &q(ProcessSort::Cpu, false))),
            [20, 40, 10, 30]
        );
        // Reversed: ascending cpu, the tie still pid-ascending.
        assert_eq!(
            pids(&run_view(&table, &q(ProcessSort::Cpu, true))),
            [10, 30, 40, 20]
        );
        // Memory descending, a refused figure last.
        assert_eq!(
            pids(&run_view(&table, &q(ProcessSort::Memory, false))),
            [10, 30, 20, 40]
        );
        assert_eq!(
            pids(&run_view(&table, &q(ProcessSort::Memory, true))),
            [40, 20, 30, 10]
        );
        assert_eq!(
            pids(&run_view(&table, &q(ProcessSort::Pid, false))),
            [10, 20, 30, 40]
        );
        assert_eq!(
            pids(&run_view(&table, &q(ProcessSort::Pid, true))),
            [40, 30, 20, 10]
        );
        // Names compare case-insensitively: alpha, beta, gamma, Zed.
        assert_eq!(
            pids(&run_view(&table, &q(ProcessSort::Name, false))),
            [20, 30, 40, 10]
        );
        assert_eq!(
            pids(&run_view(&table, &q(ProcessSort::Name, true))),
            [10, 40, 30, 20]
        );
    }

    #[test]
    fn limit_is_clamped_to_one_through_two_hundred() {
        let procs: Vec<_> = (1..=300).map(|p| raw(p, 1, "p")).collect();
        let table = table_of(procs);
        let limited = |limit| ProcessQuery {
            limit,
            sort: ProcessSort::Pid,
            ..ProcessQuery::default()
        };
        assert_eq!(run_view(&table, &limited(0)).rows.len(), 1);
        assert_eq!(run_view(&table, &limited(7)).rows.len(), 7);
        assert_eq!(run_view(&table, &limited(1_000)).rows.len(), 200);
        assert_eq!(run_view(&table, &ProcessQuery::default()).rows.len(), 50);
        assert_eq!(
            run_view(&table, &limited(7)).total,
            300,
            "total ignores the cut"
        );
    }

    #[test]
    fn filter_matches_name_user_or_pid_but_never_the_command_line() {
        let mut procs = vec![raw(10, 1, "Firefox"), raw(2_000, 1, "bash")];
        procs[1].user = "Bob".into();
        let table = table_of(procs);
        let by = |f: &str| {
            pids(&run_view(
                &table,
                &ProcessQuery {
                    filter: f.into(),
                    sort: ProcessSort::Pid,
                    ..ProcessQuery::default()
                },
            ))
        };
        assert_eq!(by("FIRE"), [10], "name, case-insensitive");
        assert_eq!(by("bob"), [2_000], "user");
        assert_eq!(by("000"), [2_000], "decimal pid");
        assert_eq!(by(""), [10, 2_000]);
        // `--secret-flag` is only in the command lines.
        assert!(by("secret").is_empty());
        assert!(by("/usr/bin").is_empty());
    }

    #[test]
    fn the_pinned_row_is_appended_once_and_only_for_the_same_start_time() {
        let table = sample_table();
        let query = |pinned| ProcessQuery {
            sort: ProcessSort::Pid,
            limit: 2,
            pinned,
            ..ProcessQuery::default()
        };
        let threads = HashMap::from([(40, 9), (10, 3)]);

        // Outside the cut: appended with its thread count.
        let snap = view(&table, &query(Some((40, 100))), &threads, 1, 0);
        assert_eq!(pids(&snap), [10, 20, 40]);
        assert_eq!(snap.rows[2].threads, Some(9));
        assert_eq!(
            snap.rows[0].threads, None,
            "list rows carry no thread count"
        );

        // Already inside the cut: not duplicated, but it carries its threads.
        let snap = view(&table, &query(Some((10, 100))), &threads, 1, 0);
        assert_eq!(pids(&snap), [10, 20]);
        assert_eq!(snap.rows[0].threads, Some(3));

        // A start time that changed (a recycled pid): not appended.
        let snap = view(&table, &query(Some((40, 999))), &threads, 1, 0);
        assert_eq!(pids(&snap), [10, 20]);
        let snap = view(&table, &query(None), &threads, 1, 0);
        assert!(snap.rows.iter().all(|r| r.threads.is_none()));
    }

    #[test]
    fn counts_come_from_the_whole_table() {
        let mut procs = vec![raw(1, 1, "a"), raw(2, 1, "b"), raw(3, 1, "c")];
        procs[0].state = ProcessState::Running;
        procs[2].state = ProcessState::Running;
        let snap = run_view(
            &table_of(procs),
            &ProcessQuery {
                filter: "b".into(),
                ..ProcessQuery::default()
            },
        );
        assert_eq!((snap.total, snap.running, snap.rows.len()), (3, 2, 1));
    }

    #[test]
    fn a_recycled_pid_takes_its_new_identity() {
        let mut tracker = ProcessTracker::default();
        let table = tracker.ingest(vec![raw(50, 100, "old")], Some(OWN_UID));
        assert_eq!(table.rows[0].name, "old");
        assert!(table.rows[0].command.contains("/usr/bin/old"));

        let table = tracker.ingest(vec![raw(50, 200, "new")], Some(OWN_UID));
        assert_eq!(table.rows[0].name, "new");
        assert!(table.rows[0].command.contains("/usr/bin/new"));
        assert_eq!(tracker.cache.len(), 1, "the predecessor's entry is gone");
    }

    #[test]
    fn an_empty_command_line_is_filled_on_a_later_refresh() {
        let mut tracker = ProcessTracker::default();
        let mut early = raw(60, 100, "");
        early.cmd.clear();
        let table = tracker.ingest(vec![early], Some(OWN_UID));
        assert_eq!(table.rows[0].name, "<unknown>");
        assert_eq!(table.rows[0].command, "");

        let table = tracker.ingest(vec![raw(60, 100, "tool")], Some(OWN_UID));
        assert_eq!(table.rows[0].name, "tool");
        assert_eq!(table.rows[0].command, "/usr/bin/tool --secret-flag");
        // Once filled the cached identity stands while the name does not change, and an
        // empty name (a refresh racing an exec) never replaces it.
        let mut later = raw(60, 100, "tool");
        later.cmd = vec!["other".into()];
        let row = &tracker.ingest(vec![later], Some(OWN_UID)).rows[0];
        assert_eq!(row.command, "/usr/bin/tool --secret-flag");
        let mut blank = raw(60, 100, "");
        blank.cmd = vec!["other".into()];
        assert_eq!(
            tracker.ingest(vec![blank], Some(OWN_UID)).rows[0].name,
            "tool"
        );
    }

    #[test]
    fn a_new_name_for_the_same_pid_and_start_replaces_the_identity() {
        let mut tracker = ProcessTracker::default();
        // Seen between fork and exec: the parent's name and command line.
        let forked = raw(80, 100, "bash");
        let table = tracker.ingest(vec![forked], Some(OWN_UID));
        assert_eq!(table.rows[0].name, "bash");
        let mut execd = raw(80, 100, "make");
        execd.user = "bob".into();
        let table = tracker.ingest(vec![execd], Some(OWN_UID));
        assert_eq!(table.rows[0].name, "make");
        assert_eq!(table.rows[0].command, "/usr/bin/make --secret-flag");
        assert_eq!(table.rows[0].user, "bob");
        assert_eq!(tracker.cache.len(), 1);
    }

    #[test]
    fn only_rows_of_our_own_uid_keep_their_command_line() {
        let mut theirs = raw(91, 1, "postgres");
        theirs.uid = Some(OWN_UID + 1);
        theirs.user = "postgres".into();
        let mut unknown = raw(92, 1, "");
        unknown.uid = None;
        let table = table_of(vec![raw(90, 1, "mine"), theirs, unknown]);
        assert_eq!(table.rows[0].command, "/usr/bin/mine --secret-flag");
        assert_eq!(table.rows[1].command, "", "another uid: no command line");
        assert_eq!(table.rows[1].name, "postgres", "name unchanged");
        assert_eq!(table.rows[1].user, "postgres", "user unchanged");
        assert_eq!(table.rows[2].command, "", "an unknown uid: no command line");
        assert_eq!(
            table.rows[2].name, "<unknown>",
            "no fallback to a dropped command line"
        );
        // Without our own uid nothing keeps a command line.
        let table = ProcessTracker::default().ingest(vec![raw(90, 1, "mine")], None);
        assert_eq!(table.rows[0].command, "");
    }

    #[test]
    fn an_empty_name_falls_back_to_the_executable_file_name() {
        let mut p = raw(70, 1, "");
        p.cmd = vec!["/opt/My App/bin/tool".into(), "-x".into()];
        let mut bare = raw(71, 1, "  ");
        bare.cmd.clear();
        let table = table_of(vec![p, bare]);
        assert_eq!(table.rows[0].name, "tool");
        assert_eq!(table.rows[1].name, "<unknown>");
    }

    #[test]
    fn threads_come_from_the_status_text() {
        let text = "Name:\tx\nThreads:\t12\nVmRSS:\t1 kB\n";
        assert_eq!(threads_from_status(text), Some(12));
        assert_eq!(threads_from_status("Name:\tx\n"), None);
        assert_eq!(threads_from_status("Threads:\tmany\n"), None);
    }

    // ── Through the sampler, over a scripted host ──

    #[derive(Default)]
    struct Script {
        procs: Vec<RawProcess>,
        statuses: HashMap<u32, String>,
        refreshes: usize,
        released: usize,
        /// When set, refresh `n` (1-based) reports this cpu figure on every row.
        cpu_by_refresh: Option<fn(usize) -> f32>,
    }

    struct FakeHost(Arc<Mutex<Script>>);

    impl HostSource for FakeHost {
        fn host_info(&mut self) -> anyhow::Result<HostInfo> {
            Ok(HostInfo::default())
        }

        fn read(&mut self, _spec: ReadSpec) -> anyhow::Result<RawReading> {
            Ok(RawReading::default())
        }

        fn read_processes(&mut self) -> anyhow::Result<Vec<RawProcess>> {
            let mut script = self.0.lock().unwrap();
            script.refreshes += 1;
            let mut procs = script.procs.clone();
            if let Some(cpu) = script.cpu_by_refresh {
                let figure = cpu(script.refreshes);
                procs.iter_mut().for_each(|p| p.cpu_percent = figure);
            }
            Ok(procs)
        }

        fn own_uid(&mut self) -> Option<u32> {
            Some(OWN_UID)
        }

        fn read_proc_status(&mut self, pid: u32) -> Option<String> {
            self.0.lock().unwrap().statuses.get(&pid).cloned()
        }

        fn release_processes(&mut self) {
            self.0.lock().unwrap().released += 1;
        }
    }

    struct Rig {
        handle: MonitorHandle,
        script: Arc<Mutex<Script>>,
        _shutdown: tokio_watch::Sender<bool>,
    }

    fn rig(procs: Vec<RawProcess>) -> Rig {
        let script = Arc::new(Mutex::new(Script {
            procs,
            ..Script::default()
        }));
        let shared = Arc::clone(&script);
        let factory: SourceFactory =
            Arc::new(move || Ok(Box::new(FakeHost(Arc::clone(&shared))) as Box<dyn HostSource>));
        let (shutdown, rx) = tokio_watch::channel(false);
        let handle = MonitorHandle::with_source(factory, rx);
        handle.set_enabled(true);
        Rig {
            handle,
            script,
            _shutdown: shutdown,
        }
    }

    impl Rig {
        fn refreshes(&self) -> usize {
            self.script.lock().unwrap().refreshes
        }

        fn script(&self) -> MutexGuard<'_, Script> {
            self.script.lock().unwrap()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_process_subscription_processes_are_never_refreshed() {
        let rig = rig(vec![raw(1, 1, "init")]);
        let mut vitals = rig.handle.subscribe(Tier::Summary);
        for _ in 0..6 {
            match vitals.recv().await {
                VitalsItem::Sample(s) => {
                    assert_eq!((s.process_count, s.running_count), (None, None));
                }
                other => panic!("expected a sample, got {other:?}"),
            }
        }
        assert_eq!(rig.refreshes(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_subscription_fills_the_counts_and_receives_its_view() {
        let mut procs = vec![raw(1, 1, "init"), raw(2, 1, "worker")];
        procs[1].state = ProcessState::Running;
        let rig = rig(procs);
        let mut vitals = rig.handle.subscribe(Tier::Summary);
        let mut sub = rig.handle.watch_processes(ProcessQuery::default());
        let snap = sub.recv().await.expect("snapshot");
        assert_eq!((snap.total, snap.running, snap.rows.len()), (2, 1, 2));
        let mut counted = false;
        for _ in 0..4 {
            if let VitalsItem::Sample(s) = vitals.recv().await
                && s.process_count == Some(2)
                && s.running_count == Some(1)
            {
                counted = true;
                break;
            }
        }
        assert!(
            counted,
            "vitals carry the counts while processes are watched"
        );
        drop(sub);
    }

    #[tokio::test(start_paused = true)]
    async fn refused_memory_stays_none_and_list_rows_have_no_threads() {
        let rig = rig(vec![with(raw(1, 1, "a"), 0.0, None), raw(2, 1, "b")]);
        let mut sub = rig.handle.watch_processes(ProcessQuery {
            sort: ProcessSort::Pid,
            ..ProcessQuery::default()
        });
        let snap = sub.recv().await.unwrap();
        assert_eq!(snap.rows[0].memory_bytes, None);
        assert_eq!(snap.rows[1].memory_bytes, Some(0));
        assert!(snap.rows.iter().all(|r| r.threads.is_none()));
    }

    #[tokio::test(start_paused = true)]
    async fn the_pinned_row_reads_its_threads_from_the_status_file() {
        let rig = rig(vec![raw(1, 5, "a"), raw(2, 5, "b"), raw(3, 5, "c")]);
        rig.script()
            .statuses
            .insert(3, "Name:\tc\nThreads:\t17\n".into());
        let mut sub = rig.handle.watch_processes(ProcessQuery {
            sort: ProcessSort::Pid,
            limit: 1,
            pinned: Some((3, 5)),
            ..ProcessQuery::default()
        });
        let snap = sub.recv().await.unwrap();
        assert_eq!(pids(&snap), [1, 3]);
        assert_eq!(snap.rows[1].threads, Some(17));
        assert_eq!(snap.rows[0].threads, None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_identity_follows_a_recycled_pid_and_fills_a_late_command_line() {
        let mut early = raw(9, 100, "");
        early.cmd.clear();
        let rig = rig(vec![early]);
        let mut sub = rig.handle.watch_processes(ProcessQuery::default());
        let snap = sub.recv().await.unwrap();
        assert_eq!(snap.rows[0].command, "");

        rig.script().procs = vec![raw(9, 100, "filled")];
        let snap = sub.recv().await.unwrap();
        assert_eq!(snap.rows[0].name, "filled");
        assert_eq!(snap.rows[0].command, "/usr/bin/filled --secret-flag");

        rig.script().procs = vec![raw(9, 777, "recycled")];
        let snap = sub.recv().await.unwrap();
        assert_eq!(snap.rows[0].name, "recycled");
        assert_eq!(snap.rows[0].command, "/usr/bin/recycled --secret-flag");
        assert_eq!(snap.rows[0].start_unix_s, 777);
    }

    #[tokio::test(start_paused = true)]
    async fn two_subscriptions_each_get_their_own_view_from_one_refresh() {
        let rig = rig(vec![
            with(raw(1, 1, "alpha"), 10.0, Some(1)),
            with(raw(2, 1, "beta"), 30.0, Some(2)),
        ]);
        let mut top = rig.handle.watch_processes(ProcessQuery {
            limit: 1,
            ..ProcessQuery::default()
        });
        let mut named = rig.handle.watch_processes(ProcessQuery {
            filter: "alp".into(),
            ..ProcessQuery::default()
        });
        let a = top.recv().await.unwrap();
        let b = named.recv().await.unwrap();
        assert_eq!(pids(&a), [2]);
        assert_eq!(pids(&b), [1]);
        assert_eq!(a.revision, b.revision, "one refresh");
        assert_eq!(
            rig.refreshes(),
            2,
            "the priming refresh, then the published one"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_interval_is_clamped_and_follows_the_fastest_watcher() {
        let rig = rig(vec![raw(1, 1, "a")]);
        let mut slow = rig.handle.watch_processes(ProcessQuery {
            interval: Duration::from_secs(10),
            ..ProcessQuery::default()
        });
        slow.recv().await.unwrap();
        let t0 = tokio::time::Instant::now();
        slow.recv().await.unwrap();
        // Idle host cadence is 15 s, so the 10 s request is what is observed here.
        assert_eq!(t0.elapsed(), Duration::from_secs(10));

        let mut fast = rig.handle.watch_processes(ProcessQuery {
            interval: Duration::from_millis(100),
            ..ProcessQuery::default()
        });
        fast.recv().await.unwrap();
        let t1 = tokio::time::Instant::now();
        fast.recv().await.unwrap();
        assert_eq!(t1.elapsed(), MIN_PROCESS_INTERVAL, "clamped to the floor");
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_last_subscription_stops_refreshes_and_clears_the_cache() {
        let rig = rig(vec![raw(4, 10, "first")]);
        let mut a = rig.handle.watch_processes(ProcessQuery::default());
        let mut b = rig.handle.watch_processes(ProcessQuery::default());
        a.recv().await.unwrap();
        b.recv().await.unwrap();
        drop(a);
        b.recv().await.unwrap();
        assert!(
            rig.refreshes() >= 2,
            "still refreshing for the remaining one"
        );

        drop(b);
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(rig.script().released, 1, "the source dropped its table");
        let settled = rig.refreshes();
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(rig.refreshes(), settled, "no refresh without a watcher");

        // Same (pid, start, name) with a new command line: a cache that survived would show
        // the old one.
        let mut changed = raw(4, 10, "first");
        changed.cmd = vec!["second".into()];
        rig.script().procs = vec![changed];
        let mut again = rig.handle.watch_processes(ProcessQuery::default());
        assert_eq!(again.recv().await.unwrap().rows[0].command, "second");
    }

    #[tokio::test(start_paused = true)]
    async fn disabling_ends_the_feed_and_a_disabled_monitor_yields_nothing() {
        let rig = rig(vec![raw(1, 1, "a")]);
        let mut sub = rig.handle.watch_processes(ProcessQuery::default());
        sub.recv().await.unwrap();
        rig.handle.set_enabled(false);
        assert!(sub.recv().await.is_none());
        assert!(sub.recv().await.is_none(), "terminal");
        let mut off = rig.handle.watch_processes(ProcessQuery::default());
        assert!(off.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_priming_refresh_is_never_published() {
        let rig = rig(vec![raw(1, 1, "a"), raw(2, 1, "b")]);
        // The first refresh of a table reads 0 % (sysinfo has nothing to diff against).
        rig.script().cpu_by_refresh = Some(|n| if n == 1 { 0.0 } else { 25.0 });
        let mut sub = rig.handle.watch_processes(ProcessQuery::default());
        let first = sub.recv().await.unwrap();
        assert_eq!(rig.refreshes(), 2, "published from the second refresh");
        assert!(
            first.rows.iter().all(|r| r.cpu_percent == 25.0),
            "{first:?}"
        );
        let primed_at = tokio::time::Instant::now();
        sub.recv().await.unwrap();
        assert_eq!(
            primed_at.elapsed(),
            MIN_PROCESS_INTERVAL,
            "then the normal cadence"
        );

        // Dropping the table and watching again primes again.
        drop(sub);
        tokio::time::sleep(Duration::from_secs(30)).await;
        let before = rig.refreshes();
        rig.script().cpu_by_refresh = None;
        rig.script().procs = vec![with(raw(1, 1, "a"), 40.0, Some(0))];
        let mut again = rig.handle.watch_processes(ProcessQuery::default());
        let snap = again.recv().await.unwrap();
        assert_eq!(rig.refreshes(), before + 2);
        assert_eq!(snap.rows[0].cpu_percent, 40.0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_exec_after_the_first_sighting_shows_the_new_name_and_command() {
        let rig = rig(vec![raw(7, 50, "bash")]);
        let mut sub = rig.handle.watch_processes(ProcessQuery::default());
        assert_eq!(sub.recv().await.unwrap().rows[0].name, "bash");
        let mut execd = raw(7, 50, "cargo");
        execd.cmd = vec!["cargo".into(), "build".into()];
        rig.script().procs = vec![execd];
        let snap = sub.recv().await.unwrap();
        assert_eq!(snap.rows[0].name, "cargo");
        assert_eq!(snap.rows[0].command, "cargo build");
        assert_eq!(snap.rows[0].start_unix_s, 50);
    }

    #[tokio::test(start_paused = true)]
    async fn another_uids_row_has_no_command_line_through_the_sampler() {
        let mut theirs = raw(3, 1, "sshd");
        theirs.uid = Some(0);
        theirs.user = "root".into();
        let rig = rig(vec![raw(2, 1, "mine"), theirs]);
        let mut sub = rig.handle.watch_processes(ProcessQuery {
            sort: ProcessSort::Pid,
            ..ProcessQuery::default()
        });
        let snap = sub.recv().await.unwrap();
        assert_eq!(snap.rows[0].command, "/usr/bin/mine --secret-flag");
        assert_eq!(snap.rows[1].command, "");
        assert_eq!(
            (snap.rows[1].name.as_str(), snap.rows[1].user.as_str()),
            ("sshd", "root")
        );
    }
}
