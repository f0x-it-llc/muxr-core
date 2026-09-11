//! query — short-lived cli-client query helper (Phase C1).
//!
//! Zellij's own `zellij action list-panes/list-tabs` works by:
//! 1. Connect to the session socket (no `AttachClient` handshake).
//! 2. Send `ClientToServerMsg::Action { is_cli_client: true, … }`.
//! 3. Block until `ServerToClientMsg::Log { lines }` arrives.
//! 4. Disconnect.
//!
//! This module implements that flow so `GetLayout` can issue typed IPC queries
//! without holding a long-lived attachment.
//!
//! **Verified on the dev host (C1):** A bare `connect → Action(is_cli_client=true) →
//! recv` does NOT receive a Log reply in 0.45.1 — the server never delivers Log
//! to a client that has not completed the `AttachClient` handshake.  We therefore
//! do a minimal attach first (invisible `is_web_client=false` attach) before
//! sending the query action, then drain messages until `Log` or a terminal
//! `Exit`/`ClientExited`.
//!
//! _(The research note says it should work without attach; empirical testing shows
//! it does not — see C1 findings.)_

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use zellij_utils::input::actions::Action;
use zellij_utils::ipc::{ClientToServerMsg, IpcSenderWithContext, ServerToClientMsg};
use zellij_utils::pane_size::Size;

// Needed for `set_recv_timeout` on `interprocess::local_socket::Stream`.
// The method is part of the `Stream` trait in the interprocess crate; without
// this import the method is not in scope even though `ipc_connect` returns that
// type.
use interprocess::local_socket::prelude::*;

/// Neutral-large terminal size for ephemeral query/action AttachClients (FA).
///
/// zellij sizes a tab from the MINIMUM across the clients currently focused on
/// that tab (a per-tab minimum since 0.45.1, no longer session-wide). Ephemeral
/// query/action clients must therefore attach at a size LARGER than any real
/// client so they never become that minimum and shrink the tab they land on
/// (which, in single-pane mode, would shrink the fullscreened pane the phone
/// sees). Modest enough to avoid a giant grid allocation; larger than any phone
/// viewport.
pub const NEUTRAL_ATTACH_ROWS: u16 = 100;
pub const NEUTRAL_ATTACH_COLS: u16 = 320;

/// Maximum number of messages to drain while waiting for the Log reply.
const LOG_DRAIN_LIMIT: usize = 200;

/// How long to wait for a Log reply before giving up (per query).
const LOG_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-recv read timeout on the IPC socket used by `query_session`.
///
/// This bounds each individual `recv_server_msg()` call inside the drain loop
/// so the socket can never block indefinitely waiting for data that never
/// arrives (e.g. because the zellij server is hung or the session exited
/// without sending EOF).  The existing `LOG_TIMEOUT` deadline remains as a
/// higher-level backstop across all messages; `RECV_TIMEOUT` fires per-call
/// and causes `recv_server_msg` to return `None` (read error → treated as
/// stream-closed), which surfaces as a clear `Err` to callers rather than an
/// infinite hang.
///
/// Set conservatively at 5 s — well inside `LOG_TIMEOUT` so a single stalled
/// recv doesn't silently exhaust the outer deadline.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

// ─── Public API ───────────────────────────────────────────────────────────────

/// Issue a single query [`Action`] to a named session and return the `Log` lines.
///
/// Opens a short-lived IPC connection, performs a minimal `AttachClient`
/// handshake (required in 0.45.1), sends the action with `is_cli_client=true`,
/// drains messages until `ServerToClientMsg::Log` (success) or a terminal
/// condition (error).
///
/// This is **blocking** — call from a `tokio::task::spawn_blocking` context.
pub fn query_session(session: &str, action: Action) -> Result<Vec<String>> {
    // Defence in depth (Major G): validate before building a socket path.
    crate::ipc::validate_session_name(session).map_err(|e| anyhow!(e))?;
    let socket_path = session_socket_path(session);
    if !socket_path.exists() {
        bail!(
            "session socket {} does not exist — is '{}' running?",
            socket_path.display(),
            session
        );
    }

    // ── Open connection ──────────────────────────────────────────────────────
    let stream = zellij_utils::consts::ipc_connect(&socket_path)
        .with_context(|| format!("ipc_connect failed for {}", socket_path.display()))?;

    // Bound every recv so a stalled or hung session socket can't block forever.
    //
    // `set_recv_timeout` is on the `interprocess::local_socket::Stream` trait
    // (brought in via `interprocess::local_socket::prelude::*` above) and maps
    // to `setsockopt(SO_RCVTIMEO)` on the underlying `UnixStream`.  We set it
    // BEFORE wrapping in `IpcSenderWithContext` because:
    //   1. The wrapper erases the concrete type to `Box<dyn IpcStream>`, which
    //      does NOT expose `set_recv_timeout` — there is no way to reach it
    //      after wrapping.
    //   2. `IpcSenderWithContext::get_receiver` clones the socket via
    //      `try_clone_stream()` which calls `UnixStream::try_clone()` (dup).
    //      A dup'd fd shares the same socket and inherits socket-level options
    //      (including `SO_RCVTIMEO`), so the timeout applies to the receiver
    //      half as well.
    //
    // On timeout `recv_server_msg` returns an `io::Error(TimedOut)`, which the
    // zellij IPC layer maps to `None`; we treat `None` as "stream closed" and
    // bail with a clear error rather than spinning forever.
    if let Err(e) = stream.set_recv_timeout(Some(RECV_TIMEOUT)) {
        // Non-fatal: worst case we fall back to the LOG_TIMEOUT deadline.
        log::warn!("query_session: could not set socket recv timeout: {e}");
    }

    let mut sender: IpcSenderWithContext<ClientToServerMsg> = IpcSenderWithContext::new(stream);
    let mut receiver = sender.get_receiver::<ServerToClientMsg>();

    // ── Minimal AttachClient handshake ───────────────────────────────────────
    // In zellij 0.45.1 the server does not route a Log reply to clients that
    // have not completed the attach handshake.  We send a minimal invisible
    // attach (web_client=false) to register with the router, then immediately
    // send the query action.
    //
    // FA: attach at a NEUTRAL-LARGE size, NOT 24×80. zellij sizes a tab from the
    // MINIMUM terminal size across the clients currently focused on that tab
    // (per-tab since 0.45.1); a 24×80 ephemeral query client would transiently
    // shrink the tab it lands on (and, in single-pane mode, the fullscreened
    // pane tracks it → the phone sees a tiny pane). A large size keeps this
    // transient client from ever becoming that minimum, so it never perturbs
    // the real client's geometry.
    use zellij_utils::input::cli_assets::CliAssets;
    let cli_assets = CliAssets {
        terminal_window_size: Size {
            rows: NEUTRAL_ATTACH_ROWS as usize,
            cols: NEUTRAL_ATTACH_COLS as usize,
        },
        ..Default::default()
    };
    sender
        .send_client_msg(ClientToServerMsg::AttachClient {
            cli_assets,
            tab_position_to_focus: None,
            pane_to_focus: None,
            is_web_client: false,
        })
        .context("query: failed to send AttachClient")?;

    // ── Send query action ────────────────────────────────────────────────────
    sender
        .send_client_msg(ClientToServerMsg::Action {
            action,
            terminal_id: None,
            client_id: None,
            is_cli_client: true,
        })
        .context("query: failed to send Action")?;

    // ── Drain until Log ──────────────────────────────────────────────────────
    let deadline = std::time::Instant::now() + LOG_TIMEOUT;
    for i in 0..LOG_DRAIN_LIMIT {
        if std::time::Instant::now() > deadline {
            bail!(
                "query: timed out waiting for Log reply after {} messages",
                i
            );
        }
        let Some((msg, _ctx)) = receiver.recv_server_msg() else {
            bail!("query: IPC stream closed before Log reply arrived");
        };
        match msg {
            ServerToClientMsg::Log { lines } => {
                log::debug!(
                    "query: got Log({} lines) after {} messages",
                    lines.len(),
                    i + 1
                );
                return Ok(lines);
            }
            ServerToClientMsg::LogError { lines } => {
                bail!("query: server returned LogError: {:?}", lines);
            }
            ServerToClientMsg::Exit { exit_reason } => {
                bail!("query: session exited during query: {:?}", exit_reason);
            }
            // Drain everything else (Connected, Render, QueryTerminalSize, etc.)
            other => {
                log::trace!("query: draining message {i}: {other:?}");
            }
        }
    }
    bail!(
        "query: exceeded {} messages without receiving a Log reply",
        LOG_DRAIN_LIMIT
    )
}

/// Resolve the IPC socket path for a named session.
///
/// Delegates to [`crate::ipc::session_socket_path`] — one canonical implementation.
fn session_socket_path(session_name: &str) -> PathBuf {
    crate::ipc::session_socket_path(session_name)
}

// ─── Typed query helpers ─────────────────────────────────────────────────────

/// Query `ListTabs` for a session and return the JSON string (the single Log line).
///
/// Caller parses into `Vec<TabInfo>` using `serde_json::from_str`.
pub fn query_list_tabs_json(session: &str) -> Result<String> {
    let action = Action::ListTabs {
        show_state: true,
        show_dimensions: true,
        show_panes: false,
        show_layout: false,
        show_all: true,
        output_json: true,
    };
    let lines = query_session(session, action)
        .with_context(|| format!("query_list_tabs_json: session '{session}'"))?;
    // The JSON is a single line from the server.
    let json = lines.join("\n");
    if json.is_empty() {
        bail!("query_list_tabs_json: server returned empty Log for session '{session}'");
    }
    Ok(json)
}

/// Query `ListPanes` for a session and return the JSON string.
///
/// Caller parses into `Vec<PaneListEntry>` using `serde_json::from_str`.
pub fn query_list_panes_json(session: &str) -> Result<String> {
    let action = Action::ListPanes {
        show_tab: true,
        show_command: true,
        show_state: true,
        show_geometry: true,
        show_all: true,
        output_json: true,
    };
    let lines = query_session(session, action)
        .with_context(|| format!("query_list_panes_json: session '{session}'"))?;
    let json = lines.join("\n");
    if json.is_empty() {
        bail!("query_list_panes_json: server returned empty Log for session '{session}'");
    }
    Ok(json)
}

/// Query the **current display size** of a session (active tab's display area).
///
/// Currently UNUSED by any production path: the relay now passes every
/// attaching client's own rows/cols on both tiers, read-only included (see
/// `crate::relay::attach_relay`), so nothing calls this to pre-resolve a size
/// before attaching — only the [`crate::multiplexer::MuxBackend`] trait
/// declaration, its two backend impls, and test mocks reference it.
///
/// It exists because that was not always the policy: before the
/// read-only→explorer arc, a read-only observer attached with the session's
/// *current* size instead of its own (review round-2 Major A), reasoning from
/// `zellij-server/src/lib.rs`'s `min_client_terminal_size` (deleted in
/// 0.45.1, which stopped resizing the whole session to the minimum across
/// every attached client in favor of a per-tab recompute from only the
/// clients focused on that tab). It is retained on the trait as a
/// still-useful query — removing it is a `MuxBackend` API decision, not one
/// this comment makes.
///
/// Returns `(rows, cols)` taken from the **active** tab's
/// `display_area_rows`/`display_area_columns` (falling back to the first tab if
/// none is marked active) — reading per-tab dimensions is now more correct than
/// before, since tabs can legitimately differ in size. Blocking — call from
/// `spawn_blocking`.
pub fn query_session_size(session: &str) -> Result<(u16, u16)> {
    let json = query_list_tabs_json(session)
        .with_context(|| format!("query_session_size: session '{session}'"))?;

    use zellij_utils::data::ListTabsResponse;
    let tabs: ListTabsResponse = serde_json::from_str(&json)
        .with_context(|| format!("query_session_size: parse ListTabs JSON for '{session}'"))?;

    let tab = tabs
        .iter()
        .find(|t| t.active)
        .or_else(|| tabs.first())
        .ok_or_else(|| anyhow!("query_session_size: session '{session}' has no tabs"))?;

    // display_area_* is the shared session geometry (independent of any one
    // client's viewport).  Clamp into u16 and guard against a zero reading.
    let rows = clamp_usize_dim(tab.display_area_rows);
    let cols = clamp_usize_dim(tab.display_area_columns);
    if rows == 0 || cols == 0 {
        bail!(
            "query_session_size: session '{session}' reported zero display area \
             ({}x{})",
            tab.display_area_rows,
            tab.display_area_columns
        );
    }
    Ok((rows, cols))
}

/// Clamp a `usize` dimension from zellij into a `u16` (0 stays 0 → caller errors).
fn clamp_usize_dim(v: usize) -> u16 {
    v.min(u16::MAX as usize) as u16
}

/// Whether `pane` is a FLOATING pane in `session`. Floating panes cannot be
/// fullscreened (zellij's fullscreen toggle no-ops while floating panes are
/// visible), so the relay resizes them to fill instead — this tells the two
/// paths apart. Returns false if the pane is not found. Blocking — call from
/// `spawn_blocking`.
pub fn pane_is_floating(session: &str, pane: zellij_utils::data::PaneId) -> Result<bool> {
    let (is_floating, _, _) = pane_is_floating_with_visibility(session, pane)?;
    Ok(is_floating)
}

/// Whether `pane` is a FLOATING pane, whether floating panes are currently
/// visible in the active tab, and which floating pane (if any) is currently
/// focused — all derived from live zellij state in one call (two IPC queries,
/// one `spawn_blocking`).
///
/// Returns `(is_floating, are_floating_panes_visible, focused_floating_pane)`.
/// - `is_floating`: true if the target pane is a floating pane.
/// - `are_floating_panes_visible`: true if floating panes are currently shown in
///   the active tab (from `TabInfo.are_floating_panes_visible`). When no active
///   tab is found, falls back to false (safe: causes fill rather than hide, which
///   is the correct behaviour after an out-of-band hide).
/// - `focused_floating_pane`: the [`PaneId`] of the floating pane that has
///   `is_focused == true` in the active tab (from the same ListPanes data,
///   no additional IPC). `None` if no floating pane is focused.
///
/// The relay uses `focused_floating_pane` to gate the hide decision fully on
/// live zellij state — an out-of-band SHOW/HIDE of floating panes is therefore
/// reflected immediately on the next toggle, with no double-tap needed.
///
/// Blocking — call from `spawn_blocking`.
pub fn pane_is_floating_with_visibility(
    session: &str,
    pane: zellij_utils::data::PaneId,
) -> Result<(bool, bool, Option<zellij_utils::data::PaneId>)> {
    use zellij_utils::data::{ListPanesResponse, ListTabsResponse, PaneId};

    let (want_id, want_plugin) = match pane {
        PaneId::Terminal(id) => (id, false),
        PaneId::Plugin(id) => (id, true),
    };

    // ── IPC 1: pane list — determines is_floating + focused_floating_pane ───
    let panes_json = query_list_panes_json(session)?;
    let panes: ListPanesResponse = serde_json::from_str(&panes_json).with_context(|| {
        format!("pane_is_floating_with_visibility: parse ListPanes for '{session}'")
    })?;
    let is_floating = panes.iter().any(|entry| {
        let p = &entry.pane_info;
        p.id == want_id && p.is_plugin == want_plugin && p.is_floating
    });

    // ── IPC 2: tab list — determines are_floating_panes_visible + active tab ─
    // Only needed when the pane is floating; short-circuit for tiled panes to
    // avoid the extra IPC (the relay ignores the visibility and focused-floating
    // values for tiled panes).
    let (floating_visible, focused_floating) = if is_floating {
        let tabs_json = query_list_tabs_json(session)?;
        let tabs: ListTabsResponse = serde_json::from_str(&tabs_json).with_context(|| {
            format!("pane_is_floating_with_visibility: parse ListTabs for '{session}'")
        })?;
        let active_tab = tabs.iter().find(|t| t.active).or_else(|| tabs.first());
        let visible = active_tab
            .map(|t| t.are_floating_panes_visible)
            .unwrap_or(false);

        // Derive the focused floating pane id from the same ListPanes data
        // (no new IPC). We restrict to the active tab so we don't accidentally
        // pick up a focused pane in a background tab.
        let active_tab_id = active_tab.map(|t| t.position);
        let focused = active_tab_id.and_then(|tab_pos| {
            panes
                .iter()
                .find(|entry| {
                    entry.tab_position == tab_pos
                        && entry.pane_info.is_floating
                        && entry.pane_info.is_focused
                })
                .map(|entry| {
                    if entry.pane_info.is_plugin {
                        PaneId::Plugin(entry.pane_info.id)
                    } else {
                        PaneId::Terminal(entry.pane_info.id)
                    }
                })
        });

        (visible, focused)
    } else {
        (false, None)
    };

    Ok((is_floating, floating_visible, focused_floating))
}

/// Query `ListSessions` using the filesystem (no IPC needed) and return
/// live sessions with age.
///
/// Returns `(name, age_secs, resurrectable)`.
pub fn list_sessions_with_resurrectables() -> Result<Vec<(String, u64, bool)>> {
    let live = zellij_utils::sessions::get_sessions()
        .map_err(|kind| anyhow!("failed to list sessions: {kind:?}"))?;

    let live_names: std::collections::HashSet<String> =
        live.iter().map(|(n, _)| n.clone()).collect();

    let mut result: Vec<(String, u64, bool)> = live
        .into_iter()
        .map(|(name, dur)| (name, dur.as_secs(), false))
        .collect();

    // Append resurrectable sessions that are not also live.
    let resurrectable = zellij_utils::sessions::get_resurrectable_sessions();
    for (name, dur) in resurrectable {
        if !live_names.contains(&name) {
            result.push((name, dur.as_secs(), true));
        }
    }

    Ok(result)
}
