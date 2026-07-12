//! muxrd — library crate.
//!
//! Exposes:
//! - `ipc`     — Phase-A IPC attach helpers (open session, recv renders, send input)
//! - `actions` — D1 send-action-and-await-ack helper + typed pane-op actions
//! - `grpc`    — tonic Muxr service implementation (GetVersion, Login, AttachTerminal, ListSessions, GetLayout)
//! - `auth`    — B3 bearer interceptor + SessionReadOnly extension
//! - `tls`     — B3 self-signed TLS cert generation + persistence
//! - `relay`   — B2 blocking-IPC ↔ async-gRPC bridge
//! - `query`   — C1 short-lived cli-client query helper (ListTabs/ListPanes JSON)
//! - `multiplexer` — P1 backend-agnostic `MuxBackend` trait + neutral types + `ZellijBackend`
//! - `notify`  — push-notification device registry (`push_devices.json` sidecar)
//! - `cli`     — E1 clap CLI definitions (subcommand structs)
//! - `config`  — E1 config file + precedence resolution
//! - `control` — E2 control socket (status/stop IPC contract)
//! - `proto`   — generated protobuf / gRPC types (via tonic-prost-build)

pub mod actions;
pub mod auth;
pub mod cli;
pub mod client_count;
pub mod config;
pub mod control;
pub mod grpc;
pub mod ipc;
pub mod multiplexer;
pub mod notify;
pub mod query;
pub mod ratelimit;
pub mod relay;
pub mod tls;
pub mod token_expiry;

/// Generated protobuf + tonic types for `muxr.v1`.
///
/// Included from the `$OUT_DIR` path produced by `tonic-prost-build` in
/// `build.rs`.
pub mod proto {
    tonic::include_proto!("muxr.v1");
}
