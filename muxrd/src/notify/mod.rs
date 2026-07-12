//! notify — push-notification device registry.
//!
//! `devices` persists the set of devices registered to receive pushes: a
//! 0600 JSON sidecar next to muxrd's other data-dir state, following the
//! `token_expiry` sidecar precedent (`crate::token_expiry`). muxrd never
//! stores a raw platform push token (FCM token, APNs token, ...) — only the
//! opaque `push_handle` the relay minted for that device (see
//! `RESEARCH-DELTA.md` §6). No entitlement/billing state lives here or
//! anywhere else in muxr-core (see `RESEARCH-DELTA.md` "Decisions already
//! committed") — muxrd/muxrctl/proto stay licensing-agnostic.

pub mod devices;
pub mod sender;
