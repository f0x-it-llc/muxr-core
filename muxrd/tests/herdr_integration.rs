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
//! | `smoke_create_query_kill`     | `create_session` / `query_layout` / `kill_session` round-trip |
//! | `smoke_open_attach_render_input` | `open_attach` → read `Render` frames → send input → teardown |
//!
//! ## AGPL note
//!
//! These tests drive herdr solely through its public Unix-domain sockets
//! (the JSON-API control socket and the binary wire relay socket).  herdr runs
//! as a separate, unmodified, user-installed binary; no herdr source is linked.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use muxrd::multiplexer::{HerdrBackend, MuxBackend, MuxServerMsg};

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
                let pts = std::fs::read_link(format!("/proc/{pid}/fd/0"))
                    .expect("readlink pane pty");
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
            now[idx], SMALL,
            "newly focused pane (tab {}) should be at the small size",
            idx + 1
        );
        assert_eq!(
            now[prev_idx], baseline[prev_idx],
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
