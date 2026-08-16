//! GetLayout RPC implementation with relay-routed query and B-FOCUS override.

use tonic::{Request, Response, Status};

use crate::multiplexer::{LayoutSnapshot, MuxBackend};
use crate::proto::{Layout, PaneMsg, SessionRef, TabMsg};
use crate::relay::RelayViewState;

use super::MuxrService;
use super::helpers::short_conn;

/// Timeout for the oneshot reply when routing a `QueryLayout` through the relay.
///
/// FX-QUERY: this is now the SINGLE timeout bound on a relay-routed layout query.
/// The relay's inbound arm no longer awaits (it hands the query to the render
/// thread and returns), so there is no per-action sub-timeout. When this fires we
/// drop `reply_rx`; the render thread observes the closed receiver and retires
/// the in-flight query so its stray Logs can't be misattributed. 18 s comfortably
/// covers two query round-trips (ListTabs + ListPanes) plus channel overhead.
const RELAY_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(18);

impl MuxrService {
    // ── GetLayout (C1 + BE-LAYOUT) ─────────────────────────────────────────

    pub(super) async fn get_layout_impl(
        &self,
        request: Request<SessionRef>,
    ) -> Result<Response<Layout>, Status> {
        let req = request.into_inner();
        // Option C: `session` is the opaque routing id the client echoes — used as
        // the relay-registry lookup key (`entry.session == session`). `resolve_session`
        // strips it to the owning backend + bare name (validated) for the ephemeral
        // query path.
        let session = req.session;
        let connection_id = req.connection_id;
        let (backend, bare) = self.resolve_session(&session)?;
        // FS3: full connection_id must not appear in info/warn logs.
        log::info!(
            "GetLayout: session='{session}' connection_id={}…",
            short_conn(&connection_id)
        );
        log::debug!("GetLayout: session='{session}' connection_id='{connection_id}'");

        // ── B-QUERY: route through relay if one is attached ─────────────────
        // Routing priority:
        //   1. If connection_id non-empty AND entry exists AND session matches →
        //      route to that exact relay (per-connection routing — fixes the
        //      multi-client misroute bug).
        //   2. Otherwise → find any relay registered for the session (session-
        //      scoped fallback; preserves solo-client and legacy-client behavior).
        //   3. No relay → ephemeral backend.query_layout() path.
        //
        // Both paths now hand back a neutral `LayoutSnapshot` directly (P2.00 A-2):
        // the relay branch receives the snapshot over the QueryLayout oneshot — the
        // render thread already parsed the two captured JSON Logs into it via the
        // single backend-owned parse — and the ephemeral branch delegates to
        // `self.backend.query_layout()`, which parses internally. The gRPC layer is
        // backend-agnostic: it never touches the zellij JSON wire format.
        let (snapshot, via_relay, relay_conn_id) = {
            // Try per-connection lookup first, then session-scoped fallback.
            let relay_entry: Option<(
                String,
                tokio::sync::mpsc::UnboundedSender<crate::relay::RelayControl>,
            )> = if !connection_id.is_empty() {
                // Per-connection: validate session match before cloning sender.
                self.control
                    .get(&connection_id)
                    .filter(|entry| entry.session == session)
                    .map(|entry| (connection_id.clone(), entry.sender.clone()))
            } else {
                None
            };

            // If per-connection failed, try session-scoped fallback.
            let relay_entry = relay_entry.or_else(|| {
                self.control
                    .iter()
                    .find(|entry| entry.session == session)
                    .map(|entry| (entry.key().clone(), entry.sender.clone()))
            });

            // Destructure: (conn_id_used, sender_opt)
            let (matched_conn_id, relay_sender) = match relay_entry {
                Some((cid, sender)) => (cid, Some(sender)),
                None => (String::new(), None),
            };

            if let Some(sender) = relay_sender {
                let (reply_tx, reply_rx) =
                    tokio::sync::oneshot::channel::<anyhow::Result<LayoutSnapshot>>();
                let queued =
                    sender.send(crate::relay::RelayControl::QueryLayout { reply: reply_tx });
                // `sender` is an owned clone of the UnboundedSender; the DashMap
                // Ref guard was already released above. Drop is just tidiness.
                drop(sender);

                if queued.is_ok() {
                    match tokio::time::timeout(RELAY_QUERY_TIMEOUT, reply_rx).await {
                        Ok(Ok(Ok(snap))) => {
                            // P2.00 A-2: the render thread already parsed the two
                            // captured JSON Logs into this neutral LayoutSnapshot;
                            // the gRPC layer never sees the zellij JSON wire format.
                            log::debug!(
                                "GetLayout: session='{session}' connection_id='{matched_conn_id}' \
                                 query routed via relay ({} tab(s))",
                                snap.tabs.len()
                            );
                            (snap, true, matched_conn_id)
                        }
                        Ok(Ok(Err(e))) => {
                            // Relay query OR render-thread parse failed (P2.00 A-2
                            // moved the parse into the render thread). Either way
                            // fall back to the ephemeral path, which re-queries and
                            // may succeed. The error detail stays in the server log;
                            // it never leaks to the client (and carries no layout
                            // JSON, only a serde/empty-payload message).
                            log::warn!(
                                "GetLayout: relay query/parse failed for '{session}', \
                                 falling back to ephemeral: {e:#}"
                            );
                            let snap = backend_query_layout(&backend, &bare).await?;
                            (snap, false, String::new())
                        }
                        Ok(Err(_cancelled)) => {
                            log::warn!(
                                "GetLayout: relay query oneshot cancelled for '{session}', \
                                 falling back to ephemeral"
                            );
                            let snap = backend_query_layout(&backend, &bare).await?;
                            (snap, false, String::new())
                        }
                        Err(_elapsed) => {
                            log::warn!(
                                "GetLayout: relay query timed out for '{session}' \
                                 after {RELAY_QUERY_TIMEOUT:?}, falling back to ephemeral"
                            );
                            let snap = backend_query_layout(&backend, &bare).await?;
                            (snap, false, String::new())
                        }
                    }
                } else {
                    // Relay sender was closed (relay tearing down) — fall back.
                    log::debug!(
                        "GetLayout: relay sender closed for '{session}', \
                         falling back to ephemeral"
                    );
                    let snap = backend_query_layout(&backend, &bare).await?;
                    (snap, false, String::new())
                }
            } else {
                // No relay attached for this session — use the ephemeral backend path.
                log::debug!("GetLayout: no relay for '{session}', using ephemeral query");
                let snap = backend_query_layout(&backend, &bare).await?;
                (snap, false, String::new())
            }
        };

        log::debug!(
            "GetLayout: {} tab(s) in snapshot, via_relay={via_relay} \
             relay_conn_id='{relay_conn_id}'",
            snapshot.tabs.len()
        );

        // ── B-FOCUS: read relay view state for active_tab / focused_pane ────
        // Only meaningful when a relay is attached AND the relay that served the
        // query is the CALLER'S OWN relay. We snapshot it once and use it for
        // the override pass below so we hold the DashMap guard briefly (never
        // across an .await).
        //
        // Override condition (Issue A fix):
        //   - `connection_id` must be non-empty (caller has a known relay id),
        //   - `relay_conn_id` must equal `connection_id` (the exact-connection
        //     path was taken — the query was served by the caller's own relay).
        //
        // When the fallback path was taken (request connection_id is empty, OR
        // the query fell back to an arbitrary sibling relay whose conn_id differs
        // from the request's), relay_vs is set to None so the override pass is
        // skipped entirely. Raw zellij tab/pane values are returned unchanged,
        // which is always correct because we have no reliable per-caller view
        // state in that case — applying a sibling relay's active_tab/focused_pane
        // would produce an actively wrong indicator (worse than the raw union).
        let relay_vs: Option<RelayViewState> =
            if should_apply_view_state_override(via_relay, &connection_id, &relay_conn_id) {
                // Exact-connection match: the query was served by the caller's own
                // relay. Apply the per-connection view-state override.
                self.view_state
                    .get(&relay_conn_id)
                    .map(|entry| entry.state.clone())
            } else {
                // Fallback path or no relay: skip the override.
                None
            };
        if let Some(ref vs) = relay_vs {
            log::debug!(
                "GetLayout: relay view state override applied (conn={relay_conn_id}): \
                 active_tab={:?} focused_pane={:?}",
                vs.active_tab,
                vs.focused_pane
            );
        } else if via_relay {
            log::debug!(
                "GetLayout: relay view state override SUPPRESSED (request \
                 connection_id='{connection_id}', relay_conn_id='{relay_conn_id}') — \
                 returning raw zellij values"
            );
        }

        // ── Build proto from LayoutSnapshot + B-FOCUS override ────────────────
        let tab_msgs: Vec<TabMsg> = build_tab_msgs(&snapshot, relay_vs.as_ref());

        // FS3: relay_conn_id is the matched relay's connection_id (same 32-char secret);
        // redact to the 8-char prefix at info, keep the full value at debug only.
        log::info!(
            "GetLayout: session='{}' relay_conn={}… → {} tab(s), \
             {} total pane group(s), via_relay={via_relay}",
            session,
            short_conn(&relay_conn_id),
            tab_msgs.len(),
            tab_msgs.iter().map(|t| t.panes.len()).sum::<usize>()
        );
        log::debug!(
            "GetLayout: session='{}' relay_conn='{relay_conn_id}' → {} tab(s), \
             {} total pane group(s), via_relay={via_relay}",
            session,
            tab_msgs.len(),
            tab_msgs.iter().map(|t| t.panes.len()).sum::<usize>()
        );

        Ok(Response::new(Layout { tabs: tab_msgs }))
    }
}

// ─── Private async helper ─────────────────────────────────────────────────────

/// Call `backend.query_layout()` in a blocking task, mapping errors to
/// [`Status`].
///
/// Used by all ephemeral paths in `get_layout_impl` (no relay attached, relay
/// fallback on error/timeout/cancel, relay sender closed). Replaces the old
/// `ephemeral_query` free function that opened two separate IPC connections.
async fn backend_query_layout(
    backend: &std::sync::Arc<dyn MuxBackend>,
    session: &str,
) -> Result<LayoutSnapshot, Status> {
    let backend = backend.clone();
    let session = session.to_owned();
    tokio::task::spawn_blocking(move || backend.query_layout(&session))
        .await
        .map_err(|e| Status::internal(format!("GetLayout query task panicked: {e}")))?
        .map_err(|e| {
            log::warn!("GetLayout ephemeral query failed: {e:#}");
            Status::internal(format!("GetLayout query failed: {e:#}"))
        })
}

// ─── Pure helpers (also used by tests) ───────────────────────────────────────

/// A pane is client-visible iff it is a real terminal pane. Plugin panes
/// (background plugins + tab-bar/status-bar) are excluded from GetLayout.
pub(crate) fn pane_is_client_visible(is_plugin: bool) -> bool {
    !is_plugin
}

/// Resolve the tab id the B-FOCUS override should be scoped to for this poll.
///
/// The relay tracks `active_tab` explicitly, but only the arms that name a tab
/// can set it: `SwitchTab` sets it, and `SwitchSpace` *clears* it (the old
/// space's tab id is meaningless in the new space). `FocusPane` carries a bare
/// [`PaneRef`](crate::multiplexer::PaneRef) with no tab, so a SAME-TAB focus after
/// a space switch leaves `active_tab = None` while `focused_pane` is current.
///
/// With `active_tab = None` the override used to be skipped for EVERY tab, so
/// GetLayout kept reporting the backend's daemon-global focus and the client's
/// pane badge / terminal title stuck on the pane focused before the space switch
/// (the wire stream itself had already followed the tap). So when the tracked tab
/// is missing we DERIVE it: the effective active tab is the one whose
/// client-visible panes contain the tracked `focused_pane`.
///
/// Returns:
/// - `Some(active_tab)` verbatim whenever the relay tracks one (unchanged path),
/// - `Some(derived)` when `active_tab` is `None`, `focused_pane` is `Some`, and
///   that pane is present in some tab,
/// - `None` when there is no view state, when both fields are `None`, or when the
///   tracked pane is in no tab (stale/closed pane, or a plugin pane — which is
///   never client-visible). `None` means "no scope" and callers must fall back to
///   the raw queried values.
///
/// Ties: a pane id is unique within a snapshot, so at most one tab can match;
/// `find` (first match wins) is only a defensive tiebreak.
pub(crate) fn resolve_effective_active_tab(
    relay_vs: Option<&RelayViewState>,
    snapshot: &LayoutSnapshot,
) -> Option<u64> {
    let vs = relay_vs?;
    if let Some(at) = vs.active_tab {
        return Some(at);
    }
    let fp = vs.focused_pane?;
    snapshot
        .tabs
        .iter()
        .find(|tab| {
            tab.panes
                .iter()
                .filter(|p| pane_is_client_visible(p.is_plugin))
                .any(|p| p.id == fp.id && p.is_plugin == fp.is_plugin)
        })
        .map(|tab| tab.tab_id)
}

/// Build the proto tab list from a neutral [`LayoutSnapshot`], applying the
/// B-FOCUS per-relay-client override when `relay_vs` is present.
///
/// Pure (no I/O, no locks) so the override rules are unit-testable; the caller
/// has already decided whether the override is allowed at all
/// (see [`should_apply_view_state_override`]).
///
/// Panes are already nested under their tab in `LayoutSnapshot` (no HashMap
/// grouping step needed). Plugin panes are filtered here (single chokepoint).
///
/// B-FOCUS: `relay_vs.focused_pane` is a neutral `PaneRef` (P1.03), compared
/// against each pane's `(id, is_plugin)` — byte-identical to the prior
/// `PaneId::Terminal/Plugin` match.
fn build_tab_msgs(snapshot: &LayoutSnapshot, relay_vs: Option<&RelayViewState>) -> Vec<TabMsg> {
    // Scope tab for BOTH overrides below, resolved ONCE per poll (not per tab):
    // the relay's tracked active_tab, or — when that is unknown — the tab
    // containing the tracked focused pane. `None` = no scope → raw values.
    let effective_active_tab = resolve_effective_active_tab(relay_vs, snapshot);

    snapshot
        .tabs
        .iter()
        .map(|tab| {
            // B-FOCUS: override active with the per-relay-client value.
            // When the effective active tab is known — tracked via SwitchTab or
            // derived from the tracked focused pane — the relay knows exactly
            // which tab it is on. When it is unknown, fall back to the queried
            // tab.active (best-effort, still better than a union including
            // transient clients since we routed via relay).
            let active = effective_active_tab
                .map(|at| tab.tab_id == at)
                .unwrap_or(tab.active);

            // Same predicate, reused as the is_focused scope below. Computed
            // once per tab (all panes in a tab share the same tab_id).
            let in_active_tab = effective_active_tab
                .map(|at| tab.tab_id == at)
                .unwrap_or(false);

            let panes: Vec<PaneMsg> = tab
                .panes
                .iter()
                // Plugin panes (background-only plugins like zellij:link, and
                // tab-bar/status-bar) are never user-facing terminals in the
                // Muxr model. Exclude them so they don't surface as selectable
                // panes in the client rail/picker. Single chokepoint: GetLayout
                // is the only RPC that returns a pane list.
                .filter(|p| pane_is_client_visible(p.is_plugin))
                .map(|p| {
                    // B-FOCUS: override is_focused with the per-relay-client
                    // value, but ONLY within the relay's EFFECTIVE active tab.
                    //
                    // The relay tracks a single focused pane — the one focused
                    // in ITS active tab. Each tab, though, has its own
                    // independently-focused pane. If we applied the override
                    // across ALL tabs we'd force is_focused=false on every
                    // legitimately-focused pane of the NON-active tabs (none of
                    // them match the single tracked focused_pane), hiding per-tab
                    // focus from consumers. So we scope the override to the
                    // effective active tab and leave the other tabs' queried
                    // is_focused untouched.
                    //
                    // When focused_pane is None (unknown, e.g. right after a bare
                    // SwitchTab with no subsequent FocusPane), leave the queried
                    // is_focused as-is everywhere (best-effort).
                    let is_focused = if in_active_tab {
                        relay_vs
                            .and_then(|vs| vs.focused_pane)
                            .map(|fp| fp.is_plugin == p.is_plugin && fp.id == p.id)
                            .unwrap_or(p.is_focused)
                    } else {
                        // Outside the effective active tab (or no effective tab at
                        // all — no view state, or a tracked pane that exists in no
                        // tab): keep the queried value, since each tab carries its
                        // own focus.
                        p.is_focused
                    };

                    PaneMsg {
                        id: p.id,
                        title: p.title.clone(),
                        is_focused,
                        is_floating: p.is_floating,
                        exited: p.exited,
                        command: p.command.clone(),
                        cwd: p.cwd.clone(),
                        x: p.x,
                        y: p.y,
                        rows: p.rows,
                        cols: p.cols,
                        is_plugin: p.is_plugin,
                        is_fullscreen: p.is_fullscreen,
                    }
                })
                .collect();

            TabMsg {
                position: tab.position,
                name: tab.name.clone(),
                active,
                has_bell: tab.has_bell,
                panes_to_hide: tab.panes_to_hide,
                tab_id: tab.tab_id as u32,
                panes,
                fullscreen_active: tab.fullscreen_active,
                floating_panes_visible: tab.floating_panes_visible,
            }
        })
        .collect()
}

/// Decide whether the B-FOCUS view-state override should be applied.
///
/// The override is only correct when the relay that served the query is the
/// CALLER'S OWN relay (exact `connection_id` match). When the fallback path was
/// taken (request `connection_id` is empty, or the resolved `relay_conn_id`
/// differs from the request's `connection_id`), applying a sibling relay's
/// `active_tab`/`focused_pane` would give the caller an actively wrong indicator
/// — worse than returning raw zellij values (Issue A fix).
///
/// Returns `true` only when all three conditions hold:
/// 1. the query was served via a relay (`via_relay`),
/// 2. the request carried a non-empty `connection_id` (caller has a known relay),
/// 3. the relay that served the query is the caller's own relay
///    (`relay_conn_id == request_connection_id`).
pub(crate) fn should_apply_view_state_override(
    via_relay: bool,
    request_connection_id: &str,
    relay_conn_id: &str,
) -> bool {
    via_relay && !request_connection_id.is_empty() && relay_conn_id == request_connection_id
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Plugin-pane visibility filter ───────────────────────────────────────

    #[test]
    fn terminal_pane_is_client_visible() {
        assert!(
            pane_is_client_visible(false),
            "terminal panes (is_plugin=false) must be visible to the client"
        );
    }

    #[test]
    fn plugin_pane_is_not_client_visible() {
        assert!(
            !pane_is_client_visible(true),
            "plugin panes (is_plugin=true) must be excluded from the client pane list"
        );
    }

    // ─── Issue A: view-state override suppression ─────────────────────────────

    #[test]
    fn override_applied_on_exact_connection_id_match() {
        // Exact match: via_relay + non-empty id + relay_conn_id == request id.
        assert!(
            should_apply_view_state_override(true, "conn-1", "conn-1"),
            "override must be applied when relay_conn_id == request connection_id"
        );
    }

    #[test]
    fn override_suppressed_when_request_connection_id_is_empty() {
        // Empty request connection_id → fallback path; no reliable caller identity.
        assert!(
            !should_apply_view_state_override(true, "", "conn-2"),
            "override must be suppressed when request connection_id is empty"
        );
    }

    #[test]
    fn override_suppressed_when_relay_conn_id_differs() {
        // relay_conn_id is a sibling relay's id — applying its view-state would
        // give the caller an actively wrong active_tab / focused_pane.
        assert!(
            !should_apply_view_state_override(true, "conn-A", "conn-B"),
            "override must be suppressed when relay_conn_id != request connection_id"
        );
    }

    #[test]
    fn override_suppressed_when_not_via_relay() {
        // Ephemeral query path: no relay view state at all.
        assert!(
            !should_apply_view_state_override(false, "conn-1", "conn-1"),
            "override must be suppressed when query was not served via relay"
        );
    }

    #[test]
    fn override_suppressed_when_all_empty() {
        // No connection_id and no relay: definitely no override.
        assert!(
            !should_apply_view_state_override(false, "", ""),
            "override must be suppressed with no relay and no connection_id"
        );
    }

    // ─── Same-tab focus after SwitchSpace: derived scope tab ──────────────────

    use crate::multiplexer::{PaneRef, PaneSnapshot, TabSnapshot};

    fn pane(id: u32, is_focused: bool, is_plugin: bool) -> PaneSnapshot {
        PaneSnapshot {
            id,
            title: format!("pane-{id}"),
            is_focused,
            is_plugin,
            ..PaneSnapshot::default()
        }
    }

    fn tab(tab_id: u64, active: bool, panes: Vec<PaneSnapshot>) -> TabSnapshot {
        TabSnapshot {
            tab_id,
            position: tab_id as u32,
            name: format!("tab-{tab_id}"),
            active,
            panes,
            ..TabSnapshot::default()
        }
    }

    /// Two tabs. Tab 1 is the backend's (daemon-global) active tab with pane 10
    /// focused; tab 2 holds panes 20 (unfocused) and 21 (focused). Tab 1 also
    /// carries a plugin pane, which the client-visibility filter drops.
    fn snapshot() -> LayoutSnapshot {
        LayoutSnapshot {
            tabs: vec![
                tab(
                    1,
                    true,
                    vec![
                        pane(10, true, false),
                        pane(11, false, false),
                        pane(90, true, true),
                    ],
                ),
                tab(
                    2,
                    false,
                    vec![pane(20, false, false), pane(21, true, false)],
                ),
            ],
        }
    }

    fn view_state(active_tab: Option<u64>, focused_pane: Option<PaneRef>) -> RelayViewState {
        RelayViewState {
            active_tab,
            focused_pane,
            current_space: Some("ws-b".to_owned()),
        }
    }

    /// Find a pane's `is_focused` in the built proto by (tab_id, pane id).
    fn focused_of(tabs: &[TabMsg], tab_id: u32, pane_id: u32) -> bool {
        tabs.iter()
            .find(|t| t.tab_id == tab_id)
            .and_then(|t| t.panes.iter().find(|p| p.id == pane_id))
            .unwrap_or_else(|| panic!("pane {pane_id} not found in tab {tab_id}"))
            .is_focused
    }

    #[test]
    fn same_tab_focus_without_tracked_tab_derives_the_containing_tab() {
        // Device gate: after a SwitchSpace the relay cleared active_tab, then a
        // SAME-TAB tap sent a bare FocusPane (PaneRef carries no tab). The scope
        // tab must be derived from the tracked pane, or the override is skipped
        // for every tab and the client keeps showing the pre-switch focus.
        let snap = snapshot();
        let vs = view_state(None, Some(PaneRef::terminal(20)));
        let tabs = build_tab_msgs(&snap, Some(&vs));

        // The containing tab (2) is scoped: the tracked pane wins, its sibling
        // loses the raw focus it was reported with.
        assert!(
            focused_of(&tabs, 2, 20),
            "tracked pane 20 must be reported focused in its containing tab"
        );
        assert!(
            !focused_of(&tabs, 2, 21),
            "sibling pane 21 must lose its raw is_focused inside the scoped tab"
        );
        // Other tabs keep their queried per-tab focus untouched.
        assert!(
            focused_of(&tabs, 1, 10),
            "pane 10 in the non-scoped tab must keep its queried is_focused"
        );
        assert!(
            !focused_of(&tabs, 1, 11),
            "pane 11 in the non-scoped tab must keep its queried is_focused"
        );
        // …and the derived tab is the one reported active.
        assert!(
            !tabs[0].active,
            "backend-active tab 1 must be cleared once the scope tab is derived"
        );
        assert!(tabs[1].active, "derived tab 2 must be reported active");
    }

    #[test]
    fn tracked_pane_in_no_tab_leaves_the_snapshot_untouched() {
        // Stale/closed pane: nothing to derive from → raw values, byte-identical
        // to running with no view state at all.
        let snap = snapshot();
        let vs = view_state(None, Some(PaneRef::terminal(999)));
        assert_eq!(
            build_tab_msgs(&snap, Some(&vs)),
            build_tab_msgs(&snap, None),
            "a tracked pane that exists in no tab must not change the output"
        );
    }

    #[test]
    fn tracked_plugin_pane_leaves_the_snapshot_untouched() {
        // Plugin panes are never client-visible, so a plugin PaneRef can never
        // resolve a scope tab — raw values.
        let snap = snapshot();
        let vs = view_state(None, Some(PaneRef::plugin(90)));
        assert_eq!(
            build_tab_msgs(&snap, Some(&vs)),
            build_tab_msgs(&snap, None),
            "a tracked plugin pane must not resolve a scope tab"
        );
    }

    #[test]
    fn tracked_active_tab_still_wins_over_derivation() {
        // active_tab known → behavior is exactly as before: the tracked tab is the
        // scope even when the tracked pane lives in a different tab.
        let snap = snapshot();
        let vs = view_state(Some(1), Some(PaneRef::terminal(20)));
        let tabs = build_tab_msgs(&snap, Some(&vs));

        assert!(tabs[0].active, "tracked tab 1 must stay the active tab");
        assert!(!tabs[1].active, "tab 2 must not become active");
        // Scope = tab 1; no pane there matches the tracked ref, so all of its
        // panes report unfocused (pre-existing behavior).
        assert!(
            !focused_of(&tabs, 1, 10),
            "scoped tab: no match → unfocused"
        );
        assert!(
            !focused_of(&tabs, 1, 11),
            "scoped tab: no match → unfocused"
        );
        // Tab 2 is out of scope: queried values survive.
        assert!(
            !focused_of(&tabs, 2, 20),
            "out-of-scope tab keeps raw value"
        );
        assert!(focused_of(&tabs, 2, 21), "out-of-scope tab keeps raw value");
    }

    #[test]
    fn tracked_tab_and_pane_scope_the_override_to_that_tab() {
        // The ordinary post-SwitchTab path (unchanged).
        let snap = snapshot();
        let vs = view_state(Some(2), Some(PaneRef::terminal(20)));
        let tabs = build_tab_msgs(&snap, Some(&vs));

        assert!(!tabs[0].active);
        assert!(tabs[1].active);
        assert!(
            focused_of(&tabs, 2, 20),
            "tracked pane focused in tracked tab"
        );
        assert!(!focused_of(&tabs, 2, 21), "sibling cleared in tracked tab");
        assert!(focused_of(&tabs, 1, 10), "other tab untouched");
    }

    #[test]
    fn no_tracked_tab_and_no_tracked_pane_leaves_raw_values() {
        // Nothing to say → raw values, byte-identical to no view state.
        let snap = snapshot();
        let vs = view_state(None, None);
        assert_eq!(
            build_tab_msgs(&snap, Some(&vs)),
            build_tab_msgs(&snap, None),
            "an empty view state must not change the output"
        );
        // And the raw output really is the backend's own view.
        let raw = build_tab_msgs(&snap, None);
        assert!(raw[0].active, "raw: backend-active tab 1");
        assert!(!raw[1].active, "raw: tab 2 inactive");
        assert!(focused_of(&raw, 1, 10));
        assert!(focused_of(&raw, 2, 21));
    }

    #[test]
    fn plugin_panes_are_filtered_from_the_built_tabs() {
        // Guards the client-visibility chokepoint the derivation also relies on.
        let tabs = build_tab_msgs(&snapshot(), None);
        assert_eq!(
            tabs[0].panes.len(),
            2,
            "plugin pane 90 must be filtered out"
        );
        assert!(tabs[0].panes.iter().all(|p| !p.is_plugin));
    }

    // ─── resolve_effective_active_tab (pure derivation) ───────────────────────

    #[test]
    fn effective_tab_is_none_without_view_state() {
        assert_eq!(resolve_effective_active_tab(None, &snapshot()), None);
    }

    #[test]
    fn effective_tab_prefers_the_tracked_tab() {
        let vs = view_state(Some(1), Some(PaneRef::terminal(20)));
        assert_eq!(
            resolve_effective_active_tab(Some(&vs), &snapshot()),
            Some(1),
            "a tracked active_tab must be returned verbatim"
        );
    }

    #[test]
    fn effective_tab_derives_from_the_tracked_pane() {
        let vs = view_state(None, Some(PaneRef::terminal(21)));
        assert_eq!(
            resolve_effective_active_tab(Some(&vs), &snapshot()),
            Some(2),
            "with no tracked tab, the tab containing the tracked pane is the scope"
        );
    }

    #[test]
    fn effective_tab_is_none_for_unknown_or_empty_state() {
        let snap = snapshot();
        let unknown = view_state(None, Some(PaneRef::terminal(999)));
        assert_eq!(
            resolve_effective_active_tab(Some(&unknown), &snap),
            None,
            "a pane in no tab must not resolve a scope"
        );
        let empty = view_state(None, None);
        assert_eq!(
            resolve_effective_active_tab(Some(&empty), &snap),
            None,
            "an empty view state must not resolve a scope"
        );
    }
}
