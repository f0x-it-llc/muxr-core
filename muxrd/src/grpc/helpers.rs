//! Cross-cutting free helpers used across grpc submodules.
//!
//! **Routing helpers** (`make_id`, `resolve_session`, `resolve_session_kind`,
//! `validate_session`) have moved to [`crate::multiplexer::routing`] and are
//! re-exported from `crate::multiplexer`. They live there to keep `relay`'s
//! dependency on `multiplexer` clean — the layer graph is
//! `grpc → relay → multiplexer`; `relay` must not call into `grpc` (S-M1 fix).

use tonic::{Request, Response, Status};

use crate::actions::ActionAck;
use crate::cli::BackendKind;
use crate::multiplexer::PaneRef;
use crate::proto::{ActionAck as ProtoAck, PaneTarget};

// ─── Control routing ──────────────────────────────────────────────────────────

/// Try to route a control command through a live relay's AttachClient.
///
/// Routing priority:
/// 1. **Exact per-connection match.** If `connection_id` is non-empty AND an entry
///    with that key exists AND its stored session matches `session` → route to that
///    specific relay. This step is not gated by `read_only`: an exact
///    `connection_id` match can only ever reach the CALLER's OWN relay entry,
///    which is always isolation-safe regardless of the caller's read-only status.
/// 2. **Collapsed-session sessions are fail-closed** (S-M2/S-M4). When `session`
///    names a collapsed backend (herdr — see
///    [`crate::multiplexer::is_collapsed_backend_session`]) EVERY co-attached relay
///    shares the same session id, so `entry.session == session` cannot distinguish
///    connections — connection_id is the SOLE discriminator. A session-scoped
///    fallback would let an authed client steer a *victim* connection's stream
///    by sending an empty/guessed connection_id. So for a collapsed session with no
///    exact match we return `Some(ok:false, "reattach required …")` — we never
///    steer to an arbitrary relay and never fall through to the daemon-global
///    ephemeral path. This step is also unconditional on `read_only`: it was
///    already exact-match-only before read-only callers existed here.
/// 3. **Session-scoped fallback (non-collapsed only, read-write callers only).**
///    For zellij (distinct session names per session, so `entry.session ==
///    session` IS a real discriminator) the legacy read-write behavior is
///    preserved: scan for any **writable** relay attached to `session`
///    (preserves solo-client and legacy-client flows that don't send a
///    connection_id). Read-only entries are skipped: sending to one would
///    succeed at the channel level but the inbound task would silently drop the
///    command → false `ok:true` and client UI desync (Issue B).
///
///    A **read-only caller never takes this fallback.** Some navigation RPCs
///    (`FocusPane`, `GoToTab`) are permitted for read-only tokens and skip
///    `reject_if_read_only` entirely, so — unlike step 1 — this step CAN be
///    reached by a read-only caller. Falling back to *any* writable relay on the
///    session would let a read-only viewer move a co-attached WRITABLE client's
///    own view — exactly the isolation violation relay routing exists to
///    prevent. So a read-only caller with no exact match gets the same explicit
///    fail-closed ack as the collapsed-session arm (`ok:false, "reattach
///    required …"`) — never the writable-relay fallback, and never a bare
///    `None` that would let the caller fall through to the daemon-global
///    ephemeral `run_action` path.
///
/// All commands routed through this function are mutating *at the relay level*
/// (`SwitchTab`, `FocusPane`, `ToggleFullscreen`). `FocusPane` and `GoToTab` are
/// accepted for read-only token holders at the RPC gate; the inbound task still
/// drops a mutating command for a read-only *relay* regardless.
///
/// `read_only` is the CALLER's own read-only status — the same source of truth
/// `reject_if_read_only` reads (the `SessionReadOnly` request extension),
/// fail-closed (treat-as-read-only) when the extension is absent. It only
/// affects step 3; steps 1 and 2 are unconditional on it.
///
/// Returns `Some(ok-ack)` if a relay was found and the command was queued;
/// `Some(ok:false-ack)` for a collapsed session with no exact match, or for a
/// read-only caller with no exact match on a non-collapsed session (both
/// fail-closed); `None` → caller falls back to the ephemeral CLI path
/// (non-collapsed, read-write caller, no relay found).
///
/// Never errors (returns a `Status`) on a stale / unknown / mismatched
/// `connection_id`.
pub(super) fn try_route_control(
    control: &crate::relay::ControlRegistry,
    session: &str,
    connection_id: &str,
    read_only: bool,
    cmd: crate::relay::RelayControl,
) -> Option<Response<ProtoAck>> {
    // ── 1. Exact per-connection match (clone the sender out so the DashMap Ref
    //       shard read-lock is released before we send). This path is NOT gated
    //       by `read_only`: an exact `connection_id` match can only ever reach
    //       the CALLER's OWN relay entry, which is always intentional and
    //       isolation-safe whether the caller (or that relay) is read-only or
    //       not. `FocusPane`/`GoToTab` skip `reject_if_read_only` entirely (they
    //       are permitted for read-only tokens), so that gate cannot be assumed
    //       here — ownership of the matched connection is the safety property.
    let exact = if !connection_id.is_empty() {
        control
            .get(connection_id)
            .filter(|entry| entry.session == session)
            .map(|entry| entry.sender.clone())
    } else {
        None
    };

    // ── 2. Collapsed (herdr) session → FAIL CLOSED on no exact match. ────────
    // No session-scoped fallback: it would re-point a co-attached connection's
    // stream (the S-M2/S-M4 isolation violation). Return an explicit ok:false
    // ack rather than None so the caller does NOT fall through to the
    // daemon-global ephemeral path either. Unconditional on `read_only`.
    if crate::multiplexer::is_collapsed_backend_session(session) {
        return match exact {
            Some(tx) if tx.send(cmd).is_ok() => Some(Response::new(ProtoAck {
                ok: true,
                error: String::new(),
                info: "routed via relay client (per-connection)".to_owned(),
            })),
            _ => Some(Response::new(ProtoAck {
                ok: false,
                error: "reattach required (no matching connection)".to_owned(),
                info: String::new(),
            })),
        };
    }

    // ── 3. Non-collapsed (zellij): exact match, then — for a read-write caller
    //       only — a writable session fallback. ──────────────────────────────
    let (tx, info): (
        tokio::sync::mpsc::UnboundedSender<crate::relay::RelayControl>,
        &str,
    ) = match exact {
        Some(sender) => (sender, "routed via relay client (per-connection)"),
        None if read_only => {
            // A read-only caller with no exact match must NEVER take the
            // writable session fallback below (it would move a co-attached
            // WRITABLE relay's own view — an isolation violation) and must
            // NEVER get a bare `None` either (that would let the caller fall
            // through to the daemon-global ephemeral `run_action` path, which
            // is session-scoped, not view-scoped). Mirror the collapsed-session
            // arm's explicit fail-closed ack instead.
            return Some(Response::new(ProtoAck {
                ok: false,
                error: "reattach required (no matching connection)".to_owned(),
                info: String::new(),
            }));
        }
        None => {
            // Read-write caller, connection_id absent/stale/mismatched —
            // session fallback. Only writable relays: a read-only relay's
            // inbound task would silently drop mutating commands (Issue B).
            let maybe_fallback = control
                .iter()
                .find(|entry| entry.session == session && !entry.read_only)
                .map(|entry| entry.sender.clone());
            (
                maybe_fallback?,
                "routed via relay client (session fallback, writable)",
            )
        }
    };

    if tx.send(cmd).is_ok() {
        Some(Response::new(ProtoAck {
            ok: true,
            error: String::new(),
            info: info.to_owned(),
        }))
    } else {
        // Sender dead (relay tearing down); caller falls back to ephemeral.
        None
    }
}

// ─── Read-only gate + ack helpers ──────────────────────────────────────────────

/// Read the caller's read-only status as a plain `bool`, without rejecting the
/// request.
///
/// Used by navigation RPCs (`FocusPane`, `GoToTab`) that are themselves
/// permitted for read-only tokens (so they never call [`reject_if_read_only`])
/// but must still pass the caller's read-only status into
/// [`try_route_control`] so its routing stays exact-connection-only for a
/// read-only caller.
///
/// Reads the SAME `SessionReadOnly` extension as [`reject_if_read_only`] and
/// keeps its fail-closed default: an ABSENT extension is treated as
/// read-only, never as writable.
pub(super) fn session_is_read_only<T>(request: &Request<T>) -> bool {
    match request.extensions().get::<crate::auth::SessionReadOnly>() {
        Some(ro) => ro.0,
        None => true, // fail-closed: absent extension → treat as read-only
    }
}

/// Reject a request if its session token is read-only.
///
/// The `SessionReadOnly` extension is stashed by [`crate::auth::BearerAuthLayer`]
/// on every authenticated request.  Call this at the top of every **mutating**
/// RPC; focus/scroll/reads skip it.  Returns `Status::permission_denied` for
/// read-only tokens.
///
/// **Fail-closed**: if the `SessionReadOnly` extension is MISSING the request is
/// denied.  The auth layer always sets this extension on non-public paths, so a
/// missing extension on a mutating RPC is a bug — we must never silently allow it.
pub(super) fn reject_if_read_only<T>(request: &Request<T>, rpc: &str) -> Result<(), Status> {
    match request.extensions().get::<crate::auth::SessionReadOnly>() {
        Some(ro) if ro.0 => {
            log::info!("{rpc}: rejected — session token is read-only");
            Err(Status::permission_denied(
                "session token is read-only — mutating operations are not allowed",
            ))
        }
        Some(_) => Ok(()), // extension present and not read-only
        None => {
            // The auth layer always sets SessionReadOnly on non-public paths.
            // A missing extension on a mutating RPC is a bug — deny it (fail-closed).
            log::warn!(
                "{rpc}: rejected — SessionReadOnly extension absent (auth layer bug); \
                 denying to avoid fail-open"
            );
            Err(Status::permission_denied(
                "internal auth error: read-only flag not set — request denied (fail-closed)",
            ))
        }
    }
}

/// Validate a `CreateSession --layout` value (review Major I — arbitrary layout
/// file load → host code execution).
///
/// We only allow a bare layout *name* drawn from `[A-Za-z0-9_-]` (same allowlist
/// as session names): no absolute path, no `/`, no `..`, no metacharacters.
/// zellij resolves a bare name against its own layout directory.  Anything else
/// → `Status::invalid_argument`.
pub fn validate_layout_name(layout: &str) -> Result<(), Status> {
    if layout.is_empty()
        || !layout
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Status::invalid_argument(format!(
            "invalid layout {layout:?}: only a bare layout name [A-Za-z0-9_-] is allowed \
             (absolute paths, '/', and '..' are rejected)"
        )));
    }
    Ok(())
}

/// Maximum byte length for a client-supplied tab/pane display name.
pub(super) const MAX_DISPLAY_NAME_LEN: usize = 128;

/// Validate a client-supplied tab/pane display **name** before it is forwarded
/// to the backend over IPC.
///
/// Tab/pane names are display strings, but a co-attached *desktop* zellij client
/// renders them into its own tab-bar, so an unfiltered name could smuggle ANSI
/// escape sequences into that terminal (cross-client terminal injection). We also
/// bound the length so a client cannot push a multi-megabyte name through the
/// session IPC channel.
///
/// Rejects the empty string, anything over [`MAX_DISPLAY_NAME_LEN`] bytes, and
/// any Unicode control character (ESC / NUL / newline / …). Printable Unicode is
/// allowed — this is an abuse/injection bound, not a strict allowlist.
pub(super) fn validate_display_name(name: &str, kind: &str) -> Result<(), Status> {
    if name.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{kind} name must not be empty"
        )));
    }
    if name.len() > MAX_DISPLAY_NAME_LEN {
        return Err(Status::invalid_argument(format!(
            "{kind} name too long (max {MAX_DISPLAY_NAME_LEN} bytes)"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(Status::invalid_argument(format!(
            "invalid {kind} name: control characters (ANSI escapes, newlines, NUL) are not allowed"
        )));
    }
    Ok(())
}

/// Run a blocking action helper on the blocking pool and map its result into a
/// proto [`ProtoAck`].  A `LogError` ack (`ok == false`) is surfaced as an `Ok`
/// response with `ok=false` + `error` populated (it's a logical failure, not a
/// transport error); a hard IPC failure becomes `Status::internal`.
pub(super) async fn run_action<F>(rpc: &'static str, f: F) -> Result<Response<ProtoAck>, Status>
where
    F: FnOnce() -> anyhow::Result<ActionAck> + Send + 'static,
{
    let ack = tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Status::internal(format!("{rpc}: action task panicked: {e}")))?
        .map_err(|e| {
            log::warn!("{rpc}: action failed: {e:#}");
            Status::internal(format!("{rpc}: {e:#}"))
        })?;

    log::info!(
        "{rpc}: ok={} error={:?} info={:?}",
        ack.ok,
        ack.error,
        ack.info
    );
    Ok(Response::new(ProtoAck {
        ok: ack.ok,
        error: ack.error.unwrap_or_default(),
        info: ack.info.unwrap_or_default(),
    }))
}

/// Build the neutral [`PaneRef`] from a proto [`PaneTarget`].
///
/// The [`PaneRef`] carries the same `(id, is_plugin)` pair and is passed straight
/// through to the backend handlers and the [`crate::relay::RelayControl`] variants
/// (both neutral as of P1.03 — no per-call-site pane-id conversion).
///
/// Option C: the `session` field is now an opaque routing id and is **not**
/// inspected here — callers strip + validate it via
/// [`MuxrService::resolve_session`](super::MuxrService::resolve_session).
pub(super) fn pane_ref(target: &PaneTarget) -> PaneRef {
    PaneRef {
        id: target.pane_id,
        is_plugin: target.is_plugin,
    }
}

/// Return the first 8 bytes of a connection-id as a log-safe breadcrumb.
///
/// **FS3 invariant:** `connection_id` is the sole per-connection isolation
/// discriminator (a CSPRNG 128-bit hex string in the relay, but CLIENT-supplied
/// in every subsequent handler RPC). The full value MUST NOT appear in info/warn
/// logs — use this helper for all gRPC handler breadcrumbs.
///
/// The slice length is clamped with `.min(id.len())` so that empty or short
/// client-supplied ids (which may be 0–7 chars) never panic.
///
/// Caller combines the return value with a trailing `…` suffix in the log
/// format string to visually indicate truncation, e.g.
/// `"connection_id={}…", short_conn(&connection_id)`.
pub(super) fn short_conn(id: &str) -> &str {
    &id[..8.min(id.len())]
}

// ─── Proto ↔ BackendKind conversions ─────────────────────────────────────────

/// Map a [`BackendKind`] to its proto [`crate::proto::Backend`] tag.
///
/// Used by the session-enumerating RPCs to tag each [`crate::proto::SessionInfo`]
/// and to populate `VersionInfo.available_backends`.
pub(crate) fn proto_backend(kind: BackendKind) -> crate::proto::Backend {
    match kind {
        BackendKind::Zellij => crate::proto::Backend::Zellij,
        BackendKind::Herdr => crate::proto::Backend::Herdr,
    }
}

/// Map a proto [`crate::proto::Backend`] tag to a [`BackendKind`].
///
/// Returns `None` for `BACKEND_UNSPECIFIED` (the caller decides whether an
/// unspecified backend is an error or defaults to the sole available one).
pub(crate) fn kind_from_proto(backend: crate::proto::Backend) -> Option<BackendKind> {
    match backend {
        crate::proto::Backend::Zellij => Some(BackendKind::Zellij),
        crate::proto::Backend::Herdr => Some(BackendKind::Herdr),
        crate::proto::Backend::Unspecified => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use crate::auth::SessionReadOnly;
    use crate::cli::BackendKind;
    use crate::relay::{ControlEntry, ControlRegistry, RelayControl};

    use super::*;

    // ─── try_route_control tests ─────────────────────────────────────────────

    /// Build a ControlRegistry with one writable entry and return the receiver
    /// so tests can inspect what was sent.
    fn make_registry(
        conn_id: &str,
        session: &str,
    ) -> (ControlRegistry, mpsc::UnboundedReceiver<RelayControl>) {
        make_registry_with_flags(conn_id, session, false)
    }

    /// Build a ControlRegistry with one entry of the given `read_only` flag.
    fn make_registry_with_flags(
        conn_id: &str,
        session: &str,
        read_only: bool,
    ) -> (ControlRegistry, mpsc::UnboundedReceiver<RelayControl>) {
        let registry: ControlRegistry = Arc::new(dashmap::DashMap::new());
        let (tx, rx) = mpsc::unbounded_channel::<RelayControl>();
        registry.insert(
            conn_id.to_owned(),
            ControlEntry {
                session: session.to_owned(),
                sender: tx,
                read_only,
            },
        );
        (registry, rx)
    }

    // ─── validate_display_name tests ─────────────────────────────────────────

    #[test]
    fn display_name_accepts_printable_and_rejects_abuse() {
        // Printable names (incl. spaces/unicode) are allowed.
        assert!(validate_display_name("editor", "tab").is_ok());
        assert!(validate_display_name("my pane 2 ✦", "pane").is_ok());
        // Empty is rejected.
        assert!(validate_display_name("", "tab").is_err());
        // ANSI escape / control chars are rejected (cross-client injection guard).
        assert!(validate_display_name("evil\x1b[2Jtab", "tab").is_err());
        assert!(validate_display_name("line\nbreak", "tab").is_err());
        assert!(validate_display_name("nul\0byte", "pane").is_err());
        // Over-length is rejected.
        let too_long = "a".repeat(MAX_DISPLAY_NAME_LEN + 1);
        assert!(validate_display_name(&too_long, "tab").is_err());
    }

    #[test]
    fn routes_by_connection_id_when_session_matches() {
        // A request carrying the exact connection_id that matches an entry for
        // the same session must be routed to that relay's sender.
        let (reg, mut rx) = make_registry("conn-1", "my-session");
        let result = try_route_control(
            &reg,
            "my-session",
            "conn-1",
            false,
            RelayControl::SwitchTab(42),
        );
        assert!(result.is_some(), "should route via connection_id");
        // Verify the command arrived on the relay's receiver.
        match rx.try_recv() {
            Ok(RelayControl::SwitchTab(42)) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_session_when_connection_id_is_empty() {
        // Empty connection_id → session-scoped fallback: route to any relay for
        // the session.
        let (reg, mut rx) = make_registry("conn-2", "session-A");
        let result = try_route_control(
            &reg,
            "session-A",
            "",    // empty — no connection_id from client
            false, // read-write caller
            RelayControl::SwitchTab(99),
        );
        assert!(result.is_some(), "session fallback should route");
        match rx.try_recv() {
            Ok(RelayControl::SwitchTab(99)) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_session_when_connection_id_is_stale() {
        // A stale/unknown connection_id must NOT error; it falls back to the
        // session-scoped relay.
        let (reg, mut rx) = make_registry("conn-3", "session-B");
        let result = try_route_control(
            &reg,
            "session-B",
            "stale-id-xyz", // not in the registry
            false,          // read-write caller
            RelayControl::SwitchTab(7),
        );
        assert!(
            result.is_some(),
            "stale id should fall back to session relay"
        );
        match rx.try_recv() {
            Ok(RelayControl::SwitchTab(7)) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn does_not_route_when_connection_id_session_mismatches() {
        // A connection_id that exists but is registered under a DIFFERENT
        // session must not route to it. If there's no other relay for the
        // requested session, None is returned.
        let (reg, mut rx) = make_registry("conn-4", "session-C");
        let result = try_route_control(
            &reg,
            "OTHER-SESSION", // mismatch — conn-4 belongs to session-C
            "conn-4",
            false, // read-write caller
            RelayControl::SwitchTab(1),
        );
        assert!(result.is_none(), "session mismatch should not route");
        // The relay for session-C must NOT have received anything.
        assert!(
            rx.try_recv().is_err(),
            "mismatched session must not deliver to relay"
        );
    }

    #[test]
    fn returns_none_when_no_relay_for_session() {
        // No relay registered for the requested session at all → None.
        let registry: ControlRegistry = Arc::new(dashmap::DashMap::new());
        let result = try_route_control(
            &registry,
            "nonexistent-session",
            "",
            false, // read-write caller
            RelayControl::SwitchTab(1),
        );
        assert!(
            result.is_none(),
            "no relay → None (caller uses ephemeral path)"
        );
    }

    #[test]
    fn per_connection_routing_targets_exact_relay_among_two() {
        // Two relays on the same session. A request with conn-A's id must route
        // to relay A, NOT relay B.
        let reg: ControlRegistry = Arc::new(dashmap::DashMap::new());

        let (tx_a, mut rx_a) = mpsc::unbounded_channel::<RelayControl>();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel::<RelayControl>();

        reg.insert(
            "conn-A".to_owned(),
            ControlEntry {
                session: "shared-session".to_owned(),
                sender: tx_a,
                read_only: false,
            },
        );
        reg.insert(
            "conn-B".to_owned(),
            ControlEntry {
                session: "shared-session".to_owned(),
                sender: tx_b,
                read_only: false,
            },
        );

        let result = try_route_control(
            &reg,
            "shared-session",
            "conn-A",
            false, // read-write caller
            RelayControl::SwitchTab(11),
        );
        assert!(result.is_some(), "should route via conn-A");

        // Relay A got the command.
        match rx_a.try_recv() {
            Ok(RelayControl::SwitchTab(11)) => {}
            other => panic!("relay A: unexpected: {other:?}"),
        }
        // Relay B must NOT have received anything.
        assert!(
            rx_b.try_recv().is_err(),
            "relay B must not receive cmd for conn-A"
        );
    }

    // ─── S-M2/S-M4: collapsed (herdr) session fail-closed routing ────────────

    #[test]
    fn collapsed_session_routes_on_exact_connection_id() {
        // herdr session + exact connection_id match → routes (happy path that FA1
        // makes the client hit by forwarding a real connection_id).
        let (reg, mut rx) = make_registry("conn-abc123", "herdr:herdr");
        // read_only=false: collapsed-session exact match is read_only-independent
        // (see the fn doc), a read-write caller exercises the same code path.
        let result = try_route_control(
            &reg,
            "herdr:herdr",
            "conn-abc123",
            false,
            RelayControl::SwitchTab(4),
        );
        let resp = result.expect("collapsed exact match must return an ack");
        assert!(resp.get_ref().ok, "exact match must be ok:true");
        match rx.try_recv() {
            Ok(RelayControl::SwitchTab(4)) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn collapsed_session_fails_closed_on_empty_connection_id() {
        // herdr session + empty connection_id → ok:false, and the victim relay
        // must NOT receive the command (no cross-relay steer). This is the S-M4
        // attack: an authed RW client omits connection_id to hijack a co-attached
        // connection's stream.
        let (reg, mut rx) = make_registry("victim-conn", "herdr:herdr");
        let result = try_route_control(&reg, "herdr:herdr", "", false, RelayControl::SwitchTab(9));
        let resp = result.expect("collapsed session must return an explicit ack, not None");
        assert!(!resp.get_ref().ok, "empty connection_id must fail closed");
        assert!(
            resp.get_ref().error.contains("reattach required"),
            "error must explain reattach is required: {:?}",
            resp.get_ref().error
        );
        assert!(
            rx.try_recv().is_err(),
            "victim relay must NOT receive a command via fail-closed routing"
        );
    }

    #[test]
    fn collapsed_session_fails_closed_on_wrong_connection_id() {
        // herdr session + a guessed/stale connection_id that does not match the
        // victim's entry → ok:false; the victim relay gets nothing.
        let (reg, mut rx) = make_registry("victim-conn", "herdr:herdr");
        let result = try_route_control(
            &reg,
            "herdr:herdr",
            "guessed-2",
            false, // read-write caller
            RelayControl::FocusPane(crate::multiplexer::PaneRef {
                id: 1,
                is_plugin: false,
            }),
        );
        let resp = result.expect("collapsed session must return an explicit ack");
        assert!(!resp.get_ref().ok, "wrong connection_id must fail closed");
        assert!(
            rx.try_recv().is_err(),
            "victim relay must NOT be steered by a guessed connection_id"
        );
    }

    #[test]
    fn collapsed_session_no_fallback_across_two_connections() {
        // Two herdr connections on the shared session. A request with an empty
        // connection_id must NOT be steered onto EITHER relay (no session fallback).
        let reg: ControlRegistry = Arc::new(dashmap::DashMap::new());
        let (tx_a, mut rx_a) = mpsc::unbounded_channel::<RelayControl>();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel::<RelayControl>();
        reg.insert(
            "conn-a".to_owned(),
            ControlEntry {
                session: "herdr:herdr".to_owned(),
                sender: tx_a,
                read_only: false,
            },
        );
        reg.insert(
            "conn-b".to_owned(),
            ControlEntry {
                session: "herdr:herdr".to_owned(),
                sender: tx_b,
                read_only: false,
            },
        );
        let result = try_route_control(&reg, "herdr:herdr", "", false, RelayControl::SwitchTab(1));
        assert!(!result.expect("must return ack").get_ref().ok);
        assert!(rx_a.try_recv().is_err(), "relay A must not be steered");
        assert!(rx_b.try_recv().is_err(), "relay B must not be steered");
    }

    // ─── Issue B: read-only RELAY fallback filtering (read-write CALLER) ─────
    //
    // These three tests predate the caller-level `read_only` parameter and
    // originally encoded a premise that no longer holds in full: that the
    // session-scoped fallback only ever needed to filter out a read-only
    // *relay* entry. They are rewritten (not deleted) to call the new
    // signature explicitly with `read_only: false` — i.e. they now document
    // the read-write-CALLER path specifically, preserving that coverage
    // without regression. The read-only-CALLER isolation rule this card adds
    // is covered separately by the `read_only_caller_*` tests below.

    #[test]
    fn fallback_skips_read_only_and_routes_to_writable() {
        // Two relays on the same session: one read-only, one writable. A
        // read-write CALLER sending a mutating command with an empty/stale
        // connection_id must skip the read-only entry and route to the
        // writable one (Issue B fix) — unchanged by this card.
        let reg: ControlRegistry = Arc::new(dashmap::DashMap::new());

        let (tx_ro, mut rx_ro) = mpsc::unbounded_channel::<RelayControl>();
        let (tx_rw, mut rx_rw) = mpsc::unbounded_channel::<RelayControl>();

        // Insert the read-only entry first; DashMap shard iteration order is
        // non-deterministic, but the `!entry.read_only` filter selects the
        // writable entry regardless of visit order.
        reg.insert(
            "conn-ro".to_owned(),
            ControlEntry {
                session: "sess".to_owned(),
                sender: tx_ro,
                read_only: true,
            },
        );
        reg.insert(
            "conn-rw".to_owned(),
            ControlEntry {
                session: "sess".to_owned(),
                sender: tx_rw,
                read_only: false,
            },
        );

        // Empty connection_id, read-write caller → session fallback.
        let result = try_route_control(&reg, "sess", "", false, RelayControl::SwitchTab(5));
        assert!(
            result.is_some(),
            "should route to the writable relay, not be blocked by read-only entry"
        );

        // Writable relay got the command.
        match rx_rw.try_recv() {
            Ok(RelayControl::SwitchTab(5)) => {}
            other => panic!("writable relay: unexpected: {other:?}"),
        }
        // Read-only relay must NOT have received anything.
        assert!(
            rx_ro.try_recv().is_err(),
            "read-only relay must not receive a mutating command via session fallback"
        );
    }

    #[test]
    fn fallback_returns_none_when_only_read_only_relay_exists() {
        // Only a read-only relay registered for the session. A read-write
        // CALLER's mutating command with no connection_id must return None so
        // the caller falls through to the ephemeral path — never a false
        // ok:true (Issue B fix) — unchanged by this card. (A read-ONLY
        // caller in this exact situation is a different case, covered by
        // `read_only_caller_absent_connection_id_never_reaches_a_co_attached_writable_relay`
        // below: it must get an explicit ok:false ack, not `None`.)
        let (reg, mut rx_ro) = make_registry_with_flags("conn-ro-only", "sess-ro", true);

        let result = try_route_control(&reg, "sess-ro", "", false, RelayControl::SwitchTab(9));
        assert!(
            result.is_none(),
            "only read-only relay → None (must fall through to ephemeral)"
        );
        // Confirm nothing was sent to the read-only relay.
        assert!(
            rx_ro.try_recv().is_err(),
            "read-only relay must not receive any command"
        );
    }

    #[test]
    fn stale_id_fallback_skips_read_only_and_routes_to_writable() {
        // Stale connection_id + one read-only relay + one writable relay. The
        // stale-id fallback path must also skip the read-only entry for a
        // read-write CALLER — unchanged by this card.
        let reg: ControlRegistry = Arc::new(dashmap::DashMap::new());

        let (tx_ro, mut rx_ro) = mpsc::unbounded_channel::<RelayControl>();
        let (tx_rw, mut rx_rw) = mpsc::unbounded_channel::<RelayControl>();

        reg.insert(
            "conn-ro".to_owned(),
            ControlEntry {
                session: "sess2".to_owned(),
                sender: tx_ro,
                read_only: true,
            },
        );
        reg.insert(
            "conn-rw".to_owned(),
            ControlEntry {
                session: "sess2".to_owned(),
                sender: tx_rw,
                read_only: false,
            },
        );

        let result = try_route_control(
            &reg,
            "sess2",
            "stale-xyz",
            false,
            RelayControl::SwitchTab(3),
        );
        assert!(
            result.is_some(),
            "stale id fallback should route to writable relay"
        );
        match rx_rw.try_recv() {
            Ok(RelayControl::SwitchTab(3)) => {}
            other => panic!("writable relay: unexpected: {other:?}"),
        }
        assert!(
            rx_ro.try_recv().is_err(),
            "read-only relay must not receive the command"
        );
    }

    // ─── Read-only CALLER isolation (this card) ──────────────────────────────
    //
    // A read-only caller is permitted to reach `try_route_control` at all only
    // via a navigation RPC (`FocusPane`, `GoToTab`) that skips
    // `reject_if_read_only`. These tests prove the routing itself still holds
    // the isolation boundary: a read-only caller may only ever reach its OWN
    // relay by exact `connection_id`, never a co-attached relay via fallback.

    #[test]
    fn read_only_caller_exact_connection_id_routes_to_own_relay() {
        // A read-only caller's OWN connection_id still routes: exact match is
        // caller-identity routing, not a writable-fallback, so it is permitted
        // regardless of the caller's read-only status.
        let (reg, mut rx) = make_registry("conn-ro-1", "sess-exact");
        let result = try_route_control(
            &reg,
            "sess-exact",
            "conn-ro-1",
            true, // read-only caller
            RelayControl::SwitchTab(21),
        );
        let resp = result.expect("exact match must route even for a read-only caller");
        assert!(resp.get_ref().ok, "exact match must be ok:true");
        match rx.try_recv() {
            Ok(RelayControl::SwitchTab(21)) => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn read_only_caller_absent_connection_id_never_reaches_a_co_attached_writable_relay() {
        // The test that matters most: a read-only caller omits connection_id
        // while a co-attached WRITABLE relay exists in the same (non-collapsed)
        // session. Falling back to that writable relay would let the
        // read-only caller move a DIFFERENT client's view — exactly the
        // isolation violation relay routing exists to prevent. Must return
        // ok:false, and the writable relay must receive nothing.
        let reg: ControlRegistry = Arc::new(dashmap::DashMap::new());
        let (tx_rw, mut rx_rw) = mpsc::unbounded_channel::<RelayControl>();
        reg.insert(
            "conn-other-writer".to_owned(),
            ControlEntry {
                session: "sess-iso".to_owned(),
                sender: tx_rw,
                read_only: false,
            },
        );

        let result = try_route_control(
            &reg,
            "sess-iso",
            "",   // absent connection_id
            true, // read-only caller
            RelayControl::SwitchTab(7),
        );
        let resp =
            result.expect("read-only caller with no match must get an explicit ack, not None");
        assert!(
            !resp.get_ref().ok,
            "read-only caller with no exact match must fail closed"
        );
        assert!(
            resp.get_ref().error.contains("reattach required"),
            "error must explain reattach is required: {:?}",
            resp.get_ref().error
        );
        assert!(
            rx_rw.try_recv().is_err(),
            "co-attached writable relay must NOT receive a command routed from a \
             read-only caller with no connection_id"
        );
    }

    #[test]
    fn read_only_caller_stale_connection_id_fails_closed_no_writable_fallback() {
        // Same isolation guarantee as above, but with a stale/guessed
        // connection_id rather than an empty one.
        let reg: ControlRegistry = Arc::new(dashmap::DashMap::new());
        let (tx_rw, mut rx_rw) = mpsc::unbounded_channel::<RelayControl>();
        reg.insert(
            "conn-other-writer".to_owned(),
            ControlEntry {
                session: "sess-iso2".to_owned(),
                sender: tx_rw,
                read_only: false,
            },
        );

        let result = try_route_control(
            &reg,
            "sess-iso2",
            "guessed-stale-id",
            true, // read-only caller
            RelayControl::SwitchTab(8),
        );
        let resp =
            result.expect("read-only caller with a stale id must get an explicit ack, not None");
        assert!(
            !resp.get_ref().ok,
            "stale connection_id must fail closed for a read-only caller"
        );
        assert!(
            rx_rw.try_recv().is_err(),
            "co-attached writable relay must NOT be steered by a stale connection_id"
        );
    }

    // ─── short_conn helper tests ─────────────────────────────────────────────

    #[test]
    fn short_conn_empty_does_not_panic() {
        // Client-supplied ids may be empty; must not panic (length guard).
        assert_eq!(super::short_conn(""), "");
    }

    #[test]
    fn short_conn_shorter_than_8_returns_whole_string() {
        // Short ids (3 chars) must return the full string, not panic.
        assert_eq!(super::short_conn("abc"), "abc");
    }

    #[test]
    fn short_conn_32_char_returns_8_chars() {
        // A normal CSPRNG hex id (32 chars) must be truncated to exactly 8.
        let id = "abcdef0123456789abcdef0123456789";
        assert_eq!(id.len(), 32);
        let prefix = super::short_conn(id);
        assert_eq!(prefix, "abcdef01");
        assert_eq!(prefix.len(), 8);
    }

    #[test]
    fn short_conn_exactly_8_returns_all_8() {
        assert_eq!(super::short_conn("12345678"), "12345678");
    }

    // ─── reject_if_read_only tests ───────────────────────────────────────────

    #[test]
    fn reject_if_read_only_denies_when_extension_absent() {
        // Fail-closed: a mutating RPC with no SessionReadOnly extension (auth-layer
        // bug) must be denied, never allowed.
        let req = tonic::Request::new(());
        assert!(reject_if_read_only(&req, "Test").is_err());
    }

    #[test]
    fn reject_if_read_only_denies_read_only_token() {
        let mut req = tonic::Request::new(());
        req.extensions_mut().insert(SessionReadOnly(true));
        assert!(reject_if_read_only(&req, "Test").is_err());
    }

    #[test]
    fn reject_if_read_only_allows_writable_token() {
        let mut req = tonic::Request::new(());
        req.extensions_mut().insert(SessionReadOnly(false));
        assert!(reject_if_read_only(&req, "Test").is_ok());
    }

    // ─── session_is_read_only tests ──────────────────────────────────────────

    #[test]
    fn session_is_read_only_treats_absent_extension_as_read_only() {
        // Same fail-closed default as `reject_if_read_only`: an ABSENT
        // extension must never be read as writable.
        let req = tonic::Request::new(());
        assert!(super::session_is_read_only(&req));
    }

    #[test]
    fn session_is_read_only_reflects_true() {
        let mut req = tonic::Request::new(());
        req.extensions_mut().insert(SessionReadOnly(true));
        assert!(super::session_is_read_only(&req));
    }

    #[test]
    fn session_is_read_only_reflects_false() {
        let mut req = tonic::Request::new(());
        req.extensions_mut().insert(SessionReadOnly(false));
        assert!(!super::session_is_read_only(&req));
    }

    // ─── Proto ↔ BackendKind conversion tests ────────────────────────────────

    #[test]
    fn proto_backend_maps_each_kind() {
        assert_eq!(
            proto_backend(BackendKind::Zellij),
            crate::proto::Backend::Zellij
        );
        assert_eq!(
            proto_backend(BackendKind::Herdr),
            crate::proto::Backend::Herdr
        );
    }

    #[test]
    fn kind_from_proto_inverts_proto_backend() {
        assert_eq!(
            kind_from_proto(crate::proto::Backend::Zellij),
            Some(BackendKind::Zellij)
        );
        assert_eq!(
            kind_from_proto(crate::proto::Backend::Herdr),
            Some(BackendKind::Herdr)
        );
        assert_eq!(kind_from_proto(crate::proto::Backend::Unspecified), None);
    }
}
