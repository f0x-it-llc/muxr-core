//! zellij — the [`MuxBackend`] implementation backed by a live zellij server
//! (P1.01).
//!
//! This is the ONLY file in the `multiplexer` module that imports `zellij_utils`.
//! Every method delegates to the existing `crate::{ipc, actions, query}` free
//! functions, translating neutral ↔ zellij at the boundary:
//!
//! - [`PaneRef`] ↔ `zellij_utils::data::PaneId` via
//!   [`crate::actions::pane_id_from_target`] and the reverse helper here.
//! - [`ResizeKind`]/[`ResizeDir`] → `zellij_utils::data::{Resize, Direction}`.
//! - the `ListTabs`/`ListPanes` JSON → a neutral [`LayoutSnapshot`] via
//!   [`parse_zellij_layout`], which both the ephemeral query and the relay-routed
//!   `grpc/layout.rs` path reuse (single deserialization point).
//! - `ServerToClientMsg` → [`MuxServerMsg`] in [`ZellijMuxReceiver::recv`].

use std::time::Duration;

use anyhow::{Result, anyhow};

use zellij_utils::data::{Direction, PaneId, Resize};
use zellij_utils::input::actions::Action;
use zellij_utils::ipc::ServerToClientMsg;

use crate::ipc::{AttachHandle, AttachReceiver, AttachSender};
use crate::{actions, ipc, query};

use super::types::{
    ActionAck, FullscreenHint, LayoutSnapshot, MuxEvent, MuxMouseKind, MuxServerMsg,
    PaneFullscreenContext, PaneRef, PaneSnapshot, ResizeDir, ResizeKind, ScrollDir, TabSnapshot,
};
use super::{DualHandle, MuxBackend, MuxReceiver, MuxSender};

// ─── Neutral ↔ zellij helpers ───────────────────────────────────────────────────

/// `PaneRef` → zellij `PaneId`.
fn to_pane_id(pane: PaneRef) -> PaneId {
    actions::pane_id_from_target(pane.id, pane.is_plugin)
}

/// zellij `PaneId` → `PaneRef`.
fn from_pane_id(id: PaneId) -> PaneRef {
    match id {
        PaneId::Terminal(id) => PaneRef::terminal(id),
        PaneId::Plugin(id) => PaneRef::plugin(id),
    }
}

/// Neutral `ResizeKind` → zellij `Resize`.
fn to_resize(kind: ResizeKind) -> Resize {
    match kind {
        ResizeKind::Increase => Resize::Increase,
        ResizeKind::Decrease => Resize::Decrease,
    }
}

/// Neutral `ResizeDir` → zellij `Direction`.
fn to_direction(dir: ResizeDir) -> Direction {
    match dir {
        ResizeDir::Left => Direction::Left,
        ResizeDir::Right => Direction::Right,
        ResizeDir::Up => Direction::Up,
        ResizeDir::Down => Direction::Down,
    }
}

// ─── Layout parse (the single zellij-JSON → snapshot conversion) ──────────────────

/// Parse zellij `ListTabs` + `ListPanes` JSON into a neutral [`LayoutSnapshot`].
///
/// **This is THE single zellij-JSON → snapshot parse in the codebase** (P1.03).
/// Two call sites share it, so both layout paths produce a byte-identical
/// snapshot:
///
/// - [`ZellijBackend::query_layout`] — the ephemeral path (queries the two JSON
///   payloads over short-lived connections, then parses here).
/// - `grpc/layout.rs` — the relay-routed path, which parses the two JSON strings
///   the relay captures from its persistent connection's `Log` replies (retiring
///   the former `parse_relay_json` duplicate).
///
/// Plugin panes are **included** in the snapshot (raw layout); the gRPC layer
/// applies the client-visibility filter at the proto-build stage.
///
/// **Tolerance.** The `ListTabs` decode goes through
/// [`query::parse_list_tabs_json`], so a tab object without `position` parses
/// (its array index stands in). A pair that arrives SWAPPED — a `ListPanes`
/// array in the tabs slot and a `ListTabs` array in the panes slot — is
/// recognised by shape and decoded the right way round: the relay's render
/// thread pairs untagged `Log` replies in arrival order, and a query replaced
/// mid-capture leaves its panes reply to land in the next query's tabs slot
/// (the device-gate `missing field position at line 31 column 3` was exactly a
/// pretty-printed `ListPanes` array whose first entry is a plugin pane). Any
/// other parse failure logs the first 200 bytes of each payload at `warn` so a
/// future shape change is diagnosable.
pub(crate) fn parse_zellij_layout(tabs_json: &str, panes_json: &str) -> Result<LayoutSnapshot> {
    use std::collections::HashMap;
    use zellij_utils::data::{ListPanesResponse, ListTabsResponse};

    let (tabs, panes): (ListTabsResponse, ListPanesResponse) =
        match query::parse_list_tabs_json(tabs_json) {
            Ok(tabs) => {
                let panes = serde_json::from_str(panes_json).map_err(|e| {
                    log::warn!(
                        "parse_zellij_layout: ListPanes JSON failed to parse ({e}); first 200 \
                         bytes: {:?}",
                        json_head(panes_json)
                    );
                    anyhow!("parse_zellij_layout: parse ListPanes JSON: {e}")
                })?;
                (tabs, panes)
            }
            Err(tabs_err) => {
                let swapped = query::parse_list_tabs_json(panes_json)
                    .ok()
                    .and_then(|tabs| {
                        serde_json::from_str::<ListPanesResponse>(tabs_json)
                            .ok()
                            .map(|panes| (tabs, panes))
                    });
                match swapped {
                    Some(pair) => {
                        log::warn!(
                            "parse_zellij_layout: tabs/panes payloads arrived swapped \
                             (ListPanes in the tabs slot) — decoded them the right way round"
                        );
                        pair
                    }
                    None => {
                        log::warn!(
                            "parse_zellij_layout: ListTabs JSON failed to parse ({tabs_err}); \
                             first 200 bytes: {:?}",
                            json_head(tabs_json)
                        );
                        return Err(anyhow!(
                            "parse_zellij_layout: parse ListTabs JSON: {tabs_err}"
                        ));
                    }
                }
            }
        };

    // Group panes by tab_id, preserving ListPanes order.
    let mut panes_by_tab: HashMap<usize, Vec<PaneSnapshot>> = HashMap::new();
    for entry in panes {
        let p = &entry.pane_info;
        let pane = PaneSnapshot {
            id: p.id,
            title: p.title.clone(),
            is_focused: p.is_focused,
            is_floating: p.is_floating,
            exited: p.exited,
            command: entry.pane_command.unwrap_or_default(),
            cwd: entry.pane_cwd.unwrap_or_default(),
            x: p.pane_x as u32,
            y: p.pane_y as u32,
            rows: p.pane_rows as u32,
            cols: p.pane_columns as u32,
            is_plugin: p.is_plugin,
            is_fullscreen: p.is_fullscreen,
            agent: None,
        };
        panes_by_tab.entry(entry.tab_id).or_default().push(pane);
    }

    let tab_snaps: Vec<TabSnapshot> = tabs
        .into_iter()
        .map(|tab| TabSnapshot {
            tab_id: tab.tab_id as u64,
            position: tab.position as u32,
            name: tab.name.clone(),
            active: tab.active,
            has_bell: tab.has_bell_notification,
            panes_to_hide: tab.panes_to_hide as u32,
            fullscreen_active: tab.is_fullscreen_active,
            floating_panes_visible: tab.are_floating_panes_visible,
            panes: panes_by_tab.remove(&tab.tab_id).unwrap_or_default(),
        })
        .collect();

    Ok(LayoutSnapshot { tabs: tab_snaps })
}

/// The first 200 bytes of a JSON payload (cut back to a char boundary), for
/// parse-failure diagnostics.
fn json_head(json: &str) -> &str {
    let mut end = json.len().min(200);
    while !json.is_char_boundary(end) {
        end -= 1;
    }
    &json[..end]
}

// ─── Fullscreen action sequence ─────────────────────────────────────────────────

/// The zellij actions the relay sends — as its own rendering client — for a
/// fullscreen toggle on `pane_id` with the resolved context `hint`.
///
/// Pure, so the sequence is unit-testable; [`ZellijMuxSender::toggle_fullscreen`]
/// sends exactly this list, in order. The relay has already refused a target
/// that does not exist or is a HIDDEN floating pane, so neither reaches here.
///
/// **Exit-status hazard (zellij 0.45.1).** `route_action` (zellij-server
/// `route.rs`) passes the sending connection as `cli_client_id` even for a
/// rendering client (`is_cli_client:false`), and answers an action that
/// completes with an exit status but no message with `ServerToClientMsg::Exit`
/// — which the relay reader treats as the session ending, dropping the attach.
/// `ShowFloatingPanes` / `HideFloatingPanes` always complete that way
/// (`Tab::show_floating_panes_atomic` / `hide_floating_panes_atomic`: 0 on a
/// change, 2 on a no-op, 1 with no selectable floating pane), so this sequence
/// never sends either. The actions it does send report through a message
/// (`FocusPaneByPaneId`: `LogError` for an unknown or already-focused pane) or
/// not at all (`ToggleFloatingPanes`, `ChangeFloatingPaneCoordinates`,
/// `ToggleFocusNoUiFullscreen`).
///
/// **Tab of the relay client.** `ToggleFloatingPanes` and
/// `ToggleFocusNoUiFullscreen` act on the relay client's ACTIVE tab (zellij
/// `screen.rs` `active_tab_and_connected_client_id`), whatever pane the caller
/// named; the relay therefore resolves to the hide and exit paths only after
/// placing the target on its own tab. `FocusPaneByPaneId` switches the client
/// to the target's tab itself, so the fill and enter paths need no such gate;
/// the fill path skips that focus move when the target is already the relay's
/// tracked focus ([`FullscreenHint::target_is_relay_focus`]).
///
/// **Active-pane entry.** `ToggleFocusNoUiFullscreen` reads the relay client's
/// entry in the tab's per-client active-pane map and is a no-op without one;
/// `FocusPaneByPaneId` is what inserts it. Both tiled paths therefore always
/// lead with the focus move — on exit aimed at the relay's own fullscreen pane
/// (`pane_id` is that anchor, not the caller's target), which zellij refocuses
/// without unsetting fullscreen.
fn fullscreen_actions(pane_id: PaneId, hint: FullscreenHint) -> Vec<Action> {
    use zellij_utils::data::FloatingPaneCoordinates;
    use zellij_utils::input::layout::PercentOrFixed;

    if hint.is_floating {
        if hint.floating_visible && hint.is_focused_floating {
            // Hide path: the target is the visible, focused floating pane, so
            // hide the floating layer to give the tiled panes back.
            // `ToggleFloatingPanes` hides a visible layer without an exit status
            // (`HideFloatingPanes` would answer `Exit 0` and drop the relay).
            vec![Action::ToggleFloatingPanes]
        } else {
            // Fill path: a VISIBLE, unfocused floating pane is resized to cover
            // the display (borderless, 100%×100%). This is a resize of the
            // floating pane's coordinates, NOT zellij's fullscreen state
            // (`TabInfo.is_fullscreen_active` stays false, and a second toggle
            // on the now-focused pane takes the hide path, not an exit). The
            // floating layer is already visible
            // (a hidden one is refused upstream), so no `ShowFloatingPanes`: on a
            // visible layer it completes with exit status 2 → `Exit 2`.
            let coordinates = FloatingPaneCoordinates {
                x: Some(PercentOrFixed::Percent(0)),
                y: Some(PercentOrFixed::Percent(0)),
                width: Some(PercentOrFixed::Percent(100)),
                height: Some(PercentOrFixed::Percent(100)),
                pinned: None,
                borderless: Some(true),
            };
            // No focus move when the target already holds the relay's focus.
            let mut actions = Vec::with_capacity(2);
            if !hint.target_is_relay_focus {
                actions.push(Action::FocusPaneByPaneId { pane_id });
            }
            actions.push(Action::ChangeFloatingPaneCoordinates {
                pane_id,
                coordinates,
            });
            actions
        }
    } else if hint.tab_fullscreen {
        // Exit path: the tab already has a fullscreen pane. The relay passes
        // its OWN fullscreen pane as `pane_id` (never the caller's target):
        // refocusing the fullscreen pane itself does not unset fullscreen
        // (zellij `tiled_panes::focus_pane` unsets only for a pane in
        // `panes_to_hide`), but it DOES insert this client's entry in the tab's
        // per-client active-pane map — and `ToggleFocusNoUiFullscreen` is a
        // no-op for a client without one (`toggle_active_pane_no_ui_fullscreen`
        // reads `active_panes.get(client)`). A bare toggle therefore left the
        // phone stuck in fullscreen on the device; focus first, then toggle.
        vec![
            Action::FocusPaneByPaneId { pane_id },
            Action::ToggleFocusNoUiFullscreen,
        ]
    } else {
        // Enter path: focus then active-pane NO-UI fullscreen toggle (parity with
        // `actions::toggle_pane_fullscreen`). No-UI covers the tab-bar/status-bar
        // plugin panes too and drops the pane frame, so the pane owns the whole
        // display rather than the viewport between the bars. The focus move is
        // never skipped, even when the target already holds the relay's focus:
        // it is what guarantees this client's active-pane entry exists for the
        // toggle (see the exit path above).
        vec![
            Action::FocusPaneByPaneId { pane_id },
            Action::ToggleFocusNoUiFullscreen,
        ]
    }
}

// ─── ZellijBackend ──────────────────────────────────────────────────────────────

/// The zellij-backed [`MuxBackend`]. Stateless: every call opens its own
/// short-lived IPC connection (or, for `open_attach`, a persistent one) exactly
/// as the existing free functions do.
#[derive(Debug, Default)]
pub struct ZellijBackend;

impl MuxBackend for ZellijBackend {
    // ── Session lifecycle ───────────────────────────────────────────────────

    fn list_sessions(&self) -> Result<Vec<(String, Duration)>> {
        ipc::list_sessions()
    }

    fn list_sessions_with_resurrectables(&self) -> Result<Vec<(String, u64, bool)>> {
        query::list_sessions_with_resurrectables()
    }

    fn validate_session_name(&self, name: &str) -> std::result::Result<(), String> {
        ipc::validate_session_name(name)
    }

    fn create_session(&self, name: &str, layout: Option<String>) -> Result<ActionAck> {
        actions::create_session(name, layout)
    }

    fn kill_session(&self, session: &str) -> Result<ActionAck> {
        // zellij has no logical refusal for a kill: the session either accepts the
        // direct KillSession IPC message or its socket is gone, which is a genuine
        // transport failure and stays an `Err`. The ack shape exists for herdr,
        // which can decline (worktree-group primary, last remaining space).
        actions::kill_session(session)?;
        Ok(ActionAck {
            ok: true,
            error: None,
            info: None,
        })
    }

    fn rename_session(&self, session: &str, new_name: String) -> Result<ActionAck> {
        actions::rename_session(session, new_name)
    }

    // ── Ephemeral control actions ───────────────────────────────────────────

    fn write_to_pane(&self, session: &str, pane: PaneRef, bytes: Vec<u8>) -> Result<ActionAck> {
        actions::write_to_pane(session, to_pane_id(pane), bytes)
    }

    fn focus_pane(&self, session: &str, pane: PaneRef) -> Result<ActionAck> {
        actions::focus_pane(session, to_pane_id(pane))
    }

    fn close_pane(&self, session: &str, pane: PaneRef) -> Result<ActionAck> {
        actions::close_pane(session, to_pane_id(pane))
    }

    fn new_pane(&self, session: &str, floating: bool, name: Option<String>) -> Result<ActionAck> {
        actions::new_pane(session, floating, name)
    }

    fn rename_pane(&self, session: &str, pane: PaneRef, name: String) -> Result<ActionAck> {
        actions::rename_pane(session, to_pane_id(pane), name)
    }

    fn resize_pane(
        &self,
        session: &str,
        pane: PaneRef,
        kind: ResizeKind,
        dir: Option<ResizeDir>,
    ) -> Result<ActionAck> {
        actions::resize_pane(
            session,
            to_pane_id(pane),
            to_resize(kind),
            dir.map(to_direction),
        )
    }

    fn toggle_pane_floating(&self, session: &str, pane: PaneRef) -> Result<ActionAck> {
        actions::toggle_pane_floating(session, to_pane_id(pane))
    }

    fn toggle_pane_fullscreen(&self, session: &str, pane: PaneRef) -> Result<ActionAck> {
        actions::toggle_pane_fullscreen(session, to_pane_id(pane))
    }

    fn scroll_pane(&self, session: &str, pane: PaneRef, dir: ScrollDir) -> Result<ActionAck> {
        actions::scroll_pane(session, to_pane_id(pane), dir)
    }

    fn new_tab(&self, session: &str, name: Option<String>) -> Result<ActionAck> {
        actions::new_tab(session, name)
    }

    fn close_tab(&self, session: &str, tab_id: u64) -> Result<ActionAck> {
        actions::close_tab(session, tab_id)
    }

    fn go_to_tab(&self, session: &str, tab_id: u64) -> Result<ActionAck> {
        actions::go_to_tab(session, tab_id)
    }

    fn rename_tab(&self, session: &str, tab_id: u64, name: String) -> Result<ActionAck> {
        actions::rename_tab(session, tab_id, name)
    }

    // ── Read-only queries ───────────────────────────────────────────────────

    fn query_layout(&self, session: &str) -> Result<LayoutSnapshot> {
        // Query the two JSON payloads over ephemeral connections, then parse via
        // the SINGLE zellij-JSON → snapshot parse ([`parse_zellij_layout`]). The
        // relay-routed path (`grpc/layout.rs`) reuses that same parse on the JSON
        // strings it captures from its persistent connection — so there is now
        // exactly one such deserialization in the codebase (P1.03).
        let tabs_json = query::query_list_tabs_json(session)?;
        let panes_json = query::query_list_panes_json(session)?;
        parse_zellij_layout(&tabs_json, &panes_json)
    }

    fn query_session_size(&self, session: &str) -> Result<(u16, u16)> {
        query::query_session_size(session)
    }

    fn pane_is_floating_with_visibility(
        &self,
        session: &str,
        pane: PaneRef,
    ) -> Result<(bool, bool, Option<PaneRef>)> {
        let (is_floating, visible, focused) =
            query::pane_is_floating_with_visibility(session, to_pane_id(pane))?;
        Ok((is_floating, visible, focused.map(from_pane_id)))
    }

    fn pane_fullscreen_context(
        &self,
        session: &str,
        pane: PaneRef,
        relay_focus: Option<PaneRef>,
    ) -> Result<PaneFullscreenContext> {
        // One ListTabs + ListPanes read through the single layout parse; the
        // pane's OWN tab supplies the floating visibility, focused floating pane
        // and the tab-level `is_fullscreen_active` (not per-client, unlike
        // `PaneInfo.is_fullscreen`); the same read locates the relay's tracked
        // focused pane, which tells the relay which tab IT has active.
        let snapshot = self.query_layout(session)?;
        Ok(PaneFullscreenContext::from_snapshot(
            &snapshot,
            pane,
            relay_focus,
        ))
    }

    // ── Attach (the relay seam) ─────────────────────────────────────────────

    fn open_attach(
        &self,
        session: &str,
        rows: u16,
        cols: u16,
        read_only: bool,
    ) -> Result<DualHandle> {
        // For the zellij backend the AttachClient open is identical regardless
        // of `read_only`: the relay passes THIS caller's own rows/cols on both
        // tiers (floored at MIN_TERMINAL_ROWS/MIN_TERMINAL_COLS — see
        // `crate::relay::attach_relay`), and the read-only teardown nudge lives
        // in the ShutdownGuard (send_client_exited vs send_resize). We log the
        // mode for traceability.
        log::debug!(
            "ZellijBackend::open_attach session='{session}' {rows}x{cols} read_only={read_only}"
        );
        let handle = AttachHandle::open(session, rows, cols)?;
        let session_name = handle.session_name.clone();
        let (sender, receiver) = handle.split();
        Ok(DualHandle {
            sender: Box::new(ZellijMuxSender(sender)),
            receiver: Box::new(ZellijMuxReceiver(receiver)),
            session_name,
            // zellij has no per-connection resume view: the stream is the whole
            // composited session and follows the server's own focus, so the
            // `AttachReq.resume_*` hints are ignored (see the trait default of
            // `MuxBackend::open_attach_with_resume`).
            resumed_view: None,
        })
    }

    // ── Backend identity ────────────────────────────────────────────────────

    fn backend_version(&self) -> String {
        zellij_utils::consts::VERSION.to_string()
    }
}

// ─── ZellijMuxSender ────────────────────────────────────────────────────────────

/// [`MuxSender`] newtype translating neutral sends to `AttachSender` calls,
/// always as **this** rendering client (`is_cli_client:false`).
struct ZellijMuxSender(AttachSender);

impl MuxSender for ZellijMuxSender {
    fn go_to_tab(&mut self, tab_id: u64) -> Result<()> {
        self.0
            .send_action_as_self(Action::GoToTabById { id: tab_id })
    }

    fn focus_pane(&mut self, pane: PaneRef) -> Result<()> {
        self.0.send_action_as_self(Action::FocusPaneByPaneId {
            pane_id: to_pane_id(pane),
        })
    }

    fn toggle_fullscreen(&mut self, pane: PaneRef, hint: FullscreenHint) -> Result<()> {
        // The hide / fill / exit / enter action sequence owned by this backend
        // ([`fullscreen_actions`] documents each arm and why none of them may
        // use Show/HideFloatingPanes). The relay drives this via
        // `MuxSender::toggle_fullscreen` after refusing an unknown or hidden
        // floating target; the former inline copies in
        // `relay/helpers.rs`/`relay/inbound.rs` are retired.
        for action in fullscreen_actions(to_pane_id(pane), hint) {
            log::info!("zellij relay: fullscreen sequence → {action:?}");
            self.0.send_action_as_self(action)?;
        }
        Ok(())
    }

    fn query_layout(&mut self) -> Result<()> {
        // Fire ListTabs THEN ListPanes; the two Log replies arrive in that order
        // on the receiver and the relay reader pairs them (tabs then panes).
        self.0.send_action_as_self(Action::ListTabs {
            show_state: true,
            show_dimensions: true,
            show_panes: false,
            show_layout: false,
            show_all: true,
            output_json: true,
        })?;
        self.0.send_action_as_self(Action::ListPanes {
            show_tab: true,
            show_command: true,
            show_state: true,
            show_geometry: true,
            show_all: true,
            output_json: true,
        })?;
        Ok(())
    }

    fn send_input_chars(&mut self, text: &str) -> Result<()> {
        self.0.send_chars(text)
    }

    fn send_mouse(&mut self, kind: MuxMouseKind, col: u16, row: u16) -> Result<()> {
        use zellij_utils::input::mouse::{MouseEvent, MouseEventType};
        use zellij_utils::position::Position;
        // Wheel notches are Press events with the wheel flag set and no
        // Release following — the exact encoding the real zellij client
        // produces (zellij-client input_handler.rs). Position is
        // (line, column), zero-based. Sent as-self on the persistent relay
        // connection, so the server resolves the target pane from THIS
        // client's own focus (`send_action_as_self` routes by this
        // connection's client_id, not `get_last_active_client()`) — the
        // useful property this relies on. But the resulting scroll (or
        // mouse-capture forward) lands on that pane's SHARED viewport, which
        // every other client currently watching the same pane also sees —
        // it is NOT scoped to this connection. Measured, not inferred: a
        // pane seeded with 400 numbered lines, scrolled here with 40
        // wheel-up notches, moved a third-party `zellij action dump-screen`
        // observer's view from lines 354-400 to 272-281 (~120 lines,
        // matching zellij's ~3 lines/notch).
        let mut event = MouseEvent::new();
        event.event_type = MouseEventType::Press;
        event.position = Position::new(row as i32, col);
        match kind {
            MuxMouseKind::WheelUp => event.wheel_up = true,
            MuxMouseKind::WheelDown => event.wheel_down = true,
        }
        self.0.send_action_as_self(Action::MouseEvent { event })
    }

    fn send_input_bytes(&mut self, bytes: Vec<u8>) -> Result<()> {
        self.0.send_bytes(bytes)
    }

    fn send_resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.0.send_resize(rows, cols)
    }

    fn send_client_exited(&mut self) -> Result<()> {
        self.0.send_client_exited()
    }

    fn box_clone(&self) -> Box<dyn MuxSender> {
        Box::new(ZellijMuxSender(self.0.try_clone()))
    }
}

// ─── ZellijMuxReceiver ──────────────────────────────────────────────────────────

/// [`MuxReceiver`] newtype mapping `ServerToClientMsg` → [`MuxServerMsg`] 1:1.
struct ZellijMuxReceiver(AttachReceiver);

impl MuxReceiver for ZellijMuxReceiver {
    fn recv(&mut self) -> Option<MuxServerMsg> {
        let msg = self.0.recv()?;
        Some(match msg {
            ServerToClientMsg::Render { content } => MuxServerMsg::Render(content.into_bytes()),
            ServerToClientMsg::Log { lines } => MuxServerMsg::Log(lines.join("\n")),
            ServerToClientMsg::Exit { exit_reason } => MuxServerMsg::Event(MuxEvent::Exit {
                reason: exit_reason.to_string(),
            }),
            ServerToClientMsg::RenamedSession { name } => {
                MuxServerMsg::Event(MuxEvent::RenamedSession { name })
            }
            ServerToClientMsg::ConfigFileUpdated => MuxServerMsg::Event(MuxEvent::ConfigUpdated),
            ServerToClientMsg::SwitchSession { connect_to_session } => {
                MuxServerMsg::Event(MuxEvent::SwitchSession {
                    name: connect_to_session.name.unwrap_or_default(),
                })
            }
            // An action's error reply: surfaced in the log, no remote semantics.
            // Expected on every fullscreen exit (`FocusPaneByPaneId` on the
            // relay's own, already-focused pane answers "already focused"), so
            // info rather than warn.
            ServerToClientMsg::LogError { lines } => {
                log::info!("zellij relay: LogError: {}", lines.join(" | "));
                MuxServerMsg::Other
            }
            // UnblockInputThread / Connected / QueryTerminalSize /
            // PaneRenderUpdate / … — no remote-client semantics; the relay drains
            // them while preserving its per-message loop cadence.
            _ => MuxServerMsg::Other,
        })
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_version_is_non_empty() {
        let b = ZellijBackend;
        assert!(!b.backend_version().is_empty());
    }

    #[test]
    fn validate_session_name_accepts_simple_and_rejects_traversal() {
        let b = ZellijBackend;
        assert!(b.validate_session_name("dev").is_ok());
        assert!(b.validate_session_name("../foo").is_err());
        assert!(b.validate_session_name("foo/bar").is_err());
    }

    #[test]
    fn pane_ref_round_trips_through_zellij_pane_id() {
        assert_eq!(
            from_pane_id(to_pane_id(PaneRef::terminal(5))),
            PaneRef::terminal(5)
        );
        assert_eq!(
            from_pane_id(to_pane_id(PaneRef::plugin(8))),
            PaneRef::plugin(8)
        );
        assert_eq!(to_pane_id(PaneRef::terminal(5)), PaneId::Terminal(5));
        assert_eq!(to_pane_id(PaneRef::plugin(8)), PaneId::Plugin(8));
    }

    // ─── ListTabs / ListPanes fixtures ───────────────────────────────────────

    use zellij_utils::data::{PaneInfo, PaneListEntry, TabInfo};

    /// A tab exactly as zellij 0.45.1 serializes it (`format_tabs_as_json` is
    /// `serde_json::to_string_pretty` over `TabInfo`).
    fn tab_info(tab_id: usize, position: usize, fullscreen: bool, floating: bool) -> TabInfo {
        TabInfo {
            position,
            name: format!("tab-{tab_id}"),
            active: position == 0,
            is_fullscreen_active: fullscreen,
            are_floating_panes_visible: floating,
            display_area_rows: 50,
            display_area_columns: 200,
            tab_id,
            ..Default::default()
        }
    }

    /// A `ListPanes` entry as zellij serializes it. A plugin pane carries no
    /// `pane_command`/`pane_cwd` (`skip_serializing_if`), which is what makes the
    /// first entry of a real payload end at line 31.
    fn pane_entry(id: u32, tab_id: usize, floating: bool, focused: bool) -> PaneListEntry {
        PaneListEntry {
            pane_info: PaneInfo {
                id,
                is_floating: floating,
                is_focused: focused,
                title: format!("pane-{id}"),
                is_selectable: true,
                ..Default::default()
            },
            tab_id,
            tab_position: tab_id,
            tab_name: format!("tab-{tab_id}"),
            pane_command: None,
            pane_cwd: None,
        }
    }

    /// `[tab 0, tab 1]` pretty-printed WITHOUT `position` — the 0.45.1 shape the
    /// device gate's error message named.
    fn tabs_json_without_position() -> String {
        let tabs = vec![tab_info(0, 0, false, false), tab_info(1, 1, true, false)];
        let mut value = serde_json::to_value(&tabs).unwrap();
        for tab in value.as_array_mut().unwrap() {
            tab.as_object_mut().unwrap().remove("position");
        }
        serde_json::to_string_pretty(&value).unwrap()
    }

    fn panes_json(entries: &[PaneListEntry]) -> String {
        serde_json::to_string_pretty(entries).unwrap()
    }

    #[test]
    fn list_tabs_without_position_parses_with_array_index() {
        let tabs_json = tabs_json_without_position();
        assert!(
            !tabs_json.contains("\"position\""),
            "fixture must omit position"
        );
        let panes = panes_json(&[pane_entry(1, 0, false, true), pane_entry(2, 1, false, true)]);

        let snapshot = parse_zellij_layout(&tabs_json, &panes).expect("tolerant parse");
        assert_eq!(snapshot.tabs.len(), 2);
        assert_eq!(snapshot.tabs[0].position, 0);
        assert_eq!(snapshot.tabs[1].position, 1);
        assert_eq!(snapshot.tabs[1].tab_id, 1);
        assert!(snapshot.tabs[1].fullscreen_active);
        assert_eq!(snapshot.tabs[1].panes.len(), 1);
        assert_eq!(snapshot.tabs[1].panes[0].id, 2);
    }

    #[test]
    fn list_tabs_with_position_keeps_the_reported_value() {
        let tabs = serde_json::to_string_pretty(&vec![tab_info(7, 3, false, false)]).unwrap();
        let snapshot = parse_zellij_layout(&tabs, "[]").expect("parse");
        assert_eq!(snapshot.tabs[0].position, 3);
        assert_eq!(snapshot.tabs[0].tab_id, 7);
    }

    /// The device-gate payload: a `ListPanes` array (first entry a plugin pane
    /// with no command/cwd) in the tabs slot fails at exactly line 31 column 3
    /// on the strict decode — proof of what the relay had paired.
    #[test]
    fn panes_payload_in_tabs_slot_matches_the_device_gate_error() {
        let mut plugin = pane_entry(0, 0, false, false);
        plugin.pane_info.is_plugin = true;
        let panes = panes_json(&[plugin, pane_entry(1, 0, false, true)]);
        let err = serde_json::from_str::<zellij_utils::data::ListTabsResponse>(&panes)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "missing field `position` at line 31 column 3");
    }

    /// A swapped pair — the replaced query's panes Log landed in the next
    /// query's tabs slot — is decoded the right way round instead of falling
    /// back to the ephemeral path.
    #[test]
    fn swapped_tabs_and_panes_payloads_are_recovered() {
        let tabs = serde_json::to_string_pretty(&vec![tab_info(0, 0, false, false)]).unwrap();
        let mut plugin = pane_entry(9, 0, false, false);
        plugin.pane_info.is_plugin = true;
        let panes = panes_json(&[plugin, pane_entry(1, 0, false, true)]);

        let snapshot = parse_zellij_layout(&panes, &tabs).expect("swapped pair recovered");
        assert_eq!(snapshot.tabs.len(), 1);
        assert_eq!(snapshot.tabs[0].panes.len(), 2);
    }

    #[test]
    fn unparseable_tabs_payload_still_fails() {
        let err = parse_zellij_layout("{\"not\": \"an array\"}", "[]").unwrap_err();
        assert!(
            err.to_string().contains("parse ListTabs JSON"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn json_head_caps_at_200_bytes_on_a_char_boundary() {
        let long = "é".repeat(150); // 300 bytes, 2 per char
        let head = json_head(&long);
        assert_eq!(head.len(), 200);
        assert_eq!(json_head("[]"), "[]");
        let odd = format!("x{}", "é".repeat(150)); // boundary at 199
        assert_eq!(json_head(&odd).len(), 199);
    }

    // ─── Pane fullscreen context (from the parsed layout) ────────────────────

    fn context_for(
        tabs: &[TabInfo],
        panes: &[PaneListEntry],
        pane: PaneRef,
    ) -> PaneFullscreenContext {
        context_with_focus(tabs, panes, pane, None)
    }

    fn context_with_focus(
        tabs: &[TabInfo],
        panes: &[PaneListEntry],
        pane: PaneRef,
        relay_focus: Option<PaneRef>,
    ) -> PaneFullscreenContext {
        let tabs_json = serde_json::to_string_pretty(tabs).unwrap();
        let snapshot = parse_zellij_layout(&tabs_json, &panes_json(panes)).unwrap();
        PaneFullscreenContext::from_snapshot(&snapshot, pane, relay_focus)
    }

    #[test]
    fn context_reads_the_tab_that_holds_the_pane() {
        // Tab 0 is the active one but NOT fullscreen; the target lives in tab 1,
        // which is fullscreen — the context must come from tab 1.
        let tabs = [tab_info(0, 0, false, true), tab_info(1, 1, true, false)];
        let panes = [
            pane_entry(1, 0, false, true),
            pane_entry(2, 1, false, false),
        ];
        let ctx = context_for(&tabs, &panes, PaneRef::terminal(2));
        assert!(ctx.exists);
        assert!(!ctx.is_floating);
        assert!(ctx.tab_fullscreen);
        assert!(!ctx.floating_visible);
        assert_eq!(ctx.tab_id, Some(1));
        assert!(!ctx.tab_active, "tab_info marks only position 0 active");
        assert_eq!(ctx.relay_focus_tab_id, None);
    }

    /// The same read locates the relay's tracked focused pane, so the relay
    /// can tell whether the target is on ITS active tab.
    #[test]
    fn context_locates_the_relay_focus_tab() {
        let tabs = [tab_info(0, 0, false, false), tab_info(1, 1, true, false)];
        let panes = [
            pane_entry(1, 0, false, true),
            pane_entry(2, 1, false, false),
        ];
        let ctx = context_with_focus(
            &tabs,
            &panes,
            PaneRef::terminal(2),
            Some(PaneRef::terminal(1)),
        );
        assert_eq!(ctx.tab_id, Some(1));
        assert_eq!(ctx.relay_focus_tab_id, Some(0));

        // A focus pane that no longer exists locates nothing.
        let gone = context_with_focus(
            &tabs,
            &panes,
            PaneRef::terminal(2),
            Some(PaneRef::terminal(99)),
        );
        assert_eq!(gone.relay_focus_tab_id, None);
    }

    #[test]
    fn context_reports_hidden_floating_and_unknown_panes() {
        let tabs = [tab_info(0, 0, false, false)];
        let panes = [
            pane_entry(1, 0, false, true),
            pane_entry(27, 0, true, false),
        ];
        let hidden = context_for(&tabs, &panes, PaneRef::terminal(27));
        assert!(hidden.exists && hidden.is_floating && !hidden.floating_visible);

        let unknown = context_for(&tabs, &panes, PaneRef::terminal(20));
        assert_eq!(unknown, PaneFullscreenContext::not_found());

        // Same id as a terminal, but a plugin ref: not the same pane.
        let plugin = context_for(&tabs, &panes, PaneRef::plugin(27));
        assert!(!plugin.exists);
    }

    #[test]
    fn context_reports_the_focused_visible_floating_pane() {
        let tabs = [tab_info(0, 0, false, true)];
        let panes = [pane_entry(1, 0, false, false), pane_entry(5, 0, true, true)];
        let ctx = context_for(&tabs, &panes, PaneRef::terminal(5));
        assert!(ctx.is_floating && ctx.floating_visible);
        assert_eq!(ctx.focused_floating, Some(PaneRef::terminal(5)));
    }

    // ─── Fullscreen action sequence ──────────────────────────────────────────

    fn hint(floating: bool, visible: bool, focused: bool, tab_fullscreen: bool) -> FullscreenHint {
        FullscreenHint {
            is_floating: floating,
            floating_visible: visible,
            is_focused_floating: focused,
            tab_fullscreen,
            target_is_relay_focus: false,
        }
    }

    /// [`hint`] for a target that is already the relay's tracked focus.
    fn focused_hint(floating: bool, visible: bool, focused: bool) -> FullscreenHint {
        FullscreenHint {
            target_is_relay_focus: true,
            ..hint(floating, visible, focused, false)
        }
    }

    /// Target already the relay's focus → the tiled sequences STILL lead with
    /// `FocusPaneByPaneId` (it is what guarantees this client's active-pane
    /// entry for the toggle); only the fill and hide paths never move focus.
    #[test]
    fn target_already_focused_still_focuses_before_the_toggle() {
        let focus_then_toggle = vec![
            Action::FocusPaneByPaneId {
                pane_id: PaneId::Terminal(3),
            },
            Action::ToggleFocusNoUiFullscreen,
        ];
        let enter = fullscreen_actions(PaneId::Terminal(3), focused_hint(false, false, false));
        assert_eq!(enter, focus_then_toggle);

        let fill = fullscreen_actions(PaneId::Terminal(5), focused_hint(true, true, false));
        assert_eq!(fill.len(), 1, "{fill:?}");
        assert!(
            matches!(
                fill[0],
                Action::ChangeFloatingPaneCoordinates {
                    pane_id: PaneId::Terminal(5),
                    ..
                }
            ),
            "{fill:?}"
        );

        // Hide never moves focus, flag or not.
        assert_eq!(
            fullscreen_actions(PaneId::Terminal(5), focused_hint(true, true, true)),
            vec![Action::ToggleFloatingPanes]
        );
        let exit = FullscreenHint {
            target_is_relay_focus: true,
            ..hint(false, false, false, true)
        };
        assert_eq!(
            fullscreen_actions(PaneId::Terminal(3), exit),
            focus_then_toggle
        );
    }

    /// Tab already fullscreen → refocus the anchor pane (the relay's own
    /// fullscreen pane), then the active-pane toggle. A bare toggle is a no-op
    /// for a client without an active-pane entry (the device-gate-3 hang).
    #[test]
    fn tab_fullscreen_exits_by_refocusing_then_toggling() {
        let actions = fullscreen_actions(PaneId::Terminal(3), hint(false, false, false, true));
        assert_eq!(
            actions,
            vec![
                Action::FocusPaneByPaneId {
                    pane_id: PaneId::Terminal(3)
                },
                Action::ToggleFocusNoUiFullscreen,
            ]
        );
    }

    /// Tiled pane on a tab that is not fullscreen → focus, then toggle.
    #[test]
    fn tiled_not_fullscreen_focuses_then_toggles() {
        let actions = fullscreen_actions(PaneId::Terminal(3), hint(false, false, false, false));
        assert_eq!(
            actions,
            vec![
                Action::FocusPaneByPaneId {
                    pane_id: PaneId::Terminal(3)
                },
                Action::ToggleFocusNoUiFullscreen,
            ]
        );
    }

    /// Visible focused floating pane → hide path, via `ToggleFloatingPanes`
    /// (never `HideFloatingPanes`, which zellij answers with `Exit 0`).
    #[test]
    fn visible_focused_floating_takes_the_hide_path() {
        let actions = fullscreen_actions(PaneId::Terminal(5), hint(true, true, true, false));
        assert_eq!(actions, vec![Action::ToggleFloatingPanes]);
    }

    /// Visible unfocused floating pane → fill path (focus + resize), with no
    /// `ShowFloatingPanes` (zellij answers it with `Exit 2` on a visible layer).
    #[test]
    fn visible_unfocused_floating_takes_the_fill_path() {
        let actions = fullscreen_actions(PaneId::Terminal(5), hint(true, true, false, false));
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0],
            Action::FocusPaneByPaneId {
                pane_id: PaneId::Terminal(5)
            }
        );
        match &actions[1] {
            Action::ChangeFloatingPaneCoordinates {
                pane_id,
                coordinates,
            } => {
                assert_eq!(*pane_id, PaneId::Terminal(5));
                assert_eq!(coordinates.borderless, Some(true));
            }
            other => panic!("expected ChangeFloatingPaneCoordinates, got {other:?}"),
        }
    }

    /// No sequence ever sends the floating-layer actions that complete with a
    /// bare exit status (and so drop the relay client).
    #[test]
    fn no_sequence_sends_show_or_hide_floating_panes() {
        for floating in [false, true] {
            for visible in [false, true] {
                for focused in [false, true] {
                    for tab_fullscreen in [false, true] {
                        for target_is_relay_focus in [false, true] {
                            let h = FullscreenHint {
                                target_is_relay_focus,
                                ..hint(floating, visible, focused, tab_fullscreen)
                            };
                            for action in fullscreen_actions(PaneId::Terminal(1), h) {
                                assert!(
                                    !matches!(
                                        action,
                                        Action::ShowFloatingPanes { .. }
                                            | Action::HideFloatingPanes { .. }
                                    ),
                                    "{h:?} sent {action:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn resize_conversions_match_proto_handler() {
        assert_eq!(to_resize(ResizeKind::Increase), Resize::Increase);
        assert_eq!(to_resize(ResizeKind::Decrease), Resize::Decrease);
        assert_eq!(to_direction(ResizeDir::Left), Direction::Left);
        assert_eq!(to_direction(ResizeDir::Right), Direction::Right);
        assert_eq!(to_direction(ResizeDir::Up), Direction::Up);
        assert_eq!(to_direction(ResizeDir::Down), Direction::Down);
    }
}
