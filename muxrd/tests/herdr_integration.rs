//! herdr integration smoke-test (P2.05).
//!
//! All tests here are `#[ignore]`-gated so `cargo test -p muxrd` stays green
//! without a live herdr instance.  Run them against a real herdr:
//!
//! ```text
//! # 1.  Start herdr (user-installed binary, unmodified).
//! #     herdr defaults to $HOME/.config/herdr/herdr.sock; or set HERDR_SOCKET_PATH:
//! herdr &
//!
//! # 2.  Point muxrd at it and run the ignored tests:
//! HERDR_SOCKET_PATH=/path/to/herdr.sock \
//!   cargo test -p muxrd --test herdr_integration -- --ignored
//! ```
//!
//! ## What each test exercises
//!
//! | test | exercises |
//! |------|-----------|
//! | `smoke_list_sessions`         | `HerdrBackend::list_sessions()` — JSON-API workspace list |
//! | `smoke_discovers_wire_protocol` | protocol discovery via `ping` (regression: herdr 0.7.2+) |
//! | `smoke_create_query_kill`     | `create_session` / `query_layout` / `kill_session` round-trip |
//! | `smoke_open_attach_render_input` | `open_attach` → read `Render` frames → send input → teardown |
//! | `smoke_space_scoped_layout_is_a_read_only_peek` | `query_layout_for_space` on a NON-focused workspace — right tree, no focus moved |
//! | `smoke_read_only_attach_observes_without_taking_ownership` | a read-only attach renders but never evicts the terminal's owner; a read-write one does |
//! | `smoke_observer_repoints_across_tabs_and_panes` | `go_to_tab` / `focus_pane` on a read-only attach — the re-point works in observe mode |
//! | `smoke_observer_resize_never_resizes_the_pane` | an observer's `Resize` is accepted and leaves the pane at the owner's size |
//! | `smoke_observer_wheel_scroll_does_not_reach_the_pane` | herdr drops an observer's wheel — read-only scroll is a no-op on this backend |
//!
//! The four observe-mode tests attach to the SAME workspace, so run them serially
//! (`-- --ignored --test-threads=1`): concurrent attaches take each other over by
//! design and would make the ownership assertions meaningless. They also need
//! herdr 0.9.0 or newer (`ObserveTerminal` is protocol 22's tag 7) and the herdr
//! CLI (`HERDR_BIN`, default `herdr` on `PATH`) for the server-side probes.
//!
//! ## Licence and attribution note
//!
//! These tests drive herdr solely through its public Unix-domain sockets (the
//! JSON-API control socket and the binary wire relay socket).  herdr runs as a
//! separate, unmodified, user-installed binary and no herdr source is linked.
//!
//! The message layouts they exercise live in `muxrd`'s
//! `multiplexer::herdr::wire`, which is **derived from herdr v0.9.0's
//! `src/protocol/wire.rs` (Apache-2.0) and modified**; this file is derived from
//! the same source to the extent that it pins that protocol's version floor.
//! Attribution is retained per Apache-2.0 §4, with the repository-level notice in
//! `THIRD-PARTY-NOTICES.md`.  (herdr relicensed from AGPL-3.0-or-later at v0.8.0;
//! nothing here is AGPL.)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use muxrd::multiplexer::{
    HerdrBackend, MuxBackend, MuxEvent, MuxMouseKind, MuxReceiver, MuxServerMsg, PaneRef,
};

// ─── helpers ──────────────────────────────────────────────────────────────────

/// Construct a `HerdrBackend` from the process environment.
///
/// Panics with an actionable message when `HERDR_SOCKET_PATH` is not set
/// *and* the XDG default does not exist — so test failures are diagnosed
/// immediately rather than producing a confusing socket-connect error.
fn backend() -> HerdrBackend {
    HerdrBackend::from_env()
}

/// A unique session name for tests that create a workspace.
///
/// `subsec_millis()` wraps every 1000 ms, so two creations within the same second
/// could collide; combine the full epoch-millis with a process-wide atomic counter
/// so every call is distinct regardless of timing.
fn test_session_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("muxrd-smoke-{millis}-{seq}")
}

/// The bare-name sentinel muxrd collapses the whole herdr daemon onto
/// (`multiplexer::herdr::backend::HERDR_SESSION`, crate-private).
const HERDR_SESSION: &str = "herdr";

/// How long the observe-mode tests give herdr to apply a resize or a re-point
/// before probing it. herdr applies these on its render pass, so the probe has to
/// come after one; 1.5 s is the same settle the resize-lock e2e test uses.
const SETTLE: Duration = Duration::from_millis(1500);

/// How long a frame/teardown wait may take before the assertion fails.
const WAIT: Duration = Duration::from_secs(4);

/// A marker unique to this process and call, for the input-containment probes.
fn marker(tag: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("muxrd_{tag}_{}_{seq}", std::process::id())
}

// ─── herdr's own view, for the server-side assertions ─────────────────────────
//
// The observe-mode tests need to know what herdr itself thinks: which pane is
// focused, how many rows its pty has, and what text it holds. None of that is on
// `MuxBackend` (`query_session_size` reports the tab's layout AREA, which is
// herdr's view geometry and does not follow a direct attach's size), so these
// probes go through herdr's own CLI — another client of the same public socket,
// never a second implementation of its protocol.

/// The herdr CLI to probe with: `HERDR_BIN`, else `herdr` on `PATH`. It inherits
/// this process's `HERDR_SOCKET_PATH`, so it always talks to the same instance
/// the backend under test does.
fn herdr_bin() -> String {
    std::env::var("HERDR_BIN").unwrap_or_else(|_| "herdr".to_string())
}

/// Run the herdr CLI and return its stdout, panicking with the whole output on a
/// non-zero exit — a silent probe failure would turn into a misleading assertion.
fn herdr_cli(args: &[&str]) -> String {
    let bin = herdr_bin();
    let out = std::process::Command::new(&bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| {
            panic!("could not run the herdr CLI ({bin} {args:?}): {e} — set HERDR_BIN")
        });
    assert!(
        out.status.success(),
        "herdr CLI failed ({bin} {args:?}): status={:?} stdout={} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `(pane_id, viewport_rows)` of herdr's focused pane — the pane muxrd's attach
/// lands on, and `viewport_rows` is herdr's own report of that pane's pty rows.
fn focused_pane() -> (String, u64) {
    let raw = herdr_cli(&["pane", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("herdr pane list is not JSON: {e}\n{raw}"));
    let panes = parsed["result"]["panes"]
        .as_array()
        .unwrap_or_else(|| panic!("no panes array in herdr pane list:\n{raw}"));
    let pane = panes
        .iter()
        .find(|p| p["focused"].as_bool() == Some(true))
        .or_else(|| panes.first())
        .unwrap_or_else(|| panic!("herdr reports no panes at all:\n{raw}"));
    let id = pane["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("pane without a pane_id:\n{raw}"))
        .to_string();
    let rows = pane["scroll"]["viewport_rows"]
        .as_u64()
        .unwrap_or_else(|| panic!("pane {id} reports no scroll.viewport_rows:\n{raw}"));
    (id, rows)
}

/// Every pane's `(pane_id, viewport_rows)`, for the tests that must prove a pane
/// they navigated AWAY from was left alone.
fn all_pane_rows() -> Vec<(String, u64)> {
    let raw = herdr_cli(&["pane", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("herdr pane list is not JSON: {e}\n{raw}"));
    parsed["result"]["panes"]
        .as_array()
        .unwrap_or_else(|| panic!("no panes array in herdr pane list:\n{raw}"))
        .iter()
        .map(|p| {
            (
                p["pane_id"].as_str().unwrap_or_default().to_string(),
                p["scroll"]["viewport_rows"].as_u64().unwrap_or_default(),
            )
        })
        .collect()
}

/// herdr's own text snapshot of one pane — how the input-containment probes tell
/// whether a keystroke actually reached the pty.
fn pane_text(pane_id: &str) -> String {
    herdr_cli(&["pane", "read", pane_id])
}

/// One pane's `(offset_from_bottom, max_offset_from_bottom)` — herdr's own report
/// of where its viewport sits in the scrollback, for the wheel-scroll probe.
fn pane_scroll(pane_id: &str) -> (u64, u64) {
    let raw = herdr_cli(&["pane", "list"]);
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("herdr pane list is not JSON: {e}\n{raw}"));
    let pane = parsed["result"]["panes"]
        .as_array()
        .unwrap_or_else(|| panic!("no panes array in herdr pane list:\n{raw}"))
        .iter()
        .find(|p| p["pane_id"].as_str() == Some(pane_id))
        .unwrap_or_else(|| panic!("pane {pane_id} is gone:\n{raw}"));
    (
        pane["scroll"]["offset_from_bottom"].as_u64().unwrap_or(0),
        pane["scroll"]["max_offset_from_bottom"]
            .as_u64()
            .unwrap_or(0),
    )
}

// ─── stream probe ─────────────────────────────────────────────────────────────

/// A drained attach stream: counts `Render` frames and notices the stream ending.
///
/// The receiver half blocks in `recv()`, so it has to be drained on its own
/// thread for the test to assert anything about it — and draining also keeps the
/// wire socket from backing up, exactly as the relay's reader thread does.
struct StreamProbe {
    frames: Arc<AtomicU64>,
    ended: Arc<AtomicBool>,
    exit_reason: Arc<std::sync::Mutex<Option<String>>>,
}

impl StreamProbe {
    fn start(mut receiver: Box<dyn MuxReceiver>, label: &'static str) -> Self {
        let frames = Arc::new(AtomicU64::new(0));
        let ended = Arc::new(AtomicBool::new(false));
        let exit_reason = Arc::new(std::sync::Mutex::new(None));
        let (f, e, r) = (
            Arc::clone(&frames),
            Arc::clone(&ended),
            Arc::clone(&exit_reason),
        );
        std::thread::spawn(move || {
            while let Some(msg) = receiver.recv() {
                match msg {
                    MuxServerMsg::Render(_) => {
                        f.fetch_add(1, Ordering::Relaxed);
                    }
                    MuxServerMsg::Event(MuxEvent::Exit { reason }) => {
                        println!("[observe] {label} stream exit: {reason:?}");
                        *r.lock().unwrap() = Some(reason);
                        break;
                    }
                    _ => {}
                }
            }
            e.store(true, Ordering::SeqCst);
        });
        Self {
            frames,
            ended,
            exit_reason,
        }
    }

    fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst) || self.exit_reason.lock().unwrap().is_some()
    }

    /// Wait until more than `mark` frames have arrived; `false` on timeout.
    fn frames_grow_past(&self, mark: u64) -> bool {
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline {
            if self.frames() > mark {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Wait for the stream to end; `false` if it is still alive after `WAIT`.
    fn ends(&self) -> bool {
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline {
            if self.ended() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

// ─── smoke_list_sessions ──────────────────────────────────────────────────────

/// Verify `list_sessions()` returns without error against a live herdr.
///
/// Does NOT assert on a specific workspace list (the operator's herdr may have
/// zero or many); just confirms the JSON-API round-trip succeeds.
///
/// # Run
/// ```text
/// HERDR_SOCKET_PATH=/path/to/herdr.sock \
///   cargo test -p muxrd --test herdr_integration smoke_list_sessions -- --ignored
/// ```
#[test]
#[ignore = "requires a live herdr instance (set HERDR_SOCKET_PATH)"]
fn smoke_list_sessions() {
    let b = backend();
    let sessions = b
        .list_sessions()
        .expect("list_sessions() failed against live herdr");
    println!(
        "[herdr smoke] list_sessions → {} workspace(s)",
        sessions.len()
    );
    for (name, age) in &sessions {
        println!("  workspace: {name:?}  age: {age:?}");
    }
}

// ─── smoke_discovers_wire_protocol ────────────────────────────────────────────

/// Verify muxrd **discovers** the connected herdr's wire protocol instead of
/// assuming one.
///
/// This is the regression guard for the herdr-0.7.2+ outage: muxrd hard-coded
/// protocol 14 while herdr shipped 16 (v0.7.2) then 17 (v0.7.5), and herdr enforces
/// strict equality on the handshake — rejecting clients that are older *or* newer —
/// so every `AttachTerminal` failed. `backend_version()` reports the negotiated
/// value, so a live server must yield a real protocol number here, never the
/// "unknown" fallback and never a constant.
///
/// The floor asserted below is muxrd's `HERDR_MIN_PROTOCOL` — 22, shipped by herdr
/// v0.9.0 — because the wire mirror carries exactly that one layout with no
/// compatibility branch: protocol 22 renamed and retyped the handshake and deleted a
/// server variant, so an older server is not a supported peer.
///
/// # Run
/// ```text
/// HERDR_SOCKET_PATH=/path/to/herdr.sock \
///   cargo test -p muxrd --test herdr_integration smoke_discovers_wire_protocol -- --ignored
/// ```
#[test]
#[ignore = "requires a live herdr instance (set HERDR_SOCKET_PATH)"]
fn smoke_discovers_wire_protocol() {
    let b = backend();
    let version = b.backend_version();
    println!("[herdr smoke] backend_version → {version}");

    assert_ne!(
        version, "herdr-wire-unknown",
        "protocol discovery failed against a live herdr — `ping` did not return a protocol"
    );
    assert!(
        version.starts_with("herdr-"),
        "unexpected backend_version format: {version}"
    );

    // The reported protocol must be a real number the server told us, and at least
    // the protocol muxrd's wire mirror is written against (`HERDR_MIN_PROTOCOL`).
    let protocol: u32 = version
        .rsplit("-wire-v")
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no parseable wire protocol in {version:?}"));
    assert!(
        protocol >= 22,
        "discovered protocol {protocol} is older than the protocol muxrd mirrors (22, \
         herdr v0.9.0) — upgrade herdr"
    );
}

// ─── smoke_create_query_kill ──────────────────────────────────────────────────

/// Create a workspace, query its layout, then kill it.
///
/// Exercises the full JSON-API control round-trip:
/// `create_session` → `query_layout` → `kill_session`.
///
/// # Run
/// ```text
/// HERDR_SOCKET_PATH=/path/to/herdr.sock \
///   cargo test -p muxrd --test herdr_integration smoke_create_query_kill -- --ignored
/// ```
#[test]
#[ignore = "requires a live herdr instance (set HERDR_SOCKET_PATH); creates + destroys a workspace"]
fn smoke_create_query_kill() {
    let b = backend();
    let name = test_session_name();

    // Create.
    let ack = b
        .create_session(&name, None)
        .expect("create_session() failed");
    assert!(ack.ok, "create_session returned ok:false — {ack:?}");
    println!("[herdr smoke] created workspace {name:?}  ack={ack:?}");

    // Give herdr a moment to settle (workspace may not be immediately queryable).
    std::thread::sleep(Duration::from_millis(200));

    // Verify it appears in the session list.
    let sessions = b
        .list_sessions()
        .expect("list_sessions() failed after create");
    let found = sessions.iter().any(|(n, _)| n == &name);
    assert!(
        found,
        "workspace {name:?} not found in list after create: {sessions:?}"
    );

    // Query layout.
    let layout = b
        .query_layout(&name)
        .expect("query_layout() failed for newly created workspace");
    let total_panes: usize = layout.tabs.iter().map(|t| t.panes.len()).sum();
    println!(
        "[herdr smoke] layout tabs={} panes={}",
        layout.tabs.len(),
        total_panes,
    );

    // Kill.
    b.kill_session(&name).expect("kill_session() failed");
    println!("[herdr smoke] workspace {name:?} killed");

    // Confirm it is gone.
    std::thread::sleep(Duration::from_millis(100));
    let after = b
        .list_sessions()
        .expect("list_sessions() failed after kill");
    let still_present = after.iter().any(|(n, _)| n == &name);
    assert!(
        !still_present,
        "workspace {name:?} still listed after kill: {after:?}"
    );
}

// ─── smoke_open_attach_render_input ───────────────────────────────────────────

/// `open_attach` the focused pane of an existing workspace, read a few
/// `MuxServerMsg::Render` frames, send a test string, and tear down cleanly.
///
/// Requires at least one workspace to be present in herdr (create one manually
/// before running, or run `smoke_create_query_kill` first to prove creation works
/// then let a session linger).  The test selects the first listed workspace.
///
/// # Run
/// ```text
/// HERDR_SOCKET_PATH=/path/to/herdr.sock \
///   cargo test -p muxrd --test herdr_integration smoke_open_attach_render_input -- --ignored
/// ```
#[test]
#[ignore = "requires a live herdr instance with at least one workspace (set HERDR_SOCKET_PATH)"]
fn smoke_open_attach_render_input() {
    let b = backend();

    // Pick the first available workspace.
    let sessions = b.list_sessions().expect("list_sessions() failed");
    assert!(
        !sessions.is_empty(),
        "no workspaces found in herdr — create one before running this test"
    );
    let (session_name, _) = &sessions[0];
    println!("[herdr smoke] attaching to workspace {session_name:?}");

    // Open attach (24 rows × 80 cols, read-write).
    let handle = b
        .open_attach(session_name, 24, 80, false)
        .expect("open_attach() failed");
    println!(
        "[herdr smoke] attach open — session={:?}",
        handle.session_name
    );

    // Split into sender + receiver.
    let (mut sender, mut receiver) = handle.split();

    // Read up to 5 Render frames (or until EOF / 3-second wall clock).
    //
    // We move the receiver to a background thread so the wall-clock timeout
    // can be enforced on the main thread without blocking it indefinitely.
    let (frame_tx, frame_rx) = std::sync::mpsc::channel::<MuxServerMsg>();
    std::thread::spawn(move || {
        while let Some(msg) = receiver.recv() {
            let _ = frame_tx.send(msg);
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut render_count = 0usize;
    while render_count < 5 && std::time::Instant::now() < deadline {
        match frame_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(MuxServerMsg::Render(bytes)) => {
                render_count += 1;
                println!(
                    "[herdr smoke] Render frame #{render_count}: {} bytes",
                    bytes.len()
                );
            }
            Ok(other) => {
                println!("[herdr smoke] non-Render frame: {other:?}");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                println!("[herdr smoke] recv timeout after {render_count} Render frames");
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                println!("[herdr smoke] receiver thread finished");
                break;
            }
        }
    }

    assert!(
        render_count > 0,
        "expected at least one Render frame from herdr attach; got none"
    );

    // Send some input.
    sender
        .send_input_chars("echo herdr-smoke-ok\r")
        .expect("send_input_chars() failed");
    println!("[herdr smoke] sent test input");

    // Clean teardown.
    sender.send_client_exited().ok();
    println!("[herdr smoke] client exited; test complete");
}

// ─── smoke_space_scoped_layout_is_a_read_only_peek ────────────────────────────

/// Read a **non-focused** workspace's layout by explicit space id and prove the
/// call is a pure read.
///
/// This is the daemon half of the terminal side-sheet tree: a client names a space
/// it is not currently viewing and gets that space's tabs/panes back — while
/// neither its own connection nor any co-attached desktop client is yanked to it.
///
/// Asserts both halves of the contract:
/// 1. the returned tree belongs to the **peeked** workspace (its tab ids are
///    disjoint from the session's own active-or-first workspace),
/// 2. the daemon's focused workspace is **unchanged** after the call — and
///    `space_id = None` still answers exactly like `query_layout` (back-compat).
///
/// # Harness (two workspaces, one of them not focused)
/// ```text
/// herdr workspace create        # so the daemon has ≥ 2 workspaces
/// HERDR_SOCKET_PATH=/path/to/herdr.sock \
///   cargo test -p muxrd --test herdr_integration \
///     smoke_space_scoped_layout_is_a_read_only_peek -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires a live herdr instance with at least TWO workspaces (set HERDR_SOCKET_PATH)"]
fn smoke_space_scoped_layout_is_a_read_only_peek() {
    // The bare-name sentinel muxrd collapses the whole herdr daemon onto
    // (`multiplexer::herdr::backend::HERDR_SESSION`, crate-private).
    const HERDR_SESSION: &str = "herdr";

    let b = backend();

    let before = b.list_spaces(HERDR_SESSION).expect("list_spaces() failed");
    assert!(
        before.len() >= 2,
        "this test needs ≥2 herdr workspaces (found {}) — create one before running",
        before.len()
    );
    let focused_before = before
        .iter()
        .find(|s| s.active)
        .map(|s| s.id.clone())
        .expect("herdr must report a focused workspace");
    let target = before
        .iter()
        .find(|s| !s.active)
        .expect("this test needs a NON-focused workspace to peek at");
    println!(
        "[herdr smoke] focused={focused_before:?}  peeking at {:?} ({:?})",
        target.id, target.name
    );

    // Baseline: the session's own (active-or-first) workspace layout.
    let own = b
        .query_layout(HERDR_SESSION)
        .expect("query_layout() failed");
    let own_tabs: Vec<u64> = own.tabs.iter().map(|t| t.tab_id).collect();

    // (a) The peek returns the TARGET workspace's tree.
    let scoped = b
        .query_layout_for_space(HERDR_SESSION, Some(&target.id))
        .expect("query_layout_for_space() failed for a non-focused workspace");
    let scoped_tabs: Vec<u64> = scoped.tabs.iter().map(|t| t.tab_id).collect();
    println!("[herdr smoke] own tabs={own_tabs:?}  peeked tabs={scoped_tabs:?}");
    assert!(
        !scoped.tabs.is_empty(),
        "the peeked workspace must report at least one tab"
    );
    assert!(
        scoped.tabs.iter().any(|t| !t.panes.is_empty()),
        "the peeked workspace must report at least one pane"
    );
    assert!(
        scoped_tabs.iter().all(|id| !own_tabs.contains(id)),
        "tabs must come from the PEEKED workspace, not the focused one \
         (own={own_tabs:?}, peeked={scoped_tabs:?})"
    );

    // (b) Nothing moved: the daemon's focused workspace is untouched.
    let after = b
        .list_spaces(HERDR_SESSION)
        .expect("list_spaces() failed after the peek");
    assert_eq!(
        after.iter().find(|s| s.active).map(|s| s.id.as_str()),
        Some(focused_before.as_str()),
        "a space-scoped layout read must NOT move the daemon's focused workspace"
    );

    // Back-compat: no explicit space → identical to `query_layout`.
    let none_tabs: Vec<u64> = b
        .query_layout_for_space(HERDR_SESSION, None)
        .expect("query_layout_for_space(None) failed")
        .tabs
        .iter()
        .map(|t| t.tab_id)
        .collect();
    assert_eq!(
        none_tabs, own_tabs,
        "space_id=None must be byte-identical to the ordinary query_layout() path"
    );
    println!("[herdr smoke] PASS — peeked a non-focused workspace, focus unchanged");
}

// ─── smoke_switch_restores_pane_sizes (resize-lock leak regression) ───────────

/// End-to-end regression for the herdr `direct_attach_resize_locks` leak
/// (workflow/plans/bug/herdr-pane-resize-leak/): a small muxrd attach that
/// navigates across tabs must leave every pane it LEAVES restored to the
/// desktop layout size — at switch time, not merely at detach — and after
/// teardown ALL panes must be back at desktop size.
///
/// Drives the REAL release-then-reconnect paths: `open_attach` (20×40) →
/// `MuxSender::go_to_tab` across every tab → teardown. Pane PTY sizes are
/// measured via `stty -F /dev/pts/N size` on the pane shells (children of the
/// herdr session server, in spawn order == tab order).
///
/// # Harness (isolated herdr session + REQUIRED attached desktop client)
/// ```text
/// tmux new-session -d -s e2e-desk -x 200 -y 50 'herdr --session muxrd-e2e'
/// herdr --session muxrd-e2e tab create && herdr --session muxrd-e2e tab create
/// HERDR_SOCKET_PATH=$HOME/.config/herdr/sessions/muxrd-e2e/herdr.sock \
/// HERDR_E2E_SERVER_PID=<pid of that session's `herdr server`> \
///   cargo test -p muxrd --test herdr_integration smoke_switch_restores_pane_sizes -- --ignored --nocapture
/// ```
/// The desktop client must stay attached: herdr re-imposes layout sizes during
/// its render/layout pass, which only runs while a full client is connected.
#[test]
#[ignore = "requires a live herdr session with an attached desktop client (set HERDR_SOCKET_PATH + HERDR_E2E_SERVER_PID)"]
fn smoke_switch_restores_pane_sizes() {
    const SMALL: (u16, u16) = (20, 40); // rows, cols

    let server_pid: u32 = std::env::var("HERDR_E2E_SERVER_PID")
        .expect("set HERDR_E2E_SERVER_PID to the herdr session server pid")
        .trim()
        .parse()
        .expect("HERDR_E2E_SERVER_PID must be a pid");

    /// `(rows, cols)` of every pane shell PTY under the session server, in
    /// spawn (pid) order — one shell per pane, one pane per tab in the harness.
    fn pane_sizes(server_pid: u32) -> Vec<(u16, u16)> {
        let out = std::process::Command::new("ps")
            .args(["--ppid", &server_pid.to_string(), "-o", "pid="])
            .output()
            .expect("ps failed");
        let mut pids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect();
        pids.sort_unstable();
        pids.iter()
            .map(|pid| {
                let pts =
                    std::fs::read_link(format!("/proc/{pid}/fd/0")).expect("readlink pane pty");
                let out = std::process::Command::new("stty")
                    .args(["-F", pts.to_str().unwrap(), "size"])
                    .output()
                    .expect("stty failed");
                let s = String::from_utf8_lossy(&out.stdout);
                let mut it = s.split_whitespace().filter_map(|n| n.parse().ok());
                (it.next().expect("rows"), it.next().expect("cols"))
            })
            .collect()
    }

    let b = backend();
    let sessions = b.list_sessions().expect("list_sessions() failed");
    assert!(!sessions.is_empty(), "harness session not found");
    let (session_name, _) = &sessions[0];

    let baseline = pane_sizes(server_pid);
    assert!(
        baseline.len() >= 3,
        "harness must create ≥3 tabs (one pane each); found {} pane shell(s)",
        baseline.len()
    );
    assert!(
        !baseline.contains(&SMALL),
        "baseline already contains the small size — stale state from a prior run? {baseline:?}"
    );
    println!("[e2e] baseline: {baseline:?}");

    let handle = b
        .open_attach(session_name, SMALL.0, SMALL.1, false)
        .expect("open_attach() failed");
    let (mut sender, mut receiver) = handle.split();
    // Drain frames on a background thread (so the wire socket never backs up),
    // counting Render frames — frames must KEEP FLOWING after every switch
    // (regression: herdr leaves Detached sockets open; without the sender-side
    // shutdown the reader never adopted the swapped connection and froze).
    let frames = std::sync::Arc::new(AtomicU64::new(0));
    let frames_in_drain = std::sync::Arc::clone(&frames);
    let drain = std::thread::spawn(move || {
        while let Some(msg) = receiver.recv() {
            if matches!(msg, MuxServerMsg::Render(_)) {
                frames_in_drain.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    let frames_grow_past = |mark: u64| {
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline {
            if frames.load(Ordering::Relaxed) > mark {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    };

    // Populate the tab registry + learn the tab ids (position-ordered).
    let layout = sender
        .query_layout_result()
        .expect("herdr answers layout out-of-band")
        .expect("query_layout_result() failed");
    let mut tabs: Vec<_> = layout.tabs.iter().map(|t| (t.position, t.tab_id)).collect();
    tabs.sort_unstable();
    println!("[e2e] tabs (position, id): {tabs:?}");

    let settle = || std::thread::sleep(Duration::from_millis(1500));
    settle();
    let after_attach = pane_sizes(server_pid);
    println!("[e2e] after attach:  {after_attach:?}");
    assert_eq!(
        after_attach[0], SMALL,
        "attached pane (tab 1) should be at the small client size"
    );

    // Walk every tab; after each switch the pane we LEFT must be restored.
    let mut prev_idx = 0usize;
    for (idx, (_pos, tab_id)) in tabs.iter().enumerate().skip(1) {
        let frame_mark = frames.load(Ordering::Relaxed);
        sender.go_to_tab(*tab_id).expect("go_to_tab() failed");
        settle();
        let now = pane_sizes(server_pid);
        println!(
            "[e2e] on tab {}:     {now:?}  (frames: {})",
            idx + 1,
            frames.load(Ordering::Relaxed)
        );
        assert!(
            frames_grow_past(frame_mark),
            "frames must keep flowing after switching to tab {} — reader failed to \
             adopt the new connection",
            idx + 1
        );
        assert_eq!(
            now[idx],
            SMALL,
            "newly focused pane (tab {}) should be at the small size",
            idx + 1
        );
        assert_eq!(
            now[prev_idx],
            baseline[prev_idx],
            "pane LEFT behind (tab {}) must be restored to desktop size at switch time \
             — the resize-lock leak is back",
            prev_idx + 1
        );
        prev_idx = idx;
    }

    // Teardown: graceful detach; every pane must return to desktop size.
    sender.send_client_exited().ok();
    drop(sender);
    drain.join().ok();
    settle();
    let after_detach = pane_sizes(server_pid);
    println!("[e2e] after detach:  {after_detach:?}");
    assert_eq!(
        after_detach, baseline,
        "all panes must be back at desktop size after the mobile client detaches"
    );
    println!("[e2e] PASS — no pane left stuck at the small size");
}

// ─── observe mode (read-only attaches) ────────────────────────────────────────
//
// The three tests below answer, against a live herdr, what a read-only attach
// actually is on this backend. They need herdr 0.9.0 or newer (`ObserveTerminal`
// is protocol 22's tag 7) and they attach to the same workspace, so run them
// serially:
//
// ```text
// HERDR_SOCKET_PATH=/path/to/herdr.sock HERDR_BIN=/path/to/herdr \
//   cargo test -p muxrd --test herdr_integration smoke_observ -- --ignored --nocapture --test-threads=1
// HERDR_SOCKET_PATH=/path/to/herdr.sock HERDR_BIN=/path/to/herdr \
//   cargo test -p muxrd --test herdr_integration smoke_read_only -- --ignored --nocapture
// ```

/// A read-only attach must OBSERVE herdr's terminal, never own it.
///
/// Three server-side facts are asserted, each with the paired positive that makes
/// it mean something:
///
/// 1. the observer renders — it receives frames like any attach;
/// 2. the observer does not evict the terminal's owner, while a second
///    **read-write** attach does (that is `AttachTerminal { takeover: true }`, and
///    it is the behaviour a read-only attach used to have);
/// 3. input sent on the observing connection never reaches the pty, while the
///    owner's input does.
///
/// Fact 3 is the backstop the muxrd-side read-only filter used to be alone in
/// providing: with observe mode the server refuses the write too.
#[test]
#[ignore = "requires a live herdr 0.9.0 with at least one workspace (set HERDR_SOCKET_PATH, HERDR_BIN)"]
fn smoke_read_only_attach_observes_without_taking_ownership() {
    const OWNER: (u16, u16) = (28, 90); // rows, cols
    const OBSERVER: (u16, u16) = (11, 40);

    let b = backend();
    let (pane_id, rows_before) = focused_pane();
    println!("[observe] focused pane {pane_id} at {rows_before} rows before any attach");

    // (1) The owner: an ordinary read-write attach.
    let owner = b
        .open_attach(HERDR_SESSION, OWNER.0, OWNER.1, false)
        .expect("read-write open_attach() failed");
    let (mut owner_tx, owner_rx) = owner.split();
    let owner_stream = StreamProbe::start(owner_rx, "owner");
    assert!(
        owner_stream.frames_grow_past(0),
        "the read-write attach must receive at least one render frame"
    );

    // (2) The observer: a read-only attach on the same pane, at a size the pane
    //     must NOT adopt.
    let observer = b
        .open_attach(HERDR_SESSION, OBSERVER.0, OBSERVER.1, true)
        .expect("read-only open_attach() failed — herdr rejected the observe handshake");
    let (mut observer_tx, observer_rx) = observer.split();
    let observer_stream = StreamProbe::start(observer_rx, "observer");
    assert!(
        observer_stream.frames_grow_past(0),
        "a read-only attach must still receive render frames — an observer renders"
    );
    std::thread::sleep(SETTLE);

    assert!(
        !owner_stream.ended(),
        "a read-only attach must NOT evict the terminal's owner — the owner's \
         stream ended, so the observe attach took write ownership"
    );
    let (_, rows_with_observer) = focused_pane();
    assert_eq!(
        rows_with_observer,
        u64::from(OWNER.0),
        "the pane must stay at the OWNER's size while an observer watches \
         (owner {} rows, observer {} rows, herdr reports {rows_with_observer})",
        OWNER.0,
        OBSERVER.0
    );
    println!("[observe] owner alive, pane still {rows_with_observer} rows — no ownership taken");

    // (3) Input containment, both directions.
    let obs_marker = marker("obs_input");
    observer_tx
        .send_input_chars(&format!("echo {obs_marker}\r"))
        .expect("sending input on the observing connection must not fail at the wire level");
    std::thread::sleep(SETTLE);
    let text = pane_text(&pane_id);
    assert!(
        !text.contains(&obs_marker),
        "input sent on a read-only (observe) connection reached the pty — herdr \
         is not enforcing observe mode. Marker {obs_marker} found in pane {pane_id}"
    );

    let owner_marker = marker("owner_input");
    owner_tx
        .send_input_chars(&format!("echo {owner_marker}\r"))
        .expect("send_input_chars() on the owner failed");
    std::thread::sleep(SETTLE);
    let text = pane_text(&pane_id);
    assert!(
        text.contains(&owner_marker),
        "the OWNER's input must reach the pty — without this the containment \
         assertion above proves nothing. Marker {owner_marker} missing from pane {pane_id}"
    );
    println!("[observe] observer input dropped by herdr, owner input landed");

    // (4) The paired negative: a read-WRITE attach really does take the terminal
    //     over, so the "observer did not evict the owner" assertion is sensitive.
    let usurper = b
        .open_attach(HERDR_SESSION, OWNER.0, OWNER.1, false)
        .expect("second read-write open_attach() failed");
    let (mut usurper_tx, usurper_rx) = usurper.split();
    let usurper_stream = StreamProbe::start(usurper_rx, "usurper");
    assert!(
        owner_stream.ends(),
        "a second READ-WRITE attach must take the terminal over and end the first \
         owner's stream — if it does not, this test cannot detect an eviction at all"
    );
    println!("[observe] second read-write attach evicted the owner, as expected");
    assert!(
        !observer_stream.ended(),
        "the takeover must not disturb the observer: it owns nothing to lose"
    );

    usurper_tx.send_client_exited().ok();
    observer_tx.send_client_exited().ok();
    owner_tx.send_client_exited().ok();
    drop(usurper_stream);
    println!("[observe] PASS — read-only attaches observe; only read-write attaches own");
}

/// A read-only observer must still be able to re-point: `go_to_tab` and
/// `focus_pane` are a `Detach` plus a fresh connection, and that fresh connection
/// has to come back up in observe mode too.
///
/// Asserts both halves: frames keep flowing after the move (the re-point worked),
/// and no pane the observer visits is resized to the observer's size (the
/// re-point did not promote it to owner on the way).
#[test]
#[ignore = "requires a live herdr 0.9.0 with at least TWO tabs in the focused workspace (set HERDR_SOCKET_PATH, HERDR_BIN)"]
fn smoke_observer_repoints_across_tabs_and_panes() {
    const OBSERVER: (u16, u16) = (13, 44); // rows, cols — deliberately unusual

    let b = backend();
    let rows_before = all_pane_rows();
    println!("[observe] pane rows before: {rows_before:?}");
    assert!(
        !rows_before
            .iter()
            .any(|(_, rows)| *rows == u64::from(OBSERVER.0)),
        "a pane is already at the observer's row count — stale state from an \
         earlier run would make this test vacuous: {rows_before:?}"
    );

    let handle = b
        .open_attach(HERDR_SESSION, OBSERVER.0, OBSERVER.1, true)
        .expect("read-only open_attach() failed");
    let (mut sender, receiver) = handle.split();
    let stream = StreamProbe::start(receiver, "observer");
    assert!(
        stream.frames_grow_past(0),
        "the observer must receive frames before we re-point it"
    );

    // Learn the tabs (and populate the pane registry for focus_pane below).
    let layout = sender
        .query_layout_result()
        .expect("herdr answers layout out-of-band")
        .expect("query_layout_result() failed");
    let mut tabs: Vec<_> = layout.tabs.iter().map(|t| (t.position, t.tab_id)).collect();
    tabs.sort_unstable();
    assert!(
        tabs.len() >= 2,
        "this test needs ≥2 tabs in the focused workspace, found {}",
        tabs.len()
    );
    println!("[observe] tabs (position, id): {tabs:?}");

    // Re-point across every other tab: each one is a release-then-reconnect.
    for (position, tab_id) in tabs.iter().skip(1) {
        let mark = stream.frames();
        sender
            .go_to_tab(*tab_id)
            .expect("go_to_tab() on a read-only attach failed — the observe re-point is broken");
        assert!(
            stream.frames_grow_past(mark),
            "frames must keep flowing after an observer re-points to tab {position} \
             — the reconnect did not deliver the new pane"
        );
        println!("[observe] re-pointed to tab {position}, frames still flowing");
    }

    // And a pane-level re-point, the other caller of the same mechanism.
    let first_pane = layout
        .tabs
        .iter()
        .flat_map(|t| t.panes.iter())
        .map(|p| PaneRef::terminal(p.id))
        .next()
        .expect("the layout must report at least one pane");
    let mark = stream.frames();
    sender
        .focus_pane(first_pane)
        .expect("focus_pane() on a read-only attach failed");
    assert!(
        stream.frames_grow_past(mark),
        "frames must keep flowing after an observer's focus_pane re-point"
    );

    std::thread::sleep(SETTLE);
    let rows_after = all_pane_rows();
    println!("[observe] pane rows after:  {rows_after:?}");
    assert_eq!(
        rows_after, rows_before,
        "an observer that navigated across tabs and panes must not have resized \
         ANY of them — a re-point that came back as an owner would have"
    );

    sender.send_client_exited().ok();
    assert!(
        stream.ends(),
        "the observer's stream must end after its own Detach"
    );
    println!("[observe] PASS — an observer navigates, and leaves every pane's size alone");
}

/// An observer's `Resize` is accepted and changes nothing about the pane.
///
/// muxrd forwards `Resize` unconditionally (resizing is a read-only-PERMITTED
/// operation: it changes only what this viewer sees). On herdr that frame updates
/// the observing connection's own render viewport and stops there — the pane's pty
/// keeps the owner's size. The owner's `Resize` is the paired positive proving the
/// probe can see a real resize.
#[test]
#[ignore = "requires a live herdr 0.9.0 with at least one workspace (set HERDR_SOCKET_PATH, HERDR_BIN)"]
fn smoke_observer_resize_never_resizes_the_pane() {
    const OWNER: (u16, u16) = (26, 88); // rows, cols
    const OBSERVER: (u16, u16) = (9, 32);
    const OBSERVER_RESIZED: (u16, u16) = (17, 60);
    const OWNER_RESIZED: (u16, u16) = (34, 96);

    let b = backend();

    // The owner fixes the pane's size.
    let owner = b
        .open_attach(HERDR_SESSION, OWNER.0, OWNER.1, false)
        .expect("read-write open_attach() failed");
    let (mut owner_tx, owner_rx) = owner.split();
    let owner_stream = StreamProbe::start(owner_rx, "owner");
    assert!(
        owner_stream.frames_grow_past(0),
        "owner must receive frames"
    );
    std::thread::sleep(SETTLE);

    let (pane_id, owner_rows) = focused_pane();
    assert_eq!(
        owner_rows,
        u64::from(OWNER.0),
        "an owning attach must set the pane's pty size — without this the \
         observer assertions below would be vacuous (pane {pane_id})"
    );
    println!("[observe] owner holds pane {pane_id} at {owner_rows} rows");

    // The observer attaches at a very different size, then resizes again.
    let observer = b
        .open_attach(HERDR_SESSION, OBSERVER.0, OBSERVER.1, true)
        .expect("read-only open_attach() failed");
    let (mut observer_tx, observer_rx) = observer.split();
    let observer_stream = StreamProbe::start(observer_rx, "observer");
    assert!(
        observer_stream.frames_grow_past(0),
        "observer must receive frames"
    );
    std::thread::sleep(SETTLE);
    let (_, rows) = focused_pane();
    assert_eq!(
        rows,
        u64::from(OWNER.0),
        "an observer's ATTACH size must not resize the pane (owner {} rows, \
         observer attached at {} rows, herdr reports {rows})",
        OWNER.0,
        OBSERVER.0
    );

    let mark = observer_stream.frames();
    observer_tx
        .send_resize(OBSERVER_RESIZED.0, OBSERVER_RESIZED.1)
        .expect("herdr must ACCEPT a Resize from an observing connection");
    std::thread::sleep(SETTLE);
    let (_, rows) = focused_pane();
    assert_eq!(
        rows,
        u64::from(OWNER.0),
        "an observer's RESIZE must not resize the pane (owner {} rows, observer \
         resized to {} rows, herdr reports {rows})",
        OWNER.0,
        OBSERVER_RESIZED.0
    );
    assert!(
        !observer_stream.ended(),
        "the Resize must not tear the observing stream down — herdr accepts it"
    );
    assert!(
        observer_stream.frames_grow_past(mark),
        "herdr must repaint the observer after its own Resize — the frame is how \
         the viewer learns its new viewport"
    );
    println!("[observe] observer resized its own viewport; pane still {rows} rows");

    // The paired positive: the OWNER's resize does move the pane.
    owner_tx
        .send_resize(OWNER_RESIZED.0, OWNER_RESIZED.1)
        .expect("send_resize() on the owner failed");
    std::thread::sleep(SETTLE);
    let (_, rows) = focused_pane();
    assert_eq!(
        rows,
        u64::from(OWNER_RESIZED.0),
        "the OWNER's resize must reach the pty — otherwise this probe cannot see \
         a resize at all and the assertions above are vacuous"
    );
    println!("[observe] owner resize moved the pane to {rows} rows");

    observer_tx.send_client_exited().ok();
    owner_tx.send_client_exited().ok();
    println!("[observe] PASS — an observer never resizes the pane; the owner does");
}

/// Wheel scroll is read-only-PERMITTED in muxrd's own boundary — it changes only
/// what this viewer looks at — but herdr routes `AttachScroll` through the same
/// owner-only path as input, so an observer's wheel does nothing at all. That is a
/// backend difference worth pinning rather than discovering from a bug report:
/// muxrd keeps forwarding the frame, herdr keeps dropping it.
#[test]
#[ignore = "requires a live herdr 0.9.0 with at least one workspace (set HERDR_SOCKET_PATH, HERDR_BIN)"]
fn smoke_observer_wheel_scroll_does_not_reach_the_pane() {
    const OWNER: (u16, u16) = (24, 80); // rows, cols
    const OBSERVER: (u16, u16) = (24, 80);

    let b = backend();

    // Give the pane scrollback to scroll through, from the owning connection.
    let owner = b
        .open_attach(HERDR_SESSION, OWNER.0, OWNER.1, false)
        .expect("read-write open_attach() failed");
    let (mut owner_tx, owner_rx) = owner.split();
    let owner_stream = StreamProbe::start(owner_rx, "owner");
    assert!(
        owner_stream.frames_grow_past(0),
        "owner must receive frames"
    );
    owner_tx
        .send_input_chars("seq 1 400\r")
        .expect("send_input_chars() failed");
    std::thread::sleep(SETTLE);

    let (pane_id, _) = focused_pane();
    assert!(
        pane_scroll(&pane_id).1 > 0,
        "the pane needs scrollback for this test to mean anything (pane {pane_id})"
    );
    assert_eq!(
        pane_scroll(&pane_id).0,
        0,
        "the pane must start pinned to the bottom"
    );

    // The observer's wheel: accepted by muxrd, dropped by herdr.
    let observer = b
        .open_attach(HERDR_SESSION, OBSERVER.0, OBSERVER.1, true)
        .expect("read-only open_attach() failed");
    let (mut observer_tx, observer_rx) = observer.split();
    let observer_stream = StreamProbe::start(observer_rx, "observer");
    assert!(
        observer_stream.frames_grow_past(0),
        "observer must receive frames"
    );
    for _ in 0..3 {
        observer_tx
            .send_mouse(MuxMouseKind::WheelUp, 1, 1)
            .expect("herdr must ACCEPT a wheel frame from an observing connection");
    }
    std::thread::sleep(SETTLE);
    let (offset, _) = pane_scroll(&pane_id);
    assert_eq!(
        offset, 0,
        "an observer's wheel must not scroll the shared pane — herdr routes \
         AttachScroll through its owner-only path (pane {pane_id} at offset {offset})"
    );
    assert!(
        !observer_stream.ended(),
        "the wheel frame must not tear the observing stream down"
    );

    // Paired positive: the owner's wheel does scroll it.
    for _ in 0..3 {
        owner_tx
            .send_mouse(MuxMouseKind::WheelUp, 1, 1)
            .expect("send_mouse() on the owner failed");
    }
    std::thread::sleep(SETTLE);
    let (offset, _) = pane_scroll(&pane_id);
    assert!(
        offset > 0,
        "the OWNER's wheel must scroll the pane — otherwise the assertion above \
         cannot see a scroll at all (pane {pane_id} at offset {offset})"
    );
    println!("[observe] observer wheel ignored; owner wheel scrolled to offset {offset}");

    observer_tx.send_client_exited().ok();
    owner_tx.send_client_exited().ok();
    println!("[observe] PASS — an observer's wheel never scrolls the shared pane");
}
