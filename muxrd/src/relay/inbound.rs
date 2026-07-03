//! Tokio inbound task: drives the gRPC ClientFrame stream → IPC sender.

use std::sync::Arc;

use futures::StreamExt;
use tonic::Streaming;

use tokio::sync::mpsc;

use crate::multiplexer::{FullscreenHint, MuxBackend, MuxMouseKind, MuxSender};
use crate::proto::{ClientFrame, MouseInput, MouseKind, client_frame};

use super::reader::ShutdownGuard;
use super::types::{
    ControlRegistry, FLOAT_QUERY_TIMEOUT, InFlightQuery, MAX_INPUT_FRAME_BYTES, QueryReply,
    QueryTx, RelayControl, TOKEN_RECHECK_INTERVAL, ViewStateRegistry,
};

// ─── inbound_loop ─────────────────────────────────────────────────────────────

/// Inbound loop — runs as a tokio task; owns the [`ShutdownGuard`] so the
/// reader thread is torn down when this returns (stream end or error).
///
/// Enforces two security invariants while the stream is live:
///
/// - **Major A (read-only gate):** when `read_only` is set, every inbound
///   input/resize frame is dropped (render-only) so a read-only token cannot
///   inject keystrokes or resize the session.  The gate also covers the two
///   geometry side-channels handled in [`attach_relay`] / [`ShutdownGuard`]:
///   the **attach-handshake size** (RO attaches use the session's current size,
///   never the client's) and the **teardown resize nudge** (suppressed for RO),
///   so a small read-only client can never shrink a writer's shared session.
/// - **Major H (token re-validation):** the bearer token is re-checked every
///   [`TOKEN_RECHECK_INTERVAL`]; on revocation/expiry/error the loop breaks,
///   dropping the guard and tearing the whole attach down.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn inbound_loop(
    mut inbound: Streaming<ClientFrame>,
    mut sender: Box<dyn MuxSender>,
    // The multiplexer backend — used for the hint-less `ToggleFullscreen`
    // fallback query (`pane_is_floating_with_visibility`). Cheap to clone (`Arc`).
    backend: Arc<dyn MuxBackend>,
    guard: ShutdownGuard,
    session: String,
    // Process-unique id minted at attach time; used as the registry key for
    // both `control` and `view_state` so concurrent relays on the same session
    // each own a distinct slot (fixes the multi-client misroute bug).
    connection_id: String,
    read_only: bool,
    // The grid this relay attached with (rows, cols) — updated by Resize
    // frames; bounds inbound mouse coordinates to this client's own viewport.
    attach_grid: (u16, u16),
    token: Option<String>,
    // Decrements the session's attached-client count when this task ends
    // (any exit path). Held only for its Drop; never read.
    _client_guard: crate::client_count::ClientGuard,
    // Control commands from the unary GoToTab/FocusPane RPCs, routed through
    // this rendering client (is_cli_client:false). Registry used for teardown
    // deregistration.
    mut control_rx: mpsc::UnboundedReceiver<RelayControl>,
    control: ControlRegistry,
    // Held for potential future sole-client gating; not required by the current
    // toggle logic (floating visibility queried live from zellij; tiled uses parity toggle).
    _clients: crate::client_count::SessionClients,
    // FX-QUERY: channel to the render thread carrying in-flight layout queries.
    // The QueryLayout arm hands the query off and returns — it never awaits.
    query_tx: QueryTx,
    // B-FOCUS: per-connection relay view state registry.
    view_state: ViewStateRegistry,
) {
    // The guard lives for the body of this task; on any exit path its Drop
    // signals + joins the reader thread.
    let _guard = guard;

    // FX-QUERY: monotonic sequence id stamped on each layout query so the render
    // thread can order/replace and so logs are correlatable.
    let mut next_query_seq: u64 = 0;

    // Note: the floating fill-vs-hide decision is derived fully from live zellij
    // state (`focused_floating` + `floating_visible` from the ListPanes/ListTabs
    // query below) — there is no in-process fullscreen/fill tracker (M4 fix).

    let (mut grid_rows, mut grid_cols) = attach_grid;

    let mut recheck = tokio::time::interval(TOKEN_RECHECK_INTERVAL);
    // The first tick fires immediately; skip re-validating right after the
    // layer already validated at open.
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    recheck.tick().await;

    loop {
        tokio::select! {
            // ── Major H: periodic bearer re-validation ───────────────────────
            _ = recheck.tick() => {
                if !revalidate_token(token.as_deref(), &session).await {
                    log::warn!(
                        "relay inbound [{session}]: token no longer valid — tearing down stream"
                    );
                    break;
                }
            }

            // ── Control commands routed through this rendering client ──────────
            // Each arm forwards one action as this client (is_cli_client:false)
            // so the tab/pane switch / fullscreen toggle targets the rendering
            // client deterministically.
            cmd = control_rx.recv() => { match cmd {
                Some(RelayControl::SwitchTab(tab_id)) => {
                    log::trace!("relay inbound [{session}]: SwitchTab({tab_id})");
                    // Read-only: no action is sent AND view_state is left
                    // untouched. That is correct — a RO client never moves this
                    // relay client's active tab, so its tracked active_tab must
                    // not change (overriding it would make get_layout report a tab
                    // switch that never happened). B-FOCUS state is only updated
                    // on the RW path below, after the action is actually sent.
                    if !read_only {
                        if let Err(e) = sender.go_to_tab(tab_id) {
                            log::warn!("relay inbound [{session}]: SwitchTab send failed: {e:#}");
                        } else {
                            // Update relay view state: active tab is now tab_id.
                            // focused_pane becomes None (we don't know which pane
                            // is focused in the new tab until a FocusPane follows).
                            // Key by connection_id (unique per relay) so concurrent
                            // relays on the same session each update their own slot.
                            if let Some(mut entry) = view_state.get_mut(&connection_id) {
                                entry.state.active_tab = Some(tab_id);
                                entry.state.focused_pane = None;
                            }
                        }
                    }
                }
                Some(RelayControl::FocusPane(pane)) => {
                    if read_only {
                        log::trace!(
                            "relay inbound [{session}]: dropping FocusPane (read-only token)"
                        );
                    } else if let Err(e) = sender.focus_pane(pane) {
                        log::warn!("relay inbound [{session}]: FocusPane send failed: {e:#}");
                    } else {
                        // B-FOCUS: track focused pane for this relay client.
                        // Key by connection_id so concurrent relays each update their own slot.
                        if let Some(mut entry) = view_state.get_mut(&connection_id) {
                            entry.state.focused_pane = Some(pane);
                        }
                    }
                }
                Some(RelayControl::SwitchSpace { workspace_id, reply }) => {
                    // herdr Spaces (Option A; Decision 2 — per-connection view).
                    // Mutating like SwitchTab (read-only drops it) + replied like
                    // QueryLayout (the gRPC handler awaits the ack before refreshing
                    // layout). switch_space re-points THIS relay's wire stream at the
                    // target space's focused pane via the same blocking control path
                    // go_to_tab uses (bounded by HerdrControl's 3 s per-call timeout),
                    // with NO daemon-global workspace.focus.
                    if read_only {
                        log::trace!(
                            "relay inbound [{session}]: dropping SwitchSpace (read-only token)"
                        );
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "read-only token: SwitchSpace is not allowed"
                        )));
                    } else {
                        let result = sender.switch_space(&workspace_id);
                        match &result {
                            Ok(()) => {
                                // New space → this relay's tracked active tab and
                                // focused pane are now unknown until the next
                                // GetLayout/FocusPane. Reset both so get_layout does
                                // not apply a stale override from the old space.
                                // Record the new workspace_id as this relay's current
                                // space so the gRPC GetSpaces handler can mark it
                                // connection-active (the daemon-global focus is left
                                // untouched, so the backend-reported active won't
                                // reflect this per-connection switch).
                                if let Some(mut entry) = view_state.get_mut(&connection_id) {
                                    entry.state.active_tab = None;
                                    entry.state.focused_pane = None;
                                    entry.state.current_space = Some(workspace_id.clone());
                                }
                            }
                            Err(e) => log::warn!(
                                "relay inbound [{session}]: SwitchSpace('{workspace_id}') \
                                 failed: {e:#}"
                            ),
                        }
                        let _ = reply.send(result);
                    }
                }
                Some(RelayControl::ToggleFullscreen { pane, hint }) => {
                    if read_only {
                        log::trace!(
                            "relay inbound [{session}]: dropping ToggleFullscreen (read-only token)"
                        );
                    } else {
                        // Resolve the floating context: (is_floating,
                        // floating_visible, is_focused_floating).
                        //
                        // Bug 2c: prefer the CLIENT HINT — the mobile client
                        // already polls all three, so a hint lets us skip a
                        // synchronous IPC query on this select-loop hot path
                        // (the query stalled input forwarding + the bearer
                        // recheck for a whole round trip, and spawned yet another
                        // ephemeral client on the shared session).
                        //
                        // FALLBACK (no hint — keyboard-driven / hint-less
                        // callers): a live query through the backend so an
                        // out-of-band SHOW/HIDE is reflected immediately (M4
                        // behaviour). HANG FIX (BE-HANG) preserved: the blocking
                        // query runs in spawn_blocking wrapped in
                        // tokio::time::timeout so a stalled socket can never wedge
                        // the loop; on timeout we skip the toggle rather than hang.
                        let resolved = match hint {
                            Some(h) => FullscreenHint {
                                is_floating: h.target_is_floating,
                                floating_visible: h.floating_visible,
                                is_focused_floating: h.target_is_focused_floating,
                            },
                            None => {
                                let backend = backend.clone();
                                let s = session.clone();
                                let query_fut = tokio::task::spawn_blocking(move || {
                                    backend.pane_is_floating_with_visibility(&s, pane)
                                });
                                match tokio::time::timeout(FLOAT_QUERY_TIMEOUT, query_fut).await {
                                    Ok(join_result) => {
                                        let (f, v, focused) = join_result
                                            .unwrap_or(Ok((false, false, None)))
                                            .unwrap_or((false, false, None));
                                        FullscreenHint {
                                            is_floating: f,
                                            floating_visible: v,
                                            is_focused_floating: focused == Some(pane),
                                        }
                                    }
                                    Err(_elapsed) => {
                                        log::warn!(
                                            "relay inbound [{session}]: floating-pane query \
                                             timed out after {FLOAT_QUERY_TIMEOUT:?} — \
                                             skipping ToggleFullscreen to avoid wedging the loop"
                                        );
                                        // Degrade: skip the toggle rather than hang.
                                        // The user can retry; the session is not frozen.
                                        continue;
                                    }
                                }
                            }
                        };

                        // The fill-vs-hide-vs-tiled action sequence is
                        // backend-specific and lives behind `toggle_fullscreen`
                        // (the zellij impl in `multiplexer::zellij`). The relay
                        // updates its OWN view state from the SAME resolved hint:
                        //   - hide path (floating, visible, focused) → focus is
                        //     handed back to an untracked tiled pane → None;
                        //   - fill path / tiled path → this client now focuses
                        //     `pane` → Some(pane).
                        let is_hide = resolved.is_floating
                            && resolved.floating_visible
                            && resolved.is_focused_floating;
                        if let Err(e) = sender.toggle_fullscreen(pane, resolved) {
                            log::warn!(
                                "relay inbound [{session}]: ToggleFullscreen failed: {e:#}"
                            );
                        } else {
                            // Key by connection_id so concurrent relays each update their own slot.
                            if let Some(mut entry) = view_state.get_mut(&connection_id) {
                                entry.state.focused_pane = if is_hide { None } else { Some(pane) };
                            }
                        }
                    }
                }

                // B-QUERY (BE-LAYOUT; FX-QUERY redesign): route a layout query
                // over this relay's existing persistent connection. This
                // eliminates the ephemeral AttachClient that query_session opens
                // for each GetLayout poll, stopping both the per-client focus/tab
                // union pollution and the pane-frame flicker caused by
                // attach/detach churn.
                //
                // CRITICAL (FX-QUERY): this arm NEVER awaits. The render thread
                // exclusively owns recv() and so is the only place a Log can be
                // seen — so it also owns reply-fulfillment. We:
                //   1. stamp a monotonic seq,
                //   2. hand InFlightQuery { seq, reply, … } to the render thread,
                //   3. send ListTabs THEN ListPanes,
                //   4. return immediately.
                // The render thread captures the two Logs (tabs then panes) and
                // fulfills `reply`. The single timeout bound is RELAY_QUERY_TIMEOUT
                // in grpc.rs; on timeout it drops the receiver and the render
                // thread retires the slot. Awaiting here would block input
                // forwarding + the bearer recheck for up to the full query budget
                // — exactly the select-loop block this redesign removes.
                Some(RelayControl::QueryLayout { reply }) => {
                    let seq = next_query_seq;
                    next_query_seq = next_query_seq.wrapping_add(1);
                    handle_query_layout(&mut *sender, &query_tx, reply, seq, &session);
                }

                None => {
                    // All senders dropped — registry entry already gone or being
                    // replaced; nothing to route. (Loop continues on other arms.)
                }
            } }

            // ── Inbound client frames ────────────────────────────────────────
            next = inbound.next() => match next {
                Some(Ok(frame)) => match frame.kind {
                    Some(client_frame::Kind::Input(bytes)) => {
                        // Major A: read-only tokens may observe but not inject.
                        if read_only {
                            log::trace!(
                                "relay inbound [{session}]: dropping input frame (read-only token)"
                            );
                        } else if bytes.len() > MAX_INPUT_FRAME_BYTES {
                            // Round-2 minor: cap per-frame input size (matches the
                            // WriteToPane cap) so one frame can't push an
                            // unbounded write into the session IPC channel.
                            log::warn!(
                                "relay inbound [{session}]: dropping oversized input frame \
                                 ({} bytes > {MAX_INPUT_FRAME_BYTES} byte limit)",
                                bytes.len()
                            );
                        } else if let Err(e) = forward_input(&mut *sender, bytes) {
                            log::warn!("relay inbound [{session}]: input send failed: {e:#}");
                        }
                    }
                    Some(client_frame::Kind::Resize(r)) => {
                        // Major A: resize is a mutating control too — drop for RO.
                        if read_only {
                            log::trace!(
                                "relay inbound [{session}]: dropping resize frame (read-only token)"
                            );
                        } else {
                            let rows = super::clamp_dim(r.rows, 24);
                            let cols = super::clamp_dim(r.cols, 80);
                            if let Err(e) = sender.send_resize(rows, cols) {
                                log::warn!("relay inbound [{session}]: resize send failed: {e:#}");
                            } else {
                                // Track the new grid for mouse bound checks.
                                grid_rows = rows;
                                grid_cols = cols;
                            }
                        }
                    }
                    Some(client_frame::Kind::Mouse(m)) => {
                        handle_mouse_frame(
                            &mut *sender,
                            &m,
                            read_only,
                            grid_rows,
                            grid_cols,
                            &session,
                        );
                    }
                    Some(client_frame::Kind::Attach(_)) => {
                        log::warn!(
                            "relay inbound [{session}]: unexpected second AttachReq — ignoring"
                        );
                    }
                    None => {
                        log::warn!("relay inbound [{session}]: ClientFrame with no kind — ignoring");
                    }
                },
                Some(Err(e)) => {
                    log::info!("relay inbound [{session}]: stream error (client gone): {e}");
                    break;
                }
                None => {
                    log::info!("relay inbound [{session}]: stream ended (client detached)");
                    break;
                }
            }
        }
    }
    // Deregister this relay's control channel + view state so stale unary RPCs /
    // GetLayouts stop routing here.
    //
    // Because entries are keyed by connection_id (process-unique per relay),
    // removing by connection_id is always safe: we can ONLY ever remove our OWN
    // entry — a newer attach for the same session has a DIFFERENT connection_id
    // and therefore a different key. The old `same_channel` guard (which was
    // needed when keys were session-keyed and last-attach-wins could overwrite) is
    // no longer necessary for correctness, but we keep a remove_if for the view
    // state as an extra safety belt: if for any reason the entry was already
    // removed (e.g. by an explicit deregistration path in the future), the remove
    // is a harmless no-op.
    let ctrl_removed = control.remove(&connection_id);
    let vs_removed = view_state.remove(&connection_id);
    if ctrl_removed.is_some() || vs_removed.is_some() {
        log::debug!(
            "relay inbound [{session}] connection_id={connection_id}: teardown — \
             removed registry entries"
        );
    } else {
        log::debug!(
            "relay inbound [{session}] connection_id={connection_id}: teardown — \
             registry entries already absent (no-op)"
        );
    }
    // _guard drops here → reader thread shutdown.
}

// ─── Token re-validation ──────────────────────────────────────────────────────

/// Re-validate the attach's bearer token (Major H).  Runs the blocking SQLite
/// check on the blocking pool.  Returns `true` only if the token is still
/// present and unexpired; `false` (revoke the stream) on absence, invalidity,
/// or any error — fail closed.
async fn revalidate_token(token: Option<&str>, session: &str) -> bool {
    let Some(token) = token else {
        log::warn!("relay inbound [{session}]: no token to re-validate → failing closed");
        return false;
    };
    let token = token.to_owned();
    match tokio::task::spawn_blocking(move || crate::ipc::validate_session_token(&token)).await {
        Ok(Ok(valid)) => valid,
        Ok(Err(e)) => {
            log::warn!("relay [{session}]: token re-validation DB error (failing closed): {e}");
            false
        }
        Err(e) => {
            log::warn!(
                "relay [{session}]: token re-validation task panicked (failing closed): {e}"
            );
            false
        }
    }
}

// ─── Input forwarding ────────────────────────────────────────────────────────

/// Forward raw input bytes to the focused pane.
///
/// UTF-8 text goes via `send_input_chars` (the A2-proven `WriteChars` path);
/// non-UTF-8 byte sequences (e.g. raw ESC) go via `send_input_bytes` (`Write`).
fn forward_input(sender: &mut dyn MuxSender, bytes: Vec<u8>) -> anyhow::Result<()> {
    match String::from_utf8(bytes) {
        Ok(text) => sender.send_input_chars(&text),
        Err(e) => sender.send_input_bytes(e.into_bytes()),
    }
}

// ─── Layout-query dispatch (P2.00 A-1) ─────────────────────────────────────────

/// Dispatch a relay-routed `QueryLayout`. **Never awaits and never blocks the
/// inbound task** — runs inline on the inbound `select!` arm and returns
/// immediately, so it can never wedge input forwarding or the bearer recheck.
///
/// Two paths, chosen by the backend's [`MuxSender::has_sync_layout`] predicate
/// (B1 — the predicate is cheap and side-effect-free; we can't "peek" by calling
/// `query_layout_result`, since that call performs the blocking query):
///
/// 1. **Out-of-band fast-path** (`has_sync_layout() == true`, herdr) — the backend
///    answers layout over a separate control socket via
///    [`MuxSender::query_layout_result`]. That is blocking local-socket I/O
///    (`2 + N_tabs` round-trips), so we do NOT run it inline: we `box_clone` the
///    sender and run `query_layout_result` on the **blocking pool**
///    (`spawn_blocking`), fulfilling `reply` from there. We do NOT arm an in-flight
///    query nor fire any wire actions, and the arm returns immediately — the
///    inbound select loop never stalls on the query. The existing
///    `RELAY_QUERY_TIMEOUT` in `grpc.rs` bounds the `reply` wait (herdr completes
///    in ms, so it is never hit).
///
/// 2. **In-band path** (`has_sync_layout() == false`, zellij — the default) — the
///    existing zellij flow, byte-identical: hand an [`InFlightQuery`] to the render
///    thread BEFORE firing the actions (so the first `Log` can't arrive before the
///    slot is armed), then fire ListTabs THEN ListPanes over the neutral sender and
///    return. The render thread (sole owner of `recv()`) pairs the two `Log`
///    replies and fulfills `reply`; the single timeout bound stays
///    `RELAY_QUERY_TIMEOUT` in `grpc.rs`.
fn handle_query_layout(
    sender: &mut dyn MuxSender,
    query_tx: &QueryTx,
    reply: QueryReply,
    seq: u64,
    session: &str,
) {
    // B1: out-of-band fast-path. A sync-layout backend (herdr) answers layout via
    // blocking local-socket I/O — keep it OFF the inbound task by running it on the
    // blocking pool. We branch on the cheap predicate (not on the query result)
    // because `query_layout_result` *does* the blocking work.
    if sender.has_sync_layout() {
        log::debug!(
            "relay inbound [{session}]: QueryLayout seq={seq} → sync-layout backend; \
             dispatching query_layout_result on the blocking pool (no in-flight arm)"
        );
        // `box_clone` only needs `&self`; the clone is `Send + 'static`, so it (and
        // the `reply` oneshot) move into the spawn_blocking closure. The query only
        // touches the cloned control Arc; for herdr the dup'd wire fd is unused.
        let mut q = sender.box_clone();
        tokio::task::spawn_blocking(move || {
            let res = q.query_layout_result().unwrap_or_else(|| {
                Err(anyhow::anyhow!(
                    "query_layout_result returned None for a sync-layout backend"
                ))
            });
            // Receiver may already be gone (grpc RELAY_QUERY_TIMEOUT dropped it) —
            // a closed-channel send is a harmless no-op.
            let _ = reply.send(res);
        });
        return;
    }

    log::debug!("relay inbound [{session}]: QueryLayout seq={seq} requested (in-band path)");

    // In-band (zellij) path — unchanged. Hand the query to the render thread
    // BEFORE sending the actions, so the first Log can't arrive before the render
    // thread has the slot armed. (Even if it momentarily does, the post-recv
    // drain in render_loop picks it up; arming first is the simpler ordering.)
    let in_flight = InFlightQuery {
        seq,
        reply,
        tabs: None,
    };
    if let Err(returned) = query_tx.send(in_flight) {
        // Render thread is gone; reply via the sender we get back so grpc falls
        // back to the ephemeral path.
        log::warn!(
            "relay inbound [{session}]: QueryLayout seq={seq}: render thread \
             gone (query_tx send failed)"
        );
        let _ = returned
            .0
            .reply
            .send(Err(anyhow::anyhow!("render thread not available")));
        return;
    }

    // Fire the layout query (ListTabs THEN ListPanes) over the neutral sender.
    // The InFlightQuery is already owned by the render thread; if a send fails to
    // produce a Log, its reply cancels via RELAY_QUERY_TIMEOUT / close detection.
    // We just log — we can't reach `reply` from here anymore.
    if let Err(e) = sender.query_layout() {
        log::warn!(
            "relay inbound [{session}]: QueryLayout seq={seq}: query_layout send \
             failed (render thread will retire the query): {e:#}"
        );
        return;
    }
    log::trace!(
        "relay inbound [{session}]: QueryLayout seq={seq} dispatched \
         (render thread will fulfill)"
    );
}

// ─── Mouse forwarding ────────────────────────────────────────────────────────

/// Handle one inbound [`MouseInput`] frame:
///
/// 1. **Major A gate** — mouse events reach application ptys (a wheel can
///    mutate app state, e.g. wheel-driven selection), so read-only tokens may
///    observe but never inject. RO clients fall back to the `ScrollPane` RPC,
///    which the server deliberately permits for read-only tokens.
/// 2. **Viewport bound check** — coordinates must lie inside THIS relay's own
///    grid (attach size, updated by Resize frames); anything outside is a
///    client bug or abuse and is dropped with a warning.
/// 3. Forward via [`MuxSender::send_mouse`] (as-self routing — the event lands
///    in this rendering client's viewport, never a co-attached client's).
fn handle_mouse_frame(
    sender: &mut dyn MuxSender,
    m: &MouseInput,
    read_only: bool,
    grid_rows: u16,
    grid_cols: u16,
    session: &str,
) {
    if read_only {
        log::trace!("relay inbound [{session}]: dropping mouse frame (read-only token)");
        return;
    }
    if m.col >= u32::from(grid_cols) || m.row >= u32::from(grid_rows) {
        log::warn!(
            "relay inbound [{session}]: dropping out-of-grid mouse frame \
             (col={}, row={}) for {grid_rows}x{grid_cols} grid",
            m.col,
            m.row,
        );
        return;
    }
    let kind = match m.kind() {
        MouseKind::WheelUp => MuxMouseKind::WheelUp,
        MouseKind::WheelDown => MuxMouseKind::WheelDown,
    };
    if let Err(e) = sender.send_mouse(kind, m.col as u16, m.row as u16) {
        log::warn!("relay inbound [{session}]: mouse send failed: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::{FullscreenHint, LayoutSnapshot, MuxMouseKind, PaneRef, TabSnapshot};
    use std::sync::{Arc, Mutex};

    /// A configurable [`MuxSender`] fake for the `QueryLayout` dispatch tests.
    ///
    /// `has_sync` is the backend predicate the dispatch branches on; `sync` (taken
    /// once, shared across `box_clone`s) is what `query_layout_result` returns;
    /// `wire_fired` records whether the in-band `query_layout()` wire action was
    /// sent; `cloned` records whether `box_clone` was called (the sync path clones
    /// the sender to move it onto the blocking pool).
    struct FakeSender {
        has_sync: bool,
        sync: Arc<Mutex<Option<anyhow::Result<LayoutSnapshot>>>>,
        wire_fired: Arc<Mutex<bool>>,
        cloned: Arc<Mutex<bool>>,
        /// Every `send_mouse` call, recorded for the mouse-frame gate tests.
        mouse_events: Arc<Mutex<Vec<(MuxMouseKind, u16, u16)>>>,
    }

    impl FakeSender {
        /// A minimal sender for tests that only care about `send_mouse`.
        fn plain() -> Self {
            FakeSender {
                has_sync: false,
                sync: Arc::new(Mutex::new(None)),
                wire_fired: Arc::new(Mutex::new(false)),
                cloned: Arc::new(Mutex::new(false)),
                mouse_events: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl MuxSender for FakeSender {
        fn has_sync_layout(&self) -> bool {
            self.has_sync
        }
        fn query_layout_result(&mut self) -> Option<anyhow::Result<LayoutSnapshot>> {
            self.sync.lock().unwrap().take()
        }
        fn query_layout(&mut self) -> anyhow::Result<()> {
            *self.wire_fired.lock().unwrap() = true;
            Ok(())
        }
        fn go_to_tab(&mut self, _tab_id: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn focus_pane(&mut self, _pane: PaneRef) -> anyhow::Result<()> {
            Ok(())
        }
        fn toggle_fullscreen(&mut self, _p: PaneRef, _h: FullscreenHint) -> anyhow::Result<()> {
            Ok(())
        }
        fn send_input_chars(&mut self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn send_input_bytes(&mut self, _bytes: Vec<u8>) -> anyhow::Result<()> {
            Ok(())
        }
        fn send_mouse(&mut self, kind: MuxMouseKind, col: u16, row: u16) -> anyhow::Result<()> {
            self.mouse_events.lock().unwrap().push((kind, col, row));
            Ok(())
        }
        fn send_resize(&mut self, _rows: u16, _cols: u16) -> anyhow::Result<()> {
            Ok(())
        }
        fn send_client_exited(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        fn box_clone(&self) -> Box<dyn MuxSender> {
            // Sync-layout (herdr) path clones the sender to move it onto the
            // blocking pool; the clone shares the `sync` slot so the cloned
            // sender's `query_layout_result` returns the configured snapshot.
            *self.cloned.lock().unwrap() = true;
            Box::new(FakeSender {
                has_sync: self.has_sync,
                sync: self.sync.clone(),
                wire_fired: self.wire_fired.clone(),
                cloned: self.cloned.clone(),
                mouse_events: self.mouse_events.clone(),
            })
        }
    }

    fn a_snapshot() -> LayoutSnapshot {
        LayoutSnapshot {
            tabs: vec![TabSnapshot {
                tab_id: 1,
                position: 0,
                name: "main".into(),
                active: true,
                has_bell: false,
                panes_to_hide: 0,
                fullscreen_active: false,
                floating_panes_visible: false,
                panes: vec![],
            }],
        }
    }

    /// B1 out-of-band fast-path (`has_sync_layout() == true`, herdr): the dispatch
    /// runs `query_layout_result` on the blocking pool (via `box_clone` +
    /// `spawn_blocking`) and fulfills the reply from there — NO in-flight query is
    /// armed and NO wire action is fired. Needs a runtime for `spawn_blocking`.
    #[tokio::test]
    async fn sync_layout_fulfills_via_spawn_blocking_without_wire_actions() {
        let wire_fired = Arc::new(Mutex::new(false));
        let cloned = Arc::new(Mutex::new(false));
        let mut sender = FakeSender {
            has_sync: true,
            sync: Arc::new(Mutex::new(Some(Ok(a_snapshot())))),
            wire_fired: wire_fired.clone(),
            cloned: cloned.clone(),
            mouse_events: Arc::new(Mutex::new(Vec::new())),
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let (query_tx, query_rx) = std::sync::mpsc::channel::<InFlightQuery>();

        handle_query_layout(&mut sender, &query_tx, reply_tx, 0, "s");

        // The reply is fulfilled from the blocking pool — await it.
        let snap = reply_rx
            .await
            .expect("reply sent from spawn_blocking")
            .expect("ok snapshot");
        assert_eq!(snap.tabs.len(), 1);
        // The sender was cloned to move it onto the blocking pool.
        assert!(
            *cloned.lock().unwrap(),
            "sync-layout path must box_clone the sender for spawn_blocking"
        );
        // No wire action fired and no in-flight query armed.
        assert!(
            !*wire_fired.lock().unwrap(),
            "fast-path must not fire the in-band query_layout() wire action"
        );
        assert!(
            query_rx.try_recv().is_err(),
            "fast-path must not hand an InFlightQuery to the render thread"
        );
    }

    /// In-band (zellij) path (`has_sync_layout() == false`): arms an in-flight
    /// query AND fires the wire action; the reply stays pending (the render thread
    /// fulfills it later). Byte-identical to the pre-B1 behaviour — no
    /// `query_layout_result` call, no thread-hop.
    #[test]
    fn query_layout_result_none_arms_in_flight_and_fires_wire() {
        let wire_fired = Arc::new(Mutex::new(false));
        let mut sender = FakeSender {
            has_sync: false,
            sync: Arc::new(Mutex::new(None)),
            wire_fired: wire_fired.clone(),
            cloned: Arc::new(Mutex::new(false)),
            mouse_events: Arc::new(Mutex::new(Vec::new())),
        };
        let (reply_tx, mut reply_rx) =
            tokio::sync::oneshot::channel::<anyhow::Result<LayoutSnapshot>>();
        let (query_tx, query_rx) = std::sync::mpsc::channel::<InFlightQuery>();

        handle_query_layout(&mut sender, &query_tx, reply_tx, 42, "s");

        // An InFlightQuery was handed to the render thread, carrying our seq.
        let q = query_rx.try_recv().expect("InFlightQuery armed");
        assert_eq!(q.seq, 42);
        assert!(q.tabs.is_none(), "fresh in-flight query has no tabs yet");
        // The wire action was fired.
        assert!(
            *wire_fired.lock().unwrap(),
            "in-band path must fire the query_layout() wire action"
        );
        // The reply is owned by the render thread now (moved into InFlightQuery),
        // so the inbound side never fulfilled it directly.
        assert!(
            matches!(
                reply_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "reply stays pending — the render thread fulfills it from the Log pair"
        );
        drop(q); // keeps the moved reply alive until here
    }

    // ─── handle_mouse_frame gate tests ───────────────────────────────────────

    #[test]
    fn mouse_frame_forwards_within_grid() {
        let mut sender = FakeSender::plain();
        let events = sender.mouse_events.clone();
        let m = MouseInput {
            kind: MouseKind::WheelUp as i32,
            col: 10,
            row: 5,
        };
        handle_mouse_frame(&mut sender, &m, false, 24, 80, "s");
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &[(MuxMouseKind::WheelUp, 10, 5)]
        );
    }

    #[test]
    fn mouse_frame_maps_wheel_down() {
        let mut sender = FakeSender::plain();
        let events = sender.mouse_events.clone();
        let m = MouseInput {
            kind: MouseKind::WheelDown as i32,
            col: 0,
            row: 0,
        };
        handle_mouse_frame(&mut sender, &m, false, 24, 80, "s");
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &[(MuxMouseKind::WheelDown, 0, 0)]
        );
    }

    #[test]
    fn mouse_frame_dropped_for_read_only_token() {
        // Major A: wheel events reach app ptys → read-only must never inject.
        let mut sender = FakeSender::plain();
        let events = sender.mouse_events.clone();
        let m = MouseInput {
            kind: MouseKind::WheelUp as i32,
            col: 1,
            row: 1,
        };
        handle_mouse_frame(&mut sender, &m, true, 24, 80, "s");
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn mouse_frame_dropped_outside_grid() {
        // col == grid_cols and row == grid_rows are both out of range
        // (coordinates are zero-based).
        let mut sender = FakeSender::plain();
        let events = sender.mouse_events.clone();
        for (col, row) in [(80u32, 0u32), (0, 24), (9999, 9999)] {
            let m = MouseInput {
                kind: MouseKind::WheelUp as i32,
                col,
                row,
            };
            handle_mouse_frame(&mut sender, &m, false, 24, 80, "s");
        }
        assert!(events.lock().unwrap().is_empty());
    }
}
