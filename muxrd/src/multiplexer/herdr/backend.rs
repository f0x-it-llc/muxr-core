//! herdr [`MuxBackend`] — composes the P2.02 control client, the P2.03 wire
//! relay, and the shared id registries into a functionally complete second
//! backend behind the same trait [`ZellijBackend`](crate::multiplexer::ZellijBackend)
//! implements.
//!
//! ## What this ties together
//!
//! - [`HerdrControl`] (P2.02) — the JSON-API control plane: workspace / tab /
//!   pane lifecycle + the neutral [`LayoutSnapshot`] transcode.
//! - [`relay::open_attach`] (P2.03) — the wire data plane: the single-pane ANSI
//!   relay returning a [`DualHandle`] the Phase-1 relay drives unchanged.
//! - [`HerdrPaneRegistry`] / [`HerdrTabRegistry`] — the stable `u32`/`u64` ↔
//!   herdr-`String` id maps, shared (behind `Arc`) by the control client, the
//!   wire relay, and this backend so a `PaneRef`/`tab_id` round-trips identically
//!   across layout polls and actions.
//!
//! ## session ↔ daemon, workspaces ↔ spaces (Option A — herdr Spaces)
//!
//! A herdr daemon is a flat **daemon → workspaces → tabs → panes** (there is no
//! session container above workspaces). muxrd collapses the whole daemon to **one**
//! muxr session — the bare-name sentinel [`HERDR_SESSION`] (`"herdr"`) — and
//! surfaces its **workspaces as switchable "spaces"** ([`MuxBackend::list_spaces`],
//! [`MuxSender::switch_space`](crate::multiplexer::MuxSender::switch_space)).
//!
//! So [`list_sessions`](MuxBackend::list_sessions) returns exactly one entry, and
//! every session-scoped backend op resolves the sentinel to the daemon's
//! **active-or-first** workspace via [`HerdrBackend::resolve_workspace`]
//! ([`active_or_first_workspace_id`](HerdrBackend::active_or_first_workspace_id)).
//! [`workspace_id_for`](HerdrBackend::workspace_id_for) (label match) is retained
//! for any non-sentinel name (out-of-band safety) and the space-lifecycle ops
//! address workspaces by their opaque `workspace_id` (= the neutral `space_id`).
//!
//! ## Capability gaps (herdr has no equivalent)
//!
//! herdr's JSON API does not cover every zellij ephemeral action. Where there is
//! no honest mapping this backend returns a **failed** [`ActionAck`]
//! (`ok: false`) so the client surfaces it rather than silently dropping the
//! request — except where a **benign no-op** (`ok: true`) is clearly correct
//! because the operation is meaningless or is handled elsewhere (the wire relay):
//!
//! | method | disposition | why |
//! |---|---|---|
//! | `write_to_pane` | `ok:false` | input is wire-socket only ([`relay`]), no JSON equivalent |
//! | `resize_pane` | `ok:false` | resize is wire-socket only ([`relay`]) |
//! | `scroll_pane` | `ok:false` | scrollback is wire-socket only ([`relay`]) |
//! | `focus_pane` | `ok:true` no-op | ephemeral path only; the relay's re-attach is the real focus |
//! | `toggle_pane_floating` | `ok:true` no-op | herdr has no floating layer |
//!
//! The ephemeral `focus_pane`/`write`/`resize`/`scroll` paths are used **only when
//! no relay is attached**; with a live attach the gRPC layer routes input, focus,
//! and scroll through the relay's persistent wire connection (`grpc/pane_ops.rs`),
//! so none of the `ok:false` gaps sits on the core interactive hot path.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::multiplexer::types::{
    ActionAck, LayoutSnapshot, PaneRef, ResizeDir, ResizeKind, ResumeTarget, ScrollDir,
    SpaceSnapshot,
};
use crate::multiplexer::{DualHandle, MuxBackend};

use super::api::{PaneLayoutRect, PaneZoomMode, SplitDirection, WorkspaceInfo};
use super::control::HerdrControl;
use super::paths::HerdrSocketPaths;
use super::registry::{HerdrPaneRegistry, HerdrTabRegistry};
use super::relay;

/// Default `(rows, cols)` reported when herdr exposes no usable layout area for a
/// session (no panes yet, or a zero-sized area). A conventional 24×80 keeps the
/// gRPC `GetSessionSize` reply sane rather than `0×0`.
const DEFAULT_SESSION_SIZE: (u16, u16) = (24, 80);

/// Default split direction for [`MuxBackend::new_pane`]. herdr only models
/// `Right` / `Down`; `Right` mirrors zellij's default "split vertically".
const DEFAULT_SPLIT_DIRECTION: SplitDirection = SplitDirection::Right;

/// Bare-name sentinel for herdr's single collapsed muxr session (Option A —
/// herdr Spaces). A herdr daemon has no session container; muxrd presents the
/// whole daemon as ONE session under this stable bare name (display name also
/// `"herdr"`) whose workspaces are switchable "spaces".
///
/// It is a fixed string (not a workspace label) so [`ListSessions`] is a cheap
/// constant entry and `resolve_session` round-trips it through muxrd's strict
/// `[A-Za-z0-9_-]` ipc guard (`"herdr"` passes). All session-scoped herdr ops
/// recognise it via [`HerdrBackend::resolve_workspace`] and bind to the daemon's
/// active-or-first workspace.
///
/// [`ListSessions`]: MuxBackend::list_sessions
pub(crate) const HERDR_SESSION: &str = "herdr";

/// Rejection message for an attempt to kill or rename the singular herdr session.
///
/// **Decision 5 (the herdr session is singular):** a herdr daemon has no session
/// container — muxrd presents the whole daemon as ONE non-killable/renamable
/// session ([`HERDR_SESSION`]). Its *workspaces* are the managed unit (close via
/// `CloseSpace`, rename via `RenameSpace`), never the session itself. So
/// [`kill_session`](HerdrBackend::kill_session) /
/// [`rename_session`](HerdrBackend::rename_session) reject the sentinel cleanly
/// instead of resolving it to a workspace and surfacing the opaque
/// `internal("no workspace with label 'herdr'")` (S-M3).
pub(crate) const HERDR_SESSION_IMMUTABLE_MSG: &str =
    "the herdr session cannot be killed or renamed — manage workspaces via CloseSpace/RenameSpace";

/// The herdr-backed [`MuxBackend`].
///
/// Holds the control client and the two id registries (shared behind `Arc` with
/// the control client and every wire relay it spawns), plus the resolved socket
/// pair so both planes target the same herdr instance. Cheap to construct; holds
/// no live connection (every control call is a fresh JSON round-trip, every attach
/// a fresh wire socket).
#[derive(Debug)]
pub struct HerdrBackend {
    /// JSON-API control plane (workspace/tab/pane lifecycle + layout transcode).
    control: Arc<HerdrControl>,
    /// Shared pane-id registry (`u32 ↔ herdr pane_id`/`terminal_id`).
    panes: Arc<HerdrPaneRegistry>,
    /// Shared tab-id registry (`u64 ↔ herdr tab_id`).
    tabs: Arc<HerdrTabRegistry>,
    /// Resolved herdr socket pair (JSON-API + wire). The wire path is handed to
    /// every [`relay::open_attach`].
    paths: HerdrSocketPaths,
}

impl HerdrBackend {
    /// Construct a backend for the herdr instance reachable at `paths`, building a
    /// control client that shares freshly-created registries with this backend
    /// (and, transitively, with every wire relay it opens).
    pub fn new(paths: HerdrSocketPaths) -> Self {
        let panes = Arc::new(HerdrPaneRegistry::new());
        let tabs = Arc::new(HerdrTabRegistry::new());
        let control = Arc::new(HerdrControl::new(
            paths.api.clone(),
            Arc::clone(&panes),
            Arc::clone(&tabs),
        ));
        Self {
            control,
            panes,
            tabs,
            paths,
        }
    }

    /// Construct a backend resolving herdr's socket pair from the process
    /// environment (`HERDR_SOCKET_PATH` / XDG default). This is the entry point
    /// the P2.05 selector will call when the operator selects the herdr backend.
    pub fn from_env() -> Self {
        Self::new(HerdrSocketPaths::resolve())
    }

    /// The shared pane registry (same `Arc` the control client + relays hold).
    pub fn pane_registry(&self) -> &Arc<HerdrPaneRegistry> {
        &self.panes
    }

    /// The shared tab registry.
    pub fn tab_registry(&self) -> &Arc<HerdrTabRegistry> {
        &self.tabs
    }

    /// Resolve a neutral session **name** to its herdr `workspace_id` by matching
    /// the workspace **label** (the name muxrd shows).
    ///
    /// herdr `label` is a *display name*, not a unique key (`workspace_id` is) — so
    /// the match may be zero, one, or many. Exactly-one is the only safe case:
    /// - zero matches → `Err` (no such session);
    /// - **more than one** match → `Err` rather than silently binding to whichever
    ///   is first (M1: that risked wrong-workspace targeting / destruction).
    ///
    /// muxrd's own [`create_session`](MuxBackend::create_session) refuses to
    /// manufacture a duplicate label, so the ambiguous case only arises from
    /// out-of-band herdr usage; we fail closed when it does.
    fn workspace_id_for(&self, session: &str) -> Result<String> {
        let mut matching = self
            .control
            .list_workspaces()?
            .into_iter()
            .filter(|w| w.label == session);
        let first = matching
            .next()
            .ok_or_else(|| anyhow!("herdr: no workspace with label '{session}'"))?;
        if matching.next().is_some() {
            return Err(anyhow!(
                "herdr: ambiguous session: multiple herdr workspaces labeled '{session}'"
            ));
        }
        Ok(first.workspace_id)
    }

    /// The daemon's **active (focused)** workspace id, falling back to the **first
    /// listed** when herdr reports no focused workspace. Errors only when the
    /// daemon has no workspaces at all.
    ///
    /// This is what the [`HERDR_SESSION`] sentinel binds to: on attach (and for
    /// every session-scoped op that does not carry a per-connection space) muxrd
    /// targets the workspace the user would land on. The per-connection current
    /// space — once switched — is tracked by the relay's `workspace_id`, not here.
    fn active_or_first_workspace_id(&self) -> Result<String> {
        let workspaces = self.control.list_workspaces()?;
        pick_active_or_first(&workspaces)
            .map(|w| w.workspace_id.clone())
            .ok_or_else(|| anyhow!("herdr: daemon has no workspaces to attach"))
    }

    /// Resolve a neutral muxr session name to a herdr `workspace_id`.
    ///
    /// Under Option A the herdr session is the singular [`HERDR_SESSION`] sentinel
    /// → resolve to the daemon's [`active-or-first`](Self::active_or_first_workspace_id)
    /// workspace. Any other name is matched by workspace **label** via
    /// [`workspace_id_for`](Self::workspace_id_for) (out-of-band / legacy safety).
    fn resolve_workspace(&self, session: &str) -> Result<String> {
        if session == HERDR_SESSION {
            self.active_or_first_workspace_id()
        } else {
            self.workspace_id_for(session)
        }
    }
}

/// Derive a `(rows, cols)` session size from a herdr layout `area` rectangle,
/// falling back to [`DEFAULT_SESSION_SIZE`] for any zero dimension. Pure so it is
/// unit-testable from a fixture rect.
fn session_size_from_area(area: PaneLayoutRect) -> (u16, u16) {
    let rows = if area.height == 0 {
        DEFAULT_SESSION_SIZE.0
    } else {
        area.height
    };
    let cols = if area.width == 0 {
        DEFAULT_SESSION_SIZE.1
    } else {
        area.width
    };
    (rows, cols)
}

/// Map one herdr [`WorkspaceInfo`] to the neutral [`SpaceSnapshot`]. Pure (no
/// I/O) so the workspace→space mapping is unit-testable from a fixture. `active`
/// reflects herdr's daemon-reported focused workspace; the per-connection current
/// space is marked elsewhere (gRPC layer, T03).
fn space_from_workspace(w: WorkspaceInfo) -> SpaceSnapshot {
    SpaceSnapshot {
        id: w.workspace_id,
        name: w.label,
        active: w.focused,
    }
}

/// Pick the daemon's **active (focused)** workspace, falling back to the **first
/// listed**. Pure so the [`HERDR_SESSION`]-binding rule is unit-testable without a
/// live daemon. `None` only when the daemon has no workspaces.
fn pick_active_or_first(workspaces: &[WorkspaceInfo]) -> Option<&WorkspaceInfo> {
    workspaces
        .iter()
        .find(|w| w.focused)
        .or_else(|| workspaces.first())
}

/// A benign no-op acknowledgement for an operation that is meaningless on herdr
/// (and so must not be reported as a failure).
fn noop_ack() -> ActionAck {
    ActionAck {
        ok: true,
        error: None,
        info: None,
    }
}

/// Reject a malformed explicit space (workspace) id before it is used as a herdr
/// `workspace_id`.
///
/// [`MuxBackend::query_layout_for_space`] addresses an explicit id **directly**,
/// deliberately skipping [`HerdrBackend::resolve_workspace`] — whose sentinel
/// branch falls back to the daemon's active-or-first workspace, which would
/// silently answer a foreign-space peek with the *focused* space instead. Nothing
/// downstream re-checks the id, so it must be shaped like a herdr id here.
///
/// Defence in depth, not the primary gate: the client-facing guard is
/// `grpc::space_ops::validate_space_id` (the same `[A-Za-z0-9_-.:]` allowlist plus
/// a length bound), which every gRPC space request already passes through. This
/// one covers any other caller of the trait. If herdr ever widens its id charset,
/// widen both.
fn validate_workspace_id(space_id: &str) -> Result<()> {
    if space_id.is_empty() {
        return Err(anyhow!("herdr: space_id must not be empty"));
    }
    if !space_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
    {
        return Err(anyhow!(
            "herdr: invalid space_id: only [A-Za-z0-9_-.:] characters are allowed"
        ));
    }
    Ok(())
}

/// A failed acknowledgement for a real capability gap, so the client surfaces it.
fn unsupported_ack(op: &str) -> ActionAck {
    ActionAck {
        ok: false,
        error: Some(format!("{op} is unsupported on the herdr backend")),
        info: None,
    }
}

impl MuxBackend for HerdrBackend {
    // ── Session lifecycle ───────────────────────────────────────────────────

    fn list_sessions(&self) -> Result<Vec<(String, Duration)>> {
        // Option A: a herdr daemon is ONE muxr session (its workspaces are
        // "spaces"). Return exactly the [`HERDR_SESSION`] sentinel. We still probe
        // `workspace.list` so an unreachable daemon errors here (and is skipped by
        // the gRPC fan-out) rather than advertising a phantom session.
        // herdr exposes no daemon uptime → Duration::ZERO.
        self.control.list_workspaces()?;
        Ok(vec![(HERDR_SESSION.to_string(), Duration::ZERO)])
    }

    fn list_sessions_with_resurrectables(&self) -> Result<Vec<(String, u64, bool)>> {
        // Option A: ONE daemon session (the [`HERDR_SESSION`] sentinel). herdr has
        // no resurrectable/dead-session concept → resurrectable=false; no uptime →
        // age=0. tab/pane counts are not carried by this trait return (the gRPC
        // layer fills SessionInfo.{tab,pane}_count with 0 for every backend today),
        // so the collapse needs no count query. We still probe `workspace.list` so
        // an unreachable daemon errors rather than advertising a phantom session.
        self.control.list_workspaces()?;
        Ok(vec![(HERDR_SESSION.to_string(), 0u64, false)])
    }

    fn validate_session_name(&self, name: &str) -> std::result::Result<(), String> {
        // Reuse muxrd's strict `[A-Za-z0-9_-]` guard (the security invariant —
        // path-traversal / metacharacter defence). herdr labels may be looser, but
        // muxrd's boundary stays strict regardless of backend.
        crate::ipc::validate_session_name(name)
    }

    fn create_session(&self, name: &str, layout: Option<String>) -> Result<ActionAck> {
        // herdr has no zellij `--layout`; the `layout` path is ignored (documented).
        // The name becomes the workspace label.
        if let Some(layout) = layout {
            log::debug!("HerdrBackend::create_session: ignoring layout '{layout}' (unsupported)");
        }
        // M1: refuse to manufacture a duplicate label. herdr labels are not unique,
        // and `workspace_id_for` resolves a session by label — so creating a second
        // workspace with an existing label would make every later
        // kill/rename/attach/query for that name ambiguous (and fail closed). Block
        // it here so muxrd never creates the ambiguous condition in the first place.
        if self
            .control
            .list_workspaces()?
            .iter()
            .any(|w| w.label == name)
        {
            return Ok(ActionAck {
                ok: false,
                error: Some(format!("session '{name}' already exists")),
                info: None,
            });
        }
        self.control.create_workspace(Some(name.to_string()))
    }

    fn kill_session(&self, session: &str) -> Result<()> {
        // Decision 5 (S-M3): the singular herdr session is not killable — there is
        // no session object to destroy (its workspaces are the managed unit, closed
        // via CloseSpace). Reject cleanly rather than resolving the sentinel to a
        // workspace and returning the opaque "no workspace with label 'herdr'".
        // The gRPC handler additionally short-circuits this to `invalid_argument`
        // before reaching the backend; this guard is defence-in-depth (and covers
        // the legacy bare-name path that the handler's collapsed-session check misses).
        if session == HERDR_SESSION {
            return Err(anyhow!(HERDR_SESSION_IMMUTABLE_MSG));
        }
        let workspace_id = self.workspace_id_for(session)?;
        let ack = self.control.close_workspace(&workspace_id)?;
        if ack.ok {
            Ok(())
        } else {
            Err(anyhow!(
                "herdr workspace.close failed: {}",
                ack.error.unwrap_or_else(|| "unknown error".into())
            ))
        }
    }

    fn rename_session(&self, session: &str, new_name: String) -> Result<ActionAck> {
        // Decision 5 (S-M3): the singular herdr session is not renamable — rename a
        // *workspace* via RenameSpace instead. A clean `ok:false` ack beats the
        // opaque "no workspace with label 'herdr'" internal error that resolving
        // the sentinel through `workspace_id_for` would otherwise produce.
        if session == HERDR_SESSION {
            return Ok(ActionAck {
                ok: false,
                error: Some(HERDR_SESSION_IMMUTABLE_MSG.to_owned()),
                info: None,
            });
        }
        let workspace_id = self.workspace_id_for(session)?;
        self.control.rename_workspace(&workspace_id, &new_name)
    }

    // ── Spaces (herdr workspaces) ────────────────────────────────────────────
    //
    // Option A: the daemon's workspaces ARE the spaces. These pass through to the
    // existing `workspace.*` control ops; the neutral `space_id` is herdr's opaque
    // `workspace_id` verbatim. T03 wires the gRPC GetSpaces/CreateSpace/RenameSpace/
    // CloseSpace handlers to these. `_session` is ignored — herdr has one daemon.

    fn supports_spaces(&self) -> bool {
        // Option A: herdr's workspaces ARE the space axis, so a request may name
        // one explicitly (see `query_layout_for_space`).
        true
    }

    fn list_spaces(&self, _session: &str) -> Result<Vec<SpaceSnapshot>> {
        // `WorkspaceInfo.focused` (carried into SpaceSnapshot.active by
        // `space_from_workspace`) is the daemon's reported active workspace. The
        // *connection's* current space is marked per-relay at the gRPC layer (T03)
        // using the relay's tracked workspace_id.
        Ok(self
            .control
            .list_workspaces()?
            .into_iter()
            .map(space_from_workspace)
            .collect())
    }

    fn create_space(&self, label: Option<String>) -> Result<ActionAck> {
        self.control.create_workspace(label)
    }

    fn rename_space(&self, space_id: &str, label: &str) -> Result<ActionAck> {
        self.control.rename_workspace(space_id, label)
    }

    fn close_space(&self, space_id: &str) -> Result<ActionAck> {
        self.control.close_workspace(space_id)
    }

    // ── Ephemeral control actions ───────────────────────────────────────────

    fn write_to_pane(&self, _session: &str, _pane: PaneRef, _bytes: Vec<u8>) -> Result<ActionAck> {
        // Input is wire-socket only on herdr (the relay's persistent connection);
        // there is no JSON-API equivalent. Honest failure so the client surfaces it.
        Ok(unsupported_ack("write_to_pane"))
    }

    fn focus_pane(&self, _session: &str, _pane: PaneRef) -> Result<ActionAck> {
        // Ephemeral path only (no relay attached). herdr has no focus-by-id over
        // JSON; with a live attach the relay re-attaches the wire stream to the
        // target terminal (the real focus). Benign no-op here.
        Ok(noop_ack())
    }

    fn close_pane(&self, _session: &str, pane: PaneRef) -> Result<ActionAck> {
        self.control.close_pane(pane.id)
    }

    fn new_pane(&self, session: &str, floating: bool, name: Option<String>) -> Result<ActionAck> {
        // herdr has no floating layer → `floating` is ignored. herdr's `pane.split`
        // carries no label → `name` is not applied (documented gap).
        if floating {
            log::debug!("HerdrBackend::new_pane: ignoring floating=true (no floating layer)");
        }
        if let Some(name) = &name {
            log::debug!("HerdrBackend::new_pane: ignoring name '{name}' (pane.split has no label)");
        }
        let workspace_id = self.resolve_workspace(session)?;
        self.control.split_pane(
            Some(&workspace_id),
            None, // split the focused pane
            DEFAULT_SPLIT_DIRECTION,
            true, // focus the new pane
        )
    }

    fn rename_pane(&self, _session: &str, pane: PaneRef, name: String) -> Result<ActionAck> {
        self.control.rename_pane(pane.id, Some(name))
    }

    fn resize_pane(
        &self,
        _session: &str,
        _pane: PaneRef,
        _kind: ResizeKind,
        _dir: Option<ResizeDir>,
    ) -> Result<ActionAck> {
        // Resize is wire-socket only on herdr (`ClientMessage::Resize`, driven by
        // the relay from the client's terminal geometry). No JSON-API equivalent.
        Ok(unsupported_ack("resize_pane"))
    }

    fn toggle_pane_floating(&self, _session: &str, _pane: PaneRef) -> Result<ActionAck> {
        // herdr has no floating layer; toggling is meaningless → benign no-op.
        Ok(noop_ack())
    }

    fn toggle_pane_fullscreen(&self, _session: &str, pane: PaneRef) -> Result<ActionAck> {
        // herdr's pane.zoom is the analogue of zellij pane fullscreen.
        self.control.zoom_pane(Some(pane.id), PaneZoomMode::Toggle)
    }

    fn scroll_pane(&self, _session: &str, _pane: PaneRef, _dir: ScrollDir) -> Result<ActionAck> {
        // Scrollback is wire-socket only on herdr (handled by the relay). No
        // JSON-API equivalent → honest failure.
        Ok(unsupported_ack("scroll_pane"))
    }

    fn new_tab(&self, session: &str, name: Option<String>) -> Result<ActionAck> {
        let workspace_id = self.resolve_workspace(session)?;
        self.control.create_tab(Some(&workspace_id), name)
    }

    fn close_tab(&self, _session: &str, tab_id: u64) -> Result<ActionAck> {
        self.control.close_tab(tab_id)
    }

    fn go_to_tab(&self, _session: &str, tab_id: u64) -> Result<ActionAck> {
        self.control.focus_tab(tab_id)
    }

    fn rename_tab(&self, _session: &str, tab_id: u64, name: String) -> Result<ActionAck> {
        self.control.rename_tab(tab_id, name)
    }

    // ── Read-only queries ───────────────────────────────────────────────────

    fn query_layout(&self, session: &str) -> Result<LayoutSnapshot> {
        let workspace_id = self.resolve_workspace(session)?;
        self.control.query_layout(&workspace_id)
    }

    fn query_layout_for_space(
        &self,
        session: &str,
        space_id: Option<&str>,
    ) -> Result<LayoutSnapshot> {
        match space_id {
            // Explicit space → address the workspace DIRECTLY by its opaque id.
            // `resolve_workspace` is bypassed on purpose: its [`HERDR_SESSION`]
            // branch resolves to the daemon's active-or-first workspace, which
            // would answer a foreign-space peek with the focused space instead.
            //
            // Read-only: `HerdrControl::query_layout` is already workspace-scoped
            // (`workspace.list` + `tab.list`/`pane.list` for that id) and never
            // calls `workspace.focus` — so this moves neither the daemon's focus
            // nor the calling connection's view (the relay is not involved at all).
            Some(id) => {
                validate_workspace_id(id)?;
                self.control.query_layout(id)
            }
            // No explicit space → today's path, unchanged.
            None => self.query_layout(session),
        }
    }

    fn query_session_size(&self, session: &str) -> Result<(u16, u16)> {
        // Derive from the workspace's focused-tab layout `area`. Pick the focused
        // pane (or the first listed) as the `pane.layout` addressing handle, then
        // read `area.height`/`area.width` → `(rows, cols)` (the order
        // `session_size_from_area` returns). Fall back to a sane default when the
        // workspace has no panes / no usable area.
        let workspace_id = self.resolve_workspace(session)?;
        let panes = self.control.list_panes(&workspace_id)?;
        let representative = panes.iter().find(|p| p.focused).or_else(|| panes.first());
        match representative {
            Some(pane) => {
                let layout = self.control.pane_layout(Some(&pane.pane_id))?;
                Ok(session_size_from_area(layout.area))
            }
            None => Ok(DEFAULT_SESSION_SIZE),
        }
    }

    fn pane_is_floating_with_visibility(
        &self,
        _session: &str,
        _pane: PaneRef,
    ) -> Result<(bool, bool, Option<PaneRef>)> {
        // herdr has no floating layer: nothing floats, nothing is floating-visible,
        // and there is no focused floating pane.
        Ok((false, false, None))
    }

    // ── Attach (the relay seam) ─────────────────────────────────────────────

    fn open_attach(
        &self,
        session: &str,
        rows: u16,
        cols: u16,
        read_only: bool,
    ) -> Result<DualHandle> {
        // A hint-less attach is just the resume path with an empty target: the
        // ladder's last rung IS today's "attach the workspace's focused pane".
        self.open_attach_with_resume(session, rows, cols, read_only, &ResumeTarget::default())
    }

    fn open_attach_with_resume(
        &self,
        session: &str,
        rows: u16,
        cols: u16,
        read_only: bool,
        resume: &ResumeTarget,
    ) -> Result<DualHandle> {
        // Option A: `session` is the daemon sentinel, not a workspace label —
        // resolve the daemon's active-or-first workspace, which is the FALLBACK
        // for the resume ladder in `relay::open_attach` (a resume hint may name a
        // different workspace, and wins when it still resolves). The relay then
        // holds the workspace it actually landed on and re-points it on
        // switch_space (per-connection).
        let workspace_id = self.resolve_workspace(session)?;
        relay::open_attach(
            Arc::clone(&self.control),
            self.paths.wire.clone(),
            workspace_id,
            session.to_string(),
            rows,
            cols,
            read_only,
            resume,
        )
    }

    // ── Backend identity ────────────────────────────────────────────────────

    fn backend_version(&self) -> String {
        // The wire protocol version is the meaningful compat marker between muxrd
        // and a given herdr release — and since muxrd negotiates it per connection
        // rather than pinning it, the only truthful answer comes from the server.
        // Best-effort: this is a reporting surface, so a failed probe degrades to an
        // "unknown" marker rather than propagating an error.
        match self.control.ping() {
            Ok(info) => format!("herdr-{}-wire-v{}", info.version, info.protocol),
            Err(e) => {
                log::debug!("herdr backend_version: ping failed: {e:#}");
                "herdr-wire-unknown".to_string()
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> HerdrBackend {
        // Points at a nonexistent socket: every method that does I/O will error,
        // but the pure / no-op dispositions below need no live herdr.
        HerdrBackend::new(HerdrSocketPaths::from_api_socket("/nonexistent/herdr.sock"))
    }

    // ── session_size_from_area derivation ─────────────────────────────────────

    #[test]
    fn session_size_derives_rows_cols_from_area() {
        let (rows, cols) = session_size_from_area(PaneLayoutRect {
            x: 0,
            y: 0,
            width: 220,
            height: 50,
        });
        assert_eq!((rows, cols), (50, 220));
    }

    #[test]
    fn session_size_falls_back_on_zero_dimensions() {
        let (rows, cols) = session_size_from_area(PaneLayoutRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });
        assert_eq!((rows, cols), DEFAULT_SESSION_SIZE);
    }

    #[test]
    fn session_size_falls_back_per_dimension() {
        // A zero in only one axis still substitutes just that axis.
        let (rows, cols) = session_size_from_area(PaneLayoutRect {
            x: 0,
            y: 0,
            width: 100,
            height: 0,
        });
        assert_eq!((rows, cols), (DEFAULT_SESSION_SIZE.0, 100));
    }

    // ── capability-gap dispositions (no live herdr required) ──────────────────

    #[test]
    fn write_resize_scroll_are_honest_failures() {
        let b = backend();
        let p = PaneRef::terminal(1);
        let w = b.write_to_pane("s", p, b"x".to_vec()).unwrap();
        assert!(!w.ok);
        assert!(w.error.as_deref().unwrap().contains("write_to_pane"));

        let r = b
            .resize_pane("s", p, ResizeKind::Increase, Some(ResizeDir::Left))
            .unwrap();
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("resize_pane"));

        let s = b.scroll_pane("s", p, ScrollDir::HalfPageUp).unwrap();
        assert!(!s.ok);
        assert!(s.error.as_deref().unwrap().contains("scroll_pane"));
    }

    #[test]
    fn focus_and_toggle_floating_are_benign_noops() {
        let b = backend();
        let p = PaneRef::terminal(1);
        // focus_pane: relay re-attach is the real focus; ephemeral path no-ops ok.
        assert!(b.focus_pane("s", p).unwrap().ok);
        // toggle_pane_floating: herdr has no floating layer → meaningless, ok no-op.
        assert!(b.toggle_pane_floating("s", p).unwrap().ok);
    }

    // ── pure trait dispositions ───────────────────────────────────────────────

    #[test]
    fn pane_floating_visibility_is_always_false() {
        let b = backend();
        let (is_floating, visible, focused) = b
            .pane_is_floating_with_visibility("s", PaneRef::terminal(1))
            .unwrap();
        assert!(!is_floating);
        assert!(!visible);
        assert!(focused.is_none());
    }

    /// With no reachable herdr the `ping` probe fails, so `backend_version()` must
    /// degrade to the "unknown" marker rather than panicking or reporting a
    /// fabricated protocol number. (The negotiated-value path needs a live server
    /// and is covered by the `herdr_integration` smoke tests.)
    #[test]
    fn backend_version_degrades_when_server_unreachable() {
        let b = backend();
        assert_eq!(b.backend_version(), "herdr-wire-unknown");
    }

    // ── validate_session_name keeps the strict security guard ─────────────────

    #[test]
    fn validate_session_name_is_strict() {
        let b = backend();
        assert!(b.validate_session_name("dev").is_ok());
        assert!(b.validate_session_name("my-work_1").is_ok());
        assert!(b.validate_session_name("../foo").is_err());
        assert!(b.validate_session_name("foo/bar").is_err());
        assert!(b.validate_session_name("").is_err());
        assert!(b.validate_session_name("with space").is_err());
    }

    // ── Spaces: workspace→space mapping + active-or-first resolution ───────────

    fn workspace(workspace_id: &str, label: &str, focused: bool) -> WorkspaceInfo {
        serde_json::from_value(serde_json::json!({
            "workspace_id": workspace_id,
            "number": 0,
            "label": label,
            "focused": focused,
            "pane_count": 1,
            "tab_count": 1,
            "active_tab_id": "tab-1",
            "agent_status": "idle",
        }))
        .expect("WorkspaceInfo fixture")
    }

    #[test]
    fn space_from_workspace_maps_id_label_focused() {
        let s = space_from_workspace(workspace("ws-7", "logs", true));
        assert_eq!(s.id, "ws-7");
        assert_eq!(s.name, "logs");
        assert!(s.active, "active mirrors WorkspaceInfo.focused");

        let inactive = space_from_workspace(workspace("ws-8", "api", false));
        assert!(!inactive.active);
    }

    #[test]
    fn pick_active_or_first_prefers_focused() {
        let workspaces = vec![
            workspace("ws-1", "main", false),
            workspace("ws-2", "logs", true), // focused → chosen even though not first
            workspace("ws-3", "api", false),
        ];
        assert_eq!(
            pick_active_or_first(&workspaces).map(|w| w.workspace_id.as_str()),
            Some("ws-2")
        );
    }

    #[test]
    fn pick_active_or_first_falls_back_to_first_when_none_focused() {
        let workspaces = vec![
            workspace("ws-1", "main", false),
            workspace("ws-2", "logs", false),
        ];
        assert_eq!(
            pick_active_or_first(&workspaces).map(|w| w.workspace_id.as_str()),
            Some("ws-1")
        );
    }

    #[test]
    fn pick_active_or_first_is_none_for_empty_daemon() {
        assert!(pick_active_or_first(&[]).is_none());
    }

    // ── Space-scoped layout reads ─────────────────────────────────────────────

    #[test]
    fn herdr_supports_spaces() {
        // The gRPC layer gates an explicitly space-scoped GetLayout on this
        // predicate; herdr is the one backend that answers `true` (zellij and
        // every mock keep the `false` default).
        assert!(backend().supports_spaces());
    }

    #[test]
    fn workspace_id_shape_accepts_herdr_ids_and_rejects_abuse() {
        // Mirrors grpc::space_ops::validate_space_id's allowlist (defence in depth).
        assert!(validate_workspace_id("ws-1").is_ok());
        assert!(validate_workspace_id("01HF8Z9K3T4Qm-abc.def").is_ok());
        assert!(validate_workspace_id("herdr:ws:7").is_ok());
        assert!(validate_workspace_id("").is_err(), "empty rejected");
        assert!(
            validate_workspace_id("../escape").is_err(),
            "path traversal rejected"
        );
        assert!(validate_workspace_id("a b").is_err(), "whitespace rejected");
        assert!(validate_workspace_id("has\0nul").is_err());
    }

    #[test]
    fn query_layout_for_space_rejects_a_malformed_id_before_any_io() {
        // The nonexistent socket would fail any round-trip, so assert on the
        // MESSAGE: a malformed id must be refused by the shape guard, never
        // forwarded to herdr's JSON-API.
        let b = backend();
        let err = b
            .query_layout_for_space(HERDR_SESSION, Some("../escape"))
            .expect_err("a malformed space_id must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("invalid space_id"),
            "expected the shape guard's message, got: {msg}"
        );
    }

    #[test]
    fn query_layout_for_space_rejects_an_empty_id() {
        // `Some("")` is a caller bug: "no space" is expressed as `None`, which
        // falls through to the ordinary session path. Never resolve it to the
        // active-or-first workspace by accident.
        let b = backend();
        let err = b
            .query_layout_for_space(HERDR_SESSION, Some(""))
            .expect_err("an empty space_id must be rejected");
        assert!(format!("{err:#}").contains("must not be empty"));
    }

    // ── Single-session collapse (Option A) ────────────────────────────────────

    #[test]
    fn herdr_session_sentinel_passes_the_strict_ipc_guard() {
        // The sentinel must round-trip through resolve_session, which validates the
        // bare name against the strict `[A-Za-z0-9_-]` guard.
        assert!(crate::ipc::validate_session_name(HERDR_SESSION).is_ok());
        assert_eq!(HERDR_SESSION, "herdr");
    }

    #[test]
    fn list_sessions_collapses_to_one_daemon_session() {
        // With no reachable daemon the probe errors (Err, not a phantom session);
        // this asserts the collapse never fans out per-workspace. The happy-path
        // single-entry shape is exercised live on the rig (no mock-socket seam).
        let b = backend();
        assert!(
            b.list_sessions().is_err(),
            "unreachable daemon must error, not advertise a session"
        );
        assert!(b.list_sessions_with_resurrectables().is_err());
    }

    // ── Decision 5 / S-M3: sentinel session is not killable/renamable ─────────

    #[test]
    fn kill_session_rejects_the_sentinel_cleanly() {
        // The backend (nonexistent socket) must reject the sentinel WITHOUT any
        // I/O — no workspace.list round-trip — and with a clean message, not the
        // opaque "no workspace with label 'herdr'" that workspace_id_for produces.
        let b = backend();
        let err = b
            .kill_session(HERDR_SESSION)
            .expect_err("the herdr session must not be killable");
        let msg = format!("{err:#}");
        assert_eq!(msg, HERDR_SESSION_IMMUTABLE_MSG);
        assert!(
            !msg.contains("no workspace with label"),
            "must not leak the opaque workspace-not-found error: {msg}"
        );
    }

    #[test]
    fn rename_session_rejects_the_sentinel_as_clean_ok_false() {
        // rename_session returns Result<ActionAck>, so the sentinel rejection is a
        // clean ok:false ack (not an Err / internal). No I/O is performed.
        let b = backend();
        let ack = b
            .rename_session(HERDR_SESSION, "whatever".to_owned())
            .expect("sentinel rename must be Ok(ack), not Err");
        assert!(!ack.ok, "renaming the herdr session must report ok:false");
        assert_eq!(ack.error.as_deref(), Some(HERDR_SESSION_IMMUTABLE_MSG));
    }

    // ── shared registries are the same Arcs the control client holds ──────────

    #[test]
    fn registries_are_shared_with_control_client() {
        let b = backend();
        assert!(Arc::ptr_eq(b.pane_registry(), b.control.pane_registry()));
        assert!(Arc::ptr_eq(b.tab_registry(), b.control.tab_registry()));
    }
}
