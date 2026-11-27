//! Event kernel: tmux control-mode notifications fed into the event bus.
//!
//! A control client receives pane output only for windows of its own attached
//! session, so the kernel runs one watcher per tmux session. The kernel is a
//! tokio task: at start and every [`SCAN_INTERVAL`] it lists the server's
//! sessions and starts a watcher thread for each unwatched one, up to
//! [`watch_max`] (`MUXRD_TMUX_WATCH_MAX`, default [`DEFAULT_WATCH_MAX`]; 0
//! disables the watcher). Sessions beyond the cap are logged once.
//!
//! Sessions are keyed by tmux's session id plus creation time, not by name, so
//! a renamed session keeps its watcher: on `%session-renamed` (which tmux
//! sends to every control client, for any session) the watcher reads its
//! session's current name from the listing, re-keys its agent rows under it
//! and carries on, and the kernel never sees a "new" session.
//!
//! # A watcher
//!
//! Each watcher thread owns one [`ControlClient`] (`ignore-size`,
//! `pause-after`), seeds its panes from one pane listing and subscribes to each
//! pane's title and foreground command (`refresh-client -B`). It then:
//!
//! - scans every pane's output for OSC 7501 status reports and applies them;
//! - resumes a paused pane at once (output tmux dropped while it was paused is
//!   lost by design; pauses are counted);
//! - re-lists panes on window and layout changes, dropping the rows of
//!   panes that vanished;
//! - infers a status ([`status::heuristic`]) for panes that never reported,
//!   on every subscription change and every [`TICK`];
//! - respawns a dead client with a 1, 2, 4 … 30 s backoff while the session
//!   exists, and once it is gone drops its rows and exits.
//!
//! Every visible change rewrites the pane's [`AgentRow`]; an
//! [`AgentStatusChanged`] is published only when the status or kind differs
//! from the last one published for that pane. Events carry an empty
//! `workspace_id` (tmux rows have no space), the session name as
//! `workspace_name` (the push label) and always a kind and a tab, which the
//! `WatchAgents` fast path requires.
//!
//! # Row ownership
//!
//! A pane can sit in two sessions at once (a linked window) or move between
//! them (`join-pane`, `move-window`), so two watchers may track it. A watcher
//! only writes or drops a row under its own session name: the first to write
//! owns the pane, the other stays silent until that row is gone and then,
//! within one [`TICK`], stores it again from the pane's current status. A
//! closed pane is announced with [`PaneClosed`] by the watcher that published
//! it, unless another session holds its row by then, and also when the
//! backend already removed the row (it does so on pane close and session
//! kill); a pane that never had an agent row closes silently.
//!
//! # Bounds
//!
//! Per pane the watcher keeps a few short strings and the scanner's 4 KiB
//! report carry; at most [`MAX_PANES`] panes are tracked per session, and the
//! control client's own queue caps apply.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::agent_state::{AgentRow, AgentState};
use super::backend::TmuxBackend;
use super::commands::{PaneRow, SessionRow, Tmux, WindowRow};
use super::control::{ClientFlag, CommandReply, ControlClient, Notification};
use super::status::{self, Osc7501Scanner, PaneStatus, Report};
use crate::multiplexer::events::{AgentStatusChanged, EventBus, MuxEvent, PaneClosed};
use crate::multiplexer::types::AgentStatus;

/// Environment variable capping the number of watched sessions.
pub const WATCH_MAX_ENV: &str = "MUXRD_TMUX_WATCH_MAX";
/// Sessions watched when [`WATCH_MAX_ENV`] is unset.
pub const DEFAULT_WATCH_MAX: usize = 16;
/// Panes tracked per session; further panes are ignored.
pub const MAX_PANES: usize = 512;

/// How often the kernel lists sessions.
const SCAN_INTERVAL: Duration = Duration::from_secs(5);
/// How often a watcher re-runs the heuristic for panes without a report.
const TICK: Duration = Duration::from_secs(5);
/// Longest a watcher blocks on its client before checking for shutdown.
const POLL: Duration = Duration::from_secs(1);
/// Shortest gap between two pane listings triggered by notifications.
const RELIST_GAP: Duration = Duration::from_secs(1);
/// First respawn delay after a client died.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Respawn delay ceiling.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A client that stayed up this long resets the backoff.
const STABLE: Duration = Duration::from_secs(30);
/// Granularity of an interruptible backoff sleep.
const SLEEP_STEP: Duration = Duration::from_millis(200);
/// Timeout of the one command run right after attaching.
const ATTACH_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Name of the per-pane title/command subscription.
const SUBSCRIPTION: &str = "muxr-panes";
/// Control-client flags of every watcher; `pause-after` is in seconds.
const CLIENT_FLAGS: [ClientFlag; 2] = [ClientFlag::IgnoreSize, ClientFlag::PauseAfter(5)];

/// The session cap: [`WATCH_MAX_ENV`], default [`DEFAULT_WATCH_MAX`]; 0
/// disables the watcher.
pub fn watch_max() -> usize {
    parse_watch_max(std::env::var(WATCH_MAX_ENV).ok().as_deref())
}

fn parse_watch_max(value: Option<&str>) -> usize {
    match value.map(str::trim) {
        None | Some("") => DEFAULT_WATCH_MAX,
        Some(text) => text.parse().unwrap_or_else(|_| {
            log::warn!(
                "tmux watcher: {WATCH_MAX_ENV}={text:?} is not a number; \
                 using {DEFAULT_WATCH_MAX}"
            );
            DEFAULT_WATCH_MAX
        }),
    }
}

/// Start the tmux event kernel and return its task handle.
///
/// The kernel publishes onto `bus` and writes rows into `state` (the backend's
/// own agent state); it stops its watchers and ends when `shutdown` turns true.
pub fn spawn_tmux_event_kernel(
    backend: &TmuxBackend,
    state: Arc<AgentState>,
    bus: EventBus,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let tmux = Tmux::new(backend.socket().clone(), backend.version().clone());
    tokio::spawn(run_kernel(tmux, state, bus, shutdown, watch_max()))
}

// ─── The kernel ──────────────────────────────────────────────────────────────

/// A session's identity across renames: tmux's id plus its creation time, so a
/// restarted server reusing an id is not mistaken for the old session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SessionKey {
    id: u32,
    created: u64,
}

impl SessionKey {
    fn of(row: &SessionRow) -> Self {
        Self {
            id: row.id,
            created: row.created_unix,
        }
    }
}

async fn run_kernel(
    tmux: Tmux,
    state: Arc<AgentState>,
    bus: EventBus,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    max: usize,
) {
    if max == 0 {
        log::info!("tmux watcher: disabled ({WATCH_MAX_ENV}=0)");
        return;
    }
    log::info!("tmux watcher: kernel started (at most {max} sessions)");
    let stop = Arc::new(AtomicBool::new(false));
    let mut watchers: HashMap<SessionKey, thread::JoinHandle<()>> = HashMap::new();
    let mut capped: HashSet<SessionKey> = HashSet::new();
    let mut list_failing = false;
    loop {
        if *shutdown.borrow() {
            break;
        }
        watchers.retain(|_, handle| !handle.is_finished());
        let runner = tmux.clone();
        match tokio::task::spawn_blocking(move || runner.list_sessions()).await {
            Ok(Ok(sessions)) => {
                list_failing = false;
                let watched: HashSet<SessionKey> = watchers.keys().copied().collect();
                for row in plan_starts(&sessions, &watched, max, &mut capped) {
                    let key = SessionKey::of(&row);
                    match start_watcher(&tmux, &row, &state, &bus, &stop) {
                        Ok(handle) => {
                            watchers.insert(key, handle);
                        }
                        Err(e) => log::warn!(
                            "tmux watcher: cannot start a watcher for session {:?}: {e:#}",
                            row.name
                        ),
                    }
                }
            }
            Ok(Err(e)) => {
                if !std::mem::replace(&mut list_failing, true) {
                    log::warn!("tmux watcher: cannot list sessions: {e:#}");
                }
            }
            Err(e) => log::warn!("tmux watcher: session listing task failed: {e}"),
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            () = tokio::time::sleep(SCAN_INTERVAL) => {}
        }
    }
    stop.store(true, Ordering::Relaxed);
    let handles: Vec<thread::JoinHandle<()>> = watchers.into_values().collect();
    let _ = tokio::task::spawn_blocking(move || {
        for handle in handles {
            let _ = handle.join();
        }
    })
    .await;
    log::info!("tmux watcher: kernel stopped");
}

/// The sessions to start watchers for, in listing order: unwatched,
/// addressable and within `max`. A session left out by the cap is logged once
/// (`capped` remembers it while it is listed).
fn plan_starts(
    sessions: &[SessionRow],
    watched: &HashSet<SessionKey>,
    max: usize,
    capped: &mut HashSet<SessionKey>,
) -> Vec<SessionRow> {
    let live: HashSet<SessionKey> = sessions.iter().map(SessionKey::of).collect();
    capped.retain(|key| live.contains(key) && !watched.contains(key));
    let mut running = watched.len();
    let mut starts = Vec::new();
    for row in sessions {
        let key = SessionKey::of(row);
        if watched.contains(&key) || crate::ipc::validate_session_name(&row.name).is_err() {
            continue;
        }
        if running >= max {
            if capped.insert(key) {
                log::info!(
                    "tmux watcher: not watching session {:?}: {max} sessions are watched \
                     already ({WATCH_MAX_ENV})",
                    row.name
                );
            }
            continue;
        }
        capped.remove(&key);
        running += 1;
        starts.push(row.clone());
    }
    starts
}

fn start_watcher(
    tmux: &Tmux,
    row: &SessionRow,
    state: &Arc<AgentState>,
    bus: &EventBus,
    stop: &Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>> {
    let watcher = Watcher::new(
        SessionKey::of(row),
        row.name.clone(),
        tmux.clone(),
        Arc::clone(state),
        bus.clone(),
        tmux.version().at_least(3, 8),
    );
    let socket = tmux.socket().clone();
    let stop = Arc::clone(stop);
    log::info!("tmux watcher: watching session {:?}", row.name);
    let handle = thread::Builder::new()
        .name("tmux-watch".into())
        .spawn(move || {
            watcher.run(
                |session| ControlClient::spawn(&socket, session, &CLIENT_FLAGS),
                &stop,
            );
        })?;
    Ok(handle)
}

// ─── Seams ───────────────────────────────────────────────────────────────────

/// The control-client operations a watcher drives.
trait Control {
    fn next_notification(&mut self, timeout: Duration) -> Result<Option<Notification>>;
    fn resume_pane(&mut self, pane: u32) -> Result<()>;
    fn subscribe(&mut self, name: &str, target: &str, format: &str) -> Result<()>;
    fn command(&mut self, line: &str, timeout: Duration) -> Result<CommandReply>;
}

impl Control for ControlClient {
    fn next_notification(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        ControlClient::next_notification(self, timeout)
    }

    fn resume_pane(&mut self, pane: u32) -> Result<()> {
        ControlClient::resume_pane(self, pane)
    }

    fn subscribe(&mut self, name: &str, target: &str, format: &str) -> Result<()> {
        ControlClient::subscribe(self, name, target, format)
    }

    fn command(&mut self, line: &str, timeout: Duration) -> Result<CommandReply> {
        ControlClient::command(self, line, timeout)
    }
}

/// The short-lived tmux reads a watcher makes.
trait Source {
    fn sessions(&self) -> Result<Vec<SessionRow>>;
    fn panes(&self, session: &str) -> Result<Vec<PaneRow>>;
    fn windows(&self, session: &str) -> Result<Vec<WindowRow>>;
}

impl Source for Tmux {
    fn sessions(&self) -> Result<Vec<SessionRow>> {
        self.list_sessions()
    }

    fn panes(&self, session: &str) -> Result<Vec<PaneRow>> {
        self.list_panes(session)
    }

    fn windows(&self, session: &str) -> Result<Vec<WindowRow>> {
        self.list_windows(session)
    }
}

// ─── The watcher ─────────────────────────────────────────────────────────────

/// What the watcher knows about one pane.
#[derive(Debug, Default)]
struct PaneTrack {
    window: u32,
    tab_name: String,
    title: String,
    command: String,
    running: Option<bool>,
    scanner: Osc7501Scanner,
    status: PaneStatus,
    last_output: Option<Instant>,
    /// Last heuristic result, fed back as its `prev`.
    inferred: Option<AgentStatus>,
    /// Status and kind of the last published event.
    published: Option<(AgentStatus, String)>,
    /// This watcher wrote the pane's row, under its own session.
    has_row: bool,
    probe_logged: bool,
}

/// Why a watcher stopped driving its client.
#[derive(Debug, PartialEq, Eq)]
enum End {
    Shutdown,
    Died(String),
}

/// One session's watcher; generic over its reads so tests feed fixtures.
struct Watcher<S> {
    key: SessionKey,
    session: String,
    source: S,
    state: Arc<AgentState>,
    bus: EventBus,
    /// Subscribe to `pane_command_running` too (tmux 3.8+).
    with_running: bool,
    panes: BTreeMap<u32, PaneTrack>,
    /// A pane listing is owed (layout change, unknown pane).
    dirty: bool,
    last_relist: Option<Instant>,
    pauses: u64,
    /// Panes the last listing left out because [`MAX_PANES`] were tracked.
    over_cap: HashSet<u32>,
    cap_logged: bool,
}

impl<S: Source> Watcher<S> {
    fn new(
        key: SessionKey,
        session: String,
        source: S,
        state: Arc<AgentState>,
        bus: EventBus,
        with_running: bool,
    ) -> Self {
        Self {
            key,
            session,
            source,
            state,
            bus,
            with_running,
            panes: BTreeMap::new(),
            dirty: false,
            last_relist: None,
            pauses: 0,
            over_cap: HashSet::new(),
            cap_logged: false,
        }
    }

    /// Watch until shutdown or until the session is gone, respawning the
    /// client through `connect` after each death.
    fn run<C: Control>(mut self, mut connect: impl FnMut(&str) -> Result<C>, stop: &AtomicBool) {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            match self.resolve() {
                Ok(Some(name)) => self.rename(name),
                Ok(None) => return self.session_gone(),
                Err(e) => {
                    log::debug!(
                        "tmux watcher [{}]: cannot list sessions: {e:#}",
                        self.session
                    );
                    if !sleep(backoff, stop) {
                        return;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
            let started = Instant::now();
            let end = match connect(&self.session) {
                Err(e) => End::Died(format!("{e:#}")),
                Ok(mut client) => match self.attach(&mut client, Instant::now()) {
                    Err(e) => End::Died(format!("{e:#}")),
                    Ok(()) => self.drive(&mut client, stop),
                },
            };
            let End::Died(why) = end else {
                return;
            };
            if started.elapsed() >= STABLE {
                backoff = INITIAL_BACKOFF;
            }
            if let Ok(None) = self.resolve() {
                return self.session_gone();
            }
            log::warn!(
                "tmux watcher [{}]: control client ended ({why}); retrying in {backoff:?}",
                self.session
            );
            if !sleep(backoff, stop) {
                return;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// The session's current name, `None` once it is gone.
    fn resolve(&self) -> Result<Option<String>> {
        Ok(self
            .source
            .sessions()?
            .into_iter()
            .find(|row| SessionKey::of(row) == self.key)
            .map(|row| row.name))
    }

    /// Check the client sits on this session, subscribe and seed the panes.
    fn attach<C: Control>(&mut self, client: &mut C, now: Instant) -> Result<()> {
        let reply = client.command("display-message -p '#{session_id}'", ATTACH_COMMAND_TIMEOUT)?;
        let id = reply
            .lines
            .first()
            .and_then(|line| line.trim().strip_prefix('$'))
            .and_then(|id| id.parse::<u32>().ok());
        if !reply.ok || id != Some(self.key.id) {
            return Err(anyhow!(
                "control client is not on session ${} (reply {:?})",
                self.key.id,
                reply.lines
            ));
        }
        let format = if self.with_running {
            "#{pane_title}|#{pane_current_command}|#{pane_command_running}"
        } else {
            "#{pane_title}|#{pane_current_command}"
        };
        client.subscribe(SUBSCRIPTION, "%*", format)?;
        self.relist(now)
    }

    /// Pump notifications until shutdown or client death.
    fn drive<C: Control>(&mut self, client: &mut C, stop: &AtomicBool) -> End {
        let mut next_tick = Instant::now() + TICK;
        loop {
            if stop.load(Ordering::Relaxed) {
                return End::Shutdown;
            }
            let wait = POLL.min(next_tick.saturating_duration_since(Instant::now()));
            match client.next_notification(wait) {
                Err(e) => return End::Died(format!("{e:#}")),
                Ok(Some(notification)) => {
                    if let Err(why) = self.handle(client, notification, Instant::now()) {
                        return End::Died(why);
                    }
                }
                Ok(None) => {}
            }
            let now = Instant::now();
            // An owed listing first, so the tick sees panes that just closed.
            self.maybe_relist(now);
            if now >= next_tick {
                self.tick(now);
                next_tick = now + TICK;
            }
        }
    }

    /// Apply one notification. `Err` means the client left this session.
    fn handle<C: Control>(
        &mut self,
        client: &mut C,
        notification: Notification,
        now: Instant,
    ) -> std::result::Result<(), String> {
        match notification {
            Notification::Output { pane, bytes }
            | Notification::ExtendedOutput { pane, bytes, .. } => {
                self.on_output(pane, &bytes, now);
            }
            Notification::Pause { pane } => {
                self.pauses += 1;
                log::debug!(
                    "tmux watcher [{}]: pane %{pane} paused ({} pauses); resuming, its \
                     buffered output is dropped",
                    self.session,
                    self.pauses
                );
                if let Err(e) = client.resume_pane(pane) {
                    log::debug!(
                        "tmux watcher [{}]: resume of pane %{pane} failed: {e:#}",
                        self.session
                    );
                }
            }
            Notification::WindowAdd { .. }
            | Notification::WindowClose { .. }
            | Notification::LayoutChange { .. } => self.dirty = true,
            Notification::WindowRenamed { window, name } => {
                for track in self.panes.values_mut().filter(|t| t.window == window) {
                    track.tab_name.clone_from(&name);
                }
            }
            Notification::SubscriptionChanged {
                name,
                pane: Some(pane),
                value,
            } if name == SUBSCRIPTION => self.on_subscription(pane, &value, now),
            // tmux sends `%session-renamed` to every control client, for any
            // session, so the name it carries may be another session's: the
            // listing (by id) says whether this one was renamed.
            Notification::SessionRenamed { name } => match self.resolve() {
                Ok(Some(current)) => self.rename(current),
                Ok(None) => {}
                Err(e) => log::debug!(
                    "tmux watcher [{}]: cannot confirm rename to {name:?}: {e:#}",
                    self.session
                ),
            },
            Notification::SessionChanged { session, name } => {
                if session != self.key.id {
                    return Err(format!("client moved to session ${session} ({name:?})"));
                }
                self.rename(name);
            }
            _ => {}
        }
        Ok(())
    }

    /// A pane this watcher does not track: a listing is owed unless the last
    /// one left it out for the cap (re-listing would leave it out again).
    fn untracked(&mut self, pane: u32) {
        if !self.over_cap.contains(&pane) {
            self.dirty = true;
        }
    }

    fn on_output(&mut self, pane: u32, bytes: &[u8], now: Instant) {
        let Some(track) = self.panes.get_mut(&pane) else {
            self.untracked(pane);
            return;
        };
        track.last_output = Some(now);
        if track.has_row {
            self.state.touch_output(pane, now);
        }
        for report in track.scanner.feed(bytes) {
            match report {
                Report::Probe => {
                    if !std::mem::replace(&mut track.probe_logged, true) {
                        log::info!(
                            "tmux watcher [{}]: pane %{pane} probed for OSC 7501; this tmux \
                             does not answer, status is inferred",
                            self.session
                        );
                    }
                }
                Report::Status { .. } => {
                    if track.status.apply(&report, now).is_some() {
                        let row = reported_row(track, &self.session);
                        store(&self.state, &self.bus, &self.session, pane, track, row);
                    }
                }
            }
        }
    }

    fn on_subscription(&mut self, pane: u32, value: &str, now: Instant) {
        let Some(track) = self.panes.get_mut(&pane) else {
            self.untracked(pane);
            return;
        };
        if self.with_running {
            let mut parts = value.rsplitn(3, '|');
            let running = parts.next().unwrap_or_default();
            let command = parts.next().unwrap_or_default();
            track.title = parts.next().unwrap_or_default().to_owned();
            track.command = command.to_owned();
            track.running = match running.trim() {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            };
        } else {
            let mut parts = value.rsplitn(2, '|');
            let command = parts.next().unwrap_or_default();
            track.title = parts.next().unwrap_or_default().to_owned();
            track.command = command.to_owned();
        }
        self.evaluate(pane, now);
    }

    /// Re-store every pane whose row is gone, then re-run the heuristic for
    /// every pane without a report.
    fn tick(&mut self, now: Instant) {
        let panes: Vec<u32> = self.panes.keys().copied().collect();
        for pane in panes {
            // With a listing owed, a missing row may belong to a pane that
            // just closed; the next tick takes over once it is listed.
            if !self.dirty {
                self.adopt(pane);
            }
            self.evaluate(pane, now);
        }
    }

    /// When no session holds the row of `pane` (its owner dropped it, the
    /// backend removed it, or this watcher lost a race for it), forget that
    /// this watcher stored or published it and store it again from the pane's
    /// current status: reported here, inferred by the following
    /// [`Self::evaluate`].
    fn adopt(&mut self, pane: u32) {
        let Some(track) = self.panes.get_mut(&pane) else {
            return;
        };
        if self.state.mark(pane).is_some() {
            return;
        }
        track.has_row = false;
        track.published = None;
        if track.status.has_report() {
            let row = reported_row(track, &self.session);
            store(&self.state, &self.bus, &self.session, pane, track, row);
        }
    }

    /// Infer the status of `pane` when it never sent a report.
    fn evaluate(&mut self, pane: u32, now: Instant) {
        let Some(track) = self.panes.get_mut(&pane) else {
            return;
        };
        if track.status.has_report() {
            return;
        }
        let age = track
            .last_output
            .map(|at| now.saturating_duration_since(at));
        let next = status::heuristic(
            &track.command,
            &track.title,
            age,
            track.running,
            track.inferred,
        );
        track.inferred = next;
        let Some(status) = next else {
            // Not (or no longer) an agent: its row goes, like a closed pane.
            let stored = std::mem::replace(&mut track.has_row, false);
            let published = track.published.take().is_some();
            if stored || published {
                drop_row(&self.state, &self.bus, &self.session, pane, published);
            }
            return;
        };
        let kind = command_kind(&track.command);
        let unchanged = track.has_row
            && track
                .published
                .as_ref()
                .is_some_and(|(s, k)| *s == status && *k == kind);
        if unchanged {
            return;
        }
        let row = AgentRow {
            status,
            kind,
            display_kind: Some(status::inferred_display_kind(&track.command)),
            title: None,
            summary: None,
            tab_id: u64::from(track.window),
            tab_name: track.tab_name.clone(),
            change_seq: 0,
            completion_seq: None,
            last_output: track.last_output,
            session: self.session.clone(),
        };
        store(&self.state, &self.bus, &self.session, pane, track, row);
    }

    /// Re-list panes when one is owed and the last listing is old enough.
    fn maybe_relist(&mut self, now: Instant) {
        if !self.dirty
            || self
                .last_relist
                .is_some_and(|at| now.saturating_duration_since(at) < RELIST_GAP)
        {
            return;
        }
        if let Err(e) = self.relist(now) {
            log::debug!(
                "tmux watcher [{}]: pane listing failed: {e:#}",
                self.session
            );
        }
    }

    /// Read the session's panes: close the ones that vanished, then refresh
    /// known ones and seed new ones up to [`MAX_PANES`].
    fn relist(&mut self, now: Instant) -> Result<()> {
        self.last_relist = Some(now);
        let panes = self.source.panes(&self.session)?;
        let windows = self.source.windows(&self.session)?;
        self.dirty = false;
        // Prune first, so the cap counts only panes that still exist.
        let listed: HashSet<u32> = panes.iter().map(|p| p.id).collect();
        let gone: Vec<u32> = self
            .panes
            .keys()
            .copied()
            .filter(|pane| !listed.contains(pane))
            .collect();
        for pane in gone {
            self.close_pane(pane);
        }
        self.over_cap.clear();
        let mut fresh = Vec::new();
        for row in panes {
            if !self.panes.contains_key(&row.id) {
                if self.panes.len() >= MAX_PANES {
                    self.over_cap.insert(row.id);
                    if !std::mem::replace(&mut self.cap_logged, true) {
                        log::info!(
                            "tmux watcher [{}]: tracking at most {MAX_PANES} panes; further \
                             panes are ignored",
                            self.session
                        );
                    }
                    continue;
                }
                fresh.push(row.id);
            }
            let tab_name = windows
                .iter()
                .find(|w| w.id == row.window_id)
                .map(|w| w.name.clone())
                .unwrap_or_default();
            let track = self.panes.entry(row.id).or_default();
            track.window = row.window_id;
            track.tab_name = tab_name;
            track.title = row.title;
            track.command = row.current_command;
            track.running = row.command_running;
        }
        for pane in fresh {
            self.evaluate(pane, now);
        }
        Ok(())
    }

    /// Forget a pane that left this session (see [`drop_row`]): a row another
    /// session's watcher holds (a moved or linked pane) is left alone.
    fn close_pane(&mut self, pane: u32) {
        if let Some(track) = self.panes.remove(&pane)
            && (track.has_row || track.published.is_some())
        {
            drop_row(
                &self.state,
                &self.bus,
                &self.session,
                pane,
                track.published.is_some(),
            );
        }
    }

    /// Follow a rename: rows of tracked panes move to the new name.
    fn rename(&mut self, name: String) {
        if name == self.session {
            return;
        }
        let old = std::mem::replace(&mut self.session, name);
        log::info!(
            "tmux watcher: session {old:?} renamed to {:?}",
            self.session
        );
        for (pane, mut row) in self.state.rows_for_session(&old) {
            if self.panes.get(&pane).is_some_and(|t| t.has_row) {
                row.session.clone_from(&self.session);
                self.state.set_pane(pane, row);
            }
        }
    }

    /// The session ended: drop every tracked pane's row and announce it.
    fn session_gone(&mut self) {
        log::info!(
            "tmux watcher: session {:?} is gone; watcher stopped",
            self.session
        );
        let panes: Vec<u32> = self.panes.keys().copied().collect();
        for pane in panes {
            self.close_pane(pane);
        }
    }
}

/// The row a pane's applied reports describe.
fn reported_row(track: &PaneTrack, session: &str) -> AgentRow {
    AgentRow {
        status: track.status.status(),
        kind: track
            .status
            .kind()
            .map_or_else(|| command_kind(&track.command), str::to_owned),
        display_kind: track.status.display_kind().map(str::to_owned),
        title: track.status.title().map(str::to_owned),
        summary: track.status.summary().map(str::to_owned),
        tab_id: u64::from(track.window),
        tab_name: track.tab_name.clone(),
        change_seq: 0,
        completion_seq: None,
        last_output: track.last_output,
        session: session.to_owned(),
    }
}

/// Who holds the row of a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    Nobody,
    /// A row under the asking watcher's session.
    This,
    /// A row under another session: that session's watcher owns the pane.
    Other,
}

fn holder(state: &AgentState, session: &str, pane: u32) -> Holder {
    if state.mark(pane).is_none() {
        Holder::Nobody
    } else if state
        .rows_for_session(session)
        .iter()
        .any(|(id, _)| *id == pane)
    {
        Holder::This
    } else {
        Holder::Other
    }
}

/// Let go of `pane`: nothing happens while another session holds its row.
/// Otherwise the row is removed when it is under `session`, and
/// [`PaneClosed`] is published when this watcher had published the pane, even
/// when the row is already gone: the backend removes rows itself when it
/// closes a pane or kills a session, and consumers holding the agent still
/// need to hear that it went.
fn drop_row(state: &AgentState, bus: &EventBus, session: &str, pane: u32, published: bool) {
    match holder(state, session, pane) {
        Holder::Other => return,
        Holder::This => {
            state.remove_pane(pane);
        }
        Holder::Nobody => {}
    }
    if published {
        publish(bus, MuxEvent::PaneClosed(PaneClosed { pane }));
    }
}

/// Write `row` and publish it when its status or kind changed since the last
/// event for the pane.
///
/// A pane is owned by the first watcher that writes its row: a pane another
/// session's watcher already holds (a linked window, a pane moved in before
/// that watcher forgot it) is left to it, so one pane never has two writers or
/// duplicate events. Once that row is gone the next change or tick here takes
/// over ([`Watcher::adopt`]).
fn store(
    state: &AgentState,
    bus: &EventBus,
    session: &str,
    pane: u32,
    track: &mut PaneTrack,
    row: AgentRow,
) {
    if holder(state, session, pane) == Holder::Other {
        track.has_row = false;
        track.published = None;
        return;
    }
    let status = row.status;
    let kind = row.kind.clone();
    let title = row
        .title
        .clone()
        .or_else(|| (!track.title.is_empty()).then(|| track.title.clone()));
    let tab = row.tab_id;
    state.set_pane(pane, row);
    track.has_row = true;
    if track
        .published
        .as_ref()
        .is_some_and(|(s, k)| *s == status && *k == kind)
    {
        return;
    }
    log::debug!("tmux watcher [{session}]: pane %{pane} is {status:?} ({kind})");
    track.published = Some((status, kind.clone()));
    publish(
        bus,
        MuxEvent::AgentStatusChanged(AgentStatusChanged {
            pane,
            workspace_id: String::new(),
            // The session name labels pushes; `WatchAgents` matches on the
            // (empty) `workspace_id` only.
            workspace_name: Some(session.to_owned()),
            status,
            title,
            kind: Some(kind),
            name: None,
            tab: Some(tab),
            synthetic: false,
        }),
    );
}

fn publish(bus: &EventBus, event: MuxEvent) {
    // No receiver is fine: the bus is lossy and consumers re-read.
    let _ = bus.send(event);
}

/// The agent kind of a pane without an `app`: its command's base name.
fn command_kind(command: &str) -> String {
    match command.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => "agent".to_owned(),
    }
}

/// Sleep `total`, waking early (and answering false) on shutdown.
fn sleep(total: Duration, stop: &AtomicBool) -> bool {
    let deadline = Instant::now() + total;
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(SLEEP_STEP));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use tokio::sync::broadcast;

    use super::*;
    use crate::multiplexer::events::EVENT_BUS_CAPACITY;

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// Sessions, panes and windows the fake source answers; shared so a test
    /// can change them while the watcher holds the source.
    #[derive(Default)]
    struct World {
        sessions: Vec<SessionRow>,
        panes: Vec<PaneRow>,
        windows: Vec<WindowRow>,
        pane_reads: usize,
    }

    #[derive(Clone, Default)]
    struct FakeSource(Arc<Mutex<World>>);

    impl FakeSource {
        fn world(&self) -> std::sync::MutexGuard<'_, World> {
            self.0.lock().unwrap()
        }
    }

    impl Source for FakeSource {
        fn sessions(&self) -> Result<Vec<SessionRow>> {
            Ok(self.world().sessions.clone())
        }

        fn panes(&self, _session: &str) -> Result<Vec<PaneRow>> {
            let mut world = self.world();
            world.pane_reads += 1;
            Ok(world.panes.clone())
        }

        fn windows(&self, _session: &str) -> Result<Vec<WindowRow>> {
            Ok(self.world().windows.clone())
        }
    }

    /// A control client fed from a fixture queue; dies once it runs dry.
    #[derive(Default)]
    struct FakeControl {
        queue: VecDeque<Notification>,
        session_id: u32,
        resumed: Vec<u32>,
        subscriptions: Vec<(String, String, String)>,
        commands: Vec<String>,
    }

    impl Control for FakeControl {
        fn next_notification(&mut self, _timeout: Duration) -> Result<Option<Notification>> {
            self.queue
                .pop_front()
                .map(Some)
                .ok_or_else(|| anyhow!("fixture exhausted"))
        }

        fn resume_pane(&mut self, pane: u32) -> Result<()> {
            self.resumed.push(pane);
            Ok(())
        }

        fn subscribe(&mut self, name: &str, target: &str, format: &str) -> Result<()> {
            self.subscriptions
                .push((name.into(), target.into(), format.into()));
            Ok(())
        }

        fn command(&mut self, line: &str, _timeout: Duration) -> Result<CommandReply> {
            self.commands.push(line.to_owned());
            Ok(CommandReply {
                ok: true,
                lines: vec![format!("${}", self.session_id)],
            })
        }
    }

    const KEY: SessionKey = SessionKey {
        id: 4,
        created: 1000,
    };

    fn session_row(name: &str) -> SessionRow {
        SessionRow {
            name: name.into(),
            id: KEY.id,
            created_unix: KEY.created,
            attached: 0,
            windows: 1,
        }
    }

    fn pane_row(id: u32, window: u32, command: &str) -> PaneRow {
        PaneRow {
            id,
            window_id: window,
            index: 0,
            active: true,
            title: format!("t{id}"),
            current_command: command.into(),
            current_path: "/".into(),
            dead: false,
            left: 0,
            top: 0,
            width: 80,
            height: 24,
            floating: false,
            z: 0,
            command_running: None,
            pid: 1,
        }
    }

    fn window_row(id: u32, name: &str) -> WindowRow {
        WindowRow {
            id,
            index: 0,
            name: name.into(),
            active: true,
            bell: false,
            zoomed: false,
            width: 80,
            height: 24,
            panes: 1,
        }
    }

    struct Rig {
        source: FakeSource,
        state: Arc<AgentState>,
        rx: broadcast::Receiver<MuxEvent>,
        watcher: Watcher<FakeSource>,
        client: FakeControl,
    }

    /// A watcher on session `s` (window @1 "main") already attached, with
    /// panes `%id` running `command`.
    fn rig(panes: &[(u32, &str)]) -> Rig {
        let source = FakeSource::default();
        {
            let mut world = source.world();
            world.sessions = vec![session_row("s")];
            world.panes = panes.iter().map(|(id, c)| pane_row(*id, 1, c)).collect();
            world.windows = vec![window_row(1, "main")];
        }
        let state = Arc::new(AgentState::new());
        let (bus, rx) = broadcast::channel(EVENT_BUS_CAPACITY);
        let mut watcher = Watcher::new(
            KEY,
            "s".into(),
            source.clone(),
            Arc::clone(&state),
            bus,
            false,
        );
        let mut client = FakeControl {
            session_id: KEY.id,
            ..FakeControl::default()
        };
        watcher.attach(&mut client, Instant::now()).unwrap();
        Rig {
            source,
            state,
            rx,
            watcher,
            client,
        }
    }

    impl Rig {
        fn feed(&mut self, notification: Notification) {
            self.watcher
                .handle(&mut self.client, notification, Instant::now())
                .unwrap();
        }

        fn events(&mut self) -> Vec<MuxEvent> {
            let mut out = Vec::new();
            while let Ok(ev) = self.rx.try_recv() {
                out.push(ev);
            }
            out
        }
    }

    fn report(pane: u32, body: &str) -> Notification {
        Notification::ExtendedOutput {
            pane,
            age_ms: 0,
            bytes: format!("\x1b]7501;{body}\x1b\\").into_bytes(),
        }
    }

    fn status_events(events: &[MuxEvent]) -> Vec<&AgentStatusChanged> {
        events
            .iter()
            .filter_map(|e| match e {
                MuxEvent::AgentStatusChanged(ev) => Some(ev),
                _ => None,
            })
            .collect()
    }

    // ── Configuration ───────────────────────────────────────────────────────

    #[test]
    fn watch_max_defaults_and_parses() {
        assert_eq!(parse_watch_max(None), DEFAULT_WATCH_MAX);
        assert_eq!(parse_watch_max(Some("")), DEFAULT_WATCH_MAX);
        assert_eq!(parse_watch_max(Some("0")), 0);
        assert_eq!(parse_watch_max(Some(" 3 ")), 3);
        assert_eq!(parse_watch_max(Some("many")), DEFAULT_WATCH_MAX);
    }

    // ── Attach ──────────────────────────────────────────────────────────────

    #[test]
    fn attach_subscribes_to_every_pane_and_seeds_panes() {
        let r = rig(&[(3, "bash"), (5, "bash")]);
        assert_eq!(
            r.client.subscriptions,
            [(
                SUBSCRIPTION.to_owned(),
                "%*".to_owned(),
                "#{pane_title}|#{pane_current_command}".to_owned()
            )]
        );
        assert_eq!(r.watcher.panes.keys().copied().collect::<Vec<_>>(), [3, 5]);
        assert_eq!(r.watcher.panes[&3].tab_name, "main");
        assert!(r.state.rows_for_session("s").is_empty(), "no agent yet");
    }

    #[test]
    fn attach_on_3_8_also_subscribes_to_command_running() {
        let source = FakeSource::default();
        source.world().sessions = vec![session_row("s")];
        let (bus, _rx) = broadcast::channel(4);
        let mut w = Watcher::new(
            KEY,
            "s".into(),
            source,
            Arc::new(AgentState::new()),
            bus,
            true,
        );
        let mut client = FakeControl {
            session_id: KEY.id,
            ..FakeControl::default()
        };
        w.attach(&mut client, Instant::now()).unwrap();
        assert!(
            client.subscriptions[0]
                .2
                .ends_with("|#{pane_command_running}")
        );
    }

    #[test]
    fn attach_refuses_a_client_on_another_session() {
        let source = FakeSource::default();
        let (bus, _rx) = broadcast::channel(4);
        let mut w = Watcher::new(
            KEY,
            "s".into(),
            source,
            Arc::new(AgentState::new()),
            bus,
            false,
        );
        let mut client = FakeControl {
            session_id: 9,
            ..FakeControl::default()
        };
        assert!(w.attach(&mut client, Instant::now()).is_err());
        assert!(client.subscriptions.is_empty());
    }

    // ── Reports ─────────────────────────────────────────────────────────────

    #[test]
    fn blocked_report_publishes_one_status_event() {
        let mut r = rig(&[(3, "bash")]);
        r.feed(report(3, "state=blocked:kind=permission:app=claude"));
        let events = r.events();
        assert_eq!(events.len(), 1, "{events:?}");
        let ev = status_events(&events)[0];
        assert_eq!(ev.pane, 3);
        assert_eq!(ev.status, AgentStatus::Blocked);
        assert!(ev.workspace_id.is_empty());
        assert_eq!(ev.workspace_name.as_deref(), Some("s"), "the push label");
        assert_eq!(ev.kind.as_deref(), Some("claude"));
        assert_eq!(ev.tab, Some(1));
        assert_eq!(ev.name, None);
        assert!(!ev.synthetic);

        let rows = r.state.rows_for_session("s");
        assert_eq!(rows.len(), 1);
        let row = &rows[0].1;
        assert_eq!(row.status, AgentStatus::Blocked);
        assert_eq!(row.kind, "claude");
        assert_eq!(row.summary.as_deref(), Some("permission"));
        assert_eq!(row.tab_id, 1);
        assert_eq!(row.tab_name, "main");
        assert!(row.last_output.is_some());
    }

    #[test]
    fn plain_output_carries_reports_too() {
        let mut r = rig(&[(3, "bash")]);
        r.feed(Notification::Output {
            pane: 3,
            bytes: b"\x1b]7501;state=working\x07".to_vec(),
        });
        let events = r.events();
        let ev = status_events(&events)[0];
        assert_eq!(ev.status, AgentStatus::Working);
        assert_eq!(ev.kind.as_deref(), Some("bash"), "no app: the command");
    }

    #[test]
    fn report_split_across_chunks_is_found() {
        let mut r = rig(&[(3, "bash")]);
        let whole = "\x1b]7501;state=blocked:app=codex\x1b\\";
        let (a, b) = whole.split_at(9);
        for part in [a, b] {
            r.feed(Notification::ExtendedOutput {
                pane: 3,
                age_ms: 3,
                bytes: part.as_bytes().to_vec(),
            });
        }
        assert_eq!(status_events(&r.events()).len(), 1);
    }

    #[test]
    fn clear_after_blocked_goes_idle() {
        let mut r = rig(&[(3, "bash")]);
        r.feed(report(3, "state=blocked:app=claude"));
        r.events();
        r.feed(report(3, "state=clear"));
        let events = r.events();
        let ev = status_events(&events)[0];
        assert_eq!(ev.status, AgentStatus::Idle);
        assert_eq!(r.state.rows_for_session("s")[0].1.status, AgentStatus::Idle);
    }

    #[test]
    fn summary_only_change_rewrites_the_row_without_an_event() {
        let mut r = rig(&[(3, "bash")]);
        r.feed(report(3, "state=working:app=claude"));
        r.events();
        let before = r.state.rows_for_session("s")[0].1.change_seq;
        r.feed(report(3, "state=working:app=claude:progress=40"));
        assert!(r.events().is_empty(), "same status and kind: no event");
        let row = r.state.rows_for_session("s")[0].1.clone();
        assert!(row.change_seq > before);
        assert_eq!(row.summary.as_deref(), Some("working 40%"));
    }

    #[test]
    fn probe_publishes_nothing() {
        let mut r = rig(&[(3, "bash")]);
        r.feed(report(3, "?"));
        r.feed(report(3, "?"));
        assert!(r.events().is_empty());
        assert!(r.state.rows_for_session("s").is_empty());
        assert!(r.watcher.panes[&3].probe_logged);
    }

    #[test]
    fn output_of_an_unknown_pane_owes_a_listing() {
        let mut r = rig(&[(3, "bash")]);
        r.source.world().panes.push(pane_row(8, 1, "bash"));
        r.feed(report(8, "state=blocked"));
        assert!(r.events().is_empty());
        assert!(r.watcher.dirty);
        r.watcher.maybe_relist(Instant::now() + RELIST_GAP);
        assert!(r.watcher.panes.contains_key(&8));
    }

    // ── Flow control ────────────────────────────────────────────────────────

    #[test]
    fn pause_is_resumed_at_once() {
        let mut r = rig(&[(3, "bash")]);
        r.feed(Notification::Pause { pane: 3 });
        assert_eq!(r.client.resumed, [3]);
        assert_eq!(r.watcher.pauses, 1);
        assert!(r.events().is_empty());
    }

    // ── Pane lifecycle ──────────────────────────────────────────────────────

    #[test]
    fn vanished_pane_is_closed_after_a_layout_change() {
        let mut r = rig(&[(3, "bash"), (5, "bash")]);
        r.feed(report(5, "state=working:app=claude"));
        r.events();
        // Both vanish; only the one with an agent row is announced.
        r.source.world().panes.clear();
        r.feed(Notification::LayoutChange { window: 1 });
        r.watcher.maybe_relist(Instant::now() + RELIST_GAP);
        let events = r.events();
        assert_eq!(
            events,
            [MuxEvent::PaneClosed(PaneClosed { pane: 5 })],
            "{events:?}"
        );
        assert!(r.state.rows_for_session("s").is_empty());
        assert!(r.watcher.panes.is_empty());
    }

    #[test]
    fn relists_are_rate_limited() {
        let mut r = rig(&[(3, "bash")]);
        let reads = r.source.world().pane_reads;
        let now = Instant::now();
        r.feed(Notification::WindowAdd { window: 2 });
        r.watcher.maybe_relist(now);
        assert_eq!(r.source.world().pane_reads, reads, "too soon after attach");
        r.watcher.maybe_relist(now + RELIST_GAP);
        assert_eq!(r.source.world().pane_reads, reads + 1);
        r.watcher.maybe_relist(now + RELIST_GAP * 3);
        assert_eq!(r.source.world().pane_reads, reads + 1, "nothing owed");
    }

    #[test]
    fn panes_beyond_the_cap_are_ignored() {
        let source = FakeSource::default();
        source.world().panes = (0..MAX_PANES as u32 + 3)
            .map(|id| pane_row(id, 1, "bash"))
            .collect();
        let (bus, _rx) = broadcast::channel(4);
        let mut w = Watcher::new(
            KEY,
            "s".into(),
            source,
            Arc::new(AgentState::new()),
            bus,
            false,
        );
        w.relist(Instant::now()).unwrap();
        assert_eq!(w.panes.len(), MAX_PANES);
        assert!(w.cap_logged);

        // Output of a pane left out for the cap owes no listing: one would
        // leave it out again.
        let extra = MAX_PANES as u32 + 1;
        w.on_output(extra, b"\x1b]7501;state=blocked\x07", Instant::now());
        w.on_subscription(extra, "t|claude", Instant::now());
        assert!(!w.dirty, "an over-cap pane must not churn listings");
        // A pane never listed still does.
        w.on_output(9999, b"x", Instant::now());
        assert!(w.dirty);
    }

    #[test]
    fn the_cap_counts_panes_after_pruning() {
        let source = FakeSource::default();
        source.world().panes = (0..MAX_PANES as u32)
            .map(|id| pane_row(id, 1, "bash"))
            .collect();
        let (bus, _rx) = broadcast::channel(4);
        let mut w = Watcher::new(
            KEY,
            "s".into(),
            source.clone(),
            Arc::new(AgentState::new()),
            bus,
            false,
        );
        w.relist(Instant::now()).unwrap();
        assert_eq!(w.panes.len(), MAX_PANES);
        {
            // One pane closes and a new one opens in the same listing.
            let mut world = source.world();
            world.panes.remove(0);
            world.panes.push(pane_row(MAX_PANES as u32, 1, "bash"));
        }
        w.relist(Instant::now()).unwrap();
        assert_eq!(w.panes.len(), MAX_PANES);
        assert!(w.panes.contains_key(&(MAX_PANES as u32)));
        assert!(!w.panes.contains_key(&0));
        assert!(w.over_cap.is_empty());
    }

    // ── Inferred status ─────────────────────────────────────────────────────

    #[test]
    fn known_agent_with_fresh_output_is_inferred_working() {
        let mut r = rig(&[(3, "claude")]);
        let seeded = r.events();
        assert_eq!(
            status_events(&seeded)[0].status,
            AgentStatus::Idle,
            "a quiet agent is seeded idle"
        );
        r.feed(Notification::ExtendedOutput {
            pane: 3,
            age_ms: 0,
            bytes: b"thinking...".to_vec(),
        });
        r.watcher.tick(Instant::now());
        let events = r.events();
        let ev = status_events(&events)[0];
        assert_eq!(ev.status, AgentStatus::Working);
        assert_eq!(ev.kind.as_deref(), Some("claude"));
        assert_eq!(ev.tab, Some(1));
        assert!(ev.workspace_id.is_empty());
        let row = &r.state.rows_for_session("s")[0].1;
        assert_eq!(row.status, AgentStatus::Working);
        assert_eq!(row.display_kind.as_deref(), Some("claude (inferred)"));

        r.watcher.tick(Instant::now());
        assert!(
            r.events().is_empty(),
            "an unchanged inference is not republished"
        );
    }

    #[test]
    fn inferred_status_follows_the_subscription() {
        let mut r = rig(&[(3, "bash")]);
        assert!(r.events().is_empty());
        r.feed(Notification::SubscriptionChanged {
            name: SUBSCRIPTION.into(),
            pane: Some(3),
            value: "Do you want | to proceed?|codex".into(),
        });
        let events = r.events();
        let ev = status_events(&events)[0];
        assert_eq!(ev.status, AgentStatus::Blocked);
        assert_eq!(ev.kind.as_deref(), Some("codex"));
        assert_eq!(r.watcher.panes[&3].title, "Do you want | to proceed?");

        r.feed(Notification::SubscriptionChanged {
            name: SUBSCRIPTION.into(),
            pane: Some(3),
            value: "t3|bash".into(),
        });
        assert_eq!(
            r.events(),
            [MuxEvent::PaneClosed(PaneClosed { pane: 3 })],
            "the agent exited: its row goes"
        );
        assert!(r.state.rows_for_session("s").is_empty());
    }

    #[test]
    fn inferred_done_is_published_once() {
        let source = FakeSource::default();
        {
            let mut world = source.world();
            world.panes = vec![pane_row(3, 1, "claude")];
            world.windows = vec![window_row(1, "main")];
        }
        let (bus, mut rx) = broadcast::channel(16);
        let mut w = Watcher::new(
            KEY,
            "s".into(),
            source,
            Arc::new(AgentState::new()),
            bus,
            true,
        );
        let now = Instant::now();
        w.relist(now).unwrap();
        w.on_output(3, b"x", now);
        w.on_subscription(3, "t|claude|1", now);
        w.on_subscription(3, "t|claude|0", now);
        w.tick(now + Duration::from_secs(10));
        let statuses: Vec<AgentStatus> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                MuxEvent::AgentStatusChanged(ev) => Some(ev.status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            [AgentStatus::Idle, AgentStatus::Working, AgentStatus::Done]
        );
    }

    #[test]
    fn a_report_takes_over_from_the_inference() {
        let mut r = rig(&[(3, "claude")]);
        r.events();
        r.feed(report(3, "state=blocked:kind=question:app=claude"));
        r.watcher.tick(Instant::now() + Duration::from_secs(30));
        let events = r.events();
        assert_eq!(status_events(&events).len(), 1);
        let row = &r.state.rows_for_session("s")[0].1;
        assert_eq!(row.status, AgentStatus::Blocked);
        assert_eq!(row.display_kind.as_deref(), Some("claude"));
    }

    // ── Sessions ────────────────────────────────────────────────────────────

    #[test]
    fn rename_rekeys_rows_without_closing_panes() {
        let mut r = rig(&[(3, "bash"), (5, "bash")]);
        r.feed(report(3, "state=blocked:app=claude"));
        r.events();
        r.source.world().sessions = vec![session_row("t")];
        r.feed(Notification::SessionRenamed { name: "t".into() });
        assert!(r.events().is_empty(), "no PaneClosed burst");
        assert!(r.state.rows_for_session("s").is_empty());
        let rows = r.state.rows_for_session("t");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 3);
        assert_eq!(rows[0].1.status, AgentStatus::Blocked);
        assert_eq!(r.watcher.session, "t");

        r.feed(report(5, "state=working:app=codex"));
        assert_eq!(
            r.state.rows_for_session("t").len(),
            2,
            "new rows use the new name"
        );
    }

    #[test]
    fn another_sessions_rename_is_ignored() {
        // tmux tells every control client about every rename.
        let mut r = rig(&[(3, "bash")]);
        r.feed(report(3, "state=blocked:app=claude"));
        r.events();
        let mut other = session_row("other-renamed");
        other.id = KEY.id + 1;
        r.source.world().sessions.push(other);
        r.feed(Notification::SessionRenamed {
            name: "other-renamed".into(),
        });
        assert_eq!(r.watcher.session, "s");
        assert_eq!(r.state.rows_for_session("s").len(), 1);
        assert!(r.state.rows_for_session("other-renamed").is_empty());
    }

    #[test]
    fn session_changed_to_ours_renames_and_to_another_detaches() {
        let mut r = rig(&[(3, "bash")]);
        r.feed(Notification::SessionChanged {
            session: KEY.id,
            name: "u".into(),
        });
        assert_eq!(r.watcher.session, "u");
        let moved = r.watcher.handle(
            &mut r.client,
            Notification::SessionChanged {
                session: KEY.id + 1,
                name: "other".into(),
            },
            Instant::now(),
        );
        assert!(moved.is_err());
    }

    #[test]
    fn rename_while_detached_is_picked_up_on_respawn() {
        let source = FakeSource::default();
        {
            let mut world = source.world();
            world.sessions = vec![session_row("new")];
            world.panes = vec![pane_row(3, 1, "bash")];
        }
        let state = Arc::new(AgentState::new());
        let (bus, mut rx) = broadcast::channel(16);
        let w = Watcher::new(
            KEY,
            "old".into(),
            source.clone(),
            Arc::clone(&state),
            bus,
            false,
        );
        let stop = AtomicBool::new(false);
        let mut connected = Vec::new();
        let world = source.clone();
        w.run(
            |name| {
                connected.push(name.to_owned());
                // The session ends while this client is attached.
                world.world().sessions.clear();
                Ok(FakeControl {
                    session_id: KEY.id,
                    queue: VecDeque::from([report(3, "state=done:app=claude")]),
                    ..FakeControl::default()
                })
            },
            &stop,
        );
        assert_eq!(connected, ["new"]);
        let events: Vec<MuxEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            matches!(&events[0], MuxEvent::AgentStatusChanged(ev) if ev.status == AgentStatus::Done)
        );
        assert_eq!(events[1], MuxEvent::PaneClosed(PaneClosed { pane: 3 }));
        assert!(state.rows_for_session("new").is_empty());
    }

    #[test]
    fn gone_session_drops_its_agent_rows_and_exits() {
        let source = FakeSource::default();
        {
            let mut world = source.world();
            world.sessions = vec![session_row("s")];
            world.panes = vec![pane_row(3, 1, "bash"), pane_row(5, 1, "claude")];
        }
        let state = Arc::new(AgentState::new());
        let (bus, mut rx) = broadcast::channel(16);
        let w = Watcher::new(
            KEY,
            "s".into(),
            source.clone(),
            Arc::clone(&state),
            bus,
            false,
        );
        let stop = AtomicBool::new(false);
        let world = source.clone();
        w.run(
            |_| {
                world.world().sessions.clear();
                Ok(FakeControl {
                    session_id: KEY.id,
                    ..FakeControl::default()
                })
            },
            &stop,
        );
        let closed: Vec<u32> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                MuxEvent::PaneClosed(c) => Some(c.pane),
                _ => None,
            })
            .collect();
        assert_eq!(closed, [5], "only the pane with a published agent row");
        assert!(state.rows_for_session("s").is_empty());
    }

    // ── Panes in two sessions ───────────────────────────────────────────────

    /// A watcher on session `name` (id `id`) over `state` and `bus`, attached,
    /// whose listing holds `panes` (`(id, command)`) in window @1.
    fn watcher_on(
        name: &str,
        id: u32,
        panes: &[(u32, &str)],
        state: &Arc<AgentState>,
        bus: &EventBus,
    ) -> (FakeSource, Watcher<FakeSource>) {
        let key = SessionKey { id, created: 1 };
        let source = FakeSource::default();
        {
            let mut world = source.world();
            world.sessions = vec![SessionRow {
                name: name.into(),
                id,
                created_unix: 1,
                attached: 0,
                windows: 1,
            }];
            world.panes = panes.iter().map(|(p, c)| pane_row(*p, 1, c)).collect();
            world.windows = vec![window_row(1, "main")];
        }
        let mut w = Watcher::new(
            key,
            name.into(),
            source.clone(),
            Arc::clone(state),
            bus.clone(),
            false,
        );
        let mut client = FakeControl {
            session_id: id,
            ..FakeControl::default()
        };
        w.attach(&mut client, Instant::now()).unwrap();
        (source, w)
    }

    fn drain(rx: &mut broadcast::Receiver<MuxEvent>) -> Vec<MuxEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn a_linked_pane_has_one_owner_and_one_event() {
        let state = Arc::new(AgentState::new());
        let (bus, mut rx) = broadcast::channel(16);
        let (_sa, mut a) = watcher_on("a", 1, &[(3, "bash")], &state, &bus);
        let (_sb, mut b) = watcher_on("b", 2, &[(3, "bash")], &state, &bus);
        // Both clients receive the linked pane's output.
        let now = Instant::now();
        let bytes = b"\x1b]7501;state=blocked:app=claude\x1b\\";
        a.on_output(3, bytes, now);
        b.on_output(3, bytes, now);
        let events = drain(&mut rx);
        assert_eq!(status_events(&events).len(), 1, "{events:?}");
        assert_eq!(state.rows_for_session("a").len(), 1);
        assert!(state.rows_for_session("b").is_empty());
        assert!(a.panes[&3].has_row && !b.panes[&3].has_row);

        // A later change in the non-owner stays silent too.
        b.on_output(3, b"\x1b]7501;state=working:app=claude\x1b\\", now);
        assert!(drain(&mut rx).is_empty());
        assert_eq!(
            state.rows_for_session("a")[0].1.status,
            AgentStatus::Blocked
        );
    }

    #[test]
    fn a_pane_leaving_another_sessions_listing_keeps_its_owners_row() {
        let state = Arc::new(AgentState::new());
        let (bus, mut rx) = broadcast::channel(16);
        let (sa, mut a) = watcher_on("a", 1, &[(3, "bash")], &state, &bus);
        let (_sb, mut b) = watcher_on("b", 2, &[(3, "bash")], &state, &bus);
        b.on_output(
            3,
            b"\x1b]7501;state=blocked:app=claude\x1b\\",
            Instant::now(),
        );
        drain(&mut rx);

        // The window is unlinked from `a`: `a` forgets the pane locally.
        sa.world().panes.clear();
        a.relist(Instant::now()).unwrap();
        assert!(drain(&mut rx).is_empty(), "no PaneClosed for b's pane");
        assert!(!a.panes.contains_key(&3));
        let rows = state.rows_for_session("b");
        assert_eq!(rows.len(), 1, "b's row survives");

        // b keeps writing it.
        b.on_output(3, b"\x1b]7501;state=clear\x1b\\", Instant::now());
        let events = drain(&mut rx);
        assert_eq!(status_events(&events)[0].status, AgentStatus::Idle);
        assert_eq!(state.rows_for_session("b")[0].1.status, AgentStatus::Idle);
    }

    #[test]
    fn a_moved_pane_is_taken_over_once_its_old_row_is_gone() {
        let state = Arc::new(AgentState::new());
        let (bus, mut rx) = broadcast::channel(16);
        let (sa, mut a) = watcher_on("a", 1, &[(3, "claude")], &state, &bus);
        assert_eq!(state.rows_for_session("a").len(), 1, "inferred idle");
        drain(&mut rx);

        // join-pane moves %3 into `b`; b sees it before a re-lists.
        let (_sb, mut b) = watcher_on("b", 2, &[(3, "claude")], &state, &bus);
        assert!(drain(&mut rx).is_empty(), "a still owns it");
        assert!(state.rows_for_session("b").is_empty());

        sa.world().panes.clear();
        a.relist(Instant::now()).unwrap();
        assert_eq!(
            drain(&mut rx),
            [MuxEvent::PaneClosed(PaneClosed { pane: 3 })]
        );
        b.tick(Instant::now());
        let events = drain(&mut rx);
        let ev = status_events(&events)[0];
        assert_eq!(ev.workspace_name.as_deref(), Some("b"));
        assert_eq!(state.rows_for_session("b").len(), 1);
    }

    #[test]
    fn a_reporting_pane_is_taken_over_within_one_tick() {
        let state = Arc::new(AgentState::new());
        let (bus, mut rx) = broadcast::channel(16);
        let (sa, mut a) = watcher_on("a", 1, &[(3, "bash")], &state, &bus);
        let (_sb, mut b) = watcher_on("b", 2, &[(3, "bash")], &state, &bus);
        let bytes = b"\x1b]7501;state=blocked:kind=question:app=claude\x1b\\";
        a.on_output(3, bytes, Instant::now());
        b.on_output(3, bytes, Instant::now());
        assert_eq!(status_events(&drain(&mut rx)).len(), 1, "a owns it");

        sa.world().panes.clear();
        a.relist(Instant::now()).unwrap();
        drain(&mut rx);

        // No new report arrives: the tick alone re-stores b's reported status.
        b.tick(Instant::now());
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "{events:?}");
        let ev = status_events(&events)[0];
        assert_eq!(ev.status, AgentStatus::Blocked);
        assert_eq!(ev.workspace_name.as_deref(), Some("b"));
        let rows = state.rows_for_session("b");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.summary.as_deref(), Some("question"));
    }

    #[test]
    fn a_race_loser_takes_over_once_the_winner_lets_go() {
        let state = Arc::new(AgentState::new());
        let (bus, mut rx) = broadcast::channel(16);
        let (sa, mut a) = watcher_on("a", 1, &[(3, "claude")], &state, &bus);
        let (_sb, mut b) = watcher_on("b", 2, &[(3, "claude")], &state, &bus);
        // b lost a race: it believes it stored and published the row a holds.
        let track = b.panes.get_mut(&3).unwrap();
        track.has_row = true;
        track.published = Some((AgentStatus::Idle, "claude".into()));
        drain(&mut rx);
        b.tick(Instant::now());
        assert!(drain(&mut rx).is_empty(), "a still holds it");
        assert!(state.rows_for_session("b").is_empty());

        sa.world().panes.clear();
        a.relist(Instant::now()).unwrap();
        assert_eq!(
            drain(&mut rx),
            [MuxEvent::PaneClosed(PaneClosed { pane: 3 })]
        );
        b.tick(Instant::now());
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            status_events(&events)[0].workspace_name.as_deref(),
            Some("b")
        );
        assert_eq!(state.rows_for_session("b").len(), 1);
    }

    // ── Rows the backend removed ────────────────────────────────────────────

    #[test]
    fn a_pane_the_backend_closed_is_announced_once() {
        let mut r = rig(&[(3, "bash")]);
        r.feed(report(3, "state=blocked:app=claude"));
        r.events();
        // `TmuxBackend::close_pane` drops the row, then tmux reports the
        // layout change.
        r.state.remove_pane(3);
        r.feed(Notification::LayoutChange { window: 1 });
        let now = Instant::now();
        r.watcher.tick(now);
        assert!(r.events().is_empty(), "no takeover while a listing is owed");
        assert!(r.state.rows_for_session("s").is_empty());

        r.source.world().panes.clear();
        r.watcher.maybe_relist(now + RELIST_GAP);
        assert_eq!(r.events(), [MuxEvent::PaneClosed(PaneClosed { pane: 3 })]);
        r.watcher.tick(now + RELIST_GAP);
        assert!(r.events().is_empty());
    }

    #[test]
    fn a_session_the_backend_killed_announces_each_agent_once() {
        let mut r = rig(&[(3, "bash"), (5, "claude"), (7, "bash")]);
        r.feed(report(3, "state=blocked:app=claude"));
        r.events();
        // `TmuxBackend::kill_session` drops the session's rows first.
        r.state.remove_session("s");
        r.watcher.session_gone();
        assert_eq!(
            r.events(),
            [
                MuxEvent::PaneClosed(PaneClosed { pane: 3 }),
                MuxEvent::PaneClosed(PaneClosed { pane: 5 }),
            ],
            "the agents, once each; not the plain shell"
        );
    }

    #[test]
    fn shutdown_stops_before_connecting() {
        let source = FakeSource::default();
        source.world().sessions = vec![session_row("s")];
        let (bus, _rx) = broadcast::channel(4);
        let w = Watcher::new(
            KEY,
            "s".into(),
            source,
            Arc::new(AgentState::new()),
            bus,
            false,
        );
        let stop = AtomicBool::new(true);
        let mut connects = 0;
        w.run(
            |_| -> Result<FakeControl> {
                connects += 1;
                Err(anyhow!("unreachable"))
            },
            &stop,
        );
        assert_eq!(connects, 0);
    }

    // ── Kernel planning ─────────────────────────────────────────────────────

    fn listed(id: u32, name: &str) -> SessionRow {
        SessionRow {
            name: name.into(),
            id,
            created_unix: 1,
            attached: 0,
            windows: 1,
        }
    }

    #[test]
    fn plan_starts_unwatched_sessions_up_to_the_cap() {
        let sessions = [listed(0, "a"), listed(1, "b"), listed(2, "c")];
        let watched = HashSet::from([SessionKey::of(&sessions[0])]);
        let mut capped = HashSet::new();
        let starts = plan_starts(&sessions, &watched, 2, &mut capped);
        assert_eq!(starts, [listed(1, "b")]);
        assert_eq!(capped, HashSet::from([SessionKey::of(&sessions[2])]));

        // Logged once: a second scan leaves it capped without a new entry.
        let again = plan_starts(&sessions, &watched, 2, &mut capped);
        assert_eq!(again, [listed(1, "b")]);
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn plan_treats_a_renamed_session_as_watched() {
        let before = [listed(0, "a")];
        let watched = HashSet::from([SessionKey::of(&before[0])]);
        let after = [listed(0, "renamed")];
        assert!(plan_starts(&after, &watched, 16, &mut HashSet::new()).is_empty());
    }

    #[test]
    fn plan_skips_unaddressable_session_names() {
        let sessions = [listed(0, "bad/name"), listed(1, "ok")];
        let starts = plan_starts(&sessions, &HashSet::new(), 16, &mut HashSet::new());
        assert_eq!(starts, [listed(1, "ok")]);
    }

    #[test]
    fn command_kind_is_the_base_name() {
        assert_eq!(command_kind("/usr/bin/claude"), "claude");
        assert_eq!(command_kind("codex"), "codex");
        assert_eq!(command_kind(""), "agent");
    }
}
