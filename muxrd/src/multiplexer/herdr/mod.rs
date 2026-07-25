//! Independently-authored types matching herdr's public v0.7.1 wire/JSON protocol for interop.
//! Not derived from herdr's AGPL source; herdr runs as a separate, unmodified, user-installed
//! binary driven over its public sockets.
//!
//! # herdr backend — Phase 2 foundation
//!
//! This module is the interface boundary between muxrd (MIT) and herdr (AGPL-3.0).
//! herdr is a separate, unmodified, user-installed binary.  muxrd drives it solely
//! through herdr's **public** Unix-domain sockets:
//!
//! - [`wire`] — binary relay socket (bincode v14 + 4-byte LE length frames).
//!   Used for terminal attach: send [`wire::ClientMessage`], receive
//!   [`wire::ServerMessage`].
//!
//! - [`api`] — line-delimited JSON control socket.  Used for workspace/tab/pane
//!   lifecycle:  [`api::ApiRequest`] lines in, [`api::ApiRawResponse`] lines out.
//!
//! - [`paths`] — resolves herdr's two socket paths (JSON-API + derived wire).
//! - [`registry`] — stable `u32`/`u64` ↔ herdr `String` id maps.
//! - [`control`] — the JSON-API control client + neutral layout transcode (P2.02).
//! - [`relay`] — the wire terminal relay (single-pane attach): the
//!   `MuxSender`/`MuxReceiver`/`DualHandle` data plane (P2.03).
//! - [`backend`] — `HerdrBackend`, the `MuxBackend` impl composing control +
//!   relay + registries into a complete second backend (P2.04).
//!
//! # AGPL discipline
//!
//! The struct definitions in `wire.rs` and `api.rs` are **independently authored**
//! to match herdr's public wire format (field names, types, discriminant order,
//! serde attributes).  They are an interoperability interface — the same shapes any
//! third-party client must replicate — and are kept in MIT-licensed muxrd.
//!
//! herdr's source code was used only to verify the wire format (field order,
//! discriminant tags, serde tagging); no code was copied or adapted.

pub mod api;
pub mod backend;
pub mod control;
pub mod paths;
pub mod registry;
pub mod relay;
pub mod subscribe;
pub mod wire;
