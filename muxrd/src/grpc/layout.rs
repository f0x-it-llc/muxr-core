//! GetLayout RPC implementation with relay-routed query and B-FOCUS override.
//!
//! Two mutually exclusive paths, chosen by `SessionRef.space_id`:
//!
//! - **empty `space_id` (every legacy client)** — "this connection's view": route
//!   the query through the caller's relay when one is attached, fall back to an
//!   ephemeral backend query, then apply the B-FOCUS per-connection view-state
//!   override. Unchanged.
//! - **non-empty `space_id`** — a **read-only peek** at an explicitly named herdr
//!   space. It always bypasses relay *routing* (the relay's stream points at the
//!   space the connection is viewing, and re-pointing it to read another one is
//!   exactly the focus move this feature must not make) and moves no focus —
//!   neither the connection's nor the daemon's. Whether the B-FOCUS override
//!   applies depends on **whose** space was named:
//!   - the caller's **OWN** current space → the override applies exactly as on the
//!     legacy path. It must: herdr's raw focus tracks the *desktop* (the relay is
//!     pure re-attach — `switch_space`/`go_to_tab`/`focus_pane` never call herdr's
//!     daemon-global focus), so without the override the same space would answer
//!     differently depending on whether the client named it;
//!   - a **foreign** space → no override, since the connection's tracked
//!     `active_tab`/`focused_pane` belong to a different space and would be an
//!     actively wrong indicator here.
//!
//!   "Own" is decided ONLY from an exact tracked space id
//!   ([`ConnectionSpace::Space`]). A hint-less relay (attached with no resume
//!   hint) never gets one — its `current_space` stays `None` for the
//!   connection's whole life — so its own-space peek gets NO override either;
//!   see [`MuxrService::space_scoped_layout`] for why that gap is the correct,
//!   if incomplete, answer.
//!
//! Both paths converge on the same snapshot → proto tail ([`build_layout`]), so
//! the plugin-pane filter chokepoint cannot drift between them. GetLayout is a
//! **read** on both paths: no `reject_if_read_only` gate (matching GetSpaces).

use tonic::{Request, Response, Status};

use crate::multiplexer::{LayoutSnapshot, MuxBackend, UnknownSpace};
use crate::proto::{Layout, PaneMsg, SessionRef, TabMsg};
use crate::relay::RelayViewState;

use super::MuxrService;
use super::helpers::short_conn;
use super::space_ops::{ConnectionSpace, validate_space_id};

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
        let space_id = req.space_id;
        let (backend, bare) = self.resolve_session(&session)?;
        // FS3: full connection_id must not appear in info/warn logs.
        log::info!(
            "GetLayout: session='{session}' connection_id={}…",
            short_conn(&connection_id)
        );
        log::debug!("GetLayout: session='{session}' connection_id='{connection_id}'");

        // ── Space-scoped read: an explicit named-space peek ──────────────────
        // Routes around the relay (its stream points at the space the connection
        // is viewing) but NOT necessarily around the B-FOCUS override: the named
        // space may well be the caller's own, and then the override is as correct
        // here as it is on the legacy path. `space_scoped_layout` decides.
        // `should_apply_view_state_override` still never runs for it — that
        // predicate is about which RELAY served a query, and no relay served this
        // one. Either way a pure read: no focus moves anywhere.
        if is_space_scoped(&space_id) {
            return self
                .space_scoped_layout(&session, &connection_id, &backend, &bare, &space_id)
                .await;
        }

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
        let layout = build_layout(&snapshot, relay_vs.as_ref());

        // FS3: relay_conn_id is the matched relay's connection_id (same 32-char secret);
        // redact to the 8-char prefix at info, keep the full value at debug only.
        log::info!(
            "GetLayout: session='{}' relay_conn={}… → {} tab(s), \
             {} total pane group(s), via_relay={via_relay}",
            session,
            short_conn(&relay_conn_id),
            layout.tabs.len(),
            layout.tabs.iter().map(|t| t.panes.len()).sum::<usize>()
        );
        log::debug!(
            "GetLayout: session='{}' relay_conn='{relay_conn_id}' → {} tab(s), \
             {} total pane group(s), via_relay={via_relay}",
            session,
            layout.tabs.len(),
            layout.tabs.iter().map(|t| t.panes.len()).sum::<usize>()
        );

        Ok(Response::new(layout))
    }

    // ── GetLayout, space-scoped read ────────────────────────────────────────

    /// Answer a GetLayout for an explicitly named space — a **read-only peek**.
    ///
    /// Deliberately routes around the relay: the connection's relay stream points
    /// at the space the connection is *viewing*, and re-pointing it to read
    /// another one is exactly the focus move this feature must not make. The
    /// answer therefore comes from a direct, workspace-scoped backend query.
    ///
    /// The B-FOCUS override is applied **iff the named space is the caller's own
    /// current space** ([`space_is_own`]) — decided ONLY from the exact tracked id
    /// in [`ConnectionSpace::Space`]. That is not a nicety: the herdr relay is pure
    /// re-attach — it never calls herdr's daemon-global focus — so the raw queried
    /// `active`/`is_focused` track the DESKTOP's focus, not this connection's.
    /// Without the override, `GetLayout{space_id: <my own space>}` would
    /// contradict the empty-`space_id` answer for the very same space. For a
    /// genuinely foreign space the override is skipped, since the caller's tracked
    /// `active_tab`/`focused_pane` live in a different space and the raw values are
    /// the truth there.
    ///
    /// **The DaemonActive gap.** [`ConnectionSpace::DaemonActive`] — a relay
    /// attached with no resume hint, whose `current_space` therefore stays `None`
    /// for its whole life — is treated as foreign here too, same as `Unknown`, even
    /// though at attach time it *was* viewing the daemon's then-active workspace. A
    /// round-1 predicate tried to resolve "own" for this case anyway by asking the
    /// backend which space is CURRENTLY daemon-active
    /// ([`crate::multiplexer::MuxBackend::list_spaces`]) and asserting the relay
    /// must still be viewing it — sound only until the desktop switches workspaces
    /// out from under the connection, after which it produces both a false
    /// positive (a peek at the NEW daemon-active space wrongly inherits this
    /// connection's stale view state) and a false negative (a peek at the
    /// connection's own, unchanged space is wrongly judged foreign). Reverted for
    /// exactly that reason: a missing override here is a stale indicator, but a
    /// wrong one is an actively false one, and the raw (desktop-focus) values this
    /// gap falls back to are never worse than what the legacy empty-`space_id` path
    /// would report for the same hint-less connection. The real fix is seeding
    /// `current_space` at attach time with the resolved target workspace id — the
    /// `resumed_view_for` / `view_state_from_resumed` plumbing already does exactly
    /// this for a *resumed* attach (see `multiplexer::herdr::relay` and
    /// `relay::mod`); extending it to the hint-less case is the follow-up,
    /// deliberately not done here to keep `relay/` untouched.
    ///
    /// Errors:
    /// - malformed `space_id` → `invalid_argument` (same guard as the mutating
    ///   space ops — [`validate_space_id`]),
    /// - a backend with no space axis (zellij) → `invalid_argument`,
    /// - an id naming no live space → `not_found`,
    /// - any other backend failure → a terse `internal` (see [`space_query_status`]).
    async fn space_scoped_layout(
        &self,
        session: &str,
        connection_id: &str,
        backend: &std::sync::Arc<dyn MuxBackend>,
        bare: &str,
        space_id: &str,
    ) -> Result<Response<Layout>, Status> {
        validate_space_id(space_id)?;
        if rejects_space_scope(space_id, backend.supports_spaces()) {
            log::info!(
                "GetLayout: session='{session}' rejected space_id='{space_id}' — \
                 backend has no space axis"
            );
            return Err(Status::invalid_argument(
                "space_id: this session's backend does not support spaces",
            ));
        }

        // Query first: an unknown id fails here (`not_found`) before we even ask
        // the own-space question below.
        let snapshot = space_query_layout(backend, bare, space_id).await?;

        // ── Is the named space the caller's OWN? ─────────────────────────────
        // Same per-connection rule GetSpaces uses (`connection_space`, exact
        // connection_id, no session-scoped fallback) — but unlike GetSpaces we do
        // NOT collapse `DaemonActive` into a resolved answer. Only
        // `ConnectionSpace::Space` names a space this connection is actually known
        // to be viewing; `DaemonActive` (a hint-less relay whose own space was
        // never tracked) and `Unknown` both read as foreign here — see this
        // function's doc for why a stale-daemon-lookup resolution was tried and
        // reverted.
        let view = self.connection_space(session, connection_id);
        let own_space = space_is_own(&view, space_id);

        // Own space → the tracked view state describes exactly this space; apply
        // the same override the legacy path applies (clone out of the DashMap
        // guard — never held across an `.await`).
        let relay_vs: Option<RelayViewState> = if own_space {
            self.view_state
                .get(connection_id)
                .filter(|entry| entry.session == session)
                .map(|entry| entry.state.clone())
        } else {
            None
        };

        let layout = build_layout(&snapshot, relay_vs.as_ref());
        log::info!(
            "GetLayout: session='{session}' space_id='{space_id}' → {} tab(s), \
             {} total pane group(s), space_scoped=true own_space={own_space}",
            layout.tabs.len(),
            layout.tabs.iter().map(|t| t.panes.len()).sum::<usize>()
        );
        Ok(Response::new(layout))
    }
}

// ─── Private async helpers ────────────────────────────────────────────────────

/// Query the **session's own** layout in a blocking task, mapping errors to
/// [`Status`].
///
/// Used by all ephemeral paths in `get_layout_impl` (no relay attached, relay
/// fallback on error/timeout/cancel, relay sender closed). Passes `None` for the
/// space, which the backend default makes byte-identical to a direct
/// `query_layout()` call. The space-scoped read has its own mapper
/// ([`space_query_layout`]) because its error hygiene differs.
async fn backend_query_layout(
    backend: &std::sync::Arc<dyn MuxBackend>,
    session: &str,
) -> Result<LayoutSnapshot, Status> {
    let backend = backend.clone();
    let session = session.to_owned();
    tokio::task::spawn_blocking(move || backend.query_layout_for_space(&session, None))
        .await
        .map_err(|e| Status::internal(format!("GetLayout query task panicked: {e}")))?
        .map_err(|e| {
            log::warn!("GetLayout backend query failed: {e:#}");
            Status::internal(format!("GetLayout query failed: {e:#}"))
        })
}

/// Query a **named space's** layout in a blocking task, mapping errors to terse
/// statuses ([`space_query_status`]).
async fn space_query_layout(
    backend: &std::sync::Arc<dyn MuxBackend>,
    session: &str,
    space_id: &str,
) -> Result<LayoutSnapshot, Status> {
    let b = backend.clone();
    let session_owned = session.to_owned();
    let space = space_id.to_owned();
    tokio::task::spawn_blocking(move || b.query_layout_for_space(&session_owned, Some(&space)))
        .await
        .map_err(|e| {
            log::warn!("GetLayout: space-scoped query task panicked: {e}");
            Status::internal("GetLayout: query task failed")
        })?
        .map_err(|e| space_query_status(space_id, &e))
}

// ─── Pure helpers (also used by tests) ───────────────────────────────────────

/// A pane is client-visible iff it is a real terminal pane. Plugin panes
/// (background plugins + tab-bar/status-bar) are excluded from GetLayout.
pub(crate) fn pane_is_client_visible(is_plugin: bool) -> bool {
    !is_plugin
}

/// Whether the request names an explicit space to read.
///
/// The **empty string is the back-compat contract**: every client written before
/// space-scoped GetLayout sends it (proto3 scalars default to `""`), and it must
/// mean "behave exactly as before" — relay-routed, B-FOCUS-overridden, bound to
/// the connection's / backend's own space.
pub(crate) fn is_space_scoped(space_id: &str) -> bool {
    !space_id.is_empty()
}

/// Whether an explicit space scope must be **rejected** for this backend.
///
/// Naming a space only means something on a backend that has a space axis
/// ([`MuxBackend::supports_spaces`] — herdr). Asking for one on zellij is a client
/// error, not a request to silently ignore: answering with the session's ordinary
/// layout would look like a successful peek at a space that does not exist. An
/// empty `space_id` is never rejected — that is the legacy path on every backend.
pub(crate) fn rejects_space_scope(space_id: &str, backend_supports_spaces: bool) -> bool {
    is_space_scoped(space_id) && !backend_supports_spaces
}

/// Whether the named space is the **caller's own** current space — the condition
/// under which the space-scoped read applies the B-FOCUS override.
///
/// `view` is the caller's tracked per-connection view
/// ([`MuxrService::connection_space`]). Only [`ConnectionSpace::Space`] can
/// answer "own": it is the one arm that carries an exact herdr workspace id this
/// connection is known to be viewing (set only by `switch_space` / a resumed
/// attach — see `relay::view_state_from_resumed`).
///
/// - [`Unknown`](ConnectionSpace::Unknown) → `false`. No view state means no
///   override to apply anyway (and no way to tell whose space this is).
/// - [`DaemonActive`](ConnectionSpace::DaemonActive) → `false`, always. A relay
///   attached with no resume hint never gets a tracked `current_space` — it stays
///   `None` for the connection's whole life, not just until the first switch —
///   so there is no tracked id to compare against; treating it as foreign is the
///   conservative answer (a missing override is a stale indicator, a wrong one is
///   an actively false one). A round-1 predicate instead asked the backend which
///   space is CURRENTLY daemon-active and compared against that, which is sound
///   only until the desktop switches workspaces out from under the connection —
///   reverted (see [`MuxrService::space_scoped_layout`] for the full failure
///   mode).
/// - [`Space(id)`](ConnectionSpace::Space) → own iff the ids match.
pub(crate) fn space_is_own(view: &ConnectionSpace, space_id: &str) -> bool {
    matches!(view, ConnectionSpace::Space(current) if current == space_id)
}

/// Map a space-scoped backend failure to a **terse** [`Status`].
///
/// Two rules, both deliberate:
///
/// 1. An [`UnknownSpace`] anywhere in the chain → `not_found`, so a client can
///    tell "no such space" from "a real space that happens to be empty". Nothing
///    else in the chain is inspected — no string matching.
/// 2. Everything else → a fixed `internal` message. The error chain is logged
///    server-side and **never forwarded**: it carries the herdr socket path, and
///    its wording varies per failure mode, which would turn every error into a
///    finer-grained existence oracle than the `not_found` above (that one is the
///    deliberate, documented disclosure — see the `space_id` note in
///    `muxr.proto`). The legacy path keeps its own chain-forwarding mapper; only
///    the space path, which takes a client-chosen id, is hardened here.
pub(crate) fn space_query_status(space_id: &str, err: &anyhow::Error) -> Status {
    if UnknownSpace::in_chain(err) {
        log::info!("GetLayout: space_id='{space_id}' names no live space → not_found");
        log::debug!("GetLayout: unknown space_id='{space_id}': {err:#}");
        Status::not_found("space_id: no such space")
    } else {
        log::warn!("GetLayout: space-scoped query failed for space_id='{space_id}': {err:#}");
        Status::internal("GetLayout: backend error")
    }
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

/// The single snapshot → proto [`Layout`] tail, shared by **both** GetLayout
/// paths: the relay/ephemeral path (which passes its resolved `relay_vs`) and the
/// space-scoped read (which always passes `None` — a foreign space has no
/// per-connection view state).
///
/// Sharing it is the point: the tab/pane mapping — including the plugin-pane
/// filter chokepoint inside [`build_tab_msgs`] — can never drift between the two.
fn build_layout(snapshot: &LayoutSnapshot, relay_vs: Option<&RelayViewState>) -> Layout {
    Layout {
        tabs: build_tab_msgs(snapshot, relay_vs),
    }
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

    // ─── Space-scoped read: branch predicates ────────────────────────────────

    #[test]
    fn empty_space_id_is_not_space_scoped() {
        // The back-compat contract: every pre-feature client sends "" and must
        // keep the relay-routed / B-FOCUS-overridden path.
        assert!(
            !is_space_scoped(""),
            "an empty space_id must take the legacy path"
        );
    }

    #[test]
    fn non_empty_space_id_is_space_scoped() {
        assert!(is_space_scoped("ws-2"));
    }

    #[test]
    fn space_scope_rejected_only_when_named_on_a_space_less_backend() {
        // Named space + no space axis (zellij) → reject.
        assert!(
            rejects_space_scope("ws-2", false),
            "naming a space on a backend without one must be rejected"
        );
        // Named space + space axis (herdr) → honour.
        assert!(!rejects_space_scope("ws-2", true));
        // No space named → never rejected, on either backend (legacy path).
        assert!(!rejects_space_scope("", false));
        assert!(!rejects_space_scope("", true));
    }

    // ─── Space-scoped read: shared mapping tail ──────────────────────────────

    #[test]
    fn space_scoped_mapping_matches_the_raw_relay_less_mapping() {
        // Both paths converge on `build_layout`; the space path passes relay_vs
        // = None, so its output must equal the normal path's raw (no view state)
        // output for the same snapshot — plugin-pane filter included.
        let snap = snapshot();
        let layout = build_layout(&snap, None);
        assert_eq!(layout.tabs, build_tab_msgs(&snap, None));
        assert!(
            layout.tabs[0].panes.iter().all(|p| !p.is_plugin),
            "the plugin-pane chokepoint applies to the space-scoped path too"
        );
    }

    // ─── Space-scoped read: end-to-end routing through get_layout_impl ────────

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::cli::BackendKind;
    use crate::multiplexer::{
        ActionAck, BackendSet, DualHandle, MuxBackend, ResizeDir, ResizeKind, ScrollDir,
    };
    use crate::proto::SessionRef;
    use crate::relay::{ControlEntry, RelayControl, ViewStateEntry};
    use tonic::Request;

    /// A spaces-capable backend that reports which query arm was taken.
    ///
    /// `query_layout` (session path) yields tab 1; `query_layout_for_space`
    /// with an explicit **known** id yields tab 2 — so a test can tell from the
    /// response alone which arm ran, and the recorded `last_space` proves the id
    /// reached the backend verbatim. The tab is the same for every known space:
    /// which space was read is asserted through `last_space`, not the payload.
    ///
    /// It knows two spaces — [`DAEMON_ACTIVE_SPACE`] (the daemon's active one) and
    /// [`OTHER_SPACE`] — and models the real herdr contract for anything else: an
    /// unknown id fails with [`UnknownSpace`], wrapped in a context layer carrying
    /// an internal-looking detail so tests can prove neither the chain nor the
    /// socket path reaches the client.
    #[derive(Debug, Default)]
    struct SpacesBackend {
        session_queries: AtomicUsize,
        space_queries: AtomicUsize,
        list_spaces_calls: AtomicUsize,
        last_space: std::sync::Mutex<Option<String>>,
    }

    /// The stub daemon's active space (what a relay that never switched space is
    /// viewing — the `current_space = None` case).
    const DAEMON_ACTIVE_SPACE: &str = "ws-1";
    /// A second, non-active space on the stub daemon.
    const OTHER_SPACE: &str = "ws-2";
    /// An internal detail the stub's error chain carries; it must never reach the
    /// client (it stands in for the herdr socket path).
    const STUB_INTERNAL_DETAIL: &str = "/run/user/1000/herdr.sock";

    impl SpacesBackend {
        /// One tab with two panes — the first focused. Two panes so that a
        /// would-be B-FOCUS override is *visible* in the response (it would move
        /// `is_focused` onto the tracked pane).
        fn snapshot(tab_id: u64) -> LayoutSnapshot {
            let base = tab_id as u32 * 10;
            LayoutSnapshot {
                tabs: vec![tab(
                    tab_id,
                    true,
                    vec![pane(base, true, false), pane(base + 1, false, false)],
                )],
            }
        }
    }

    impl MuxBackend for SpacesBackend {
        fn supports_spaces(&self) -> bool {
            true
        }
        fn query_layout(&self, _: &str) -> anyhow::Result<LayoutSnapshot> {
            self.session_queries.fetch_add(1, Ordering::Relaxed);
            Ok(Self::snapshot(1))
        }
        fn query_layout_for_space(
            &self,
            session: &str,
            space_id: Option<&str>,
        ) -> anyhow::Result<LayoutSnapshot> {
            match space_id {
                Some(id) => {
                    self.space_queries.fetch_add(1, Ordering::Relaxed);
                    *self.last_space.lock().expect("last_space lock") = Some(id.to_owned());
                    if !matches!(id, DAEMON_ACTIVE_SPACE | OTHER_SPACE) {
                        // The herdr contract: unknown id → typed `UnknownSpace`,
                        // never an empty Ok. Wrapped in context so the test also
                        // proves `in_chain` looks THROUGH context layers.
                        return Err(anyhow::Error::new(UnknownSpace::new(id))
                            .context(format!("herdr: workspace.list via {STUB_INTERNAL_DETAIL}")));
                    }
                    Ok(Self::snapshot(2))
                }
                // Mirrors the trait default: no space named → the session path.
                None => self.query_layout(session),
            }
        }
        fn list_spaces(&self, _: &str) -> anyhow::Result<Vec<crate::multiplexer::SpaceSnapshot>> {
            self.list_spaces_calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![
                crate::multiplexer::SpaceSnapshot {
                    id: DAEMON_ACTIVE_SPACE.to_owned(),
                    name: "one".to_owned(),
                    active: true,
                },
                crate::multiplexer::SpaceSnapshot {
                    id: OTHER_SPACE.to_owned(),
                    name: "two".to_owned(),
                    active: false,
                },
            ])
        }
        fn list_sessions(&self) -> anyhow::Result<Vec<(String, Duration)>> {
            unimplemented!()
        }
        fn list_sessions_with_resurrectables(&self) -> anyhow::Result<Vec<(String, u64, bool)>> {
            unimplemented!()
        }
        fn validate_session_name(&self, _: &str) -> Result<(), String> {
            unimplemented!()
        }
        fn create_session(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn kill_session(&self, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn rename_session(&self, _: &str, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn write_to_pane(&self, _: &str, _: PaneRef, _: Vec<u8>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn focus_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_pane(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_pane(&self, _: &str, _: bool, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_pane(&self, _: &str, _: PaneRef, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn resize_pane(
            &self,
            _: &str,
            _: PaneRef,
            _: ResizeKind,
            _: Option<ResizeDir>,
        ) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_floating(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn toggle_pane_fullscreen(&self, _: &str, _: PaneRef) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn scroll_pane(&self, _: &str, _: PaneRef, _: ScrollDir) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn new_tab(&self, _: &str, _: Option<String>) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn close_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn go_to_tab(&self, _: &str, _: u64) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn rename_tab(&self, _: &str, _: u64, _: String) -> anyhow::Result<ActionAck> {
            unimplemented!()
        }
        fn query_session_size(&self, _: &str) -> anyhow::Result<(u16, u16)> {
            unimplemented!()
        }
        fn pane_is_floating_with_visibility(
            &self,
            _: &str,
            _: PaneRef,
        ) -> anyhow::Result<(bool, bool, Option<PaneRef>)> {
            unimplemented!()
        }
        fn open_attach(&self, _: &str, _: u16, _: u16, _: bool) -> anyhow::Result<DualHandle> {
            unimplemented!()
        }
        fn backend_version(&self) -> String {
            "spaces-stub".to_owned()
        }
    }

    /// A service driving a single herdr-kind [`SpacesBackend`], plus a handle on
    /// that backend so tests can read its call counters.
    fn spaces_service() -> (MuxrService, Arc<SpacesBackend>) {
        let backend = Arc::new(SpacesBackend::default());
        let service = MuxrService::with_backends(BackendSet::single(
            BackendKind::Herdr,
            Arc::clone(&backend) as Arc<dyn MuxBackend>,
        ));
        (service, backend)
    }

    #[tokio::test]
    async fn explicit_space_id_reads_that_space_without_the_relay() {
        // The space arm must run — with the id forwarded verbatim — and the
        // session arm must NOT (no relay is registered, so the legacy path would
        // otherwise take the ephemeral session query).
        let (service, backend) = spaces_service();
        let layout = service
            .get_layout_impl(Request::new(SessionRef {
                session: "herdr:herdr".to_owned(),
                connection_id: "conn-1".to_owned(),
                space_id: "ws-2".to_owned(),
            }))
            .await
            .expect("space-scoped GetLayout must succeed")
            .into_inner();

        assert_eq!(
            backend.space_queries.load(Ordering::Relaxed),
            1,
            "the space arm must have been queried"
        );
        assert_eq!(
            backend.session_queries.load(Ordering::Relaxed),
            0,
            "the session arm must NOT run for an explicit space"
        );
        assert_eq!(
            backend
                .last_space
                .lock()
                .expect("last_space lock")
                .as_deref(),
            Some("ws-2"),
            "the space id must reach the backend verbatim"
        );
        assert_eq!(
            layout.tabs.iter().map(|t| t.tab_id).collect::<Vec<_>>(),
            vec![2],
            "the response must carry the NAMED space's tabs"
        );
    }

    #[tokio::test]
    async fn empty_space_id_takes_the_session_path() {
        // Back-compat: "" must behave exactly as before — the ordinary
        // (here relay-less → ephemeral) session query.
        let (service, backend) = spaces_service();
        let layout = service
            .get_layout_impl(Request::new(SessionRef {
                session: "herdr:herdr".to_owned(),
                connection_id: String::new(),
                space_id: String::new(),
            }))
            .await
            .expect("legacy GetLayout must succeed")
            .into_inner();

        assert_eq!(backend.session_queries.load(Ordering::Relaxed), 1);
        assert_eq!(
            backend.space_queries.load(Ordering::Relaxed),
            0,
            "an empty space_id must never reach the space arm"
        );
        assert_eq!(
            layout.tabs.iter().map(|t| t.tab_id).collect::<Vec<_>>(),
            vec![1],
            "the response must carry the session's own tabs"
        );
    }

    #[tokio::test]
    async fn space_id_on_a_zellij_session_is_invalid_argument() {
        // zellij has no space axis: a named space is a client error, never a
        // silently space-less answer. Rejected BEFORE any backend query.
        let service = MuxrService::new(); // sole backend: zellij
        let status = service
            .get_layout_impl(Request::new(SessionRef {
                session: "zellij:dev".to_owned(),
                connection_id: String::new(),
                space_id: "ws-2".to_owned(),
            }))
            .await
            .expect_err("a space_id on zellij must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("does not support spaces"),
            "unexpected message: {}",
            status.message()
        );
    }

    #[tokio::test]
    async fn foreign_space_id_bypasses_the_relay_and_the_b_focus_override() {
        // The caller HAS a live relay and tracked view state for this session, and
        // is viewing ws-1 while asking for ws-2. A FOREIGN-space peek must bypass
        // both: no RelayControl is queued, and the returned panes keep their RAW
        // is_focused (the tracked pane belongs to the caller's own space, not the
        // one being read).
        let (service, backend) = spaces_service();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RelayControl>();
        service.control.insert(
            "conn-1".to_owned(),
            ControlEntry {
                session: "herdr:herdr".to_owned(),
                sender: tx,
                read_only: false,
            },
        );
        service.view_state.insert(
            "conn-1".to_owned(),
            ViewStateEntry {
                session: "herdr:herdr".to_owned(),
                state: RelayViewState {
                    active_tab: Some(2),
                    focused_pane: Some(PaneRef::terminal(21)),
                    current_space: Some("ws-1".to_owned()),
                },
            },
        );

        let layout = service
            .get_layout_impl(Request::new(SessionRef {
                session: "herdr:herdr".to_owned(),
                connection_id: "conn-1".to_owned(),
                space_id: "ws-2".to_owned(),
            }))
            .await
            .expect("space-scoped GetLayout must succeed")
            .into_inner();

        assert!(
            rx.try_recv().is_err(),
            "the relay must not be asked to serve a foreign-space read"
        );
        assert_eq!(backend.space_queries.load(Ordering::Relaxed), 1);
        assert!(
            focused_of(&layout.tabs, 2, 20),
            "raw is_focused must survive — the B-FOCUS override must not run"
        );
        assert!(
            !focused_of(&layout.tabs, 2, 21),
            "the caller's tracked pane must NOT be marked focused in a foreign space"
        );
    }

    // ─── M1: the own-space B-FOCUS override ──────────────────────────────────

    /// Register a live relay + per-connection view state for `conn-1`: tracking
    /// pane 21 (in tab 2 — the tab every space query answers with) and viewing
    /// `current_space`. Returns the relay's control receiver so a test can prove
    /// nothing was routed through it.
    fn attach_conn_1(
        service: &MuxrService,
        current_space: Option<&str>,
    ) -> tokio::sync::mpsc::UnboundedReceiver<RelayControl> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<RelayControl>();
        service.control.insert(
            "conn-1".to_owned(),
            ControlEntry {
                session: "herdr:herdr".to_owned(),
                sender: tx,
                read_only: false,
            },
        );
        service.view_state.insert(
            "conn-1".to_owned(),
            ViewStateEntry {
                session: "herdr:herdr".to_owned(),
                state: RelayViewState {
                    active_tab: Some(2),
                    focused_pane: Some(PaneRef::terminal(21)),
                    current_space: current_space.map(str::to_owned),
                },
            },
        );
        rx
    }

    /// Ask for `space_id` as `conn-1`.
    async fn space_layout_as_conn_1(
        service: &MuxrService,
        space_id: &str,
    ) -> Result<Layout, Status> {
        service
            .get_layout_impl(Request::new(SessionRef {
                session: "herdr:herdr".to_owned(),
                connection_id: "conn-1".to_owned(),
                space_id: space_id.to_owned(),
            }))
            .await
            .map(Response::into_inner)
    }

    #[test]
    fn own_space_predicate_covers_the_three_view_states() {
        // A tracked current_space: plain id equality — the only arm that can ever
        // answer "own".
        assert!(space_is_own(
            &ConnectionSpace::Space(OTHER_SPACE.to_owned()),
            OTHER_SPACE,
        ));
        assert!(!space_is_own(
            &ConnectionSpace::Space(DAEMON_ACTIVE_SPACE.to_owned()),
            OTHER_SPACE,
        ));
        // Never switched (current_space = None): the connection's own space was
        // never tracked, so it is ALWAYS foreign here — regardless of which space
        // is named, including the daemon's actual active one. (The regression this
        // guards against: a round-1 predicate resolved this arm via a live
        // daemon-active lookup, which went stale the moment the desktop switched
        // workspaces out from under the connection.)
        assert!(!space_is_own(
            &ConnectionSpace::DaemonActive,
            DAEMON_ACTIVE_SPACE,
        ));
        assert!(!space_is_own(&ConnectionSpace::DaemonActive, OTHER_SPACE));
        // No view state at all: nothing to apply.
        assert!(!space_is_own(
            &ConnectionSpace::Unknown,
            DAEMON_ACTIVE_SPACE,
        ));
    }

    #[tokio::test]
    async fn own_space_request_applies_the_b_focus_override() {
        // M1: the named space IS the space this connection is viewing, so its
        // tracked focus is the truth for it — the raw queried values track the
        // DESKTOP's focus (the herdr relay is pure re-attach and never calls
        // herdr's daemon-global focus). Without this, naming your own space would
        // contradict the empty-space_id answer for that same space.
        let (service, backend) = spaces_service();
        let mut rx = attach_conn_1(&service, Some(OTHER_SPACE));

        let layout = space_layout_as_conn_1(&service, OTHER_SPACE)
            .await
            .expect("own-space GetLayout must succeed");

        assert!(
            rx.try_recv().is_err(),
            "an own-space read must still bypass relay ROUTING (no focus moves)"
        );
        assert_eq!(backend.space_queries.load(Ordering::Relaxed), 1);
        assert!(
            focused_of(&layout.tabs, 2, 21),
            "the connection's tracked pane must be reported focused in its own space"
        );
        assert!(
            !focused_of(&layout.tabs, 2, 20),
            "the raw (desktop-focused) pane must lose is_focused under the override"
        );
        assert_eq!(
            backend.list_spaces_calls.load(Ordering::Relaxed),
            0,
            "a tracked current_space answers the own-space question with no extra round trip"
        );
    }

    #[tokio::test]
    async fn a_hint_less_relay_gets_no_override_for_the_daemon_active_space() {
        // REGRESSION GUARD (round-1 revert): the `current_space == None` relay
        // never gets its own space tracked — not "until the first switch", but for
        // its whole life — so it must get NO override here, even when the named
        // space happens to be the daemon's active one. A round-1 predicate
        // resolved this via a live `daemon_active_space()` lookup and applied the
        // override on a match; that broke the moment the desktop switched
        // workspaces out from under the connection (this connection's stale view
        // state could then land on a DIFFERENT, now-active space). No daemon
        // lookup must run at all any more.
        let (service, backend) = spaces_service();
        let _rx = attach_conn_1(&service, None);

        let layout = space_layout_as_conn_1(&service, DAEMON_ACTIVE_SPACE)
            .await
            .expect("space-scoped GetLayout must succeed");

        assert_eq!(
            backend.list_spaces_calls.load(Ordering::Relaxed),
            0,
            "the DaemonActive arm must never resolve the daemon-active space any more"
        );
        assert!(
            focused_of(&layout.tabs, 2, 20),
            "raw is_focused must survive — a hint-less relay's own-space peek is untracked"
        );
        assert!(
            !focused_of(&layout.tabs, 2, 21),
            "the tracked pane must NOT be marked focused: no override applies"
        );
    }

    #[tokio::test]
    async fn a_hint_less_relay_treats_a_non_active_space_as_foreign() {
        // Same `current_space == None` relay, naming a space the daemon does NOT
        // have active — foreign either way, before or after the fix: raw values.
        let (service, backend) = spaces_service();
        let _rx = attach_conn_1(&service, None);

        let layout = space_layout_as_conn_1(&service, OTHER_SPACE)
            .await
            .expect("foreign-space GetLayout must succeed");

        assert_eq!(
            backend.list_spaces_calls.load(Ordering::Relaxed),
            0,
            "the DaemonActive arm never resolves a daemon lookup any more"
        );
        assert!(
            focused_of(&layout.tabs, 2, 20),
            "raw is_focused must survive for a foreign space"
        );
        assert!(
            !focused_of(&layout.tabs, 2, 21),
            "the caller's tracked pane must NOT be marked focused in a foreign space"
        );
    }

    // ─── M2: unknown space id + error hygiene ────────────────────────────────

    #[tokio::test]
    async fn unknown_space_id_is_not_found_with_no_internal_chain() {
        // An id naming no live space must be distinguishable from a real (empty)
        // space — and the status must carry none of the backend's chain, which
        // includes the herdr socket path.
        let (service, backend) = spaces_service();
        let _rx = attach_conn_1(&service, Some(OTHER_SPACE));

        let status = space_layout_as_conn_1(&service, "ws-nope")
            .await
            .expect_err("an unknown space_id must be rejected");

        assert_eq!(status.code(), tonic::Code::NotFound);
        let msg = status.message();
        assert!(msg.contains("no such space"), "unexpected message: {msg:?}");
        assert!(
            !msg.contains(STUB_INTERNAL_DETAIL),
            "the herdr socket path must never reach the client: {msg:?}"
        );
        assert!(
            !msg.contains("workspace.list"),
            "no internal chain may reach the client: {msg:?}"
        );
        assert_eq!(
            backend.space_queries.load(Ordering::Relaxed),
            1,
            "the id must have been resolved against the backend, not guessed at"
        );
    }

    #[test]
    fn space_query_status_is_not_found_for_unknown_and_terse_for_everything_else() {
        // Both errors carry a context layer with an internal-looking detail: the
        // unknown one must still be recognised THROUGH it, and neither may
        // forward it.
        let unknown = anyhow::Error::new(UnknownSpace::new("ws-9"))
            .context(format!("herdr: workspace.list via {STUB_INTERNAL_DETAIL}"));
        let status = space_query_status("ws-9", &unknown);
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert!(
            !status.message().contains(STUB_INTERNAL_DETAIL),
            "unexpected message: {:?}",
            status.message()
        );

        let other = anyhow::anyhow!("connection refused")
            .context(format!("herdr: connect {STUB_INTERNAL_DETAIL}"));
        let status = space_query_status("ws-9", &other);
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(
            status.message(),
            "GetLayout: backend error",
            "the error chain must never be forwarded on the space path"
        );
    }

    #[tokio::test]
    async fn malformed_space_id_is_invalid_argument() {
        // Same guard as the mutating space ops (`validate_space_id`): a
        // path-traversal-shaped id never reaches the backend.
        let (service, backend) = spaces_service();
        let status = service
            .get_layout_impl(Request::new(SessionRef {
                session: "herdr:herdr".to_owned(),
                connection_id: String::new(),
                space_id: "../escape".to_owned(),
            }))
            .await
            .expect_err("a malformed space_id must be rejected");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(
            backend.space_queries.load(Ordering::Relaxed),
            0,
            "a malformed id must not reach the backend"
        );
    }
}
