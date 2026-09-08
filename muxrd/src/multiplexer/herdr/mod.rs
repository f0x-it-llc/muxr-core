//! Types matching herdr's public wire/JSON protocol for interop, verified live
//! against herdr v0.9.0. herdr runs as a separate, unmodified, user-installed
//! binary driven over its public sockets.
//!
//! # herdr backend — Phase 2 foundation
//!
//! This module is the interface boundary between muxrd (MIT) and herdr
//! (Apache-2.0 as of herdr v0.8.0; AGPL-3.0-or-later before that). herdr is a
//! separate, unmodified, user-installed binary.  muxrd drives it solely
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
//! # Licence discipline
//!
//! herdr relicensed from AGPL-3.0-or-later to Apache-2.0 at v0.8.0
//! (`github.com/herdrdev/herdr`). Apache-2.0 permits derivative works, so the
//! struct definitions under this module may now be derived directly from
//! herdr's own source (field names, types, discriminant order, serde
//! attributes) rather than reverse-engineered from the wire format alone —
//! the former independent-authorship rule has been relaxed accordingly.
//!
//! A file whose types are derived this way carries its own header naming the
//! upstream source, the Apache-2.0 licence, and the fact that it was modified
//! for muxrd's connection-per-request client (see `api.rs`). MIT and
//! Apache-2.0 are both permissive and combine freely in one MIT-licensed
//! crate, so that attribution costs nothing extra. A file that still
//! describes itself only as "independently authored" has not been revisited
//! under the new licence and must not be assumed derived.

pub mod api;
pub mod backend;
pub mod control;
pub mod paths;
pub mod registry;
pub mod relay;
pub mod subscribe;
pub mod wire;
