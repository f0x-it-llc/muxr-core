//! update — daily check for a newer muxrd release.
//!
//! Once a day the daemon asks github.com which muxr-core release is the
//! newest and remembers it when it is newer than the running binary. Clients
//! read the result through [`UpdateState`]; nothing is downloaded or
//! installed.
//!
//! ## What goes over the wire
//!
//! One `GET` to [`RELEASES_LATEST_URL`] with redirects disabled. GitHub
//! answers with a redirect to `/releases/tag/<tag>`, and the tag is read from
//! the `Location` header — the same method `install.sh` uses, and it is not
//! subject to the REST API rate limit. The request carries a
//! `muxrd/<version>` user agent and nothing else: no body, no token, no
//! identifier. The `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` environment variables
//! are honoured; an operating system's own proxy settings are not read.
//!
//! ## Opting out
//!
//! `update_check = false` in the config file or `MUXRD_UPDATE_CHECK=0` (see
//! `config`). When disabled the task is never spawned, so no request is made.
//!
//! ## State
//!
//! In memory only. A restart forgets the result and the first check runs
//! [`FIRST_CHECK_DELAY`] after start-up.

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use tokio::sync::watch;

/// The page GitHub redirects to the newest release's tag page.
pub const RELEASES_LATEST_URL: &str = "https://github.com/f0x-it-llc/muxr-core/releases/latest";

/// Time between checks.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Delay before the first check, so start-up is never held up by the network.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);

/// Per-request timeout for one check.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Path marker that precedes the tag in a release URL.
const TAG_PATH_MARKER: &str = "/releases/tag/";

// ─── Version arithmetic ───────────────────────────────────────────────────────

/// Split a release tag or version into `(major, minor, patch)`.
///
/// Accepts `0.5.5` and `v0.5.5`, with surrounding whitespace. A pre-release or
/// build tag after the patch number (`-rc1`, `+build`) is ignored; anything
/// else (`0.5`, `0.5.5.1`, `0.5.5x`) is not a version. Same rules as the
/// zellij version parser in `multiplexer::detect`.
fn parse_version_triple(version: &str) -> Option<(u32, u32, u32)> {
    fn number(digits: &str) -> Option<u32> {
        // `str::parse` alone would accept a leading `+`.
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()
    }
    let version = version.trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    let mut parts = version.splitn(3, '.');
    let major = number(parts.next()?)?;
    let minor = number(parts.next()?)?;
    let rest = parts.next()?;
    let (patch, tag) = match rest.find(|c: char| !c.is_ascii_digit()) {
        Some(end) => rest.split_at(end),
        None => (rest, ""),
    };
    if !(tag.is_empty() || tag.starts_with('-') || tag.starts_with('+')) {
        return None;
    }
    Some((major, minor, number(patch)?))
}

/// Read the release tag out of a redirect `Location` value.
///
/// The tag is the single path segment after `/releases/tag/`, in an absolute
/// or a relative URL. A query string or fragment is dropped first, so a
/// marker that only appears inside one (a login redirect's `return_to`) is
/// not a tag. Anything else yields `None`.
fn tag_from_location(location: &str) -> Option<&str> {
    let path = location.trim().split(['?', '#']).next()?;
    let (_, tag) = path.split_once(TAG_PATH_MARKER)?;
    let tag = tag.strip_suffix('/').unwrap_or(tag);
    if tag.is_empty() || tag.contains('/') {
        return None;
    }
    Some(tag)
}

/// `candidate` as `MAJOR.MINOR.PATCH` when it is strictly newer than
/// `running`; `None` when it is equal, older, or either side is not a version.
pub fn newer_than(running: &str, candidate: &str) -> Option<String> {
    let running = parse_version_triple(running)?;
    let candidate = parse_version_triple(candidate)?;
    (candidate > running).then(|| format!("{}.{}.{}", candidate.0, candidate.1, candidate.2))
}

// ─── Shared state ─────────────────────────────────────────────────────────────

/// The newest release known to be newer than the running daemon, if any.
///
/// Cheap to clone; every clone reads and writes the same value. Reads are
/// synchronous and never block on I/O, so the gRPC handlers and the control
/// socket's OS thread can both call [`UpdateState::latest`].
#[derive(Clone, Debug, Default)]
pub struct UpdateState {
    latest: Arc<RwLock<Option<String>>>,
}

impl UpdateState {
    /// The newer version as `MAJOR.MINOR.PATCH`, or `None` when the daemon is
    /// up to date, the check is disabled, or no check has succeeded yet.
    pub fn latest(&self) -> Option<String> {
        // A poisoned lock still holds a valid value: there is no invariant a
        // panicking writer could have broken half-way.
        self.latest
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replace the stored value. Called by the check task and by tests.
    pub fn set_latest(&self, latest: Option<String>) {
        *self
            .latest
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = latest;
    }
}

// ─── Release source ───────────────────────────────────────────────────────────

/// Where the newest release tag comes from. Abstracted so the check loop is
/// testable without network access.
pub trait ReleaseSource: Send + Sync + 'static {
    /// The newest release's tag, e.g. `v0.5.5`.
    fn latest_tag(&self) -> impl Future<Output = anyhow::Result<String>> + Send;
}

/// The production [`ReleaseSource`]: one reused `reqwest` client (rustls) that
/// reads the tag from GitHub's `releases/latest` redirect.
pub struct GithubReleases {
    client: reqwest::Client,
}

impl GithubReleases {
    /// Build the client once. Redirects are disabled because the redirect
    /// target is the answer; following it would fetch a page nobody reads.
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("muxrd/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build the update-check HTTP client")?;
        Ok(Self { client })
    }
}

impl ReleaseSource for GithubReleases {
    fn latest_tag(&self) -> impl Future<Output = anyhow::Result<String>> + Send {
        let request = self.client.get(RELEASES_LATEST_URL).send();
        async move {
            let response = request.await.context("request the latest release")?;
            let status = response.status();
            if !status.is_redirection() {
                anyhow::bail!("expected a redirect to the latest release, got HTTP {status}");
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .with_context(|| format!("HTTP {status} without a Location header"))?
                .to_str()
                .context("Location header is not valid text")?;
            let tag = tag_from_location(location)
                .with_context(|| format!("no release tag in redirect target '{location}'"))?;
            Ok(tag.to_owned())
        }
    }
}

// ─── Task loop + spawn ────────────────────────────────────────────────────────

/// Resolve once shutdown is requested: the flag is `true` or its sender is
/// gone. Returns immediately when that is already the case.
async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// The check task: wait `first_delay`, check, then check every `interval`
/// until shutdown. Shutdown interrupts a wait and an in-flight request alike.
///
/// A successful check stores [`newer_than`] of the running version and the
/// reported tag, so a release that is no longer newer clears the value. A
/// failed check keeps the previous value: one unreachable day must not make a
/// known update disappear.
pub async fn run_update_check<S: ReleaseSource>(
    state: UpdateState,
    source: S,
    running_version: String,
    first_delay: Duration,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut wait = first_delay;
    // Only the first failure of a run of failures is worth an operator's
    // attention; a daemon with no route to github.com would otherwise warn
    // once a day forever.
    let mut warned = false;
    loop {
        tokio::select! {
            () = shutdown_requested(&mut shutdown) => break,
            () = tokio::time::sleep(wait) => {}
        }
        wait = interval;

        let result = tokio::select! {
            () = shutdown_requested(&mut shutdown) => break,
            result = source.latest_tag() => result,
        };
        match result {
            Ok(tag) => {
                let latest = newer_than(&running_version, &tag);
                match &latest {
                    Some(version) => log::info!(
                        "update: muxrd {version} is available (running {running_version})"
                    ),
                    None => log::debug!(
                        "update: latest release is {tag}; running {running_version} is current"
                    ),
                }
                state.set_latest(latest);
                // A later outage is a new event and worth one warn again.
                warned = false;
            }
            Err(err) if !warned => {
                warned = true;
                log::warn!("update: could not check for a newer release: {err:#}");
            }
            Err(err) => log::debug!("update: could not check for a newer release: {err:#}"),
        }
    }
    log::debug!("update: check task stopped");
}

/// Spawn the production check task on the current tokio runtime.
///
/// Returns an error only when the HTTP client cannot be built; the caller logs
/// it and carries on without update checks.
pub fn spawn_update_check(
    state: UpdateState,
    shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let source = GithubReleases::new()?;
    tokio::spawn(run_update_check(
        state,
        source,
        env!("CARGO_PKG_VERSION").to_owned(),
        FIRST_CHECK_DELAY,
        CHECK_INTERVAL,
        shutdown,
    ));
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── parse_version_triple ─────────────────────────────────────────────────

    #[test]
    fn parses_plain_and_v_prefixed_versions() {
        assert_eq!(parse_version_triple("0.5.5"), Some((0, 5, 5)));
        assert_eq!(parse_version_triple("v0.5.5"), Some((0, 5, 5)));
        assert_eq!(parse_version_triple("  v12.0.34\n"), Some((12, 0, 34)));
    }

    #[test]
    fn ignores_pre_release_and_build_suffixes() {
        assert_eq!(parse_version_triple("v0.6.0-rc1"), Some((0, 6, 0)));
        assert_eq!(parse_version_triple("0.6.0+build.7"), Some((0, 6, 0)));
    }

    #[test]
    fn rejects_anything_that_is_not_a_version() {
        for junk in [
            "", "v", "latest", "0.5", "0.5.", "0.5.5.1", "0.5.5x", "0.+5.5", "V0.5.5", "vv0.5.5",
            "0.5.-1", "a.b.c",
        ] {
            assert_eq!(parse_version_triple(junk), None, "{junk:?}");
        }
    }

    // ── tag_from_location ────────────────────────────────────────────────────

    #[test]
    fn reads_the_tag_from_an_absolute_location() {
        assert_eq!(
            tag_from_location("https://github.com/f0x-it-llc/muxr-core/releases/tag/v0.5.5"),
            Some("v0.5.5")
        );
    }

    #[test]
    fn reads_the_tag_from_a_relative_location() {
        assert_eq!(
            tag_from_location("/f0x-it-llc/muxr-core/releases/tag/v0.5.5"),
            Some("v0.5.5")
        );
        assert_eq!(
            tag_from_location("/f0x-it-llc/muxr-core/releases/tag/v0.5.5/?x=1#top"),
            Some("v0.5.5")
        );
    }

    #[test]
    fn rejects_locations_without_a_single_tag_segment() {
        for location in [
            "",
            "https://github.com/f0x-it-llc/muxr-core/releases",
            "https://github.com/f0x-it-llc/muxr-core/releases/latest",
            "https://github.com/f0x-it-llc/muxr-core/releases/tag/",
            "https://github.com/f0x-it-llc/muxr-core/releases/tag/v0.5.5/assets",
            "https://github.com/login?return_to=x",
            "https://github.com/login?return_to=/releases/tag/v9.9.9",
            "https://github.com/login#/releases/tag/v9.9.9",
        ] {
            assert_eq!(tag_from_location(location), None, "{location:?}");
        }
    }

    // ── newer_than ───────────────────────────────────────────────────────────

    #[test]
    fn newer_than_reports_a_strictly_greater_version_normalized() {
        assert_eq!(newer_than("0.5.4", "v0.5.5"), Some("0.5.5".to_owned()));
        assert_eq!(newer_than("0.5.4", "0.6.0-rc1"), Some("0.6.0".to_owned()));
        assert_eq!(newer_than("0.9.9", "v1.0.0"), Some("1.0.0".to_owned()));
        // Numeric, not lexical: 0.5.10 is newer than 0.5.9.
        assert_eq!(newer_than("0.5.9", "v0.5.10"), Some("0.5.10".to_owned()));
    }

    #[test]
    fn newer_than_is_none_for_equal_older_or_unparsable() {
        assert_eq!(newer_than("0.5.4", "v0.5.4"), None);
        assert_eq!(newer_than("0.5.4", "v0.5.3"), None);
        assert_eq!(newer_than("0.5.4", "nightly"), None);
        assert_eq!(newer_than("dev", "v0.5.5"), None);
    }

    // ── UpdateState ──────────────────────────────────────────────────────────

    #[test]
    fn state_clones_share_one_value_across_threads() {
        let state = UpdateState::default();
        assert_eq!(state.latest(), None);
        let writer = state.clone();
        std::thread::spawn(move || writer.set_latest(Some("0.5.5".to_owned())))
            .join()
            .unwrap();
        assert_eq!(state.latest(), Some("0.5.5".to_owned()));
        state.set_latest(None);
        assert_eq!(state.latest(), None);
    }

    // ── run_update_check ─────────────────────────────────────────────────────

    /// Answers each check with the next scripted reply (the last one repeats)
    /// and counts the checks.
    #[derive(Clone)]
    struct ScriptedSource {
        replies: Arc<Mutex<VecDeque<Result<&'static str, &'static str>>>>,
        calls: Arc<AtomicUsize>,
    }

    impl ScriptedSource {
        fn new(replies: &[Result<&'static str, &'static str>]) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies.iter().copied().collect())),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ReleaseSource for ScriptedSource {
        fn latest_tag(&self) -> impl Future<Output = anyhow::Result<String>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut replies = self.replies.lock().unwrap();
            let reply = if replies.len() > 1 {
                replies.pop_front().unwrap()
            } else {
                *replies.front().expect("at least one scripted reply")
            };
            async move {
                match reply {
                    Ok(tag) => Ok(tag.to_owned()),
                    Err(message) => Err(anyhow::anyhow!(message)),
                }
            }
        }
    }

    /// A source whose request never completes, to prove shutdown interrupts it.
    struct HangingSource {
        calls: Arc<AtomicUsize>,
    }

    impl ReleaseSource for HangingSource {
        fn latest_tag(&self) -> impl Future<Output = anyhow::Result<String>> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::future::pending()
        }
    }

    const FIRST: Duration = Duration::from_secs(30);
    const EVERY: Duration = Duration::from_secs(3600);

    /// Let the spawned task run up to its next await point.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    fn start(
        source: impl ReleaseSource,
    ) -> (
        UpdateState,
        watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let state = UpdateState::default();
        let (tx, rx) = watch::channel(false);
        let task = tokio::spawn(run_update_check(
            state.clone(),
            source,
            "0.5.4".to_owned(),
            FIRST,
            EVERY,
            rx,
        ));
        (state, tx, task)
    }

    #[tokio::test(start_paused = true)]
    async fn first_check_runs_after_the_first_delay_and_then_every_interval() {
        let source = ScriptedSource::new(&[Ok("v0.5.5")]);
        let (state, _tx, _task) = start(source.clone());

        settle().await;
        tokio::time::advance(FIRST - Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(source.calls(), 0, "no check before the first delay");
        assert_eq!(state.latest(), None);

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(source.calls(), 1);
        assert_eq!(state.latest(), Some("0.5.5".to_owned()));

        tokio::time::advance(EVERY - Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(source.calls(), 1, "no second check before the interval");

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(source.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_equal_tag_leaves_the_state_empty() {
        let source = ScriptedSource::new(&[Ok("v0.5.4")]);
        let (state, _tx, _task) = start(source.clone());
        settle().await;
        tokio::time::advance(FIRST).await;
        settle().await;
        assert_eq!(source.calls(), 1);
        assert_eq!(state.latest(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_check_keeps_the_previous_value() {
        let source = ScriptedSource::new(&[Ok("v0.5.5"), Err("no route"), Err("no route")]);
        let (state, _tx, _task) = start(source.clone());
        settle().await;
        tokio::time::advance(FIRST).await;
        settle().await;
        assert_eq!(state.latest(), Some("0.5.5".to_owned()));

        for expected_calls in [2, 3] {
            tokio::time::advance(EVERY).await;
            settle().await;
            assert_eq!(source.calls(), expected_calls);
            assert_eq!(state.latest(), Some("0.5.5".to_owned()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_release_that_is_no_longer_newer_clears_the_value() {
        let source = ScriptedSource::new(&[Ok("v0.5.5"), Ok("v0.5.4")]);
        let (state, _tx, _task) = start(source);
        settle().await;
        tokio::time::advance(FIRST).await;
        settle().await;
        assert_eq!(state.latest(), Some("0.5.5".to_owned()));
        tokio::time::advance(EVERY).await;
        settle().await;
        assert_eq!(state.latest(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_the_task_during_a_wait() {
        let source = ScriptedSource::new(&[Ok("v0.5.5")]);
        let (_state, tx, task) = start(source.clone());
        settle().await;
        tx.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(source.calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_shutdown_sender_ends_the_task() {
        let source = ScriptedSource::new(&[Ok("v0.5.5")]);
        let (_state, tx, task) = start(source.clone());
        settle().await;
        tokio::time::advance(FIRST / 2).await;
        settle().await;
        drop(tx);
        task.await.unwrap();
        assert_eq!(source.calls(), 0, "the drop interrupted the first wait");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_a_request_in_flight() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_state, tx, task) = start(HangingSource {
            calls: Arc::clone(&calls),
        });
        settle().await;
        tokio::time::advance(FIRST).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the request is in flight");
        tx.send(true).unwrap();
        task.await.unwrap();
    }
}
