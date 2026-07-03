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

        let info = VersionInfo {
            server_version: SERVER_VERSION.to_owned(),
            zellij_version,
            backends: backend_versions,
            available_backends,
            // Additive capability flags (clients ignore unknown values).
            // "mouse-input": ClientFrame.mouse is understood on AttachTerminal
            // for every served backend (zellij Action::MouseEvent forwarding,
            // herdr wire InputEvents).
            capabilities: vec!["mouse-input".to_owned()],
        };
        log::debug!(
            "GetVersion → server={} zellij={:?} backends={} available={:?}",
            info.server_version,
            info.zellij_version,
            info.backends.len(),
            info.available_backends,
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

        let session_token =
            create_session_token(&req.auth_token, req.remember_me).map_err(|e| {
                log::info!("Login rejected: {e}");
                Status::unauthenticated(format!("invalid auth token: {e}"))
            })?;

        // Surface the read-only scope of the issued session so the client can
        // disable mutating controls up-front. Enforcement stays server-side;
        // this is advisory. Reuse the same DB-backed check the auth layer uses.
        let is_read_only =
            zellij_utils::web_authentication_tokens::is_session_token_read_only(&session_token)
                .unwrap_or(false);

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
