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
    ActionAck, FullscreenHint, LayoutSnapshot, MuxEvent, MuxMouseKind, MuxServerMsg, PaneRef,
    PaneSnapshot, ResizeDir, ResizeKind, ScrollDir, TabSnapshot,
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
pub(crate) fn parse_zellij_layout(tabs_json: &str, panes_json: &str) -> Result<LayoutSnapshot> {
    use std::collections::HashMap;
    use zellij_utils::data::{ListPanesResponse, ListTabsResponse};

    let tabs: ListTabsResponse = serde_json::from_str(tabs_json)
        .map_err(|e| anyhow!("parse_zellij_layout: parse ListTabs JSON: {e}"))?;
    let panes: ListPanesResponse = serde_json::from_str(panes_json)
        .map_err(|e| anyhow!("parse_zellij_layout: parse ListPanes JSON: {e}"))?;

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

    fn kill_session(&self, session: &str) -> Result<()> {
        actions::kill_session(session)
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

    // ── Attach (the relay seam) ─────────────────────────────────────────────

    fn open_attach(
        &self,
        session: &str,
        rows: u16,
        cols: u16,
        read_only: bool,
    ) -> Result<DualHandle> {
        // For the zellij backend the AttachClient open is identical regardless
        // of `read_only`: the relay pre-resolves the read-only size before
        // calling, and the read-only teardown nudge lives in the ShutdownGuard
        // (send_client_exited vs send_resize). We log the mode for traceability.
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
        // The fill-vs-hide-vs-tiled action sequence owned by this backend.
        // The relay drives this via `MuxSender::toggle_fullscreen`; the former
        // inline copies in `relay/helpers.rs`/`relay/inbound.rs` are retired.
        use zellij_utils::data::FloatingPaneCoordinates;
        use zellij_utils::input::layout::PercentOrFixed;

        let pane_id = to_pane_id(pane);
        if hint.is_floating {
            if hint.floating_visible && hint.is_focused_floating {
                // Hide the floating panes so a tiled pane can be shown again.
                self.0
                    .send_action_as_self(Action::HideFloatingPanes { tab_id: None })?;
            } else {
                // Make the floating pane fill the display area.
                self.0
                    .send_action_as_self(Action::ShowFloatingPanes { tab_id: None })?;
                self.0
                    .send_action_as_self(Action::FocusPaneByPaneId { pane_id })?;
                let coordinates = FloatingPaneCoordinates {
                    x: Some(PercentOrFixed::Percent(0)),
                    y: Some(PercentOrFixed::Percent(0)),
                    width: Some(PercentOrFixed::Percent(100)),
                    height: Some(PercentOrFixed::Percent(100)),
                    pinned: None,
                    borderless: Some(true),
                };
                self.0
                    .send_action_as_self(Action::ChangeFloatingPaneCoordinates {
                        pane_id,
                        coordinates,
                    })?;
            }
        } else {
            // Tiled: focus then active-pane fullscreen toggle (clean parity).
            self.0
                .send_action_as_self(Action::FocusPaneByPaneId { pane_id })?;
            self.0.send_action_as_self(Action::ToggleFocusFullscreen)?;
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
        // connection, so the server routes it within THIS client's viewport
        // (pane under position → app mouse-capture forwarding or scrollback),
        // never a co-attached client's.
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
            // UnblockInputThread / Connected / QueryTerminalSize / LogError /
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
