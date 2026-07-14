//! Push-notification device-registry RPC implementations.
//!
//! Mirrors `token_ops.rs`'s admin-gating + `spawn_blocking` pattern: all three
//! RPCs are ADMIN-gated (reject read-only session tokens via
//! [`reject_if_read_only`]) and do their file I/O on the blocking pool
//! (`PushDeviceStore` is plain synchronous `std::fs`). `RegisterPushTarget`
//! additionally rejects when the server has no relay configured
//! (`failed_precondition`) — a handle with nowhere to send a push is a client
//! bug, not a legitimate registration.
//!
//! Handles are the sole credential the relay accepts to push to a device:
//! never logged in full, and `ListPushTargets` returns only an 8-char
//! `handle_prefix` (see [`crate::notify::devices::HANDLE_PREFIX_LEN`]).

use tonic::{Request, Response, Status};

use crate::notify::devices::{self, PushDevice};
use crate::proto::{
    ActionAck as ProtoAck, ListPushTargetsReq, ListPushTargetsResp, PushTargetInfo,
    RegisterPushTargetReq, RemovePushTargetReq,
};

use super::MuxrService;
use super::helpers::reject_if_read_only;

/// Return the first [`devices::HANDLE_PREFIX_LEN`] characters of `handle`,
/// safe to log or return to a client. Clamped so a malformed (too-short)
/// handle can never panic — validation should have already rejected it, but
/// this stays defensive at the log/response boundary regardless.
fn handle_prefix(handle: &str) -> &str {
    &handle[..devices::HANDLE_PREFIX_LEN.min(handle.len())]
}

/// Current Unix time in seconds.
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl MuxrService {
    /// Register (or refresh) a device's push handle. MUTATING (read-only
    /// rejected). `failed_precondition` when no relay is configured.
    pub(super) async fn register_push_target_impl(
        &self,
        request: Request<RegisterPushTargetReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "RegisterPushTarget")?;

        if self.notify_relay_url.is_none() {
            return Err(Status::failed_precondition(
                "RegisterPushTarget: server has no push-notification relay configured",
            ));
        }
        // The event kernel + outbound notifier only spawn in the herdr branch
        // (`bin/muxrd.rs`), so a registration on a zellij-only server can never
        // deliver a push. Reject it up-front rather than persisting a handle
        // that will never be notified (matches the GetVersion capability gate).
        if self.backends.get(crate::cli::BackendKind::Herdr).is_none() {
            return Err(Status::failed_precondition(
                "push notifications require the herdr backend",
            ));
        }

        let req = request.into_inner();
        devices::validate_push_handle(&req.push_handle)
            .map_err(|e| Status::invalid_argument(format!("push_handle: {e}")))?;
        devices::validate_device_name(&req.device_name)
            .map_err(|e| Status::invalid_argument(format!("device_name: {e}")))?;

        let device_name = req.device_name;
        let platform = req.platform;
        let push_handle = req.push_handle;
        let registered_at = now_epoch();

        log::info!(
            "RegisterPushTarget: device_name={device_name:?} platform={platform:?} \
             handle_prefix={}…",
            handle_prefix(&push_handle)
        );

        let store = self.push_devices.clone();
        tokio::task::spawn_blocking(move || {
            store.upsert(PushDevice {
                device_name,
                push_handle,
                platform,
                registered_at,
            })
        })
        .await
        .map_err(|e| Status::internal(format!("RegisterPushTarget task panicked: {e}")))?
        .map_err(|e| {
            log::warn!("RegisterPushTarget: failed to persist device: {e:#}");
            Status::internal(format!("failed to persist device: {e:#}"))
        })?;

        Ok(Response::new(ProtoAck {
            ok: true,
            error: String::new(),
            info: String::new(),
        }))
    }

    /// List registered devices (metadata + handle prefix only). ADMIN-gated
    /// (read-only rejected): device names + handle prefixes are sensitive
    /// fleet metadata.
    pub(super) async fn list_push_targets_impl(
        &self,
        request: Request<ListPushTargetsReq>,
    ) -> Result<Response<ListPushTargetsResp>, Status> {
        reject_if_read_only(&request, "ListPushTargets")?;

        let store = self.push_devices.clone();
        let listed = tokio::task::spawn_blocking(move || store.list())
            .await
            .map_err(|e| Status::internal(format!("ListPushTargets task panicked: {e}")))?
            .map_err(|e| {
                log::warn!("ListPushTargets: failed to read devices: {e:#}");
                Status::internal(format!("failed to read devices: {e:#}"))
            })?;

        let targets: Vec<PushTargetInfo> = listed
            .iter()
            .map(|d| PushTargetInfo {
                device_name: d.device_name.clone(),
                platform: d.platform.clone(),
                registered_at: d.registered_at,
                handle_prefix: handle_prefix(&d.push_handle).to_owned(),
            })
            .collect();

        log::info!("ListPushTargets: returning {} device(s)", targets.len());
        Ok(Response::new(ListPushTargetsResp { targets }))
    }

    /// Remove a registered device by name. MUTATING (read-only rejected).
    pub(super) async fn remove_push_target_impl(
        &self,
        request: Request<RemovePushTargetReq>,
    ) -> Result<Response<ProtoAck>, Status> {
        reject_if_read_only(&request, "RemovePushTarget")?;
        let req = request.into_inner();
        if req.device_name.trim().is_empty() {
            return Err(Status::invalid_argument(
                "RemovePushTarget: device_name must not be empty",
            ));
        }
        let name = req.device_name.clone();
        log::info!("RemovePushTarget: device_name={name:?}");

        let store = self.push_devices.clone();
        let removed = tokio::task::spawn_blocking(move || store.remove_by_name(&name))
            .await
            .map_err(|e| Status::internal(format!("RemovePushTarget task panicked: {e}")))?
            .map_err(|e| {
                log::warn!("RemovePushTarget: failed to remove device: {e:#}");
                Status::internal(format!("failed to remove device: {e:#}"))
            })?;

        Ok(Response::new(ProtoAck {
            ok: removed,
            error: if removed {
                String::new()
            } else {
                format!("device '{}' not found", req.device_name)
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
    use crate::notify::devices::PushDeviceStore;
    use std::sync::Arc;

    fn temp_store() -> (PushDeviceStore, std::path::PathBuf) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "muxrd-push-ops-test-{}-{secs}.json",
            std::process::id()
        ));
        (PushDeviceStore::at_path(path.clone()), path)
    }

    /// A backend set with a herdr entry — RegisterPushTarget now requires the
    /// herdr backend to be present (only a `get(Herdr)` presence probe is done),
    /// so a trivially-constructable `ZellijBackend` registered under the `Herdr`
    /// kind is a faithful stand-in for "herdr detected" in these unit tests.
    fn backends_with_herdr() -> BackendSet {
        BackendSet::new(vec![
            (BackendKind::Zellij, Arc::new(ZellijBackend) as _),
            (BackendKind::Herdr, Arc::new(ZellijBackend) as _),
        ])
    }

    fn service_with_relay() -> (MuxrService, std::path::PathBuf) {
        let (store, path) = temp_store();
        let service = MuxrService::with_backends(backends_with_herdr())
            .with_notify_relay_url(Some("https://noti.muxr.app".to_owned()))
            .with_push_device_store(store);
        (service, path)
    }

    fn valid_handle(ch: char) -> String {
        ch.to_string().repeat(devices::MIN_HANDLE_LEN)
    }

    fn read_only_request<T>(msg: T) -> Request<T> {
        let mut req = Request::new(msg);
        req.extensions_mut()
            .insert(crate::auth::SessionReadOnly(true));
        req
    }

    fn writable_request<T>(msg: T) -> Request<T> {
        let mut req = Request::new(msg);
        req.extensions_mut()
            .insert(crate::auth::SessionReadOnly(false));
        req
    }

    #[tokio::test]
    async fn register_rejects_read_only_token() {
        let (service, path) = service_with_relay();
        let req = read_only_request(RegisterPushTargetReq {
            push_handle: valid_handle('a'),
            device_name: "phone".to_owned(),
            platform: "android".to_owned(),
        });
        let err = service
            .register_push_target_impl(req)
            .await
            .expect_err("read-only token must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn remove_rejects_read_only_token() {
        let (service, path) = service_with_relay();
        let req = read_only_request(RemovePushTargetReq {
            device_name: "phone".to_owned(),
        });
        let err = service
            .remove_push_target_impl(req)
            .await
            .expect_err("read-only token must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn register_rejects_invalid_handle_and_name() {
        let (service, path) = service_with_relay();

        let bad_handle = writable_request(RegisterPushTargetReq {
            push_handle: "not-hex".to_owned(),
            device_name: "phone".to_owned(),
            platform: "android".to_owned(),
        });
        let err = service
            .register_push_target_impl(bad_handle)
            .await
            .expect_err("invalid handle must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        let bad_name = writable_request(RegisterPushTargetReq {
            push_handle: valid_handle('a'),
            device_name: "bad/name".to_owned(),
            platform: "android".to_owned(),
        });
        let err = service
            .register_push_target_impl(bad_name)
            .await
            .expect_err("invalid device_name must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn register_without_relay_fails_precondition() {
        let (store, path) = temp_store();
        let service = MuxrService::new().with_push_device_store(store); // no relay URL set
        let req = writable_request(RegisterPushTargetReq {
            push_handle: valid_handle('a'),
            device_name: "phone".to_owned(),
            platform: "android".to_owned(),
        });
        let err = service
            .register_push_target_impl(req)
            .await
            .expect_err("no relay configured must fail precondition");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn register_without_herdr_backend_fails_precondition() {
        // Relay configured, but a zellij-only server (no herdr) can never
        // deliver a push — RegisterPushTarget must refuse rather than persist a
        // handle that will never be notified (M3).
        let (store, path) = temp_store();
        let service = MuxrService::new() // zellij-only backend set
            .with_notify_relay_url(Some("https://noti.muxr.app".to_owned()))
            .with_push_device_store(store);
        let req = writable_request(RegisterPushTargetReq {
            push_handle: valid_handle('a'),
            device_name: "phone".to_owned(),
            platform: "android".to_owned(),
        });
        let err = service
            .register_push_target_impl(req)
            .await
            .expect_err("no herdr backend must fail precondition");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message().contains("herdr"),
            "message should explain herdr is required: {}",
            err.message()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn register_then_list_shows_prefix_only() {
        let (service, path) = service_with_relay();
        let handle = valid_handle('b');

        let ack = service
            .register_push_target_impl(writable_request(RegisterPushTargetReq {
                push_handle: handle.clone(),
                device_name: "phone-1".to_owned(),
                platform: "ios".to_owned(),
            }))
            .await
            .expect("register must succeed")
            .into_inner();
        assert!(ack.ok);

        let listed = service
            .list_push_targets_impl(writable_request(ListPushTargetsReq {}))
            .await
            .expect("list must succeed")
            .into_inner();
        assert_eq!(listed.targets.len(), 1);
        let target = &listed.targets[0];
        assert_eq!(target.device_name, "phone-1");
        assert_eq!(target.platform, "ios");
        assert_eq!(target.handle_prefix, &handle[..8]);
        assert_ne!(
            target.handle_prefix, handle,
            "the full handle must never be returned by ListPushTargets"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn register_twice_with_same_name_replaces_not_duplicates() {
        let (service, path) = service_with_relay();

        service
            .register_push_target_impl(writable_request(RegisterPushTargetReq {
                push_handle: valid_handle('a'),
                device_name: "phone-1".to_owned(),
                platform: "android".to_owned(),
            }))
            .await
            .expect("first register must succeed");
        service
            .register_push_target_impl(writable_request(RegisterPushTargetReq {
                push_handle: valid_handle('c'),
                device_name: "phone-1".to_owned(),
                platform: "android".to_owned(),
            }))
            .await
            .expect("re-register must succeed");

        let listed = service
            .list_push_targets_impl(writable_request(ListPushTargetsReq {}))
            .await
            .expect("list must succeed")
            .into_inner();
        assert_eq!(listed.targets.len(), 1, "re-register must replace");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn remove_existing_and_missing_device() {
        let (service, path) = service_with_relay();

        service
            .register_push_target_impl(writable_request(RegisterPushTargetReq {
                push_handle: valid_handle('a'),
                device_name: "phone-1".to_owned(),
                platform: "android".to_owned(),
            }))
            .await
            .expect("register must succeed");

        let ack = service
            .remove_push_target_impl(writable_request(RemovePushTargetReq {
                device_name: "phone-1".to_owned(),
            }))
            .await
            .expect("remove must succeed")
            .into_inner();
        assert!(ack.ok);

        let ack_missing = service
            .remove_push_target_impl(writable_request(RemovePushTargetReq {
                device_name: "phone-1".to_owned(),
            }))
            .await
            .expect("remove of an already-removed device must not error")
            .into_inner();
        assert!(!ack_missing.ok);
        assert!(ack_missing.error.contains("not found"));

        let _ = std::fs::remove_file(&path);
    }
}
