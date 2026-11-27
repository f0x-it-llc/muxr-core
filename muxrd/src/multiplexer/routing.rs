//! Backend-routing helpers: id minting, session resolution, and name validation.
//!
//! These live in the `multiplexer` layer (not `grpc`) because they reference only
//! `BackendSet`, `BackendKind`, and `ipc::validate_session_name` — all
//! multiplexer/ipc concerns. Both the `grpc` layer and `relay` depend on
//! `multiplexer` (the correct direction); moving these here removes the
//! layer-inversion that existed when they lived in `grpc/helpers.rs` (relay called
//! into grpc, creating a bidirectional coupling — S-M1 fix, Phase 3 R1).
//!
//! ## Error-code mapping convention
//!
//! Documented here as the single source of truth for all backend-resolution errors:
//!
//! - An **unknown/unparseable** backend kind (e.g. `screen:dev`, or a bare name on a
//!   multi-backend server) → `InvalidArgument`.  The client sent a malformed id it
//!   should not have produced.
//! - A **recognised but not-running** backend (known kind such as `herdr:`, but not
//!   present in this server's `BackendSet`) → `NotFound`.  The server knows the
//!   backend kind but is not currently driving it; the client should re-list.
//!
//! `CreateSession` in `grpc/session_ops.rs` follows the same mapping.

use std::sync::Arc;

use tonic::Status;

use crate::cli::BackendKind;

use super::{BackendSet, MuxBackend};

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Parse a `<backend>:` prefix into a [`BackendKind`].
///
/// The string scheme is **lowercase** and matches both the [`BackendKind`]
/// `Display` impl and [`make_id`] (the minting side). Keep all three in sync:
/// `zellij` / `herdr` / `tmux`.
fn parse_backend_kind(s: &str) -> Option<BackendKind> {
    match s {
        "zellij" => Some(BackendKind::Zellij),
        "herdr" => Some(BackendKind::Herdr),
        "tmux" => Some(BackendKind::Tmux),
        _ => None,
    }
}

// ─── Public surface ───────────────────────────────────────────────────────────

/// Validate a session name at an RPC boundary, mapping a rejection to
/// `Status::invalid_argument` (review Major G — path traversal).
///
/// Call this at the top of every handler/resolver that accepts a session name
/// before that name is used to build a socket path or reaches the `zellij`
/// binary.  Delegates to [`crate::ipc::validate_session_name`].
pub(crate) fn validate_session(session: &str) -> Result<(), Status> {
    crate::ipc::validate_session_name(session).map_err(Status::invalid_argument)
}

/// True when `id` names a backend whose sessions **collapse to a single shared
/// session id** — currently only herdr (Option A collapses every herdr workspace
/// into the singular `herdr:herdr` muxr session).
///
/// For such ids `entry.session == session` is constant-true across *every*
/// co-attached relay, so the `connection_id` is the SOLE per-connection
/// discriminator. Any **mutating** per-connection control routing on a collapsed
/// session MUST therefore require an exact `connection_id` match and drop the
/// session-scoped fallback — otherwise an authed RW client could steer a victim
/// connection's stream by sending an empty/guessed `connection_id`
/// (Round-1 S-M2/S-M4). zellij sessions have distinct names and are unaffected.
pub(crate) fn is_collapsed_backend_session(id: &str) -> bool {
    matches!(id.split_once(':'), Some(("herdr", _)))
}

/// Mint an opaque, backend-qualified session id: `"<backend>:<bare>"`.
///
/// The inverse of [`resolve_session`]. The `<backend>` token is the lowercase
/// [`BackendKind`] `Display` form, so a round-trip
/// (`resolve_session(make_id(k, n))`) yields `(backend_for(k), n)`. Used by the
/// session-enumerating RPCs so every `id` they emit resolves cleanly here.
///
/// Kept beside [`resolve_session`] as the single source of truth for the wire
/// format. Consumed by the session-enumerating RPCs (`ListSessions` /
/// `CreateSession`) and the relay's backend-qualified client-count key.
pub(crate) fn make_id(kind: BackendKind, bare: &str) -> String {
    format!("{kind}:{bare}")
}

/// Resolve an opaque session `id` to the backend that owns it and the bare
/// session name to hand that backend (Option C routing).
///
/// Wire format: `id == "<backend>:<bare>"` (e.g. `"zellij:dev"`). The split is on
/// the **first** `':'`; the prefix maps to a [`BackendKind`] via
/// [`parse_backend_kind`], which selects the concrete backend from `backends`.
///
/// Errors (see module-level convention for the full mapping):
/// - unknown `<backend>` token → `InvalidArgument`,
/// - a recognised backend that this server is not running → `NotFound`
///   (the client should re-list sessions),
/// - a bare name failing the session-name guard → `InvalidArgument`.
///
/// **Back-compat:** an `id` with no `':'` is treated as a legacy *bare* name
/// (an older single-backend client, or a hand-typed name). It resolves to the
/// sole backend when exactly one is running; on a multi-backend server it is
/// rejected (`InvalidArgument`) asking the client to re-list, since there is no
/// way to know which backend owns an unqualified name.
///
/// **Security:** only the **bare** name is run through the [`validate_session`]
/// guard (path-traversal / allowlist) — never the full id — preserving the
/// invariant that every name reaching a backend / socket path is validated.
pub(crate) fn resolve_session(
    backends: &BackendSet,
    id: &str,
) -> Result<(Arc<dyn MuxBackend>, String), Status> {
    let (_, backend, bare) = resolve_session_kind(backends, id)?;
    Ok((backend, bare))
}

/// Like [`resolve_session`] but also returns the owning [`BackendKind`].
///
/// The relay needs the kind to build the backend-qualified client-count key
/// ([`make_id`]) so its `attach`/`detach` count bucket matches the one
/// `ListSessions` reads — even when the client supplied a legacy bare name on a
/// single-backend server (the kind is then the sole backend's). Keeping this as
/// the kind-aware core lets [`resolve_session`] stay a thin shim for the many
/// id-only call sites.
pub(crate) fn resolve_session_kind(
    backends: &BackendSet,
    id: &str,
) -> Result<(BackendKind, Arc<dyn MuxBackend>, String), Status> {
    let (kind, backend, bare) = match id.split_once(':') {
        Some((kind_str, bare)) => {
            // Any ':' means the id is backend-qualified (bare names are
            // [A-Za-z0-9_-] and never contain ':'). An unrecognised prefix is a
            // client error → InvalidArgument (see module-level error convention).
            let kind = parse_backend_kind(kind_str).ok_or_else(|| {
                Status::invalid_argument(format!(
                    "session id {id:?} names an unknown backend {kind_str:?} \
                     (expected one of: zellij, herdr, tmux)"
                ))
            })?;
            // Recognised kind, but not running on this server → NotFound.
            let backend = backends.get(kind).ok_or_else(|| {
                Status::not_found(format!(
                    "session id {id:?} targets the '{kind}' backend, which is not \
                     running on this server — re-list sessions"
                ))
            })?;
            (kind, backend.clone(), bare.to_owned())
        }
        None => {
            // No prefix → legacy bare name. Only resolvable on a single-backend
            // server; otherwise the owner is ambiguous.
            if backends.len() == 1 {
                let kind = backends
                    .kinds()
                    .next()
                    .expect("BackendSet invariant: at least one backend");
                (kind, backends.primary().clone(), id.to_owned())
            } else {
                return Err(Status::invalid_argument(format!(
                    "session {id:?} is missing a '<backend>:' prefix and this server \
                     runs multiple backends — re-list sessions to obtain a \
                     backend-qualified id"
                )));
            }
        }
    };
    // Validate the BARE name only (never the full id) — preserves the
    // path-traversal / allowlist invariant at the backend boundary.
    validate_session(&bare)?;
    Ok((kind, backend, bare))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::cli::BackendKind;
    use crate::multiplexer::{BackendSet, MuxBackend, ZellijBackend};

    use super::*;

    /// A single-zellij set (the production default at `MuxrService::new`).
    fn zellij_only_set() -> BackendSet {
        BackendSet::single(BackendKind::Zellij, Arc::new(ZellijBackend))
    }

    /// A two-backend set. `ZellijBackend` stands in for both kinds: `resolve_session`
    /// only uses the kind for registry lookup and never invokes a backend method,
    /// so this exercises multi-backend routing/ambiguity without a live herdr.
    fn two_backend_set() -> BackendSet {
        BackendSet::new(vec![
            (
                BackendKind::Zellij,
                Arc::new(ZellijBackend) as Arc<dyn MuxBackend>,
            ),
            (
                BackendKind::Herdr,
                Arc::new(ZellijBackend) as Arc<dyn MuxBackend>,
            ),
        ])
    }

    #[test]
    fn is_collapsed_backend_session_is_true_only_for_herdr() {
        // herdr collapses every workspace onto a single session id → collapsed.
        assert!(is_collapsed_backend_session("herdr:herdr"));
        assert!(is_collapsed_backend_session("herdr:anything"));
        // zellij sessions have distinct names → NOT collapsed (keep session fallback).
        assert!(!is_collapsed_backend_session("zellij:dev"));
        assert!(!is_collapsed_backend_session("zellij:herdr"));
        // legacy bare names / unknown prefixes are not the herdr sentinel.
        assert!(!is_collapsed_backend_session("herdr"));
        assert!(!is_collapsed_backend_session("dev"));
        assert!(!is_collapsed_backend_session(""));
    }

    #[test]
    fn validate_session_accepts_a_plain_name() {
        assert!(validate_session("backend-dev_1").is_ok());
    }

    #[test]
    fn validate_session_rejects_path_traversal() {
        assert!(validate_session("../etc").is_err());
        assert!(validate_session("a/b").is_err());
    }

    // ─── Option C: resolve_session / make_id (T04) ───────────────────────────

    #[test]
    fn resolve_session_strips_prefix_to_bare_name() {
        let (_, bare) =
            resolve_session(&zellij_only_set(), "zellij:dev").expect("zellij:dev should resolve");
        assert_eq!(bare, "dev");
    }

    #[test]
    fn resolve_session_rejects_path_traversal_in_bare_name() {
        // SECURITY: the prefix is valid but the BARE name escapes — it must be
        // rejected by the ipc guard, which runs on the stripped bare name
        // ("../etc"), never on the full id.
        let err = resolve_session(&zellij_only_set(), "zellij:../etc")
            .expect_err("zellij:../etc must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn resolve_session_rejects_path_traversal_with_slash() {
        let err = resolve_session(&zellij_only_set(), "zellij:a/b")
            .expect_err("zellij:a/b must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn resolve_session_rejects_unknown_backend_prefix() {
        let err = resolve_session(&zellij_only_set(), "screen:dev")
            .expect_err("unknown backend prefix must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn tmux_prefix_parses_and_make_id_round_trips() {
        assert_eq!(parse_backend_kind("tmux"), Some(BackendKind::Tmux));
        assert_eq!(make_id(BackendKind::Tmux, "dev"), "tmux:dev");
        // A tmux id on a server without tmux is a recognised-but-absent
        // backend (NotFound), not an unknown kind.
        let err = resolve_session(&zellij_only_set(), "tmux:dev")
            .expect_err("tmux is not running on a zellij-only server");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn resolve_session_not_found_when_backend_absent() {
        // A herdr-qualified id on a zellij-only server → NotFound (client re-lists).
        let err = resolve_session(&zellij_only_set(), "herdr:dev")
            .expect_err("absent backend → NotFound");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn resolve_session_bare_name_falls_back_on_single_backend() {
        // Back-compat: a legacy bare name resolves to the sole backend.
        let (_, bare) = resolve_session(&zellij_only_set(), "dev")
            .expect("bare name should resolve on a single-backend server");
        assert_eq!(bare, "dev");
    }

    #[test]
    fn resolve_session_bare_name_rejected_on_multi_backend() {
        // Ambiguous: a bare name on a multi-backend server has no determinable owner.
        let err = resolve_session(&two_backend_set(), "dev")
            .expect_err("bare name is ambiguous on a multi-backend server");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn resolve_session_routes_each_prefix_to_its_backend() {
        let set = two_backend_set();
        let (_, bare_z) = resolve_session(&set, "zellij:a").expect("zellij prefix resolves");
        assert_eq!(bare_z, "a");
        let (_, bare_h) = resolve_session(&set, "herdr:b").expect("herdr prefix resolves");
        assert_eq!(bare_h, "b");
    }

    #[test]
    fn make_id_round_trips_through_resolve_session() {
        let set = two_backend_set();
        let id = make_id(BackendKind::Herdr, "proj");
        assert_eq!(
            id, "herdr:proj",
            "make_id uses the lowercase Display scheme"
        );
        let (_, bare) = resolve_session(&set, &id).expect("a minted id must resolve");
        assert_eq!(bare, "proj");
    }

    #[test]
    fn list_sessions_minted_id_round_trips_with_kind() {
        // T05: an id minted exactly as `list_sessions_impl` mints it
        // (`make_id(kind, bare)`) must resolve back to the SAME backend kind +
        // bare name via the kind-aware resolver. This is the round-trip the
        // ListSessions → AttachTerminal handoff depends on.
        let set = two_backend_set();
        for kind in [BackendKind::Zellij, BackendKind::Herdr] {
            let id = make_id(kind, "dev");
            let (resolved_kind, _, bare) =
                resolve_session_kind(&set, &id).expect("minted id must resolve");
            assert_eq!(resolved_kind, kind, "round-trip must preserve the backend");
            assert_eq!(bare, "dev");
        }
    }

    #[test]
    fn client_count_is_isolated_per_backend_qualified_id() {
        // Carried T04 fix: two same-name sessions on different backends must NOT
        // share one connected-client bucket. The relay keys `attach` by
        // `make_id(kind, bare)` and `ListSessions` reads `count(make_id(...))`,
        // so `zellij:dev` and `herdr:dev` are independent counters.
        let clients = crate::client_count::SessionClients::new();
        let zellij_dev = make_id(BackendKind::Zellij, "dev");
        let herdr_dev = make_id(BackendKind::Herdr, "dev");
        assert_ne!(zellij_dev, herdr_dev);

        let _g_z = clients.attach(&zellij_dev);
        assert_eq!(clients.count(&zellij_dev), 1);
        assert_eq!(
            clients.count(&herdr_dev),
            0,
            "the herdr:dev bucket must be unaffected by a zellij:dev attach"
        );

        let _g_h1 = clients.attach(&herdr_dev);
        let _g_h2 = clients.attach(&herdr_dev);
        assert_eq!(clients.count(&herdr_dev), 2);
        assert_eq!(
            clients.count(&zellij_dev),
            1,
            "the zellij:dev bucket must be unaffected by herdr:dev attaches"
        );
    }
}
