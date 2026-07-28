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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use crate::multiplexer::types::{ActionAck, LayoutSnapshot, PaneSnapshot, TabSnapshot};

use super::api::{
    ApiRequest, ApiResponseBody, ApiResult, HerdrServerInfo, LayoutDescription, PaneCloseParams,
    PaneDirection, PaneFocusDirectionParams, PaneInfo, PaneLayoutParams, PaneLayoutSnapshot,
    PaneRenameParams, PaneSplitParams, PaneZoomMode, PaneZoomParams, PingParams, SplitDirection,
    TabCloseParams, TabCreateParams, TabFocusParams, TabInfo, TabRenameParams,
    WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceInfo, WorkspaceListParams,
    WorkspaceRenameParams,
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
    fn call_action(&self, method: &str, params: serde_json::Value) -> Result<ActionAck> {
        match self.call_raw(method, params)? {
            ApiResponseBody::Ok { .. } => Ok(ack_ok()),
            ApiResponseBody::Err { error } => {
                Ok(ack_err(format!("{}: {}", error.code, error.message)))
            }
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
    pub fn close_workspace(&self, workspace_id: &str) -> Result<ActionAck> {
        let params = Self::to_params(
            "workspace.close",
            WorkspaceCloseParams {
                workspace_id: workspace_id.to_string(),
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

    /// Build the neutral [`LayoutSnapshot`] for a workspace (muxrd "session").
    ///
    /// Fetches the workspace's tabs (`tab.list`), its panes with `terminal_id`s
    /// (`pane.list`), and one absolute-cell layout per tab (`pane.layout`),
    /// populating the shared registries and transcoding into the neutral shape.
    pub fn query_layout(&self, workspace_id: &str) -> Result<LayoutSnapshot> {
        // M2: the per-session active tab comes from the workspace's own
        // `WorkspaceInfo.active_tab_id`, NOT from `TabInfo.focused`. herdr's
        // `TabInfo.focused` is *globally* unique — true only for the one tab of
        // herdr's currently-active workspace — so for any other workspace every
        // tab would report `focused=false`, leaving the snapshot with no active
        // tab. Resolve the workspace's own active tab id here (one extra
        // `workspace.list` round-trip; cheap over the local socket).
        let active_tab_id = self
            .list_workspaces()?
            .into_iter()
            .find(|w| w.workspace_id == workspace_id)
            .map(|w| w.active_tab_id)
            .unwrap_or_default();

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
}
