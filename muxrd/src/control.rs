//! control — the muxrd control socket (Phase E2).
//!
//! A tiny local-IPC contract used by `status`/`stop` to talk to a running
//! server.  It mirrors zellij's `web_server_commands` pattern (a
//! length-prefixed message over an `interprocess` `LocalSocketStream`) but uses
//! our **own** JSON contract instead of protobuf — the surface is two requests
//! (`Shutdown`, `Status`) and two responses (`Ok`, `Status{...}`).
//!
//! ## Wire format
//!
//! Each message is a `u32` little-endian length prefix followed by that many
//! bytes of `serde_json`.  Request and response use the same framing.
//!
//! ## Lifecycle
//!
//! The running server (foreground or daemon) calls [`spawn_listener`] which
//! binds the socket at [`socket_path`] and spawns a blocking accept loop on a
//! dedicated OS thread (NOT a tokio task — the accept loop is sync IPC and we
//! keep it off the runtime).  On a `Shutdown` request it fires the provided
//! shutdown trigger (a `tokio::sync::oneshot` sender wired into
//! `serve_with_shutdown`) and returns `Ok`.
//!
//! `status`/`stop` use [`query`] to send a single request and read the reply.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, CertMode};

/// Filename of the control socket inside the data dir.
const SOCKET_NAME: &str = "control.sock";

/// Serde default for [`StatusInfo::cert_mode`]: assume `SelfSigned` when the
/// field is absent (older server).
fn default_cert_mode() -> CertMode {
    CertMode::SelfSigned
}

/// Upper bound on a single control message body (64 KiB).
///
/// The wire format is a `u32` length prefix followed by that many bytes.  A
/// crafted prefix could otherwise request a multi-gigabyte `vec![0u8; len]`
/// allocation (review Major D — control-socket hardening); we reject anything
/// larger than this before allocating.  The real messages are tiny JSON blobs.
const MAX_CONTROL_MSG: usize = 64 * 1024;

/// A request sent to the running server over the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlRequest {
    /// Ask the server to shut down gracefully.
    Shutdown,
    /// Ask the server for its status (version / bind / pid / uptime).
    Status,
    /// Turn host monitoring on or off at runtime. The choice is persisted to
    /// the config file (`monitoring` key).
    SetMonitoring {
        /// The desired state.
        enabled: bool,
    },
}

/// A response from the running server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlResponse {
    /// Generic acknowledgement (e.g. after `Shutdown`).
    Ok,
    /// Status payload (reply to [`ControlRequest::Status`]).
    Status(StatusInfo),
    /// The request took effect at runtime but a follow-up step failed
    /// (e.g. persisting `monitoring` to the config file). Carries the message.
    Error(String),
}

/// Server status reported over the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    /// The server crate version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// The address the server is bound to.
    pub bind_addr: String,
    /// The server process id.
    pub pid: u32,
    /// Seconds the server has been running.
    pub uptime_secs: u64,
    /// Total number of mobile clients currently attached across all sessions.
    ///
    /// Defaults to 0 so that older clients / messages that pre-date this field
    /// still deserialize correctly (`#[serde(default)]`).
    #[serde(default)]
    pub client_count: usize,
    /// The active TLS / transport mode (self_signed, external, h2c).
    ///
    /// Defaults to `SelfSigned` when deserialising a response from an older
    /// server that pre-dates this field, which is the most conservative
    /// assumption for backward compatibility.
    #[serde(default = "default_cert_mode")]
    pub cert_mode: CertMode,
    /// The configured push-notification relay URL, or `None` when push
    /// notifications are disabled (`config::EffectiveConfig::notify_relay_url`).
    ///
    /// `#[serde(default)]` so a message from an older server (pre-dating this
    /// field) still deserialises fine.
    #[serde(default)]
    pub notify_relay_url: Option<String>,
    /// Number of devices currently registered for push notifications.
    ///
    /// A fresh read of `notify::devices::PushDeviceStore` taken at query
    /// time (not cached) — see [`spawn_listener`]. `#[serde(default)]` so a
    /// message from an older server (pre-dating this field) still
    /// deserialises fine.
    #[serde(default)]
    pub push_device_count: usize,
    /// The newest released muxrd version when it is newer than `version`, or
    /// `None` when the server is current, has not checked yet, or has the
    /// daily check disabled (`config::EffectiveConfig::update_check`).
    ///
    /// A fresh read of `update::UpdateState` taken at query time — see
    /// [`spawn_listener`]. `#[serde(default)]` so a message from an older
    /// server (pre-dating this field) still deserialises fine.
    #[serde(default)]
    pub latest_version: Option<String>,
    /// Whether host monitoring is currently on (read from the live monitor
    /// handle at query time). `#[serde(default)]` so a message from an older
    /// server (pre-dating this field) still deserialises fine.
    #[serde(default)]
    pub monitoring_enabled: bool,
}

/// Path to the control socket: `data_dir()/control.sock`.
pub fn socket_path() -> Result<PathBuf> {
    Ok(config::data_dir()?.join(SOCKET_NAME))
}

// ── Framing helpers ───────────────────────────────────────────────────────────

fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let bytes = serde_json::to_vec(msg).context("control: serialize message")?;
    let len = u32::try_from(bytes.len()).context("control: message too large")?;
    w.write_all(&len.to_le_bytes())
        .context("control: write length prefix")?;
    w.write_all(&bytes).context("control: write body")?;
    w.flush().context("control: flush")?;
    Ok(())
}

fn read_msg<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<T> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)
        .context("control: read length prefix")?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    // Reject an oversized length prefix BEFORE allocating, so a crafted prefix
    // can't drive a giant `vec![0u8; len]` allocation (review Major D).
    if len > MAX_CONTROL_MSG {
        anyhow::bail!("control: message length {len} exceeds maximum {MAX_CONTROL_MSG}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).context("control: read body")?;
    serde_json::from_slice(&buf).context("control: deserialize message")
}

// ── Client side (status / stop) ───────────────────────────────────────────────

/// Upper bound on a single control-socket round trip (connect already
/// completes near-instantly for Unix-domain sockets; this bounds the
/// send/receive of the request/response pair).
///
/// Without this, a daemon whose control thread has accepted a connection but
/// is wedged (deadlocked, stuck holding a lock before it reads/replies) would
/// hang `query` — and therefore `start`'s liveness check, `status`, and
/// `stop` — indefinitely. A timed-out query is treated exactly like a
/// connect failure: "unresponsive" (every caller already maps any `Err` from
/// `query` to that meaning, including `start_staleness_decision` in
/// `bin/muxrd.rs`), so no caller-side change was needed to wire this in.
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Send a single request to the running server and read its response.
///
/// Returns an error if the socket is absent, the server is unresponsive, or
/// the round trip exceeds [`QUERY_TIMEOUT`] — callers treat all three the
/// same way: "not running" / "unresponsive".
pub fn query(req: &ControlRequest) -> Result<ControlResponse> {
    query_at(&socket_path()?, req, QUERY_TIMEOUT)
}

/// Testable core of [`query`]: connect to `path`, bound the round trip to
/// `timeout`, send `req`, and read the reply.
///
/// Split out from `query` so tests can point it at a throwaway socket path
/// instead of the real (shared, non-overridable) data-dir control socket.
fn query_at(path: &Path, req: &ControlRequest, timeout: Duration) -> Result<ControlResponse> {
    if !path.exists() {
        anyhow::bail!("control socket {} does not exist", path.display());
    }
    let mut stream = zellij_utils::consts::ipc_connect(path)
        .with_context(|| format!("control: connect to {}", path.display()))?;
    // Bring the `Stream` trait's timeout setters into scope (the concrete type
    // returned by `ipc_connect` implements it; see interprocess::local_socket).
    use interprocess::local_socket::traits::Stream as _;
    stream
        .set_recv_timeout(Some(timeout))
        .context("control: set recv timeout")?;
    stream
        .set_send_timeout(Some(timeout))
        .context("control: set send timeout")?;
    write_msg(&mut stream, req)?;
    read_msg(&mut stream)
}

// ── Server side (listener) ────────────────────────────────────────────────────

/// Shared shutdown trigger: a oneshot sender consumed on the first `Shutdown`.
type ShutdownTrigger = Mutex<Option<tokio::sync::oneshot::Sender<()>>>;

/// Persists the `monitoring` choice; [`config::set_monitoring`] in production.
pub type PersistMonitoring = Box<dyn Fn(bool) -> Result<()> + Send + Sync>;

/// Everything the request handler reads or acts on, so [`handle_request`] can
/// be driven without a socket.
pub struct ControlContext {
    /// The effective server bind address (reported in `Status`).
    pub bind_addr: String,
    /// The instant the server began (for `uptime_secs`).
    pub started_at: Instant,
    /// Fired the first time a `Shutdown` request arrives.
    pub shutdown: ShutdownTrigger,
    /// Per-session attached-client registry (total count in `Status`).
    pub clients: crate::client_count::SessionClients,
    /// The active TLS / transport mode reported in `Status`.
    pub cert_mode: CertMode,
    /// The resolved push-notification relay URL (or `None` when disabled).
    pub notify_relay_url: Option<String>,
    /// Push-device registry; counted afresh on every `Status`.
    pub push_devices: crate::notify::devices::PushDeviceStore,
    /// State the daily update check writes; read afresh on every `Status`.
    pub update: crate::update::UpdateState,
    /// The host-monitoring switch.
    pub monitor: crate::monitor::MonitorHandle,
    /// The tokio runtime to enter before touching `monitor`: the accept loop
    /// runs on a plain OS thread, and enabling the monitor spawns a task.
    pub runtime: tokio::runtime::Handle,
    /// Persists the monitoring choice.
    pub persist_monitoring: PersistMonitoring,
}

/// Answer one control request. Callable from any thread: runtime-dependent
/// work enters [`ControlContext::runtime`] itself.
pub fn handle_request(req: &ControlRequest, ctx: &ControlContext) -> ControlResponse {
    match req {
        ControlRequest::Status => ControlResponse::Status(StatusInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            bind_addr: ctx.bind_addr.clone(),
            pid: std::process::id(),
            uptime_secs: ctx.started_at.elapsed().as_secs(),
            client_count: ctx.clients.total_count(),
            cert_mode: ctx.cert_mode,
            notify_relay_url: ctx.notify_relay_url.clone(),
            push_device_count: ctx.push_devices.count().unwrap_or_else(|e| {
                log::warn!("control: failed to read push device count: {e:#}");
                0
            }),
            latest_version: ctx.update.latest(),
            monitoring_enabled: ctx.monitor.is_enabled(),
        }),
        ControlRequest::Shutdown => {
            log::info!("control: shutdown requested");
            if let Some(tx) = ctx.shutdown.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ControlResponse::Ok
        }
        ControlRequest::SetMonitoring { enabled } => {
            log::info!("control: set monitoring = {enabled}");
            {
                let _guard = ctx.runtime.enter();
                ctx.monitor.set_enabled(*enabled);
            }
            // Enabling is refused once shutdown has fired (or without a runtime): report
            // that, and do not persist a choice the daemon did not take.
            if ctx.monitor.is_enabled() != *enabled {
                log::warn!("control: monitoring could not be turned on (shutting down)");
                return ControlResponse::Error(
                    "monitoring could not be turned on: the server is shutting down; \
                     nothing was saved"
                        .to_owned(),
                );
            }
            match (ctx.persist_monitoring)(*enabled) {
                Ok(()) => ControlResponse::Ok,
                Err(e) => {
                    log::warn!("control: monitoring flipped but not persisted: {e:#}");
                    ControlResponse::Error(format!(
                        "monitoring is now {} but could not be saved to the config file: {e:#}",
                        if *enabled { "on" } else { "off" }
                    ))
                }
            }
        }
    }
}

/// Spawn the control-socket accept loop on a dedicated OS thread.
///
/// `ctx` carries every input of the `Status` reply and the monitoring switch
/// (see [`ControlContext`]); `push_device_count` and `latest_version` are read
/// afresh on every `Status` request, so they stay accurate while the server
/// runs. A push-store read failure degrades to `0` rather than failing the
/// whole response. `ctx.shutdown` fires the first time a `Shutdown` request
/// arrives; it should be the trigger side of a `serve_with_shutdown` future.
///
/// The socket is bound up-front (so a `status`/`stop` race right after start
/// still finds it) and removed by the caller on exit (see [`cleanup`]).
pub fn spawn_listener(ctx: ControlContext) -> Result<()> {
    let path = socket_path()?;

    // Remove any stale socket file from a previous (crashed) run; ipc_bind
    // fails with EADDRINUSE on a leftover socket path.
    let _ = std::fs::remove_file(&path);

    // `incoming()` comes from the ListenerExt trait — bring it into scope.
    use interprocess::local_socket::traits::ListenerExt;

    let listener = zellij_utils::consts::ipc_bind(&path)
        .with_context(|| format!("control: bind {}", path.display()))?;

    std::thread::Builder::new()
        .name("muxrd-control".to_string())
        .spawn(move || {
            for conn in listener.incoming() {
                let mut stream = match conn {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("control: accept error: {e}");
                        continue;
                    }
                };
                let req: ControlRequest = match read_msg(&mut stream) {
                    Ok(r) => r,
                    Err(e) => {
                        log::warn!("control: bad request: {e:#}");
                        continue;
                    }
                };
                let resp = handle_request(&req, &ctx);
                if let Err(e) = write_msg(&mut stream, &resp) {
                    log::warn!("control: failed to write response: {e:#}");
                }
                if matches!(req, ControlRequest::Shutdown) {
                    // The server is winding down; stop accepting.
                    break;
                }
            }
            log::debug!("control: accept loop exited");
        })
        .context("control: spawn accept thread")?;

    Ok(())
}

/// Remove the control socket file (best-effort).
pub fn cleanup() {
    if let Ok(path) = socket_path() {
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    /// A bound throwaway control socket that removes its file on drop, so a
    /// failing assertion cannot leak it.
    struct SocketFixture {
        path: PathBuf,
        listener: Option<interprocess::local_socket::Listener>,
    }

    impl SocketFixture {
        fn bind(tag: &str, secs: u64) -> Self {
            // Keep the path short (see the sun_path note in the tests below).
            let path =
                std::env::temp_dir().join(format!("mxc{}_{tag}{secs}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let listener = zellij_utils::consts::ipc_bind(&path).expect("bind fake control socket");
            Self {
                path,
                listener: Some(listener),
            }
        }

        fn take_listener(&mut self) -> interprocess::local_socket::Listener {
            self.listener.take().expect("listener already taken")
        }
    }

    impl Drop for SocketFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A failing assertion inside the fixture's scope must not leave the
    /// socket file behind.
    #[test]
    fn socket_fixture_removes_its_file_when_the_test_panics() {
        let path = std::sync::Mutex::new(None::<PathBuf>);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let fixture = SocketFixture::bind("leak", 0);
            *path.lock().unwrap() = Some(fixture.path.clone());
            assert!(fixture.path.exists(), "fixture should have bound");
            panic!("deliberate assertion failure inside the fixture");
        }));
        assert!(result.is_err());
        let path = path.lock().unwrap().take().expect("path recorded");
        assert!(!path.exists(), "socket file leaked after a panic");
    }

    /// A peer that accepts the connection and never reads/replies must not
    /// hang `query_at` forever — the bounded recv timeout must fire and the
    /// call must return `Err` well within a generous margin around the bound.
    #[test]
    fn query_at_times_out_on_a_silent_peer() {
        use interprocess::local_socket::traits::ListenerExt;

        // Keep the socket path SHORT: `sun_path` caps at ~104 bytes on macOS and
        // the CI runner's temp dir is deep (`/var/folders/…`). A compact
        // pid+seconds name stays under the cap (mirrors relay.rs's
        // `unique_socket_path`).
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_secs();
        let mut fixture = SocketFixture::bind("silent", secs);
        let sock_path = fixture.path.clone();
        let listener = fixture.take_listener();

        // Accept exactly one connection and hold it open without ever
        // reading or replying — simulating a wedged control thread.
        std::thread::spawn(move || {
            if let Some(Ok(_stream)) = listener.incoming().next() {
                std::thread::sleep(Duration::from_secs(30));
            }
        });

        let timeout = Duration::from_millis(200);
        let start = Instant::now();
        let result = query_at(&sock_path, &ControlRequest::Status, timeout);
        let elapsed = start.elapsed();

        assert!(result.is_err(), "query against a silent peer must fail");
        assert!(
            elapsed < Duration::from_secs(5),
            "query_at took {elapsed:?}, expected to bail near the {timeout:?} bound"
        );
    }

    /// A path with no socket present must fail fast (no timeout involved).
    #[test]
    fn query_at_missing_socket_errors_immediately() {
        let path = std::env::temp_dir().join("muxrd-control-test-does-not-exist.sock");
        let _ = std::fs::remove_file(&path);
        let result = query_at(&path, &ControlRequest::Status, QUERY_TIMEOUT);
        assert!(result.is_err());
    }

    // ── StatusInfo back-compat ────────────────────────────────────────────────

    /// A `Status` JSON payload from a server that pre-dates `notify_relay_url` /
    /// `push_device_count` (and even `cert_mode`, the previous addition) must
    /// still deserialize — every field added after the original four is
    /// `#[serde(default)]`. This is what lets an older muxrctl binary keep
    /// talking to a newer muxrd, and a newer muxrctl keep talking to an older
    /// muxrd, without a hard version lockstep.
    #[test]
    fn status_info_deserializes_from_pre_notify_payload() {
        let legacy_json = serde_json::json!({
            "version": "0.2.4",
            "bind_addr": "127.0.0.1:50051",
            "pid": 1234,
            "uptime_secs": 42,
        });
        let info: StatusInfo = serde_json::from_value(legacy_json)
            .expect("legacy StatusInfo payload must deserialize");

        assert_eq!(info.version, "0.2.4");
        assert_eq!(info.client_count, 0, "missing client_count defaults to 0");
        assert_eq!(
            info.cert_mode,
            CertMode::SelfSigned,
            "missing cert_mode defaults to SelfSigned"
        );
        assert_eq!(
            info.notify_relay_url, None,
            "missing notify_relay_url defaults to None"
        );
        assert_eq!(
            info.push_device_count, 0,
            "missing push_device_count defaults to 0"
        );
        assert_eq!(
            info.latest_version, None,
            "missing latest_version defaults to None"
        );
    }

    /// A payload from a server one release back — everything except
    /// `latest_version` — still deserializes, and reports no update.
    #[test]
    fn status_info_deserializes_from_pre_update_check_payload() {
        let previous_json = serde_json::json!({
            "version": "0.5.4",
            "bind_addr": "127.0.0.1:50051",
            "pid": 1234,
            "uptime_secs": 42,
            "client_count": 1,
            "cert_mode": "self_signed",
            "notify_relay_url": "https://noti.muxr.app",
            "push_device_count": 2,
        });
        let info: StatusInfo =
            serde_json::from_value(previous_json).expect("0.5.4 StatusInfo must deserialize");
        assert_eq!(info.push_device_count, 2);
        assert_eq!(info.latest_version, None);
    }

    /// A full modern payload (including the new fields) round-trips.
    #[test]
    fn status_info_round_trips_with_notify_fields() {
        let info = StatusInfo {
            version: "0.2.5".to_owned(),
            bind_addr: "127.0.0.1:50051".to_owned(),
            pid: 1234,
            uptime_secs: 10,
            client_count: 2,
            cert_mode: CertMode::SelfSigned,
            notify_relay_url: Some("https://noti.muxr.app".to_owned()),
            push_device_count: 3,
            latest_version: Some("0.2.6".to_owned()),
            monitoring_enabled: true,
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let round_tripped: StatusInfo = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(round_tripped.notify_relay_url, info.notify_relay_url);
        assert_eq!(round_tripped.push_device_count, info.push_device_count);
        assert_eq!(round_tripped.latest_version, Some("0.2.6".to_owned()));
        assert!(round_tripped.monitoring_enabled);
    }

    /// A payload from a server one release back — everything except
    /// `monitoring_enabled` — still deserialises, with monitoring off.
    #[test]
    fn status_info_deserializes_from_pre_monitoring_payload() {
        let previous_json = serde_json::json!({
            "version": "0.5.5",
            "bind_addr": "127.0.0.1:50051",
            "pid": 1234,
            "uptime_secs": 42,
            "client_count": 1,
            "cert_mode": "self_signed",
            "notify_relay_url": null,
            "push_device_count": 2,
            "latest_version": "0.5.6",
        });
        let info: StatusInfo =
            serde_json::from_value(previous_json).expect("0.5.5 StatusInfo must deserialize");
        assert_eq!(info.latest_version.as_deref(), Some("0.5.6"));
        assert!(!info.monitoring_enabled);
    }

    // ── handle_request ────────────────────────────────────────────────────────

    fn test_ctx(
        rt: &tokio::runtime::Runtime,
        persist: PersistMonitoring,
    ) -> (ControlContext, tokio::sync::watch::Sender<bool>) {
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel();
        let ctx = ControlContext {
            bind_addr: "127.0.0.1:1".to_owned(),
            started_at: Instant::now(),
            shutdown: Mutex::new(Some(shutdown_tx)),
            clients: Default::default(),
            cert_mode: CertMode::SelfSigned,
            notify_relay_url: None,
            // A path that does not exist: counting reads nothing from the
            // user's data dir.
            push_devices: crate::notify::devices::PushDeviceStore::at_path(
                std::env::temp_dir().join("muxrd-control-test-no-such-devices.json"),
            ),
            update: Default::default(),
            monitor: crate::monitor::MonitorHandle::new(stop_rx),
            runtime: rt.handle().clone(),
            persist_monitoring: persist,
        };
        (ctx, stop_tx)
    }

    fn status_monitoring(ctx: &ControlContext) -> bool {
        match handle_request(&ControlRequest::Status, ctx) {
            ControlResponse::Status(info) => info.monitoring_enabled,
            other => panic!("unexpected response {other:?}"),
        }
    }

    /// Driven from a plain std thread, like the accept loop: enabling must
    /// enter the supplied runtime rather than panic, and persist the choice.
    #[test]
    fn set_monitoring_from_a_plain_thread_flips_persists_and_reports() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let persisted = std::sync::Arc::new(Mutex::new(Vec::<bool>::new()));
        let sink = persisted.clone();
        let (ctx, _stop) = test_ctx(
            &rt,
            Box::new(move |v| {
                sink.lock().unwrap().push(v);
                Ok(())
            }),
        );
        std::thread::scope(|s| {
            s.spawn(|| {
                assert!(!status_monitoring(&ctx));
                let r = handle_request(&ControlRequest::SetMonitoring { enabled: true }, &ctx);
                assert!(matches!(r, ControlResponse::Ok), "{r:?}");
                assert!(status_monitoring(&ctx));
                let r = handle_request(&ControlRequest::SetMonitoring { enabled: false }, &ctx);
                assert!(matches!(r, ControlResponse::Ok), "{r:?}");
                assert!(!status_monitoring(&ctx));
            })
            .join()
            .unwrap();
        });
        assert_eq!(*persisted.lock().unwrap(), vec![true, false]);
    }

    #[test]
    fn set_monitoring_persist_failure_still_flips_and_reports_an_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (ctx, _stop) = test_ctx(&rt, Box::new(|_| anyhow::bail!("disk full")));
        std::thread::scope(|s| {
            s.spawn(|| {
                let r = handle_request(&ControlRequest::SetMonitoring { enabled: true }, &ctx);
                match r {
                    ControlResponse::Error(msg) => assert!(msg.contains("disk full"), "{msg}"),
                    other => panic!("expected Error, got {other:?}"),
                }
                assert!(status_monitoring(&ctx), "runtime state must still flip");
            })
            .join()
            .unwrap();
        });
    }

    #[test]
    fn set_monitoring_refused_during_shutdown_reports_an_error_and_persists_nothing() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let persisted = std::sync::Arc::new(Mutex::new(Vec::<bool>::new()));
        let sink = persisted.clone();
        let (ctx, stop) = test_ctx(
            &rt,
            Box::new(move |v| {
                sink.lock().unwrap().push(v);
                Ok(())
            }),
        );
        stop.send(true).unwrap();
        std::thread::scope(|s| {
            s.spawn(|| {
                let r = handle_request(&ControlRequest::SetMonitoring { enabled: true }, &ctx);
                match r {
                    ControlResponse::Error(msg) => {
                        assert!(msg.contains("shutting down"), "{msg}");
                    }
                    other => panic!("expected Error, got {other:?}"),
                }
                assert!(!status_monitoring(&ctx), "still off");
            })
            .join()
            .unwrap();
        });
        assert!(persisted.lock().unwrap().is_empty(), "nothing persisted");
    }
}
