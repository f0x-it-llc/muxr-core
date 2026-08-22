//! Neutral, backend-agnostic domain types spoken by the [`MuxBackend`] trait
//! (P1.01).
//!
//! **This file is `zellij_utils`-free by contract.** Nothing here may import or
//! name a `zellij_utils` type — only [`super::zellij`] (the `ZellijBackend`)
//! translates between these neutral types and zellij's. A future herdr backend
//! (Phase 2) maps the same neutral types onto its own primitives without any
//! zellij coupling.
//!
//! [`MuxBackend`]: super::MuxBackend

// ─── Re-homed neutral types (already zellij-free) ───────────────────────────────

// `ActionAck` and `ScrollDir` are defined in `crate::actions` but carry NO
// zellij types (`ActionAck` is `{ok, error, info}`; `ScrollDir` is a plain
// fieldless enum). Re-export them here so the trait surface depends on
// `crate::multiplexer` rather than `crate::actions` — the eventual home for a
// backend-neutral vocabulary. (A later phase may physically move the
// definitions here; the re-export keeps P1.01 a pure addition.)
pub use crate::actions::{ActionAck, ScrollDir};

// ─── Pane identity ──────────────────────────────────────────────────────────────

/// Neutral pane identity, mirroring the proto `PaneTarget { pane_id, is_plugin }`
/// wire contract.
///
/// The zellij backend maps this to `zellij_utils::data::PaneId` via
/// `crate::actions::pane_id_from_target(id, is_plugin)` (Terminal vs Plugin); a
/// herdr backend would map its opaque `String` pane ids onto the `u32 ↔ String`
/// space internally. Keeping the trait boundary neutral is what lets a second
/// backend avoid speaking zellij's `PaneId` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRef {
    /// The pane's numeric id (terminal or plugin id, disambiguated by `is_plugin`).
    pub id: u32,
    /// `true` for a plugin pane, `false` for a terminal pane.
    pub is_plugin: bool,
}

impl PaneRef {
    /// Construct a terminal `PaneRef`.
    pub fn terminal(id: u32) -> Self {
        Self {
            id,
            is_plugin: false,
        }
    }

    /// Construct a plugin `PaneRef`.
    pub fn plugin(id: u32) -> Self {
        Self {
            id,
            is_plugin: true,
        }
    }
}

// ─── Resize ─────────────────────────────────────────────────────────────────────

/// Neutral resize magnitude (mirrors the proto `ResizeKind` enum that
/// `grpc/pane_ops.rs` converts into `zellij_utils::data::Resize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeKind {
    Increase,
    Decrease,
}

/// Neutral resize direction (mirrors the proto `ResizeDirection` enum that
/// `grpc/pane_ops.rs` converts into `zellij_utils::data::Direction`). The
/// proto's `UNSPECIFIED` is represented by `Option::None` at the call site (a
/// uniform resize), not a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeDir {
    Left,
    Right,
    Up,
    Down,
}

// ─── Fullscreen hint ────────────────────────────────────────────────────────────

/// Resolved floating-pane context for a fullscreen toggle, passed to
/// [`MuxSender::toggle_fullscreen`].
///
/// This is the *resolved* context — the relay (P1.03) derives it either from the
/// client-supplied hint or from a live `pane_is_floating_with_visibility` query
/// before calling the sender, so the sender itself performs no IPC query. It is
/// the neutral mirror of the relay's `FloatingHint`.
///
/// [`MuxSender::toggle_fullscreen`]: super::MuxSender::toggle_fullscreen
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullscreenHint {
    /// The target pane is a floating pane.
    pub is_floating: bool,
    /// Floating panes are currently visible in the target's tab.
    pub floating_visible: bool,
    /// The target is the currently-focused, visible floating pane.
    pub is_focused_floating: bool,
}

// ─── Layout snapshot ────────────────────────────────────────────────────────────

/// A neutral snapshot of a session's tab/pane layout.
///
/// Carries EXACTLY the fields `grpc/layout.rs` extracts from zellij's
/// `ListTabsResponse`/`ListPanesResponse` to build the proto `Layout`, so a
/// later switch (P1.02) to building proto from this snapshot is byte-identical.
///
/// **Raw values only.** The per-relay-client B-FOCUS override (`is_focused` /
/// `active`) and the plugin-pane visibility filter remain gRPC-layer concerns —
/// the snapshot reports zellij's raw queried state, including plugin panes
/// (flagged via [`PaneSnapshot::is_plugin`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutSnapshot {
    pub tabs: Vec<TabSnapshot>,
}

/// A neutral snapshot of a single tab and its panes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabSnapshot {
    /// Stable tab id (`TabInfo.tab_id`). `u64` so it covers both the proto
    /// `tab_id` (`as u32`) and the B-FOCUS `active_tab` comparison (`u64`).
    pub tab_id: u64,
    /// Tab position (`TabInfo.position`).
    pub position: u32,
    /// Tab name.
    pub name: String,
    /// Raw `TabInfo.active` (before any per-relay-client override).
    pub active: bool,
    /// `TabInfo.has_bell_notification`.
    pub has_bell: bool,
    /// `TabInfo.panes_to_hide`.
    pub panes_to_hide: u32,
    /// `TabInfo.is_fullscreen_active`.
    pub fullscreen_active: bool,
    /// `TabInfo.are_floating_panes_visible`.
    pub floating_panes_visible: bool,
    /// Panes belonging to this tab, in `ListPanes` order (plugin panes included).
    pub panes: Vec<PaneSnapshot>,
}

/// A neutral snapshot of a single pane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneSnapshot {
    /// Pane id (`PaneInfo.id`); disambiguated by [`Self::is_plugin`].
    pub id: u32,
    /// Pane title.
    pub title: String,
    /// Raw `PaneInfo.is_focused` (before any per-relay-client override).
    pub is_focused: bool,
    /// `PaneInfo.is_floating`.
    pub is_floating: bool,
    /// `PaneInfo.exited`.
    pub exited: bool,
    /// Foreground command (`PaneListEntry.pane_command`, defaulted to empty).
    pub command: String,
    /// Working directory (`PaneListEntry.pane_cwd`, defaulted to empty).
    pub cwd: String,
    /// `PaneInfo.pane_x`.
    pub x: u32,
    /// `PaneInfo.pane_y`.
    pub y: u32,
    /// `PaneInfo.pane_rows`.
    pub rows: u32,
    /// `PaneInfo.pane_columns`.
    pub cols: u32,
    /// `PaneInfo.is_plugin` — plugin panes are filtered out at the gRPC layer.
    pub is_plugin: bool,
    /// `PaneInfo.is_fullscreen`.
    pub is_fullscreen: bool,
}

// ─── Space snapshot ─────────────────────────────────────────────────────────────

/// A neutral snapshot of one "space" — a switchable in-place sub-navigation unit
/// within a single muxr session.
///
/// Spaces are a herdr-only axis: a herdr daemon is collapsed to ONE muxr session
/// (the `"herdr"` sentinel; see [`HerdrBackend`](super::HerdrBackend)) whose
/// **workspaces** become switchable spaces. zellij has no spaces — its
/// [`MuxBackend::list_spaces`](super::MuxBackend::list_spaces) returns the empty
/// default.
///
/// `id` is the backend's opaque space id (herdr's `workspace_id`); `name` is the
/// display label (herdr's `WorkspaceInfo.label`); `active` reflects the backend's
/// own reported focused/active space (herdr's `WorkspaceInfo.focused`). The
/// *connection's* current space is a per-relay concern marked at the gRPC layer
/// (T03), not here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpaceSnapshot {
    /// Opaque backend space id (herdr `workspace_id`).
    pub id: String,
    /// Display label.
    pub name: String,
    /// The backend's reported active/focused space.
    pub active: bool,
}

// ─── Unknown-space error ────────────────────────────────────────────────────────

/// Typed error: the space (herdr workspace) named by an explicit `space_id` does
/// **not exist** on the backend.
///
/// Kept distinct from every other backend failure so the gRPC layer can answer a
/// space-scoped read with `not_found` instead of a generic `internal` — an
/// unknown id was previously indistinguishable from a real-but-empty space
/// (`OK` + an empty layout) or from an IPC failure.
///
/// It travels inside an [`anyhow::Error`]; callers test for it with
/// [`UnknownSpace::in_chain`] (which looks through any `.context(…)` layers added
/// on the way up), never by string-matching a message. The `Display` text is for
/// **server logs only** — the client-facing status is deliberately terse (a
/// forwarded error chain leaks the herdr socket path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSpace {
    /// The opaque space id the caller asked for. Already charset- and
    /// length-validated at the gRPC boundary (`grpc::space_ops::validate_space_id`).
    pub space_id: String,
}

impl UnknownSpace {
    /// Build the error for `space_id`.
    pub fn new(space_id: impl Into<String>) -> Self {
        Self {
            space_id: space_id.into(),
        }
    }

    /// Whether `err` **is**, or wraps, an [`UnknownSpace`].
    ///
    /// Walks the whole [`anyhow`] chain so a caller that adds context around the
    /// backend error does not silently downgrade the client's `not_found` to an
    /// `internal`.
    pub fn in_chain(err: &anyhow::Error) -> bool {
        err.chain().any(|cause| cause.is::<Self>())
    }
}

impl std::fmt::Display for UnknownSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no such space: '{}'", self.space_id)
    }
}

impl std::error::Error for UnknownSpace {}

// ─── Attach resume hint ─────────────────────────────────────────────────────────

/// A **best-effort** hint naming the view a fresh attach should land on
/// (`AttachReq.resume_*`), so a reconnecting client resumes where it left off
/// instead of snapping to the backend's daemon-global focus.
///
/// Every field is optional, but they are NOT independent: the pane hint is
/// honoured only when the tab hint is also present and the named pane sits in
/// that tab (a pane-only hint is ignored even when it resolves); the tab hint
/// needs no corroboration; the space hint is the durable axis. Resolution falls
/// through pane → tab → space on anything stale, unknown or uncorroborated. An
/// unresolvable hint must NEVER fail the attach — an all-`None` target is
/// exactly today's behavior (the session's currently-focused pane). Read-only
/// attaches drop the entire hint before resolution
/// (`relay::resume_target_for`).
///
/// Spaces/panes/tabs here are addressed with the *neutral* ids the gRPC layer
/// speaks (`SpaceSnapshot::id`, `TabSnapshot::tab_id`, `PaneSnapshot::id`), so
/// the hint round-trips a value the client read from `GetLayout`/`GetSpaces`.
/// Only the herdr backend honours it (see
/// [`MuxBackend::open_attach_with_resume`](super::MuxBackend::open_attach_with_resume)).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumeTarget {
    /// Opaque backend space id (herdr `workspace_id`).
    pub space_id: Option<String>,
    /// Neutral tab id, as emitted by [`TabSnapshot::tab_id`].
    pub tab_id: Option<u64>,
    /// Neutral pane id, as emitted by [`PaneSnapshot::id`].
    pub pane_id: Option<u32>,
}

impl ResumeTarget {
    /// `true` when no hint at all was supplied — the caller then gets today's
    /// behavior and the attach reports no resolved view.
    pub fn is_empty(&self) -> bool {
        self.space_id.is_none() && self.tab_id.is_none() && self.pane_id.is_none()
    }
}

/// The view an attach actually landed on, reported back by a backend that
/// resolved a [`ResumeTarget`] (herdr only).
///
/// The relay seeds its per-connection `RelayViewState` from this so the client's
/// very first `GetLayout` — carrying the freshly-minted `connection_id` — gets a
/// truthful B-FOCUS override (the resumed tab/pane) rather than the backend's
/// raw daemon-global focus flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumedView {
    /// The space (herdr workspace) the attach resolved into.
    pub space_id: String,
    /// Neutral tab id of the resolved pane's tab.
    pub tab_id: u64,
    /// The resolved pane.
    pub pane: PaneRef,
}

// ─── Attach-stream messages ─────────────────────────────────────────────────────

/// A neutral message yielded by [`MuxReceiver::recv`] — the backend-agnostic
/// translation of one server-to-client attach-stream message.
///
/// ## Mapping to zellij's `ServerToClientMsg`
///
/// This is a **1:1, stateless** translation (one input message → one output
/// message), which keeps the eventual relay reader (`render_loop`, P1.03) able
/// to preserve its exact per-message cadence (stop-flag checks, tracing):
///
/// - `Render { content }` → [`Render`](Self::Render)
/// - `Log { lines }` → [`Log`](Self::Log) (one log = one payload)
/// - `Exit`/`RenamedSession`/`ConfigFileUpdated`/`SwitchSession` → [`Event`](Self::Event)
/// - everything else (UnblockInputThread, Connected, …) → [`Other`](Self::Other)
///
/// **Note on the QueryLayout reply shape (deviation, flagged):** today the relay
/// captures *two* consecutive `Log` messages (ListTabs JSON then ListPanes JSON)
/// and pairs them in the render thread. A single neutral `Log(String)` per
/// server `Log` preserves that two-message capture exactly while keeping `recv`
/// a pure stateless translation — a buffered `Log { tabs, panes }` variant would
/// force the receiver to be query-state-aware, which it cannot be. The render
/// thread keeps the tabs-then-panes pairing in P1.03.
/// Structured mouse-event kind forwarded over the relay stream.
///
/// Backend-neutral (no zellij/herdr types): the zellij sender maps it to
/// `Action::MouseEvent` (wheel flags), the herdr sender to
/// `ClientInputEvent::Mouse { ClientMouseKind::Scroll* }`. Only wheel events
/// exist today; press/drag variants can be added without breaking either wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxMouseKind {
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone)]
pub enum MuxServerMsg {
    /// A render frame (ANSI viewport bytes).
    Render(Vec<u8>),
    /// One server `Log` payload (joined lines) — a query reply line
    /// (ListTabs/ListPanes JSON) during a relay-routed `QueryLayout`.
    Log(String),
    /// A control/lifecycle event.
    Event(MuxEvent),
    /// A message with no remote-client semantics (drained/ignored by the relay,
    /// preserving today's per-message loop cadence).
    Other,
}

/// A neutral control/lifecycle event, mirroring the `ServerToClientMsg` variants
/// the relay forwards to the client as `ControlEvent`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuxEvent {
    /// The session exited; `reason` is the human-readable exit reason.
    Exit { reason: String },
    /// The session was renamed to `name`.
    RenamedSession { name: String },
    /// The session's config file was updated.
    ConfigUpdated,
    /// The server asked this client to switch to session `name`.
    SwitchSession { name: String },
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_ref_constructors_set_is_plugin() {
        assert_eq!(
            PaneRef::terminal(7),
            PaneRef {
                id: 7,
                is_plugin: false
            }
        );
        assert_eq!(
            PaneRef::plugin(9),
            PaneRef {
                id: 9,
                is_plugin: true
            }
        );
    }

    #[test]
    fn action_ack_passthrough_is_the_same_type() {
        // Re-exported, not redefined: constructing via the multiplexer path and
        // the actions path yields the identical type.
        let a: ActionAck = ActionAck {
            ok: true,
            error: None,
            info: Some("terminal_3".into()),
        };
        let b: crate::actions::ActionAck = a.clone();
        assert_eq!(a.ok, b.ok);
        assert_eq!(a.info, b.info);
    }

    #[test]
    fn scroll_dir_is_re_exported() {
        // Compiles iff the re-export points at the same enum.
        let _d: ScrollDir = crate::actions::ScrollDir::HalfPageUp;
    }

    #[test]
    fn layout_snapshot_round_trips_through_clone_eq() {
        let snap = LayoutSnapshot {
            tabs: vec![TabSnapshot {
                tab_id: 1,
                position: 0,
                name: "main".into(),
                active: true,
                has_bell: false,
                panes_to_hide: 0,
                fullscreen_active: false,
                floating_panes_visible: false,
                panes: vec![PaneSnapshot {
                    id: 3,
                    title: "zsh".into(),
                    is_focused: true,
                    is_floating: false,
                    exited: false,
                    command: "zsh".into(),
                    cwd: "/home".into(),
                    x: 0,
                    y: 0,
                    rows: 24,
                    cols: 80,
                    is_plugin: false,
                    is_fullscreen: false,
                }],
            }],
        };
        assert_eq!(snap, snap.clone());
    }
}
