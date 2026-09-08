//! herdr **control plane** — a client over herdr's line-delimited JSON-API Unix
//! socket.
//!
//! [`HerdrControl`] performs workspace / tab / pane / layout operations and
//! transcodes herdr's per-tab [`PaneLayoutSnapshot`](super::api::PaneLayoutSnapshot)
//! into the neutral [`LayoutSnapshot`] the rest of muxrd speaks. It is the herdr
//! analogue of the zellij `query::*` + `actions::*` free functions, but every
//! call here is one **connection-per-request** JSON round-trip over the socket.
//!
//! ## Transport
//! Each call opens a fresh [`UnixStream`], writes one `ApiRequest` JSON line,
//! reads one response line, parses it, and drops the connection — mirroring the
//! zellij backend's "ephemeral connection per action" discipline. This avoids
//! shared-connection concurrency hazards and bounds every call: the stream's
//! read **and** write timeouts are set to [`READ_TIMEOUT`], so a wedged or dead
//! herdr can never block the caller indefinitely (P2.03 calls
//! [`HerdrControl::query_layout`] inline on the relay inbound task).
//!
//! ## Two layout paths, one transcode
//! [`HerdrControl::query_layout`] answers from herdr's one-call
//! `session.snapshot` when it can, and from the original
//! `workspace.list` + `tab.list` + `pane.list` + one `pane.layout` per tab
//! **fan-out** ([`HerdrControl::query_layout_fanout`]) when it cannot. Both feed
//! the *same* [`transcode_layout`], and [`layout_from_snapshot`] deliberately
//! reproduces the fan-out's own tab/layout pairing rule, so the two paths cannot
//! drift apart in what they produce — only in how many round trips they cost
//! (1 versus 3 + one per tab). The fan-out is never deleted: it is the only
//! thing that keeps the mobile client's pane tree working when the snapshot
//! method is unavailable or unusable, and entering it is logged as the degraded
//! state it is.
//!
//! ## Id translation
//! The rest of muxrd addresses panes by `u32` and tabs by `u64`; herdr uses
//! opaque `String`s. The shared [`HerdrPaneRegistry`] / [`HerdrTabRegistry`]
//! (owned by the backend, P2.04) translate between them. Action methods take the
//! neutral numeric ids and resolve them to herdr `String`s internally; an unknown
//! id yields a failed [`ActionAck`] rather than an error.
//!
//! ## Errors
//! Transport / protocol failures surface as [`anyhow::Error`]. A herdr **API-level
//! error** response (`{"error":{…}}`) on an action method is mapped to
//! `ActionAck { ok: false, error: Some(msg), .. }`, parallel to the zellij
//! backend's `ActionAck` failure surface.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::multiplexer::types::{
    ActionAck, LayoutSnapshot, PaneSnapshot, TabSnapshot, UnknownSpace,
};

use super::api::{
    ApiErrorBody, ApiRequest, ApiResponseBody, ApiResult, HerdrServerInfo, LayoutDescription,
    PaneCloseParams, PaneDirection, PaneFocusDirectionParams, PaneInfo, PaneLayoutParams,
    PaneLayoutSnapshot, PaneRenameParams, PaneSplitParams, PaneZoomMode, PaneZoomParams,
    PingParams, SessionSnapshot, SessionSnapshotParams, SplitDirection, TabCloseParams,
    TabCreateParams, TabFocusParams, TabInfo, TabRenameParams, WorkspaceCloseParams,
    WorkspaceCreateParams, WorkspaceInfo, WorkspaceListParams, WorkspaceRenameParams,
};
use super::registry::{HerdrPaneRegistry, HerdrTabRegistry};

/// Per-call read/write timeout. herdr is co-located (local Unix socket), so
/// responses arrive in milliseconds; this ceiling exists purely to guarantee
/// bounded I/O. It is well below zellij's 18 s relay query timeout that motivated
/// the synchronous herdr layout path (P2.00).
pub const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Hard ceiling on a single JSON-API response line, mirroring the wire path's
/// pre-allocation guard ([`wire::MAX_FRAME_SIZE`](super::wire::MAX_FRAME_SIZE)).
///
/// The [`READ_TIMEOUT`] only fires on an *idle* gap, so a peer streaming bytes
/// continuously without a newline could grow the response `String` unbounded and
/// OOM muxrd. We bound the read with [`Read::take`] at this ceiling and reject a
/// response that hits it without a line terminator (S1, defence-in-depth).
///
/// 8 MiB is deliberately generous — well above any realistic layout/export
/// response (largest legitimate payload is a `layout.export` tree, kilobytes even
/// for huge workspaces) — while still bounding a hostile stream. It is 4× the
/// wire `MAX_FRAME_SIZE` because the control plane carries whole-workspace JSON
/// snapshots rather than single per-frame terminal output.
pub const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// herdr JSON-API control client. Cheap to construct; holds no live connection.
#[derive(Debug)]
pub struct HerdrControl {
    /// Path to herdr's JSON-API socket (resolved via [`super::paths`]).
    api_socket: PathBuf,
    /// Shared pane-id registry (owned by the backend, P2.04).
    panes: Arc<HerdrPaneRegistry>,
    /// Shared tab-id registry (owned by the backend, P2.04).
    tabs: Arc<HerdrTabRegistry>,
    /// Monotonic JSON-API request-id source.
    next_req_id: AtomicU64,
    /// Per-call read/write timeout.
    read_timeout: Duration,
    /// `true` while [`Self::query_layout`] is serving from the fan-out because
    /// `session.snapshot` failed. Only a *transition* is logged (`warn!` on
    /// entering the degraded state, `info!` on leaving it), so a persistently
    /// broken snapshot method is visible in the log without one `warn!` per
    /// layout poll. Relaxed ordering: this is a logging latch, never a guard.
    snapshot_degraded: AtomicBool,
}

impl HerdrControl {
    /// Construct a control client for the herdr instance at `api_socket`, sharing
    /// the given registries with the wire relay / backend.
    pub fn new(
        api_socket: PathBuf,
        panes: Arc<HerdrPaneRegistry>,
        tabs: Arc<HerdrTabRegistry>,
    ) -> Self {
        Self {
            api_socket,
            panes,
            tabs,
            next_req_id: AtomicU64::new(1),
            read_timeout: READ_TIMEOUT,
            snapshot_degraded: AtomicBool::new(false),
        }
    }

    /// The shared pane registry (so the wire relay can resolve `u32 → terminal_id`).
    pub fn pane_registry(&self) -> &Arc<HerdrPaneRegistry> {
        &self.panes
    }

    /// The shared tab registry. Used by the relay's per-connection tab switch
    /// (`HerdrMuxSender::go_to_tab` → `herdr_tab_id`) to map neutral `u64` tab ids
    /// to herdr's String ids without a daemon-global `tab.focus`.
    pub fn tab_registry(&self) -> &Arc<HerdrTabRegistry> {
        &self.tabs
    }

    // ── Transport ───────────────────────────────────────────────────────────

    /// Next request id, e.g. `"muxrd-7"`.
    fn next_id(&self) -> String {
        format!("muxrd-{}", self.next_req_id.fetch_add(1, Ordering::Relaxed))
    }

    /// One connection-per-request JSON round-trip. Returns the raw envelope body
    /// (success or herdr API error), or an [`anyhow::Error`] for transport /
    /// protocol failures.
    fn call_raw(&self, method: &str, params: serde_json::Value) -> Result<ApiResponseBody> {
        let req = ApiRequest::new(self.next_id(), method, params);
        let mut line =
            serde_json::to_string(&req).with_context(|| format!("serialize herdr {method}"))?;
        line.push('\n');

        let stream = UnixStream::connect(&self.api_socket).with_context(|| {
            format!(
                "connect herdr JSON-API socket {}",
                self.api_socket.display()
            )
        })?;
        stream
            .set_read_timeout(Some(self.read_timeout))
            .context("set herdr socket read timeout")?;
        stream
            .set_write_timeout(Some(self.read_timeout))
            .context("set herdr socket write timeout")?;

        (&stream)
            .write_all(line.as_bytes())
            .with_context(|| format!("write herdr {method} request"))?;

        // S1: bound the response read. `read_line` would otherwise grow `resp`
        // without limit, and the read timeout only trips on an idle gap — a peer
        // trickling bytes forever could OOM muxrd. Cap at MAX_RESPONSE_BYTES via
        // `Read::take` (the control-plane analogue of the wire MAX_FRAME_SIZE guard).
        let mut reader = BufReader::new((&stream).take(MAX_RESPONSE_BYTES));
        let mut resp = String::new();
        let read = reader
            .read_line(&mut resp)
            .with_context(|| format!("read herdr {method} response"))?;
        if read == 0 {
            return Err(anyhow!(
                "herdr closed the JSON-API connection without responding to {method}"
            ));
        }
        // If we filled the cap without reaching a newline, the response is either
        // hostile or malformed — refuse it rather than parse a truncated line.
        if read as u64 >= MAX_RESPONSE_BYTES && !resp.ends_with('\n') {
            return Err(anyhow!(
                "herdr {method} response exceeded the {MAX_RESPONSE_BYTES}-byte \
                 limit without a newline"
            ));
        }

        let raw: super::api::ApiRawResponse = serde_json::from_str(resp.trim_end())
            .with_context(|| format!("parse herdr {method} response"))?;
        Ok(raw.body)
    }

    /// Round-trip a method that returns a typed [`ApiResult`].
    fn call_typed(&self, method: &str, params: serde_json::Value) -> Result<ApiResult> {
        match self.call_raw(method, params)? {
            ApiResponseBody::Ok { result } => serde_json::from_value(result)
                .with_context(|| format!("decode herdr {method} result")),
            ApiResponseBody::Err { error } => Err(anyhow!(
                "herdr {method} error {}: {}",
                error.code,
                error.message
            )),
        }
    }

    /// Round-trip an action method, collapsing the response into an [`ActionAck`].
    /// A herdr API-level error becomes a failed ack (not an `Err`); only transport
    /// failures propagate as `Err`. The success payload is intentionally ignored —
    /// only success/failure matters at the action boundary.
    ///
    /// herdr 0.9.0 added two refusal codes for a close that would take a whole
    /// worktree group down with it: `workspace_group_close_required` from
    /// `workspace.close`, which an ordinary (non-group) `CloseSpace` reaches
    /// whenever the named space is a group primary, and `confirmation_required`
    /// from `tab.close` / `pane.close`, which carry no group flag at all. Both are
    /// recognised and reworded — via [`worktree_group_refusal`] — so the caller
    /// learns *why* (a worktree group) and *how to proceed*, instead of the generic
    /// `"<code>: <message>"` every other API error gets.
    ///
    /// `method` is passed to the rewording because this is the shared error path
    /// for twelve methods and `confirmation_required` is a **generic** herdr code:
    /// re-narrating, say, a `pane.zoom` refusal as a worktree-group close would be
    /// both wrong and a nudge towards a destructive action. Only the methods whose
    /// refusal the wording actually describes are reworded.
    fn call_action(&self, method: &str, params: serde_json::Value) -> Result<ActionAck> {
        match self.call_raw(method, params)? {
            ApiResponseBody::Ok { .. } => Ok(ack_ok()),
            ApiResponseBody::Err { error } => Ok(worktree_group_refusal(method, &error)
                .unwrap_or_else(|| ack_err(format!("{}: {}", error.code, error.message)))),
        }
    }

    fn to_params<P: Serialize>(method: &str, params: P) -> Result<serde_json::Value> {
        serde_json::to_value(params).with_context(|| format!("serialize herdr {method} params"))
    }

    // ── Server discovery ──────────────────────────────────────────────────────

    /// `ping` — discover the connected server's version and wire protocol.
    ///
    /// This is how muxrd learns which protocol version to send in the binary relay
    /// `Hello`. herdr enforces **strict equality** on that handshake — it rejects
    /// clients that are older *or* newer than itself — so the version must be
    /// discovered per connection rather than compiled in. The JSON-API socket
    /// queried here is stable across herdr releases; `ping` has reported
    /// `protocol` since at least 0.7.1.
    pub fn ping(&self) -> Result<HerdrServerInfo> {
        let params = Self::to_params("ping", PingParams {})?;
        match self.call_typed("ping", params)? {
            ApiResult::Pong {
                version, protocol, ..
            } => Ok(HerdrServerInfo { version, protocol }),
            other => Err(unexpected("ping", &other)),
        }
    }

    // ── Workspace lifecycle ───────────────────────────────────────────────────

    /// `workspace.list` — all herdr workspaces (muxrd's "sessions").
    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let params = Self::to_params("workspace.list", WorkspaceListParams {})?;
        match self.call_typed("workspace.list", params)? {
            ApiResult::WorkspaceList { workspaces } => Ok(workspaces),
            other => Err(unexpected("workspace.list", &other)),
        }
    }

    /// `workspace.create` — create and focus a new workspace.
    pub fn create_workspace(&self, label: Option<String>) -> Result<ActionAck> {
        let params = Self::to_params(
            "workspace.create",
            WorkspaceCreateParams {
                focus: true,
                label,
                ..Default::default()
            },
        )?;
        self.call_action("workspace.create", params)
    }

    /// `workspace.rename`.
    pub fn rename_workspace(&self, workspace_id: &str, label: &str) -> Result<ActionAck> {
        let params = Self::to_params(
            "workspace.rename",
            WorkspaceRenameParams {
                workspace_id: workspace_id.to_string(),
                label: label.to_string(),
            },
        )?;
        self.call_action("workspace.rename", params)
    }

    /// `workspace.close`.
    ///
    /// Group intent is the **caller's** to express: `close_group` goes on the wire
    /// exactly as given, and every muxrd caller that has no group intent passes
    /// `false` (herdr's own default). A group close is destructive well beyond the
    /// workspace the caller named — it takes every linked-worktree workspace of the
    /// group with it — so it is never inferred from the fact that a workspace was
    /// named.
    ///
    /// With `false`, herdr refuses a worktree-group **primary** with
    /// `workspace_group_close_required` rather than closing the group. That refusal
    /// is by design and is surfaced to the operator (see [`worktree_group_refusal`]);
    /// it is what tells them a group exists at all. The flag is inert on an ordinary
    /// workspace, or on a linked worktree closed on its own.
    pub fn close_workspace(&self, workspace_id: &str, close_group: bool) -> Result<ActionAck> {
        let params = Self::to_params(
            "workspace.close",
            WorkspaceCloseParams {
                workspace_id: workspace_id.to_string(),
                close_group,
            },
        )?;
        self.call_action("workspace.close", params)
    }

    // ── Tab lifecycle ──────────────────────────────────────────────────────────

    /// `tab.create` in the given workspace (or the focused one when `None`).
    pub fn create_tab(
        &self,
        workspace_id: Option<&str>,
        label: Option<String>,
    ) -> Result<ActionAck> {
        let params = Self::to_params(
            "tab.create",
            TabCreateParams {
                workspace_id: workspace_id.map(str::to_string),
                focus: true,
                label,
                ..Default::default()
            },
        )?;
        self.call_action("tab.create", params)
    }

    /// `tab.focus` — resolves the neutral `u64` tab id via the tab registry.
    pub fn focus_tab(&self, tab_id: u64) -> Result<ActionAck> {
        let Some(herdr_tab) = self.tabs.herdr_tab_id(tab_id) else {
            return Ok(unknown_tab(tab_id));
        };
        let params = Self::to_params("tab.focus", TabFocusParams { tab_id: herdr_tab })?;
        self.call_action("tab.focus", params)
    }

    /// `tab.close`.
    pub fn close_tab(&self, tab_id: u64) -> Result<ActionAck> {
        let Some(herdr_tab) = self.tabs.herdr_tab_id(tab_id) else {
            return Ok(unknown_tab(tab_id));
        };
        let params = Self::to_params("tab.close", TabCloseParams { tab_id: herdr_tab })?;
        self.call_action("tab.close", params)
    }

    /// `tab.rename`.
    pub fn rename_tab(&self, tab_id: u64, label: String) -> Result<ActionAck> {
        let Some(herdr_tab) = self.tabs.herdr_tab_id(tab_id) else {
            return Ok(unknown_tab(tab_id));
        };
        let params = Self::to_params(
            "tab.rename",
            TabRenameParams {
                tab_id: herdr_tab,
                label,
            },
        )?;
        self.call_action("tab.rename", params)
    }

    // ── Pane lifecycle ─────────────────────────────────────────────────────────

    /// `pane.split` — split `target` (or the focused pane when `None`).
    pub fn split_pane(
        &self,
        workspace_id: Option<&str>,
        target: Option<u32>,
        direction: SplitDirection,
        focus: bool,
    ) -> Result<ActionAck> {
        let target_pane_id = match self.resolve_opt_pane(target) {
            Ok(p) => p,
            Err(ack) => return Ok(ack),
        };
        let params = Self::to_params(
            "pane.split",
            PaneSplitParams {
                workspace_id: workspace_id.map(str::to_string),
                target_pane_id,
                direction,
                ratio: None,
                cwd: None,
                focus,
                env: HashMap::new(),
            },
        )?;
        self.call_action("pane.split", params)
    }

    /// `pane.close`.
    pub fn close_pane(&self, pane: u32) -> Result<ActionAck> {
        let Some(pane_id) = self.panes.herdr_pane_id(pane) else {
            return Ok(unknown_pane(pane));
        };
        let params = Self::to_params("pane.close", PaneCloseParams { pane_id })?;
        self.call_action("pane.close", params)
    }

    /// `pane.rename`.
    pub fn rename_pane(&self, pane: u32, label: Option<String>) -> Result<ActionAck> {
        let Some(pane_id) = self.panes.herdr_pane_id(pane) else {
            return Ok(unknown_pane(pane));
        };
        let params = Self::to_params("pane.rename", PaneRenameParams { pane_id, label })?;
        self.call_action("pane.rename", params)
    }

    /// `pane.focus_direction` — directional focus from `pane` (or the focused pane).
    #[allow(dead_code)] // Phase 3: directional pane focus not yet wired to a trait method
    pub fn focus_pane_direction(
        &self,
        pane: Option<u32>,
        direction: PaneDirection,
    ) -> Result<ActionAck> {
        let pane_id = match self.resolve_opt_pane(pane) {
            Ok(p) => p,
            Err(ack) => return Ok(ack),
        };
        let params = Self::to_params(
            "pane.focus_direction",
            PaneFocusDirectionParams { pane_id, direction },
        )?;
        self.call_action("pane.focus_direction", params)
    }

    /// `pane.zoom` — herdr's analogue of zellij pane fullscreen.
    pub fn zoom_pane(&self, pane: Option<u32>, mode: PaneZoomMode) -> Result<ActionAck> {
        let pane_id = match self.resolve_opt_pane(pane) {
            Ok(p) => p,
            Err(ack) => return Ok(ack),
        };
        let params = Self::to_params("pane.zoom", PaneZoomParams { pane_id, mode })?;
        self.call_action("pane.zoom", params)
    }

    // ── Read-only queries ──────────────────────────────────────────────────────

    /// `tab.list` for a workspace.
    pub fn list_tabs(&self, workspace_id: &str) -> Result<Vec<TabInfo>> {
        let params = Self::to_params(
            "tab.list",
            WorkspaceScopedParams {
                workspace_id: Some(workspace_id),
            },
        )?;
        match self.call_typed("tab.list", params)? {
            ApiResult::TabList { tabs } => Ok(tabs),
            other => Err(unexpected("tab.list", &other)),
        }
    }

    /// `pane.list` for a workspace — the call that yields each pane's
    /// `terminal_id` (needed to populate the pane registry for the wire relay).
    pub fn list_panes(&self, workspace_id: &str) -> Result<Vec<PaneInfo>> {
        let params = Self::to_params(
            "pane.list",
            WorkspaceScopedParams {
                workspace_id: Some(workspace_id),
            },
        )?;
        match self.call_typed("pane.list", params)? {
            ApiResult::PaneList { panes } => Ok(panes),
            other => Err(unexpected("pane.list", &other)),
        }
    }

    /// `pane.layout` — absolute-cell layout of the tab containing `pane_id`
    /// (or the focused tab when `None`).
    pub fn pane_layout(&self, pane_id: Option<&str>) -> Result<PaneLayoutSnapshot> {
        let params = Self::to_params(
            "pane.layout",
            PaneLayoutParams {
                pane_id: pane_id.map(str::to_string),
            },
        )?;
        match self.call_typed("pane.layout", params)? {
            ApiResult::PaneLayout { layout } => Ok(layout),
            other => Err(unexpected("pane.layout", &other)),
        }
    }

    /// `layout.export` — recursive layout description for a tab/pane.
    #[allow(dead_code)] // Phase 3: full layout-tree export not yet surfaced to the gRPC layer
    pub fn layout_export(
        &self,
        tab_id: Option<&str>,
        pane_id: Option<&str>,
    ) -> Result<LayoutDescription> {
        let params = Self::to_params(
            "layout.export",
            super::api::LayoutExportParams {
                tab_id: tab_id.map(str::to_string),
                pane_id: pane_id.map(str::to_string),
            },
        )?;
        match self.call_typed("layout.export", params)? {
            ApiResult::LayoutExport { layout } => Ok(*layout),
            other => Err(unexpected("layout.export", &other)),
        }
    }

    /// `session.snapshot` — the whole daemon (workspaces, tabs, panes and one
    /// layout per tab) in a single round trip.
    ///
    /// Present since herdr 0.9.0, which is also this module's protocol floor
    /// ([`HERDR_MIN_PROTOCOL`](super::wire::HERDR_MIN_PROTOCOL) = 22). A server
    /// that does not know the method answers a `method_not_found` API error,
    /// which [`call_typed`](Self::call_typed) turns into an `Err` — exactly what
    /// [`query_layout`](Self::query_layout) needs to fall back on.
    pub fn session_snapshot(&self) -> Result<SessionSnapshot> {
        let params = Self::to_params("session.snapshot", SessionSnapshotParams {})?;
        match self.call_typed("session.snapshot", params)? {
            ApiResult::SessionSnapshot { snapshot } => Ok(*snapshot),
            other => Err(unexpected("session.snapshot", &other)),
        }
    }

    /// Build the neutral [`LayoutSnapshot`] for a workspace (muxrd "session").
    ///
    /// Tries herdr's one-call [`session_snapshot`](Self::session_snapshot) first
    /// (1 round trip) and falls back to the original
    /// [`query_layout_fanout`](Self::query_layout_fanout) (3 + one per tab) on
    /// **any** failure of that path. The fallback is deliberate and permanent:
    /// this read serves the mobile client's pane tree, so it is the wrong place
    /// to have no second option.
    ///
    /// Two outcomes send us to the fan-out, and they are *not* the same thing:
    ///
    /// - the snapshot call itself failed (transport, API error, unparseable
    ///   body) — a **degraded** state, logged `warn!` on entry and `info!` on
    ///   recovery so a persistent fallback is visible without one `warn!` per
    ///   poll;
    /// - the snapshot parsed but carries no such workspace. The snapshot method
    ///   is healthy; we still re-ask through the fan-out rather than concluding
    ///   `not_found` here, because the fan-out is the path whose
    ///   [`UnknownSpace`] answer the gRPC layer's `not_found` contract is built
    ///   on, and re-deriving it from `workspace.list` costs nothing on an error
    ///   path that is already rare.
    ///
    /// **Existence:** either path resolves `workspace_id` against the
    /// daemon's own workspace list; an id that names no live workspace fails
    /// with [`UnknownSpace`] rather than answering `Ok` with an empty layout.
    /// That distinction is what lets the gRPC layer answer a space-scoped
    /// `GetLayout` with `not_found`, and it is also the failure the `CloseSpace`
    /// client-recovery contract expects when a relay polls a workspace that was
    /// closed underneath it.
    pub fn query_layout(&self, workspace_id: &str) -> Result<LayoutSnapshot> {
        match self.session_snapshot() {
            Ok(snapshot) => {
                match layout_from_snapshot(&snapshot, workspace_id, &self.panes, &self.tabs) {
                    Some(layout) => {
                        self.note_snapshot_ok();
                        log::debug!(
                            "herdr: layout for workspace {workspace_id} served from session.snapshot \
                             (herdr {} protocol {}, {} tab(s))",
                            snapshot.version,
                            snapshot.protocol,
                            layout.tabs.len()
                        );
                        return Ok(layout);
                    }
                    None => {
                        // The snapshot itself is fine — it simply does not list
                        // this workspace. Not a degradation; let the fan-out
                        // produce the authoritative `UnknownSpace`.
                        self.note_snapshot_ok();
                        log::debug!(
                            "herdr: session.snapshot lists no workspace {workspace_id} — \
                             re-checking through the layout fan-out"
                        );
                    }
                }
            }
            Err(e) => self.note_snapshot_degraded(&format!("{e:#}")),
        }
        self.query_layout_fanout(workspace_id)
    }

    /// Latch the snapshot path as healthy, reporting a recovery once.
    fn note_snapshot_ok(&self) {
        if self.snapshot_degraded.swap(false, Ordering::Relaxed) {
            log::info!(
                "herdr: session.snapshot is working again — layout queries are back on the \
                 one-call path"
            );
        }
    }

    /// Latch the snapshot path as degraded, reporting the *transition* once at
    /// `warn!` and every repeat at `debug!`.
    fn note_snapshot_degraded(&self, cause: &str) {
        if self.snapshot_degraded.swap(true, Ordering::Relaxed) {
            log::debug!("herdr: still falling back to the layout fan-out ({cause})");
        } else {
            log::warn!(
                "herdr: session.snapshot unusable — falling back to the layout fan-out, so every \
                 layout query now costs 3 + one-per-tab round trips until it recovers ({cause})"
            );
        }
    }

    /// The original layout fan-out, kept as [`query_layout`](Self::query_layout)'s
    /// fallback.
    ///
    /// Fetches the workspace's tabs (`tab.list`), its panes with `terminal_id`s
    /// (`pane.list`), and one absolute-cell layout per tab (`pane.layout`),
    /// populating the shared registries and transcoding into the neutral shape.
    fn query_layout_fanout(&self, workspace_id: &str) -> Result<LayoutSnapshot> {
        // M2: the per-session active tab comes from the workspace's own
        // `WorkspaceInfo.active_tab_id`, NOT from `TabInfo.focused`. herdr's
        // `TabInfo.focused` is *globally* unique — true only for the one tab of
        // herdr's currently-active workspace — so for any other workspace every
        // tab would report `focused=false`, leaving the snapshot with no active
        // tab. Resolve the workspace's own active tab id here (one extra
        // `workspace.list` round-trip; cheap over the local socket).
        //
        // The same lookup doubles as the EXISTENCE check: no matching workspace
        // means the caller named an id that does not exist, so we fail with the
        // typed `UnknownSpace` instead of the former `unwrap_or_default()` — which
        // produced an empty-tab-id snapshot indistinguishable from a real space.
        let active_tab_id = self
            .list_workspaces()?
            .into_iter()
            .find(|w| w.workspace_id == workspace_id)
            .map(|w| w.active_tab_id)
            .ok_or_else(|| anyhow::Error::new(UnknownSpace::new(workspace_id)))?;

        let tabs = self.list_tabs(workspace_id)?;
        let panes = self.list_panes(workspace_id)?;

        // Fetch one PaneLayoutSnapshot per tab, keyed by herdr tab_id. We pick a
        // representative pane from each tab (pane.layout addresses a tab by one of
        // its panes); a tab with no panes simply has no geometry snapshot.
        let mut tab_layouts: HashMap<String, PaneLayoutSnapshot> = HashMap::new();
        let mut representative: HashMap<&str, &str> = HashMap::new();
        for pane in &panes {
            representative
                .entry(pane.tab_id.as_str())
                .or_insert(pane.pane_id.as_str());
        }
        for tab in &tabs {
            if tab_layouts.contains_key(&tab.tab_id) {
                continue;
            }
            if let Some(pane_id) = representative.get(tab.tab_id.as_str()) {
                let layout = self.pane_layout(Some(pane_id))?;
                tab_layouts.insert(layout.tab_id.clone(), layout);
            }
        }

        Ok(transcode_layout(
            &tabs,
            &panes,
            &tab_layouts,
            &active_tab_id,
            &self.panes,
            &self.tabs,
        ))
    }

    /// Resolve an optional neutral pane id to an optional herdr `pane_id`. `None`
    /// passes through (targets the focused pane); an unknown id is reported as a
    /// failed [`ActionAck`].
    fn resolve_opt_pane(
        &self,
        pane: Option<u32>,
    ) -> std::result::Result<Option<String>, ActionAck> {
        match pane {
            None => Ok(None),
            Some(id) => self
                .panes
                .herdr_pane_id(id)
                .map(Some)
                .ok_or_else(|| unknown_pane(id)),
        }
    }
}

// ─── Layout transcode (pure, fixture-testable) ────────────────────────────────

/// Workspace-scoped query params shared by `tab.list` / `pane.list`. Authored
/// locally (P2.01's `api.rs` does not model these list params) so `api.rs` stays
/// untouched by this task.
#[derive(Debug, Serialize)]
struct WorkspaceScopedParams<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<&'a str>,
}

/// Project one workspace out of a whole-daemon [`SessionSnapshot`] and transcode
/// it into the neutral [`LayoutSnapshot`], or `None` when the snapshot lists no
/// such workspace. Pure (no I/O) so the snapshot path is fixture-testable
/// against the fan-out it replaces.
///
/// ### Why this reproduces the fan-out's pairing rule rather than improving on it
///
/// The fan-out addresses `pane.layout` through a *representative pane* of each
/// tab, so a tab with no panes never gets a geometry snapshot at all. A whole-
/// daemon snapshot may well carry a layout for such a tab. Honouring it would
/// make this path answer `fullscreen_active` from real geometry where the
/// fan-out answers `false` — a strictly better answer, and a **divergence**
/// between two paths that are supposed to be interchangeable. Since the tab has
/// no panes, the "better" answer describes nothing a client can render, so the
/// representative-pane rule is reproduced here verbatim (`representative` below)
/// and equivalence is preserved by construction rather than by test coverage
/// alone.
///
/// Tab and pane order is taken from the snapshot as herdr emits it, exactly as
/// the fan-out takes it from `tab.list` / `pane.list`; neither path sorts, and
/// `TabSnapshot.position` (herdr's `TabInfo.number`) remains the ordering key a
/// consumer should use.
fn layout_from_snapshot(
    snapshot: &SessionSnapshot,
    workspace_id: &str,
    pane_reg: &HerdrPaneRegistry,
    tab_reg: &HerdrTabRegistry,
) -> Option<LayoutSnapshot> {
    // Same source of truth as the fan-out: the workspace's OWN active tab, never
    // herdr's globally-unique `TabInfo.focused` (see `query_layout_fanout`).
    // Absence here is the existence check.
    let active_tab_id = snapshot
        .workspaces
        .iter()
        .find(|w| w.workspace_id == workspace_id)
        .map(|w| w.active_tab_id.as_str())?;

    let tabs: Vec<TabInfo> = snapshot
        .tabs
        .iter()
        .filter(|t| t.workspace_id == workspace_id)
        .cloned()
        .collect();
    let panes: Vec<PaneInfo> = snapshot
        .panes
        .iter()
        .filter(|p| p.workspace_id == workspace_id)
        .cloned()
        .collect();

    // The fan-out's rule: a tab is given geometry only when it has a pane to
    // address `pane.layout` through.
    let mut representative: HashSet<&str> = HashSet::with_capacity(tabs.len());
    for pane in &panes {
        representative.insert(pane.tab_id.as_str());
    }
    let mut tab_layouts: HashMap<String, PaneLayoutSnapshot> = HashMap::new();
    for layout in &snapshot.layouts {
        if layout.workspace_id == workspace_id && representative.contains(layout.tab_id.as_str()) {
            tab_layouts
                .entry(layout.tab_id.clone())
                .or_insert_with(|| layout.clone());
        }
    }

    Some(transcode_layout(
        &tabs,
        &panes,
        &tab_layouts,
        active_tab_id,
        pane_reg,
        tab_reg,
    ))
}

/// Transcode herdr's per-tab layout + pane metadata into the neutral
/// [`LayoutSnapshot`]. Pure (no I/O) so it is unit-testable with JSON fixtures.
///
/// Every pane in `panes` is registered first (so `terminal_id` is recorded for
/// the wire relay even for tabs whose geometry was not fetched); geometry then
/// comes from each tab's [`PaneLayoutSnapshot`].
///
/// ### Neutral field sources
/// | `TabSnapshot` field | herdr source |
/// |---|---|
/// | `tab_id` | tab registry id for `TabInfo.tab_id` |
/// | `position` | `TabInfo.number` |
/// | `name` | `TabInfo.label` |
/// | `active` | `TabInfo.tab_id == WorkspaceInfo.active_tab_id` (per-workspace; **not** the global `TabInfo.focused`) |
/// | `fullscreen_active` | tab layout `zoomed` |
/// | `has_bell` / `panes_to_hide` / `floating_panes_visible` | `false` / `0` / `false` (herdr lacks) |
///
/// | `PaneSnapshot` field | herdr source |
/// |---|---|
/// | `id` | pane registry id for `PaneLayoutPane.pane_id` |
/// | `x` / `y` | `PaneLayoutRect.x` / `.y` |
/// | `rows` / `cols` | `PaneLayoutRect.height` / `.width` |
/// | `is_focused` | `PaneLayoutPane.focused` |
/// | `is_fullscreen` | tab layout `zoomed` |
/// | `title` | `PaneInfo.title` ?? `PaneInfo.label` ?? `""` |
/// | `cwd` | `PaneInfo.cwd` ?? `""` |
/// | `command` | `""` (herdr `PaneInfo` carries no foreground command string) |
/// | `is_plugin` / `is_floating` / `exited` | `false` (herdr has no plugin/floating/exited concept here) |
fn transcode_layout(
    tabs: &[TabInfo],
    panes: &[PaneInfo],
    tab_layouts: &HashMap<String, PaneLayoutSnapshot>,
    active_tab_id: &str,
    pane_reg: &HerdrPaneRegistry,
    tab_reg: &HerdrTabRegistry,
) -> LayoutSnapshot {
    // Register every pane up front so terminal_id is known regardless of which
    // tab's geometry we fetched, and build a pane_id → PaneInfo lookup.
    let mut info_by_pane: HashMap<&str, &PaneInfo> = HashMap::with_capacity(panes.len());
    for pane in panes {
        pane_reg.assign_or_get(&pane.pane_id, &pane.terminal_id);
        info_by_pane.insert(pane.pane_id.as_str(), pane);
    }

    let tab_snaps = tabs
        .iter()
        .map(|tab| {
            let tab_id = tab_reg.assign_or_get(&tab.tab_id);
            let layout = tab_layouts.get(&tab.tab_id);
            let zoomed = layout.map(|l| l.zoomed).unwrap_or(false);

            let pane_snaps = layout
                .map(|l| {
                    l.panes
                        .iter()
                        .map(|lp| {
                            let info = info_by_pane.get(lp.pane_id.as_str()).copied();
                            let terminal_id = info.map(|i| i.terminal_id.as_str()).unwrap_or("");
                            let id = pane_reg.assign_or_get(&lp.pane_id, terminal_id);
                            let title = info
                                .and_then(|i| i.title.clone().or_else(|| i.label.clone()))
                                .unwrap_or_default();
                            let cwd = info.and_then(|i| i.cwd.clone()).unwrap_or_default();
                            PaneSnapshot {
                                id,
                                title,
                                is_focused: lp.focused,
                                is_floating: false,
                                exited: false,
                                command: String::new(),
                                cwd,
                                x: lp.rect.x as u32,
                                y: lp.rect.y as u32,
                                rows: lp.rect.height as u32,
                                cols: lp.rect.width as u32,
                                is_plugin: false,
                                is_fullscreen: zoomed,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            TabSnapshot {
                tab_id,
                position: tab.number as u32,
                name: tab.label.clone(),
                // M2: per-workspace active tab, not herdr's global `TabInfo.focused`.
                active: tab.tab_id == active_tab_id,
                has_bell: false,
                panes_to_hide: 0,
                fullscreen_active: zoomed,
                floating_panes_visible: false,
                panes: pane_snaps,
            }
        })
        .collect();

    LayoutSnapshot { tabs: tab_snaps }
}

// ─── ActionAck helpers ────────────────────────────────────────────────────────

fn ack_ok() -> ActionAck {
    ActionAck {
        ok: true,
        error: None,
        info: None,
    }
}

fn ack_err(message: String) -> ActionAck {
    ActionAck {
        ok: false,
        error: Some(message),
        info: None,
    }
}

fn unknown_pane(id: u32) -> ActionAck {
    ack_err(format!("unknown herdr pane id {id}"))
}

fn unknown_tab(id: u64) -> ActionAck {
    ack_err(format!("unknown herdr tab id {id}"))
}

fn unexpected(method: &str, result: &ApiResult) -> anyhow::Error {
    anyhow!("herdr {method} returned unexpected result: {result:?}")
}

/// Reword herdr's two worktree-group-close refusal codes (new in 0.9.0) into an
/// [`ActionAck`] that names the cause and the fix, or `None` — so
/// [`HerdrControl::call_action`] falls back to the generic `"<code>: <message>"`
/// shape — for any other code, **and for any other method**.
///
/// The match is on `(method, code)`, not the code alone: `call_action` is the
/// shared error path for twelve methods and `confirmation_required` is a generic
/// herdr code, so a refusal from an unrelated method (`pane.zoom`, `tab.rename`, …)
/// must keep its own wording rather than be re-narrated as a worktree-group close.
fn worktree_group_refusal(method: &str, error: &ApiErrorBody) -> Option<ActionAck> {
    match (method, error.code.as_str()) {
        // `workspace.close` on a worktree-group primary without `close_group`.
        // This is an EXPECTED path, not defence-in-depth: `close_workspace` sends
        // the caller's own flag, which is `false` by default, so every close of a
        // group primary that did not ask for a group close lands here. Surfacing it
        // is the point — it is how the operator learns the space has linked
        // worktrees and that removing them takes a second, explicit request.
        ("workspace.close", "workspace_group_close_required") => Some(ack_err(format!(
            "this space is a worktree-group primary with linked worktree spaces — \
             closing it alone is not possible; re-issue CloseSpace with group intent \
             (close_group=true) to close the whole group ({})",
            error.message
        ))),
        // `tab.close` / `pane.close` on the last tab/pane of a worktree-group
        // primary: closing it would close the workspace, and with it the group.
        // Neither method has a flag to override this, so the way through is a
        // CloseSpace that asks for the group explicitly — plain CloseSpace refuses
        // the primary exactly as this does.
        ("tab.close" | "pane.close", "confirmation_required") => Some(ack_err(format!(
            "this would close the last of its space, and that space is a \
             worktree-group primary — close the space with group intent \
             (CloseSpace with close_group=true) to remove the group ({})",
            error.message
        ))),
        _ => None,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::herdr::api::{
        AgentStatus, PaneLayoutPane, PaneLayoutRect, PaneLayoutSnapshot,
    };

    fn tab(tab_id: &str, number: usize, label: &str, focused: bool) -> TabInfo {
        serde_json::from_value(serde_json::json!({
            "tab_id": tab_id,
            "workspace_id": "ws-1",
            "number": number,
            "label": label,
            "focused": focused,
            "pane_count": 1,
            "agent_status": "idle",
        }))
        .expect("TabInfo fixture")
    }

    fn pane(pane_id: &str, terminal_id: &str, tab_id: &str, title: Option<&str>) -> PaneInfo {
        serde_json::from_value(serde_json::json!({
            "pane_id": pane_id,
            "terminal_id": terminal_id,
            "workspace_id": "ws-1",
            "tab_id": tab_id,
            "focused": false,
            "title": title,
            "cwd": "/home/u",
            "agent_status": "idle",
            "state_labels": {},
            "revision": 1,
        }))
        .expect("PaneInfo fixture")
    }

    fn layout(tab_id: &str, zoomed: bool, panes: Vec<PaneLayoutPane>) -> PaneLayoutSnapshot {
        PaneLayoutSnapshot {
            workspace_id: "ws-1".into(),
            tab_id: tab_id.into(),
            zoomed,
            area: PaneLayoutRect {
                x: 0,
                y: 0,
                width: 220,
                height: 50,
            },
            focused_pane_id: panes
                .iter()
                .find(|p| p.focused)
                .map(|p| p.pane_id.clone())
                .unwrap_or_default(),
            panes,
            splits: vec![],
        }
    }

    fn lp(pane_id: &str, focused: bool, rect: PaneLayoutRect) -> PaneLayoutPane {
        PaneLayoutPane {
            pane_id: pane_id.into(),
            focused,
            rect,
        }
    }

    #[test]
    fn transcode_maps_two_pane_tab_to_neutral_snapshot() {
        let pane_reg = HerdrPaneRegistry::new();
        let tab_reg = HerdrTabRegistry::new();

        let tabs = vec![tab("tab-1", 0, "main", true)];
        let panes = vec![
            pane("pane-left", "term-l", "tab-1", Some("editor")),
            pane("pane-right", "term-r", "tab-1", None),
        ];
        let mut tab_layouts = HashMap::new();
        tab_layouts.insert(
            "tab-1".to_string(),
            layout(
                "tab-1",
                false,
                vec![
                    lp(
                        "pane-left",
                        true,
                        PaneLayoutRect {
                            x: 0,
                            y: 0,
                            width: 110,
                            height: 50,
                        },
                    ),
                    lp(
                        "pane-right",
                        false,
                        PaneLayoutRect {
                            x: 110,
                            y: 0,
                            width: 110,
                            height: 50,
                        },
                    ),
                ],
            ),
        );

        let snap = transcode_layout(&tabs, &panes, &tab_layouts, "tab-1", &pane_reg, &tab_reg);

        assert_eq!(snap.tabs.len(), 1);
        let t = &snap.tabs[0];
        assert_eq!(t.name, "main");
        assert_eq!(t.position, 0);
        assert!(t.active);
        assert!(!t.fullscreen_active);
        assert_eq!(t.panes.len(), 2);

        let left = &t.panes[0];
        assert_eq!(left.title, "editor");
        assert_eq!(left.cwd, "/home/u");
        assert!(left.is_focused);
        assert_eq!((left.x, left.y, left.cols, left.rows), (0, 0, 110, 50));
        assert!(!left.is_plugin && !left.is_floating && !left.exited);

        let right = &t.panes[1];
        assert_eq!(right.title, ""); // no title, no label
        assert_eq!(right.x, 110);
        assert!(!right.is_focused);

        // Registry was populated: neutral ids round-trip to herdr/terminal ids.
        assert_eq!(
            pane_reg.herdr_pane_id(left.id).as_deref(),
            Some("pane-left")
        );
        assert_eq!(pane_reg.terminal_id(left.id).as_deref(), Some("term-l"));
        assert_eq!(
            pane_reg.herdr_pane_id(right.id).as_deref(),
            Some("pane-right")
        );
        assert_eq!(pane_reg.terminal_id(right.id).as_deref(), Some("term-r"));

        // Tab id round-trips too.
        assert_eq!(tab_reg.herdr_tab_id(t.tab_id).as_deref(), Some("tab-1"));
    }

    #[test]
    fn transcode_marks_zoomed_tab_fullscreen() {
        let pane_reg = HerdrPaneRegistry::new();
        let tab_reg = HerdrTabRegistry::new();
        let tabs = vec![tab("tab-z", 1, "zoom", false)];
        let panes = vec![pane("pane-a", "term-a", "tab-z", Some("vim"))];
        let mut tab_layouts = HashMap::new();
        tab_layouts.insert(
            "tab-z".to_string(),
            layout(
                "tab-z",
                true,
                vec![lp(
                    "pane-a",
                    true,
                    PaneLayoutRect {
                        x: 0,
                        y: 0,
                        width: 220,
                        height: 50,
                    },
                )],
            ),
        );

        let snap = transcode_layout(&tabs, &panes, &tab_layouts, "tab-z", &pane_reg, &tab_reg);
        assert!(snap.tabs[0].fullscreen_active);
        assert!(snap.tabs[0].panes[0].is_fullscreen);
    }

    #[test]
    fn transcode_active_tab_uses_workspace_active_tab_id_not_global_focus() {
        // M2 regression: this workspace is NOT herdr's globally-active one, so every
        // `TabInfo.focused` is false (herdr's single global focus lives on a
        // different workspace). The workspace's own `active_tab_id` still names its
        // active tab — and that, not `TabInfo.focused`, must drive `active`.
        let pane_reg = HerdrPaneRegistry::new();
        let tab_reg = HerdrTabRegistry::new();
        let tabs = vec![
            tab("tab-1", 0, "one", false), // focused == false (global focus elsewhere)
            tab("tab-2", 1, "two", false), // focused == false too
        ];
        let panes = vec![
            pane("pane-1", "term-1", "tab-1", None),
            pane("pane-2", "term-2", "tab-2", None),
        ];
        let rect = PaneLayoutRect {
            x: 0,
            y: 0,
            width: 220,
            height: 50,
        };
        let mut tab_layouts = HashMap::new();
        tab_layouts.insert(
            "tab-1".to_string(),
            layout("tab-1", false, vec![lp("pane-1", true, rect)]),
        );
        tab_layouts.insert(
            "tab-2".to_string(),
            layout("tab-2", false, vec![lp("pane-2", true, rect)]),
        );

        // The workspace reports `active_tab_id == "tab-2"`.
        let snap = transcode_layout(&tabs, &panes, &tab_layouts, "tab-2", &pane_reg, &tab_reg);

        assert_eq!(snap.tabs.len(), 2);
        assert!(
            !snap.tabs[0].active,
            "tab-1 is not the workspace's active tab"
        );
        assert!(
            snap.tabs[1].active,
            "tab-2 == WorkspaceInfo.active_tab_id → exactly this tab is active despite focused=false"
        );
    }

    #[test]
    fn transcode_tab_without_layout_has_no_panes_but_registers_terminal_ids() {
        let pane_reg = HerdrPaneRegistry::new();
        let tab_reg = HerdrTabRegistry::new();
        // Two tabs, but only the first tab's geometry was fetched.
        let tabs = vec![tab("tab-1", 0, "one", true), tab("tab-2", 1, "two", false)];
        let panes = vec![
            pane("pane-1", "term-1", "tab-1", None),
            pane("pane-2", "term-2", "tab-2", None),
        ];
        let mut tab_layouts = HashMap::new();
        tab_layouts.insert(
            "tab-1".to_string(),
            layout(
                "tab-1",
                false,
                vec![lp(
                    "pane-1",
                    true,
                    PaneLayoutRect {
                        x: 0,
                        y: 0,
                        width: 220,
                        height: 50,
                    },
                )],
            ),
        );

        let snap = transcode_layout(&tabs, &panes, &tab_layouts, "tab-1", &pane_reg, &tab_reg);
        assert_eq!(snap.tabs.len(), 2);
        assert_eq!(snap.tabs[0].panes.len(), 1);
        assert!(snap.tabs[1].panes.is_empty());

        // Even the un-rendered tab's pane got a terminal_id registered for relay.
        let id2 = pane_reg.assign_or_get("pane-2", "term-2");
        assert_eq!(pane_reg.terminal_id(id2).as_deref(), Some("term-2"));
    }

    // ── session.snapshot vs the fan-out ───────────────────────────────────────
    //
    // The snapshot is an OPTIMISATION of a path that still works. What has to be
    // proved is therefore not that it parses, but that it cannot answer anything
    // different from the path it short-circuits: a divergence here surfaces on a
    // device as a wrong pane tree, not as a failure.

    /// One daemon's state as JSON: `(workspaces, tabs, panes, layouts)`.
    ///
    /// Deliberately wider than the workspace under test — a second workspace, a
    /// pane-less tab, and a layout for that pane-less tab — because those are
    /// exactly the rows the snapshot carries and the workspace-scoped fan-out
    /// never sees. Both paths are fed from this one fixture, so "equivalent
    /// inputs" is literal rather than a claim about two hand-written fixtures.
    fn daemon_fixture() -> (
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
    ) {
        let workspaces = serde_json::json!([
            {
                "workspace_id": "ws-1", "number": 1, "label": "main", "focused": true,
                "pane_count": 2, "tab_count": 2, "active_tab_id": "w1:t1",
                "agent_status": "idle"
            },
            {
                "workspace_id": "ws-2", "number": 2, "label": "other", "focused": false,
                "pane_count": 1, "tab_count": 1, "active_tab_id": "w2:t1",
                "agent_status": "working"
            },
        ]);
        let tabs = serde_json::json!([
            {
                "tab_id": "w2:t1", "workspace_id": "ws-2", "number": 1, "label": "foreign",
                "focused": false, "pane_count": 1, "agent_status": "idle"
            },
            {
                "tab_id": "w1:t1", "workspace_id": "ws-1", "number": 1, "label": "shell",
                "focused": true, "pane_count": 2, "agent_status": "idle"
            },
            {
                "tab_id": "w1:t2", "workspace_id": "ws-1", "number": 2, "label": "empty",
                "focused": false, "pane_count": 0, "agent_status": "idle"
            },
        ]);
        let panes = serde_json::json!([
            {
                "pane_id": "w2:p9", "terminal_id": "term-9", "workspace_id": "ws-2",
                "tab_id": "w2:t1", "focused": true, "agent_status": "idle",
                "state_labels": {}, "revision": 1
            },
            {
                "pane_id": "w1:p1", "terminal_id": "term-1", "workspace_id": "ws-1",
                "tab_id": "w1:t1", "focused": true, "title": "vim", "cwd": "/home/u",
                "agent_status": "idle", "state_labels": {}, "revision": 3
            },
            {
                "pane_id": "w1:p2", "terminal_id": "term-2", "workspace_id": "ws-1",
                "tab_id": "w1:t1", "focused": false, "label": "logs", "cwd": "/var",
                "agent_status": "working", "state_labels": {}, "revision": 4
            },
        ]);
        let layouts = serde_json::json!([
            {
                "workspace_id": "ws-1", "tab_id": "w1:t1", "zoomed": false,
                "area": { "x": 0, "y": 0, "width": 200, "height": 50 },
                "focused_pane_id": "w1:p1",
                "panes": [
                    { "pane_id": "w1:p1", "focused": true,
                      "rect": { "x": 0, "y": 0, "width": 100, "height": 50 } },
                    { "pane_id": "w1:p2", "focused": false,
                      "rect": { "x": 100, "y": 0, "width": 100, "height": 50 } }
                ],
                "splits": []
            },
            {
                // A tab with no panes. The fan-out addresses `pane.layout` through
                // a representative pane, so it can never fetch this one; the
                // snapshot hands it over unasked, `zoomed` and all. Honouring it
                // would make the two paths disagree on `fullscreen_active` for a
                // tab that has nothing to render.
                "workspace_id": "ws-1", "tab_id": "w1:t2", "zoomed": true,
                "area": { "x": 0, "y": 0, "width": 200, "height": 50 },
                "focused_pane_id": "", "panes": [], "splits": []
            },
            {
                "workspace_id": "ws-2", "tab_id": "w2:t1", "zoomed": true,
                "area": { "x": 0, "y": 0, "width": 80, "height": 24 },
                "focused_pane_id": "w2:p9",
                "panes": [
                    { "pane_id": "w2:p9", "focused": true,
                      "rect": { "x": 0, "y": 0, "width": 80, "height": 24 } }
                ],
                "splits": []
            },
        ]);
        (workspaces, tabs, panes, layouts)
    }

    /// A `session.snapshot` response body built from [`daemon_fixture`], carrying
    /// the `agents` array and the three `focused_*` ids muxrd deliberately does
    /// not model — so the test also proves those are ignored rather than fatal.
    fn snapshot_json() -> serde_json::Value {
        let (workspaces, tabs, panes, layouts) = daemon_fixture();
        serde_json::json!({
            "version": "0.9.0",
            "protocol": 22,
            "focused_workspace_id": "ws-1",
            "focused_tab_id": "w1:t1",
            "focused_pane_id": "w1:p1",
            "workspaces": workspaces,
            "tabs": tabs,
            "panes": panes,
            "layouts": layouts,
            "agents": [ { "anything": "herdr adds here" } ],
        })
    }

    /// The fan-out's own assembly for `ws-1`, straight from the same fixture:
    /// its workspace-scoped `tab.list` / `pane.list`, and one `pane.layout` per
    /// tab that has a pane to address it through.
    fn fanout_inputs() -> (
        Vec<TabInfo>,
        Vec<PaneInfo>,
        HashMap<String, PaneLayoutSnapshot>,
    ) {
        let (_, tabs, panes, layouts) = daemon_fixture();
        let tabs: Vec<TabInfo> = serde_json::from_value::<Vec<TabInfo>>(tabs)
            .expect("TabInfo fixtures")
            .into_iter()
            .filter(|t| t.workspace_id == "ws-1")
            .collect();
        let panes: Vec<PaneInfo> = serde_json::from_value::<Vec<PaneInfo>>(panes)
            .expect("PaneInfo fixtures")
            .into_iter()
            .filter(|p| p.workspace_id == "ws-1")
            .collect();
        let mut tab_layouts = HashMap::new();
        for l in serde_json::from_value::<Vec<PaneLayoutSnapshot>>(layouts)
            .expect("PaneLayoutSnapshot fixtures")
        {
            // Exactly the fan-out's rule: only a tab with a representative pane.
            if l.workspace_id == "ws-1" && panes.iter().any(|p| p.tab_id == l.tab_id) {
                tab_layouts.insert(l.tab_id.clone(), l);
            }
        }
        (tabs, panes, tab_layouts)
    }

    /// THE equivalence proof, at fixture level: projecting one workspace out of a
    /// whole-daemon snapshot yields byte-for-byte the neutral layout the fan-out
    /// transcode produces from the same rows — including the neutral ids, which
    /// only match if both paths register panes and tabs in the same order.
    #[test]
    fn the_snapshot_projection_equals_the_fanout_transcode() {
        let snapshot: SessionSnapshot =
            serde_json::from_value(snapshot_json()).expect("SessionSnapshot must parse");

        let snap_panes = HerdrPaneRegistry::new();
        let snap_tabs = HerdrTabRegistry::new();
        let from_snapshot = layout_from_snapshot(&snapshot, "ws-1", &snap_panes, &snap_tabs)
            .expect("ws-1 is present in the snapshot");

        let (tabs, panes, tab_layouts) = fanout_inputs();
        let fan_panes = HerdrPaneRegistry::new();
        let fan_tabs = HerdrTabRegistry::new();
        let from_fanout =
            transcode_layout(&tabs, &panes, &tab_layouts, "w1:t1", &fan_panes, &fan_tabs);

        assert_eq!(
            from_snapshot, from_fanout,
            "the snapshot path must not answer anything the fan-out would not"
        );

        // Guard the properties the equality alone would not name, so a future
        // change to BOTH paths cannot quietly rewrite them together.
        assert_eq!(from_snapshot.tabs.len(), 2, "ws-2's tab must not leak in");
        let t1 = &from_snapshot.tabs[0];
        assert!(t1.active, "the workspace's OWN active_tab_id decides this");
        assert_eq!(t1.panes.len(), 2);
        let t2 = &from_snapshot.tabs[1];
        assert!(
            !t2.fullscreen_active,
            "a pane-less tab has no fan-out geometry, so the snapshot's zoomed=true \
             for it must be dropped rather than diverge"
        );
        assert!(t2.panes.is_empty());
    }

    /// A snapshot that does not list the workspace is not an answer — it must
    /// yield `None` so `query_layout` re-asks through the fan-out, whose
    /// `UnknownSpace` is what the gRPC `not_found` contract is built on.
    #[test]
    fn a_workspace_absent_from_the_snapshot_is_not_an_empty_layout() {
        let snapshot: SessionSnapshot =
            serde_json::from_value(snapshot_json()).expect("SessionSnapshot must parse");
        assert!(
            layout_from_snapshot(
                &snapshot,
                "ws-does-not-exist",
                &HerdrPaneRegistry::new(),
                &HerdrTabRegistry::new()
            )
            .is_none(),
            "an unknown workspace must never transcode to an empty-but-Ok layout"
        );
    }

    /// muxrd models only the fields it consumes. A snapshot missing everything
    /// else still parses (it is an optimisation, not a contract), and a snapshot
    /// with no workspaces simply has nothing to project.
    #[test]
    fn a_sparse_snapshot_parses_and_projects_to_nothing() {
        let snapshot: SessionSnapshot =
            serde_json::from_value(serde_json::json!({})).expect("every field defaults");
        assert!(snapshot.workspaces.is_empty());
        assert!(
            layout_from_snapshot(
                &snapshot,
                "ws-1",
                &HerdrPaneRegistry::new(),
                &HerdrTabRegistry::new()
            )
            .is_none()
        );
    }

    #[test]
    fn ack_helpers_shape() {
        assert!(ack_ok().ok);
        let e = ack_err("boom".into());
        assert!(!e.ok);
        assert_eq!(e.error.as_deref(), Some("boom"));
        assert!(!unknown_pane(7).ok);
        assert!(!unknown_tab(7).ok);
    }

    // Touch an unused import path so the fixtures compile cleanly under all-targets.
    #[allow(dead_code)]
    fn _agent_status_is_reachable() -> AgentStatus {
        AgentStatus::Idle
    }

    // ── worktree-group close refusals (herdr 0.9.0) ───────────────────────────
    //
    // These drive a real `HerdrControl` against a one-shot fake herdr JSON-API
    // server on a throwaway Unix socket — the only way to prove what actually
    // goes out on the wire (Change One) and how a canned refusal comes back
    // (Change Two) without a live herdr instance.

    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicUsize;

    fn unique_socket_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mxr_hr_ctl_{tag}_{}_{nanos}_{n}.sock",
            std::process::id()
        ))
    }

    /// Spawn a one-shot fake herdr JSON-API server: bind, accept a single
    /// connection, read one request line, reply with `response_json` (a bare
    /// JSON object — the trailing newline is added here), then hand the raw
    /// request line back to the caller for inspection.
    fn fake_herdr_once(
        sock: &std::path::Path,
        response_json: &str,
    ) -> std::thread::JoinHandle<String> {
        let listener = UnixListener::bind(sock).expect("bind fake herdr socket");
        let mut response = response_json.to_string();
        response.push('\n');
        std::thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept fake herdr connection");
            let mut reader = BufReader::new(&conn);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request line");
            (&conn)
                .write_all(response.as_bytes())
                .expect("write fake herdr response");
            line
        })
    }

    fn control_over(sock: &std::path::Path) -> HerdrControl {
        HerdrControl::new(
            sock.to_path_buf(),
            Arc::new(HerdrPaneRegistry::new()),
            Arc::new(HerdrTabRegistry::new()),
        )
    }

    /// The DEFAULT close must put `close_group: false` on the wire — group intent
    /// is opt-in, so a caller that did not ask for it must not get it, and the
    /// value must be explicit rather than left to herdr's own default.
    #[test]
    fn close_workspace_sends_close_group_false_by_default() {
        let sock = unique_socket_path("wsclose");
        let server = fake_herdr_once(&sock, r#"{"id":"muxrd-1","result":{"type":"ok"}}"#);
        let control = control_over(&sock);

        let ack = control
            .close_workspace("ws-1", false)
            .expect("close_workspace must round-trip over the fake socket");
        assert!(ack.ok);

        let sent = server.join().expect("fake server thread must not panic");
        let req: serde_json::Value =
            serde_json::from_str(sent.trim_end()).expect("sent line must be JSON");
        assert_eq!(req["method"], "workspace.close");
        assert_eq!(req["params"]["workspace_id"], "ws-1");
        assert_eq!(
            req["params"]["close_group"], false,
            "a close with no group intent must never request a group close"
        );

        let _ = std::fs::remove_file(&sock);
    }

    /// An EXPLICIT group close puts `close_group: true` on the wire — the caller's
    /// value is forwarded, never a fixed one.
    #[test]
    fn close_workspace_sends_close_group_true_when_asked() {
        let sock = unique_socket_path("wsclsg");
        let server = fake_herdr_once(&sock, r#"{"id":"muxrd-1","result":{"type":"ok"}}"#);
        let control = control_over(&sock);

        let ack = control
            .close_workspace("ws-1", true)
            .expect("close_workspace must round-trip over the fake socket");
        assert!(ack.ok);

        let sent = server.join().expect("fake server thread must not panic");
        let req: serde_json::Value =
            serde_json::from_str(sent.trim_end()).expect("sent line must be JSON");
        assert_eq!(req["method"], "workspace.close");
        assert_eq!(req["params"]["close_group"], true);

        let _ = std::fs::remove_file(&sock);
    }

    /// `workspace_group_close_required` — the EXPECTED answer to a default close of
    /// a worktree-group primary — is surfaced as an unsuccessful acknowledgement
    /// (never a transport error) whose message names the worktree-group cause AND
    /// tells the caller how to re-issue with group intent.
    #[test]
    fn workspace_group_close_required_is_surfaced_distinctly() {
        let sock = unique_socket_path("wsgroup");
        let _server = fake_herdr_once(
            &sock,
            r#"{"id":"muxrd-1","error":{"code":"workspace_group_close_required","message":"workspace has linked worktree workspaces; use --group (close_group=true in the API) to close the group"}}"#,
        );
        let control = control_over(&sock);

        let ack = control
            .close_workspace("ws-1", false)
            .expect("a herdr API error is a failed ack, not a transport Err");
        assert!(!ack.ok, "a logical refusal is an unsuccessful ack");
        let msg = ack.error.expect("refusal must carry a message");
        // The cause: this space is a worktree-group primary with linked worktrees.
        assert!(msg.contains("worktree-group primary"), "message: {msg}");
        assert!(msg.contains("linked worktree"), "message: {msg}");
        // The way forward: re-issue with group intent.
        assert!(msg.contains("close_group=true"), "message: {msg}");
        assert!(msg.contains("CloseSpace"), "message: {msg}");

        let _ = std::fs::remove_file(&sock);
    }

    /// `confirmation_required` from `tab.close` — closing the last tab of a
    /// worktree-group primary — is surfaced distinctly, naming the worktree-group
    /// cause and pointing at a CloseSpace that carries group intent (a plain
    /// CloseSpace refuses the primary too, so the old "close the space" advice
    /// would have sent the operator into a second refusal).
    #[test]
    fn confirmation_required_on_tab_close_is_surfaced_distinctly() {
        let sock = unique_socket_path("tabclose");
        let _server = fake_herdr_once(
            &sock,
            r#"{"id":"muxrd-1","error":{"code":"confirmation_required","message":"closing this tab would close a worktree group"}}"#,
        );
        let panes = Arc::new(HerdrPaneRegistry::new());
        let tabs = Arc::new(HerdrTabRegistry::new());
        let tab_id = tabs.assign_or_get("tab-1");
        let control = HerdrControl::new(sock.clone(), panes, tabs);

        let ack = control
            .close_tab(tab_id)
            .expect("a herdr API error is a failed ack, not a transport Err");
        assert!(!ack.ok, "a logical refusal is an unsuccessful ack");
        let msg = ack.error.expect("refusal must carry a message");
        assert!(msg.contains("worktree-group primary"), "message: {msg}");
        assert!(msg.contains("close_group=true"), "message: {msg}");

        let _ = std::fs::remove_file(&sock);
    }

    /// `confirmation_required` from `pane.close` — closing the last pane of a
    /// worktree-group primary — is surfaced the same way.
    #[test]
    fn confirmation_required_on_pane_close_is_surfaced_distinctly() {
        let sock = unique_socket_path("paneclose");
        let _server = fake_herdr_once(
            &sock,
            r#"{"id":"muxrd-1","error":{"code":"confirmation_required","message":"closing this pane would close a worktree group"}}"#,
        );
        let panes = Arc::new(HerdrPaneRegistry::new());
        let pane_id = panes.assign_or_get("pane-1", "term-1");
        let tabs = Arc::new(HerdrTabRegistry::new());
        let control = HerdrControl::new(sock.clone(), panes, tabs);

        let ack = control
            .close_pane(pane_id)
            .expect("a herdr API error is a failed ack, not a transport Err");
        assert!(!ack.ok, "a logical refusal is an unsuccessful ack");
        let msg = ack.error.expect("refusal must carry a message");
        assert!(msg.contains("worktree-group primary"), "message: {msg}");
        assert!(msg.contains("close_group=true"), "message: {msg}");

        let _ = std::fs::remove_file(&sock);
    }

    /// A plain, unrelated herdr API error must still fall back to the generic
    /// `"<code>: <message>"` shape — the two new codes are the only ones reworded.
    #[test]
    fn unrelated_error_code_keeps_the_generic_shape() {
        let sock = unique_socket_path("plainerr");
        let _server = fake_herdr_once(
            &sock,
            r#"{"id":"muxrd-1","error":{"code":"not_found","message":"workspace not found"}}"#,
        );
        let control = control_over(&sock);

        let ack = control
            .close_workspace("ws-missing", false)
            .expect("a herdr API error is a failed ack, not a transport Err");
        assert!(!ack.ok);
        assert_eq!(ack.error.as_deref(), Some("not_found: workspace not found"));

        let _ = std::fs::remove_file(&sock);
    }

    /// The rewording is scoped by METHOD, not by code alone. `call_action` is the
    /// shared error path for twelve methods and `confirmation_required` is a
    /// GENERIC herdr code, so the same code from a method that has nothing to do
    /// with closing anything — `pane.zoom` — must keep the generic shape rather
    /// than be re-narrated as a worktree-group close pointing at CloseSpace.
    #[test]
    fn confirmation_required_from_an_unrelated_method_keeps_the_generic_shape() {
        let sock = unique_socket_path("zoomcnf");
        let _server = fake_herdr_once(
            &sock,
            r#"{"id":"muxrd-1","error":{"code":"confirmation_required","message":"zoom needs confirmation"}}"#,
        );
        let panes = Arc::new(HerdrPaneRegistry::new());
        let pane_id = panes.assign_or_get("pane-1", "term-1");
        let tabs = Arc::new(HerdrTabRegistry::new());
        let control = HerdrControl::new(sock.clone(), panes, tabs);

        let ack = control
            .zoom_pane(Some(pane_id), PaneZoomMode::Toggle)
            .expect("a herdr API error is a failed ack, not a transport Err");
        assert!(!ack.ok);
        assert_eq!(
            ack.error.as_deref(),
            Some("confirmation_required: zoom needs confirmation"),
            "an unrelated method's refusal must not be reworded"
        );

        let _ = std::fs::remove_file(&sock);
    }

    /// The same code from `workspace.close` — a method the wording does not
    /// describe either (its refusal code is `workspace_group_close_required`) —
    /// also keeps the generic shape.
    #[test]
    fn confirmation_required_from_workspace_close_keeps_the_generic_shape() {
        let sock = unique_socket_path("wscnf");
        let _server = fake_herdr_once(
            &sock,
            r#"{"id":"muxrd-1","error":{"code":"confirmation_required","message":"needs confirmation"}}"#,
        );
        let control = control_over(&sock);

        let ack = control
            .close_workspace("ws-1", false)
            .expect("a herdr API error is a failed ack, not a transport Err");
        assert!(!ack.ok);
        assert_eq!(
            ack.error.as_deref(),
            Some("confirmation_required: needs confirmation")
        );

        let _ = std::fs::remove_file(&sock);
    }

    /// The other half of the method scoping: `workspace_group_close_required` from
    /// a method that cannot legitimately produce it keeps the generic shape too.
    #[test]
    fn group_close_required_from_an_unrelated_method_keeps_the_generic_shape() {
        let sock = unique_socket_path("tabgrp");
        let _server = fake_herdr_once(
            &sock,
            r#"{"id":"muxrd-1","error":{"code":"workspace_group_close_required","message":"nonsense from tab.close"}}"#,
        );
        let panes = Arc::new(HerdrPaneRegistry::new());
        let tabs = Arc::new(HerdrTabRegistry::new());
        let tab_id = tabs.assign_or_get("tab-1");
        let control = HerdrControl::new(sock.clone(), panes, tabs);

        let ack = control
            .close_tab(tab_id)
            .expect("a herdr API error is a failed ack, not a transport Err");
        assert!(!ack.ok);
        assert_eq!(
            ack.error.as_deref(),
            Some("workspace_group_close_required: nonsense from tab.close")
        );

        let _ = std::fs::remove_file(&sock);
    }

    /// Spawn a fake herdr JSON-API server that answers `n` sequential
    /// connection-per-request round trips from a `method → body` table (the body
    /// being the `"result":{…}` or `"error":{…}` fragment), echoing each
    /// request's own id. Any method absent from the table gets a
    /// `method_not_found` error — which is precisely how a herdr without
    /// `session.snapshot` would answer. Returns the ordered method trace.
    fn fake_herdr_scripted(
        sock: &std::path::Path,
        n: usize,
        responses: Vec<(&'static str, String)>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(sock).expect("bind fake herdr socket");
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..n {
                let Ok((conn, _)) = listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(&conn);
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let req: serde_json::Value =
                    serde_json::from_str(line.trim_end()).expect("request must be JSON");
                let method = req["method"].as_str().unwrap_or_default().to_string();
                let id = req["id"].as_str().unwrap_or("muxrd-0").to_string();
                let body = responses
                    .iter()
                    .find(|(m, _)| *m == method)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_else(|| {
                        format!(
                            r#""error":{{"code":"method_not_found","message":"unknown method {method}"}}"#
                        )
                    });
                let mut out = format!(r#"{{"id":"{id}",{body}}}"#);
                out.push('\n');
                let _ = (&conn).write_all(out.as_bytes());
                seen.push(method);
            }
            seen
        })
    }

    /// The `"result":{…}` fragments for the fan-out's four methods, built from
    /// [`daemon_fixture`].
    ///
    /// `tab.list` and `pane.list` are **workspace-scoped** on the real herdr —
    /// muxrd sends `{"workspace_id":…}` and gets back only that workspace's rows
    /// — so the fixture filters them the same way. Serving the unscoped arrays
    /// here would make the fan-out look like it returns another workspace's tabs,
    /// which it does not. `workspace.list` is genuinely daemon-wide.
    fn fanout_responses() -> Vec<(&'static str, String)> {
        let (workspaces, tabs, panes, layouts) = daemon_fixture();
        let scoped = |rows: serde_json::Value| -> serde_json::Value {
            serde_json::Value::Array(
                rows.as_array()
                    .expect("fixture rows are an array")
                    .iter()
                    .filter(|r| r["workspace_id"] == "ws-1")
                    .cloned()
                    .collect(),
            )
        };
        let tabs = scoped(tabs);
        let panes = scoped(panes);
        let tab_layout = layouts
            .as_array()
            .and_then(|l| l.first())
            .cloned()
            .expect("ws-1/t1 layout is the first fixture entry");
        vec![
            (
                "workspace.list",
                format!(r#""result":{{"type":"workspace_list","workspaces":{workspaces}}}"#),
            ),
            (
                "tab.list",
                format!(r#""result":{{"type":"tab_list","tabs":{tabs}}}"#),
            ),
            (
                "pane.list",
                format!(r#""result":{{"type":"pane_list","panes":{panes}}}"#),
            ),
            (
                "pane.layout",
                format!(r#""result":{{"type":"pane_layout","layout":{tab_layout}}}"#),
            ),
        ]
    }

    fn snapshot_response() -> (&'static str, String) {
        (
            "session.snapshot",
            format!(
                r#""result":{{"type":"session_snapshot","snapshot":{}}}"#,
                snapshot_json()
            ),
        )
    }

    /// The whole point of the change: one round trip, not 3 + one per tab.
    #[test]
    fn query_layout_serves_the_common_case_from_one_session_snapshot_call() {
        let sock = unique_socket_path("snap");
        // Only the snapshot is answerable. Every fan-out method would come back
        // `method_not_found`, so a layout can only be produced by the snapshot.
        let server = fake_herdr_scripted(&sock, 1, vec![snapshot_response()]);
        let control = control_over(&sock);

        let layout = control
            .query_layout("ws-1")
            .expect("the snapshot alone must be enough to answer");

        let methods = server.join().expect("fake server thread must not panic");
        assert_eq!(
            methods,
            vec!["session.snapshot".to_string()],
            "the snapshot path must not also fan out"
        );
        assert_eq!(layout.tabs.len(), 2);
        assert_eq!(layout.tabs[0].panes.len(), 2);

        let _ = std::fs::remove_file(&sock);
    }

    /// The fallback is the safety net the whole design rests on: a herdr that
    /// cannot serve `session.snapshot` must still get the identical layout out of
    /// the fan-out, at 3 + one-per-tab round trips.
    #[test]
    fn query_layout_falls_back_to_the_fanout_when_the_snapshot_fails() {
        // Path 1: snapshot works.
        let snap_sock = unique_socket_path("snapok");
        let snap_server = fake_herdr_scripted(&snap_sock, 1, vec![snapshot_response()]);
        let via_snapshot = control_over(&snap_sock)
            .query_layout("ws-1")
            .expect("snapshot path must answer");
        snap_server
            .join()
            .expect("fake server thread must not panic");
        let _ = std::fs::remove_file(&snap_sock);

        // Path 2: the SAME daemon state, but `session.snapshot` is not served —
        // exactly what a herdr predating the method answers.
        let fan_sock = unique_socket_path("snapfb");
        let fan_server = fake_herdr_scripted(&fan_sock, 5, fanout_responses());
        let via_fanout = control_over(&fan_sock)
            .query_layout("ws-1")
            .expect("the fan-out must still answer when the snapshot cannot");
        let methods = fan_server
            .join()
            .expect("fake server thread must not panic");
        let _ = std::fs::remove_file(&fan_sock);

        assert_eq!(
            methods,
            vec![
                "session.snapshot".to_string(),
                "workspace.list".to_string(),
                "tab.list".to_string(),
                "pane.list".to_string(),
                "pane.layout".to_string(),
            ],
            "the snapshot is tried first, then the full fan-out — and only one \
             pane.layout, for the one tab with a pane"
        );
        assert_eq!(
            via_snapshot, via_fanout,
            "an optimisation that answers differently from its fallback is a bug, \
             and this one is invisible until it reaches a device"
        );
    }

    /// A workspace that exists nowhere must still fail with `UnknownSpace` after
    /// the snapshot round trip — the fallback re-derives the authoritative
    /// answer rather than the snapshot path inventing one.
    #[test]
    fn an_unknown_workspace_still_fails_closed_through_both_paths() {
        let sock = unique_socket_path("snapunk");
        let mut responses = fanout_responses();
        responses.push(snapshot_response());
        let server = fake_herdr_scripted(&sock, 2, responses);

        let err = control_over(&sock)
            .query_layout("ws-nope")
            .expect_err("an unknown workspace must not answer Ok");
        assert!(
            err.downcast_ref::<UnknownSpace>().is_some(),
            "the gRPC not_found contract needs the typed UnknownSpace, got: {err:#}"
        );

        let methods = server.join().expect("fake server thread must not panic");
        assert_eq!(
            methods,
            vec!["session.snapshot".to_string(), "workspace.list".to_string()],
            "a healthy snapshot that lists no such workspace still defers to the \
             fan-out's existence check"
        );

        let _ = std::fs::remove_file(&sock);
    }
}
