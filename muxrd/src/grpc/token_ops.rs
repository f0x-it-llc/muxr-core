//! Auth and token-management RPC implementations.

use tonic::{Request, Response, Status};
use zellij_utils::web_authentication_tokens::create_session_token;

use crate::proto::{
    ActionAck as ProtoAck, CreateTokenReq, Empty, LoginRequest, LoginResponse, RevokeTokenReq,
    TokenInfo, TokenList, VersionInfo,
};

use super::MuxrService;
use super::SERVER_VERSION;
use super::helpers::{self, reject_if_read_only};

/// Failure modes of the blocking `Login` work, kept distinct so the RPC can
/// return a specific message for an expired pairing token versus a generic
/// "invalid auth token" for everything else (no internal detail leaked).
enum LoginErr {
    /// The auth token carried an opt-in muxr-side expiry that has passed.
    Expired,
    /// The token was rejected by the zellij token DB (or a DB error occurred).
    Invalid(anyhow::Error),
}

impl MuxrService {
    // ── GetVersion ──────────────────────────────────────────────────────────

    pub(super) async fn get_version_impl(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<VersionInfo>, Status> {
        // Phase 3: report a per-backend version for EVERY backend this server
        // drives, plus the set of available backends. No session is needed — the
        // versions come straight off each backend (`backend_version()`):
        //   - zellij → `zellij_utils::consts::VERSION` (the linked version),
        //   - herdr  → `herdr-wire-v<N>` (the wire-protocol compat marker, which
        //     is the meaningful version between muxrd and a running herdr).
        // `zellij_version` is kept as a back-compat scalar = the zellij backend's
        // version if present, else "" (older clients read only this field).
        let mut backend_versions: Vec<crate::proto::BackendVersion> = Vec::new();
        let mut available_backends: Vec<i32> = Vec::new();
        let mut zellij_version = String::new();

        for (kind, backend) in self.backends.iter() {
            let proto_kind = helpers::proto_backend(kind);
            let version = backend.backend_version();
            if kind == crate::cli::BackendKind::Zellij {
                zellij_version = version.clone();
            }
            available_backends.push(proto_kind as i32);
            backend_versions.push(crate::proto::BackendVersion {
                backend: proto_kind as i32,
                version,
            });
        }

        let mut capabilities = vec!["mouse-input".to_owned()];
        // Additive capability flag — advertised only when push can ACTUALLY
        // deliver: a relay URL is configured (`config::EffectiveConfig::
        // notify_relay_url`) AND the herdr backend is present. The event kernel
        // + outbound notifier spawn only in the herdr branch (`bin/muxrd.rs`),
        // so a zellij-only server that merely has a relay URL configured could
        // accept registrations it can never fulfil — gate on both so clients
        // never see "push-notifications" (nor a non-empty
        // `notification_relay_url`) unless a push would land.
        let herdr_present = self.backends.get(crate::cli::BackendKind::Herdr).is_some();
        let push_available = self.notify_relay_url.is_some() && herdr_present;
        if push_available {
            capabilities.push("push-notifications".to_owned());
        }

        let info = VersionInfo {
            server_version: SERVER_VERSION.to_owned(),
            zellij_version,
            backends: backend_versions,
            available_backends,
            capabilities,
            // Empty (proto3 default) unless push is actually available — clients
            // treat an empty field as "server does not support push".
            notification_relay_url: if push_available {
                self.notify_relay_url.clone().unwrap_or_default()
            } else {
                String::new()
            },
        };
        log::debug!(
            "GetVersion → server={} zellij={:?} backends={} available={:?} notify={}",
            info.server_version,
            info.zellij_version,
            info.backends.len(),
            info.available_backends,
            !info.notification_relay_url.is_empty(),
        );
        Ok(Response::new(info))
    }

    // ── Login ───────────────────────────────────────────────────────────────

    pub(super) async fn login_impl(
        &self,
        request: Request<LoginRequest>,
    ) -> Result<Response<LoginResponse>, Status> {
        let req = request.into_inner();
        log::info!("Login attempt (remember_me={})", req.remember_me);

        // `create_session_token` (and the read-only check below) are disk-backed
        // token-DB + hashing operations. `Login` is a PUBLIC, unauthenticated RPC,
        // so running them directly on the async runtime would let an unauthenticated
        // flood of Login requests stall runtime workers (and every live terminal
        // stream). Offload to a blocking pool, mirroring create_token_impl.
        let auth_token = req.auth_token;
        let remember_me = req.remember_me;
        let outcome = tokio::task::spawn_blocking(move || {
            // Opt-in muxr-side expiry (see token_expiry): a time-boxed pairing
            // token is refused once its deadline passes, before a session is
            // minted. Tokens with no recorded expiry are long-lived (unaffected).
            if crate::token_expiry::is_expired(&auth_token) {
                return Err(LoginErr::Expired);
            }
            let session_token = create_session_token(&auth_token, remember_me)
                .map_err(|e| LoginErr::Invalid(e.into()))?;
            // Surface the read-only scope so the client can disable mutating
            // controls up-front. Enforcement stays server-side (fail-closed in the
            // auth layer); this is advisory, but default to read-only on error so a
            // client never believes it has write access it lacks.
            let is_read_only =
                zellij_utils::web_authentication_tokens::is_session_token_read_only(&session_token)
                    .unwrap_or(true);
            Ok((session_token, is_read_only))
        })
        .await
        .map_err(|e| Status::internal(format!("Login task panicked: {e}")))?;

        let (session_token, is_read_only) = match outcome {
            Ok(v) => v,
            Err(LoginErr::Expired) => {
                log::info!("Login rejected: pairing token expired");
                return Err(Status::unauthenticated(
                    "pairing token expired — request a fresh pairing QR",
                ));
            }
            Err(LoginErr::Invalid(e)) => {
                // Log the detailed cause server-side, but return a generic message
                // so an unauthenticated caller learns nothing about internal state.
                log::info!("Login rejected: {e:#}");
                return Err(Status::unauthenticated("invalid auth token"));
            }
        };

        log::info!("Login succeeded — issued session token (read_only={is_read_only})");
        Ok(Response::new(LoginResponse {
            session_token,
            is_read_only,
        }))
    }

    // ── Token management (Phase F) ────────────────────────────────────────────
    //
    // Thin wrappers over the same `web_authentication_tokens` ops the CLI uses,
    // against zellij's shared tokens.db.  All three are ADMIN-gated: a read-only
    // session token is rejected (`reject_if_read_only`) so observers cannot mint
    // or revoke credentials.  The token DB is shared with real zellij — these
    // operate on the same tokens the `zellij web`/`muxrd` CLI manage.

    /// Create a new auth token. MUTATING (read-only rejected).  The secret is
    /// returned ONCE in `TokenInfo.token`.
    pub(super) async fn create_token_impl(
        &self,
        request: Request<CreateTokenReq>,
    ) -> Result<Response<TokenInfo>, Status> {
        reject_if_read_only(&request, "CreateToken")?;
        let req = request.into_inner();
        // An empty name lets zellij auto-generate one (CLI parity: Option<String>).
        let name = {
            let n = req.name.trim();
            if n.is_empty() {
                None
            } else {
                Some(n.to_owned())
            }
        };
        let read_only = req.read_only;
        log::info!("CreateToken: name={name:?} read_only={read_only}");

        let (token, actual_name) = tokio::task::spawn_blocking(move || {
            zellij_utils::web_authentication_tokens::create_token(name, read_only)
        })
        .await
        .map_err(|e| Status::internal(format!("CreateToken task panicked: {e}")))?
        .map_err(|e| {
            log::warn!("CreateToken: failed: {e:#}");
            Status::internal(format!("create token failed: {e:#}"))
        })?;

        Ok(Response::new(TokenInfo {
            name: actual_name,
            token, // secret — returned only here, never on ListTokens
            read_only,
            created_at: String::new(), // not surfaced by create_token; fetch via ListTokens
        }))
    }

    /// List existing auth tokens (metadata only — never the secret).
    /// Read-only rejected (token names are sensitive).
    pub(super) async fn list_tokens_impl(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<TokenList>, Status> {
        reject_if_read_only(&request, "ListTokens")?;

        let tokens =
            tokio::task::spawn_blocking(zellij_utils::web_authentication_tokens::list_tokens)
                .await
                .map_err(|e| Status::internal(format!("ListTokens task panicked: {e}")))?
                .map_err(|e| {
                    log::warn!("ListTokens: failed: {e:#}");
                    Status::internal(format!("list tokens failed: {e:#}"))
                })?;

        let proto_tokens: Vec<TokenInfo> = tokens
            .into_iter()
            .map(|t| TokenInfo {
                name: t.name,
                token: String::new(), // never expose existing secrets
                read_only: t.read_only,
                created_at: t.created_at,
            })
            .collect();

        log::info!("ListTokens: returning {} token(s)", proto_tokens.len());
        Ok(Response::new(TokenList {
            tokens: proto_tokens,
        }))
    }

    /// Revoke an auth token by name. MUTATING (read-only rejected).
    pub(super) async fn revoke_token_impl(
        &self,
        request: Request<RevokeTokenReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "RevokeToken")?;
        let req = request.into_inner();
        if req.name.trim().is_empty() {
            return Err(Status::invalid_argument(
                "RevokeToken: name must not be empty",
            ));
        }
        let name = req.name.clone();
        log::info!("RevokeToken: name='{name}'");

        let removed = tokio::task::spawn_blocking(move || {
            zellij_utils::web_authentication_tokens::revoke_token(&name)
        })
        .await
        .map_err(|e| Status::internal(format!("RevokeToken task panicked: {e}")))?
        .map_err(|e| {
            log::warn!("RevokeToken: failed: {e:#}");
            Status::internal(format!("revoke token failed: {e:#}"))
        })?;

        Ok(Response::new(ProtoAck {
            ok: removed,
            error: if removed {
                String::new()
            } else {
                format!(
                    "token '{}' not found (already revoked or never existed)",
                    req.name
                )
            },
            info: String::new(),
        }))
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::BackendKind;
    use crate::multiplexer::{BackendSet, ZellijBackend};
    use std::sync::Arc;

    /// A [`BackendSet`] containing a herdr entry. Push delivery only needs the
    /// herdr backend to be *present*; `get_version_impl` merely probes for its
    /// key and calls `backend_version()`, so a trivially-constructable backend
    /// (here `ZellijBackend`) registered under the `Herdr` kind is a faithful
    /// stand-in for "herdr detected" without standing up a real herdr relay.
    fn backends_with_herdr() -> BackendSet {
        BackendSet::new(vec![
            (BackendKind::Zellij, Arc::new(ZellijBackend) as _),
            (BackendKind::Herdr, Arc::new(ZellijBackend) as _),
        ])
    }

    /// No relay configured (the default) → neither the `"push-notifications"`
    /// capability nor a non-empty `notification_relay_url` is advertised.
    #[tokio::test]
    async fn get_version_without_relay_has_no_push_capability() {
        let service = MuxrService::with_backends(backends_with_herdr());

        let info = service
            .get_version_impl(Request::new(Empty {}))
            .await
            .expect("GetVersion must not error")
            .into_inner();

        assert!(
            !info.capabilities.iter().any(|c| c == "push-notifications"),
            "capabilities should not include push-notifications: {:?}",
            info.capabilities
        );
        assert_eq!(
            info.notification_relay_url, "",
            "notification_relay_url should be empty (proto3 default) when unset"
        );
    }

    /// A configured relay URL **and** herdr present → both the capability and
    /// the field are populated.
    #[tokio::test]
    async fn get_version_with_relay_and_herdr_advertises_push_capability_and_url() {
        let service = MuxrService::with_backends(backends_with_herdr())
            .with_notify_relay_url(Some("https://noti.muxr.app".to_owned()));

        let info = service
            .get_version_impl(Request::new(Empty {}))
            .await
            .expect("GetVersion must not error")
            .into_inner();

        assert!(
            info.capabilities.iter().any(|c| c == "push-notifications"),
            "capabilities should include push-notifications: {:?}",
            info.capabilities
        );
        assert_eq!(info.notification_relay_url, "https://noti.muxr.app");
    }

    /// A relay URL configured but NO herdr backend (zellij-only server) → push
    /// is unreachable, so neither the capability nor the URL is advertised even
    /// though a relay is set. This is the M3 fix: don't dangle a capability the
    /// server can never fulfil.
    #[tokio::test]
    async fn get_version_with_relay_but_no_herdr_does_not_advertise_push() {
        // `MuxrService::new()` builds a zellij-only backend set.
        let service =
            MuxrService::new().with_notify_relay_url(Some("https://noti.muxr.app".to_owned()));

        let info = service
            .get_version_impl(Request::new(Empty {}))
            .await
            .expect("GetVersion must not error")
            .into_inner();

        assert!(
            !info.capabilities.iter().any(|c| c == "push-notifications"),
            "a zellij-only server must not advertise push even with a relay URL: {:?}",
            info.capabilities
        );
        assert_eq!(
            info.notification_relay_url, "",
            "notification_relay_url must stay empty without a herdr backend"
        );
    }
}
