//! herdr's binary wire protocol, mirrored for muxrd — **derived from herdr, and modified**.
//!
//! # Derivation, licence and attribution
//!
//! The message layouts in this file are derived from herdr's own protocol source,
//! `src/protocol/wire.rs` at tag `v0.9.0` of the upstream herdr repository
//! (<https://github.com/herdrdev/herdr>), which herdr licenses under the **Apache
//! License, Version 2.0** — herdr relicensed from AGPL-3.0-or-later at v0.8.0.
//!
//! **This file is a modified derivation, not a copy.** It mirrors only the subset
//! of the protocol muxrd speaks, carries muxrd's own framing helpers, diagnostics,
//! doc comments and tests, and deliberately leaves the client-owned shell payloads
//! unmirrored (see [What muxrd mirrors](#what-muxrd-mirrors-and-what-it-skips)).
//! Attribution is retained per Apache-2.0 §4; `THIRD-PARTY-NOTICES.md` at the
//! repository root carries the repository-level notice.
//!
//! herdr itself is still a **separate, unmodified, user-installed binary** that
//! muxrd drives only over its public Unix-domain sockets; no herdr code is linked.
//!
//! # Binary wire protocol — terminal relay socket
//!
//! Every frame is `[u32 LE payload length][bincode 2 payload]`
//! (`bincode::config::standard()`). Enum discriminants are **positional** — the
//! 0-based declaration index — so the `ClientMessage` / `ServerMessage` declaration
//! order below must match herdr's exactly or every frame is misread. herdr also
//! rejects a frame whose decoded length differs from its declared length, so a
//! missing or extra field is a hard decode failure on the server, never a defaulted
//! value: a frame that ends up one field short is dropped and the connection closed
//! *without* a reply.
//!
//! # Version handling — discovered, never pinned
//!
//! herdr enforces **strict equality** on the handshake protocol version: it rejects
//! a client that is older *or* newer than itself. muxrd therefore discovers the
//! server's version over the stable JSON-API socket (`HerdrControl::ping`) and
//! echoes it back, rather than compiling one in — a pinned constant is guaranteed to
//! break on some herdr release, and did (muxrd sent 14 while herdr shipped 16 in
//! v0.7.2 and 17 in v0.7.5, breaking every attach). [`HERDR_MIN_PROTOCOL`] and
//! [`HERDR_MAX_TESTED_PROTOCOL`] are **diagnostics only**: neither is ever sent, and
//! neither may gate or refuse an attach.
//!
//! # herdr does NOT only append variants
//!
//! Up to protocol 17 herdr had only ever *appended* variants, and the layout this
//! module used to carry rested on that assumption. **That assumption is false.**
//! Between 17 (v0.7.5) and 22 (v0.9.0) herdr:
//!
//! - **deleted** the semantic-frame server variant that sat at tag 1, shifting every
//!   server tag above it down by one — the decode break this mirror was rewritten for;
//! - **renamed and retyped** the client handshake (`Hello` → `TerminalHello`), dropping
//!   its requested-encoding, keybindings and launch-mode fields;
//! - **added** a field to the client `Resize` and to the server `MouseCapture`;
//! - **reused** client tag 7 (batched input events → observe-terminal).
//!
//! So this module mirrors exactly **one** protocol version, 22, with no compatibility
//! branch and no version-dependent layout. A server reporting anything else is a
//! diagnostic warning, not a supported peer. Re-derive the tables below from herdr's
//! source on every rebaseline; do not assume a newer release only appended.
//!
//! ## `ClientMessage` tags (protocol 22)
//!
//! | Tag | Variant | Tag | Variant |
//! |---|---|---|---|
//! | 0 | `TerminalHello` | 11 | `ClientShellHello` |
//! | 1 | `Input` | 12 | `ClientShellResize` |
//! | 2 | `ClipboardImage` | 13 | `ClientShellPaneInput` |
//! | 3 | `Resize` | 14 | `ClientShellPopupInput` |
//! | 4 | `Detach` | 15 | `ClientShellEndpointRequest` |
//! | 5 | `AttachTerminal` | 16 | `AttachMouse` |
//! | 6 | `AttachScroll` | 17 | `ClientShellHostTheme` |
//! | 7 | `ObserveTerminal` | 18 | `ClientShellFocus` |
//! | 8 | `ControlTerminal` | 19 | `ClientShellMouseCapture` |
//! | 9 | `GraphicsTransmissionResult` | 20 | `EndpointControl` |
//! | 10 | `GraphicsTransmissionStarted` | | |
//!
//! ## `ServerMessage` tags (protocol 22)
//!
//! | Tag | Variant | Tag | Variant |
//! |---|---|---|---|
//! | 0 | `Welcome` | 11 | `GraphicsTransmissionRetired` |
//! | 1 | `Terminal` | 12 | `ClientShellSnapshot` |
//! | 2 | `Graphics` | 13 | `PaneSurface` |
//! | 3 | `ServerShutdown` | 14 | `SemanticNotification` |
//! | 4 | `Notify` | 15 | `ClientShellError` |
//! | 5 | `Clipboard` | 16 | `DirectTerminalKeyboardProtocol` |
//! | 6 | `WindowTitle` | 17 | `ClientShellKeyboardReportAll` |
//! | 7 | `ReloadSoundConfig` | 18 | `ClientShellEndpointResponseChunk` |
//! | 8 | `MouseCapture` | 19 | `PaneSurfacePatch` |
//! | 9 | `TerminalBell` | 20 | `EndpointControl` |
//! | 10 | `GraphicsFile` | | |
//!
//! Both tables were transcribed from herdr v0.9.0's enum declaration order and
//! cross-checked against its own tag tests (`client_message_wire_tags_reflect_current_order`,
//! `client_shell_server_message_tags_are_frozen`); the tests at the bottom of this
//! file assert every one of them.
//!
//! # What muxrd mirrors, and what it skips
//!
//! muxrd is a **direct terminal attach** client, so it mirrors those payloads in
//! full. Four `ServerMessage` payloads and two `ClientMessage` payloads are *not*
//! mirrored: they carry unbounded projections of herdr's client-owned shell UI
//! state ([`ServerMessage::ClientShellSnapshot`], [`ServerMessage::PaneSurface`],
//! [`ServerMessage::PaneSurfacePatch`]) or reach outside herdr's protocol module
//! into its input, config and API models ([`ClientMessage::ClientShellPaneInput`],
//! [`ClientMessage::ClientShellPopupInput`], [`ServerMessage::SemanticNotification`]).
//! Those six variants are declared **payload-free**, purely to hold their tags so
//! every tag around them stays correct:
//!
//! - on the read side, [`UNMIRRORED_SERVER_TAGS`] makes the reader skip such a frame
//!   by its length prefix ([`WireFrame::Unknown`]) *before* attempting a decode that
//!   would fail — the same treatment a tag above [`HIGHEST_KNOWN_SERVER_TAG`] gets;
//! - on the write side, [`encode`] refuses to encode one rather than emit a frame
//!   herdr would reject.
//!
//! Skipping is always safe: the whole payload has already been consumed using its
//! length prefix, so the stream stays byte-aligned for the next frame. That is what
//! keeps an additive herdr change from silently killing a terminal.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

// ─── Protocol constants ───────────────────────────────────────────────────────

/// Oldest herdr wire protocol version this module's layout is valid for (herdr
/// v0.9.0 ships 22, and 22 is the only version mirrored here).
///
/// muxrd **does not pin** a protocol version: herdr enforces strict equality on the
/// handshake and rejects clients older *or* newer than itself, so the only correct
/// value to send is whatever the connected server reports over the JSON-API `ping`
/// (`HerdrControl::ping`). This floor is a **diagnostic tripwire only** — it catches
/// an implausible downgrade to a server whose layout predates this mirror — and it
/// never gates or refuses an attach.
pub const HERDR_MIN_PROTOCOL: u32 = 22;

/// Highest herdr wire protocol version muxrd has been **tested** against (v0.9.0).
///
/// A diagnostic tripwire, **not a gate** — muxrd still attaches to a server
/// reporting a higher protocol, it just logs a prominent warning first. Unlike the
/// 14 → 17 era, a bump is *not* presumed additive: herdr deleted and retyped
/// variants between 17 and 22, so this warning means "re-derive the layout from
/// herdr's source", not "probably fine".
///
/// **Paired with the dev rig's pin:** `docker/Dockerfile`'s `ARG HERDR_VERSION`
/// (and compose's `${HERDR_VERSION:-…}` fallbacks) name the release that ships this
/// protocol — 0.9.0. Move both in the same change, or the rig stops testing the
/// protocol this constant claims.
pub const HERDR_MAX_TESTED_PROTOCOL: u32 = 22;

/// Highest `ServerMessage` bincode tag herdr 0.9.0 declares (`EndpointControl`).
///
/// A frame carrying a higher tag is a variant a **newer** herdr appended; it is
/// skipped via its length prefix ([`WireFrame::Unknown`]) instead of being treated
/// as a corrupt stream.
const HIGHEST_KNOWN_SERVER_TAG: u8 = 20;

/// `ServerMessage` tags herdr 0.9.0 declares but whose payload muxrd deliberately
/// does not mirror — the client-owned shell lane a direct terminal attach never
/// receives (see the module header).
///
/// Their variants exist payload-free to hold their tags, so a frame carrying one
/// must be **skipped by its length prefix, never decoded**: decoding it against a
/// payload-free variant would fail the trailing-bytes check and end the relay.
const UNMIRRORED_SERVER_TAGS: [u8; 4] = [12, 13, 14, 19];

/// Maximum frame payload we accept from the server (2 MiB, matching herdr's
/// `MAX_FRAME_SIZE`). herdr raises this only for clients that enable Kitty
/// graphics, which muxrd does not. Frames larger than this are rejected before
/// allocation.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Whether a server frame carrying `tag` must be skipped by its length prefix
/// rather than decoded: either a variant a newer herdr appended (above
/// [`HIGHEST_KNOWN_SERVER_TAG`]) or one of the [`UNMIRRORED_SERVER_TAGS`].
fn is_skippable_server_tag(tag: u8) -> bool {
    tag > HIGHEST_KNOWN_SERVER_TAG || UNMIRRORED_SERVER_TAGS.contains(&tag)
}

// ─── Enums shared between client and server ───────────────────────────────────

/// Render encoding herdr selects for a connection and reports in `Welcome`.
///
/// It is **no longer client-requested**: herdr 0.9.0 dropped the requested-encoding
/// field from the handshake and forces `TerminalAnsi` for every non-endpoint
/// client — exactly what muxrd wants. The enum survives because the handshake reply
/// still carries it, with both variants unchanged in count and order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderEncoding {
    /// Full semantic frame values (herdr's own local/default rendering mode).
    SemanticFrame,
    /// Pre-diffed terminal ANSI byte streams — what muxrd renders.
    TerminalAnsi,
}

// ─── ClientMessage supporting types ──────────────────────────────────────────

/// Terminal a client's clipboard image is bridged into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientClipboardImageTarget {
    /// The connection's directly attached terminal.
    DirectTerminal,
    /// A stable pane id in a client-owned shell.
    Pane(String),
    /// A popup terminal id in a client-owned shell.
    Popup(String),
}

/// Scroll direction for attach-mode scrollback events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachScrollDirection {
    Up,
    Down,
}

/// Input source for an attach-mode scroll event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachScrollSource {
    /// Mouse wheel scroll.
    Wheel,
    /// Page key forwarded from the client.
    PageKey {
        /// Original key bytes to forward when the child app owns page keys.
        input: Vec<u8>,
    },
}

/// Pane-surface size requested by a client-owned shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSurfaceSize {
    pub cols: u16,
    pub rows: u16,
}

/// Mouse button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMouseButton {
    Left,
    Right,
    Middle,
}

/// Mouse event kind. Declaration order matches herdr's `ClientMouseKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMouseKind {
    Down(ClientMouseButton),
    Up(ClientMouseButton),
    Drag(ClientMouseButton),
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

/// Where a structured mouse event landed: cell coordinates, or SGR pixel
/// coordinates plus the cell they resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMousePosition {
    Cell {
        column: u16,
        row: u16,
    },
    Pixels {
        x: u32,
        y: u32,
        column: u16,
        row: u16,
    },
}

/// Exact terminal geometry accompanying a structured mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMouseGeometry {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
}

/// An RGB colour observed on the client's host terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHostColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Which of the host terminal's default colours a theme update carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostDefaultColorKind {
    Foreground,
    Background,
}

/// Light/dark appearance reported by a client-owned shell's host terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostAppearance {
    Dark,
    Light,
}

/// One host terminal colour or appearance update from a client-owned shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostThemeUpdate {
    DefaultColor {
        kind: ClientHostDefaultColorKind,
        color: ClientHostColor,
    },
    PaletteColors(Vec<(u8, ClientHostColor)>),
    Appearance(ClientHostAppearance),
}

// ─── ClientMessage ────────────────────────────────────────────────────────────

/// Messages sent from muxrd → herdr over the wire relay socket.
///
/// **Declaration order is wire-critical.** bincode `standard()` encodes the variant
/// index sequentially from 0; any reordering breaks the protocol. The protocol-22
/// tag table is in this file's module header and asserted by
/// `client_message_discriminants_match_protocol_22`.
///
/// muxrd only ever sends `TerminalHello`, `Input`, `Resize`, `Detach`,
/// `AttachTerminal` and `AttachScroll`. Every other variant is declared so the tags
/// of the ones muxrd does send stay correct; the two client-owned shell input
/// variants are declared payload-free (see the module header) and [`encode`]
/// refuses them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Direct terminal handshake — must be the first message on a new connection.
    /// Tag = 0.
    ///
    /// Renamed from `Hello` and retyped in herdr 0.9.0: the requested render
    /// encoding, keybindings selection and launch mode are **gone** (herdr forces
    /// `TerminalAnsi` for a direct attach, and the launch mode is implied by which
    /// handshake is sent plus the follow-up attach).
    TerminalHello {
        /// Protocol version, discovered per connection — never a compiled-in constant.
        version: u32,
        cols: u16,
        rows: u16,
        /// Physical pixel width of a terminal cell (0 = Kitty graphics disabled).
        cell_width_px: u32,
        /// Physical pixel height of a terminal cell (0 = Kitty graphics disabled).
        cell_height_px: u32,
        /// Whether this client sends coherent geometry for SGR pixel mouse input.
        /// muxrd sends `false`: it advertises no cell dimensions and does not use
        /// pixel mouse reporting.
        pixel_mouse: bool,
    },

    /// Raw input bytes from the client's stdin. Tag = 1.
    Input { data: Vec<u8> },

    /// Image bytes for remote clipboard paste bridging. Tag = 2.
    /// muxrd does not send this variant.
    ClipboardImage {
        target: ClientClipboardImageTarget,
        /// Image file extension without a leading dot.
        extension: String,
        data: Vec<u8>,
    },

    /// Terminal resize notification. Tag = 3.
    ///
    /// Gained `pixel_mouse` in herdr 0.9.0. The encoding is positional, so a
    /// missing fifth field is a decode failure on the server, not a default.
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        /// Whether this resize carries coherent geometry for SGR pixel mouse input.
        /// muxrd sends `false`.
        pixel_mouse: bool,
    },

    /// Graceful disconnect. Tag = 4.
    Detach,

    /// Switch this connection into direct terminal-attach mode. Tag = 5.
    AttachTerminal {
        /// herdr `terminal_id` — the attach key returned by the JSON-API.
        terminal_id: String,
        /// Replace an existing writable owner for this terminal.
        takeover: bool,
    },

    /// Scrollback scroll in direct-attach mode. Tag = 6.
    ///
    /// Sent by the relay's `send_mouse` (mouse-input capability): wheel events
    /// scroll the attached pane's scrollback, or reach a mouse-capturing app via
    /// the wheel source + position.
    AttachScroll {
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        /// Crossterm-compatible modifier bitmask.
        modifiers: u8,
    },

    /// Switch this connection into read-only terminal observe mode. Tag = 7.
    /// (Protocol 17's batched-input message occupied this tag; muxrd never sent it.)
    ObserveTerminal { target: String },

    /// Switch this connection into writable terminal control mode. Tag = 8.
    ControlTerminal { target: String, takeover: bool },

    /// Result of a herdr-owned direct Kitty graphics transmission. Tag = 9.
    GraphicsTransmissionResult {
        transfer_id: u64,
        image_id: u32,
        success: bool,
    },

    /// A direct graphics command was written and flushed. Tag = 10.
    GraphicsTransmissionStarted { transfer_id: u64, image_id: u32 },

    /// Handshake for a client-owned shell around one pane surface. Tag = 11.
    ClientShellHello {
        version: u32,
        cell_width_px: u32,
        cell_height_px: u32,
        surface_size: ClientSurfaceSize,
        pixel_mouse: bool,
        direct_graphics: bool,
        /// Whether the endpoint's keymap, rather than the client's, owns shell bindings.
        endpoint_keybindings: bool,
        /// Whether this client wants shell mouse capture even without pane demand.
        mouse_capture: bool,
    },

    /// Resize the pane viewport of a client-owned shell. Tag = 12.
    ClientShellResize {
        cell_width_px: u32,
        cell_height_px: u32,
        surface_size: ClientSurfaceSize,
        pixel_mouse: bool,
    },

    /// Client-classified semantic input for a stable pane target. Tag = 13.
    ///
    /// **Payload deliberately not mirrored** (see the module header): herdr's
    /// `Vec<ClientPaneInputEvent>` reaches outside its protocol module into its
    /// input model. Declared payload-free to hold tag 13; [`encode`] refuses it, so
    /// a malformed frame can never reach herdr. Mirror the payload from herdr's
    /// source first if muxrd ever needs to send one.
    ClientShellPaneInput,

    /// Client-classified semantic input for the active popup terminal. Tag = 14.
    /// Payload deliberately not mirrored — see [`ClientMessage::ClientShellPaneInput`].
    ClientShellPopupInput,

    /// Invoke one endpoint operation through a client shell's connection. Tag = 15.
    ClientShellEndpointRequest { boot_id: String, request: String },

    /// One structured mouse event for a directly attached terminal. Tag = 16.
    ///
    /// muxrd deliberately keeps using [`ClientMessage::AttachScroll`], which still
    /// works unchanged; adopting this richer message is a separate decision.
    AttachMouse {
        kind: ClientMouseKind,
        position: ClientMousePosition,
        geometry: Option<ClientMouseGeometry>,
        /// Crossterm-compatible modifier bitmask.
        modifiers: u8,
        lines: u16,
    },

    /// Host terminal colour/appearance update from a client-owned shell. Tag = 17.
    ClientShellHostTheme { update: ClientHostThemeUpdate },

    /// Whether the outer terminal containing a client shell has focus. Tag = 18.
    ClientShellFocus { focused: bool },

    /// A client shell's mouse-capture preference after a config reload. Tag = 19.
    ClientShellMouseCapture { enabled: bool },

    /// Extensible named control message for herdr's stable endpoint protocol.
    /// Tag = 20. Append-only upstream: its tag and two-string payload are frozen.
    EndpointControl { kind: String, data: String },
}

// ─── ServerMessage supporting types ──────────────────────────────────────────

/// Terminal ANSI bytes delivered by the server for `TerminalAnsi` clients.
/// This is the primary render payload consumed by muxrd, and its layout is
/// unchanged from protocol 17.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFrame {
    /// Monotonic per-client frame sequence number.
    pub seq: u64,
    pub width: u16,
    pub height: u16,
    /// `true` = full repaint; `false` = incremental diff.
    pub full: bool,
    /// Raw ANSI escape sequences, ready to write to the terminal.
    pub bytes: Vec<u8>,
}

/// Notification kind sent from server to client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotifyKind {
    Sound,
    Toast,
    SystemToast,
}

/// Target of a herdr-owned Kitty graphics upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceGraphicsTarget {
    Pane { pane_id: String },
    Popup { terminal_id: String },
}

/// Origin of a herdr-owned Kitty graphics asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceGraphicsSource {
    Terminal {
        target: SurfaceGraphicsTarget,
        image_id: u32,
    },
    PaneLayer {
        pane_id: String,
        layer_id: String,
    },
}

/// Pixel format of a herdr-owned Kitty graphics asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceGraphicsFormat {
    Rgb,
    Rgba,
    Png,
}

/// Identity of one graphics asset a client-owned shell has been sent.
/// `None` on [`ServerMessage::GraphicsFile`] means the upload targets a direct
/// terminal client — the only case muxrd could ever see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceGraphicsAssetKey {
    pub source: SurfaceGraphicsSource,
    pub image_width: u32,
    pub image_height: u32,
    pub format: SurfaceGraphicsFormat,
    pub data_len: u64,
    pub data_fingerprint: u64,
}

// ─── ServerMessage ────────────────────────────────────────────────────────────

/// Messages sent from herdr → muxrd over the wire relay socket.
///
/// **Declaration order is wire-critical** — discriminants must match herdr's
/// `ServerMessage` exactly. The protocol-22 tag table is in this file's module
/// header and asserted by `server_message_discriminants_match_protocol_22`.
///
/// herdr 0.9.0 **deleted** protocol 17's semantic-frame variant from tag 1, moving
/// every tag above it down by one: only tag 0 still means what protocol 17 meant.
/// The four client-owned shell payloads muxrd does not mirror are declared
/// payload-free and skipped by tag before any decode (see the module header).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Handshake response. Tag = 0. Payload unchanged from protocol 17.
    Welcome {
        version: u32,
        encoding: RenderEncoding,
        /// If `Some`, the handshake failed; muxrd must surface this error and close.
        error: Option<String>,
    },

    /// Terminal ANSI bytes — muxrd's render payload. Tag = 1 (was 2 in protocol 17).
    Terminal(TerminalFrame),

    /// Client-local Kitty graphics bytes. Tag = 2.
    Graphics { bytes: Vec<u8> },

    /// Server shutting down; muxrd should close the connection. Tag = 3.
    ServerShutdown { reason: Option<String> },

    /// Notification event (sound / toast). Tag = 4.
    Notify {
        kind: NotifyKind,
        message: String,
        body: Option<String>,
    },

    /// OSC 52 clipboard data forwarded from a PTY. Tag = 5.
    Clipboard { data: String },

    /// Set the client's outer terminal window title. Tag = 6.
    WindowTitle { title: Option<String> },

    /// Reload client-local sound config. Tag = 7.
    ReloadSoundConfig,

    /// Whether the client should capture host mouse input. Tag = 8.
    ///
    /// Gained `sgr_pixels` in herdr 0.8.2. herdr enforces that a frame's decoded
    /// length matches its declared length, so mirroring only `enabled` made this a
    /// hard decode failure.
    MouseCapture {
        /// True when herdr's mouse UI is enabled or the focused pane requests
        /// mouse reporting.
        enabled: bool,
        /// True only while the focused pane requests DEC SGR pixel mode 1016.
        sgr_pixels: bool,
    },

    /// Ring the client's outer terminal for pane-originated BEL characters.
    /// Tag = 9. Delivered to a direct attach, so muxrd decodes and drains it.
    TerminalBell { count: u16 },

    /// One validated herdr-owned Kitty regular-file RGBA transmission. Tag = 10.
    /// muxrd never enables graphics, so it never asks for one.
    GraphicsFile {
        path: String,
        expected_len: u64,
        image_id: u32,
        transfer_id: u64,
        leading: Vec<u8>,
        control: String,
        /// Client-shell upload identity; `None` targets a direct terminal client.
        surface_asset: Option<SurfaceGraphicsAssetKey>,
    },

    /// Suppress a direct graphics command that expired before delivery. Tag = 11.
    GraphicsTransmissionRetired { transfer_id: u64, image_id: u32 },

    /// Initial resource projection for a client-owned shell. Tag = 12.
    ///
    /// **Payload deliberately not mirrored** (see the module header): herdr's
    /// `ClientShellSnapshot` is an unbounded projection of its whole workspace /
    /// tab / pane / agent model. Declared payload-free to hold tag 12; listed in
    /// [`UNMIRRORED_SERVER_TAGS`] so the reader skips such a frame by its length
    /// prefix instead of failing to decode it.
    ClientShellSnapshot,

    /// Rendered active-tab pane surface for a client-owned shell. Tag = 13.
    /// Payload deliberately not mirrored — see [`ServerMessage::ClientShellSnapshot`].
    PaneSurface,

    /// Ephemeral semantic notification for client-rendered shells. Tag = 14.
    /// Payload deliberately not mirrored — it reaches into herdr's config model.
    SemanticNotification,

    /// Immediate endpoint error a client-rendered shell must show. Tag = 15.
    ClientShellError { message: String },

    /// Exact Kitty keyboard flags requested by a directly attached terminal.
    /// Tag = 16. Delivered to a direct attach, so muxrd decodes and drains it —
    /// kterm owns its own keyboard protocol.
    DirectTerminalKeyboardProtocol {
        flags: u16,
        modify_other_keys_level: u8,
    },

    /// Whether a client shell's focused pane needs every key reported. Tag = 17.
    ClientShellKeyboardReportAll { enabled: bool },

    /// One ordered chunk of an endpoint operation's response. Tag = 18.
    ClientShellEndpointResponseChunk {
        boot_id: String,
        request_id: String,
        final_chunk: bool,
        data: Vec<u8>,
    },

    /// Incremental cell update for a committed pane surface. Tag = 19.
    /// Payload deliberately not mirrored — see [`ServerMessage::ClientShellSnapshot`].
    PaneSurfacePatch,

    /// Extensible named control message for herdr's stable endpoint protocol.
    /// Tag = 20. Append-only upstream: its tag and two-string payload are frozen.
    EndpointControl { kind: String, data: String },
}

// ─── Read results ─────────────────────────────────────────────────────────────

/// One frame read off the wire.
///
/// Every frame carries a 4-byte little-endian length prefix, so a frame muxrd does
/// not decode can be consumed and discarded without desynchronising the stream. Two
/// kinds of frame are reported as [`WireFrame::Unknown`] rather than as an error:
/// a tag above [`HIGHEST_KNOWN_SERVER_TAG`] (a variant a newer herdr appended) and
/// one of the [`UNMIRRORED_SERVER_TAGS`] (a client-owned shell payload muxrd
/// deliberately does not mirror). Treating either as an error would silently kill
/// the terminal relay.
#[derive(Debug)]
pub enum WireFrame {
    /// A frame decoded into a variant this module mirrors.
    Message(Box<ServerMessage>),
    /// A well-framed message muxrd does not decode; safely skipped.
    Unknown { tag: u8, len: usize },
}

// ─── Framing errors ───────────────────────────────────────────────────────────

/// Errors from framing / deframing operations.
#[derive(Debug)]
pub enum FramingError {
    /// Declared frame length exceeds [`MAX_FRAME_SIZE`].
    Oversized { claimed: usize, max: usize },
    /// Underlying I/O error.
    Io(io::Error),
    /// bincode (de)serialization failure.
    Bincode(String),
    /// Stream closed before a complete frame could be read.
    UnexpectedEof,
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversized { claimed, max } => {
                write!(f, "frame length {claimed} exceeds maximum {max}")
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Bincode(e) => write!(f, "bincode error: {e}"),
            Self::UnexpectedEof => write!(f, "unexpected end of stream"),
        }
    }
}

impl std::error::Error for FramingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FramingError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

// ─── Framing helpers ─────────────────────────────────────────────────────────

/// Encode a [`ClientMessage`] to a length-prefixed frame:
/// `[u32LE length][bincode::standard() payload]`.
///
/// The returned `Vec<u8>` is ready to write directly to the socket.
///
/// Refuses the two payload-free client-shell placeholders: they exist only to hold
/// their bincode tags, so encoding one would put a frame on the wire that herdr
/// decodes one payload short, drops, and closes the connection over — without a
/// reply. Better a local error than a silent end-of-stream.
pub fn encode(msg: &ClientMessage) -> Result<Vec<u8>, FramingError> {
    if matches!(
        msg,
        ClientMessage::ClientShellPaneInput | ClientMessage::ClientShellPopupInput
    ) {
        return Err(FramingError::Bincode(format!(
            "{msg:?} is a payload-free tag placeholder, not a message muxrd can send — \
             mirror its payload from herdr's source first"
        )));
    }
    let payload = bincode::serde::encode_to_vec(msg, bincode::config::standard())
        .map_err(|e| FramingError::Bincode(e.to_string()))?;
    let len = payload.len();
    if len > u32::MAX as usize {
        return Err(FramingError::Bincode(format!(
            "payload length {len} exceeds u32::MAX — too large to frame"
        )));
    }
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&(len as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Encode and write a [`ClientMessage`] to `writer` as a length-prefixed frame,
/// flushing afterwards.
pub fn write_message<W: Write>(writer: &mut W, msg: &ClientMessage) -> Result<(), FramingError> {
    let frame = encode(msg)?;
    writer.write_all(&frame).map_err(FramingError::Io)?;
    writer.flush().map_err(FramingError::Io)
}

/// Read one [`ServerMessage`] from `reader` (blocking).
///
/// Reads the 4-byte LE length prefix then decodes the payload with
/// `bincode::config::standard()`. Returns [`FramingError::UnexpectedEof`] on a
/// clean stream close and [`FramingError::Oversized`] if the declared length
/// exceeds [`MAX_FRAME_SIZE`].
pub fn read_server_message<R: Read>(reader: &mut R) -> Result<WireFrame, FramingError> {
    let mut len_buf = [0u8; 4];
    read_exact_or_eof(reader, &mut len_buf)?;
    let claimed = u32::from_le_bytes(len_buf) as usize;
    if claimed > MAX_FRAME_SIZE {
        return Err(FramingError::Oversized {
            claimed,
            max: MAX_FRAME_SIZE,
        });
    }
    let mut payload = vec![0u8; claimed];
    read_exact_or_eof(reader, &mut payload)?;

    // Peek the enum discriminant before decoding. bincode's `standard()` config
    // encodes the variant tag as a varint, and every value we care about here
    // (0..=250) is a single leading byte — so a frame muxrd does not mirror is
    // identifiable without a decode attempt. The whole frame has already been
    // consumed, so skipping it leaves the stream perfectly aligned.
    let Some(&tag) = payload.first() else {
        return Err(FramingError::Bincode(
            "empty frame payload — no variant tag".to_string(),
        ));
    };
    if is_skippable_server_tag(tag) {
        return Ok(WireFrame::Unknown { tag, len: claimed });
    }

    let (msg, consumed) = bincode::serde::decode_from_slice::<ServerMessage, _>(
        &payload,
        bincode::config::standard(),
    )
    .map_err(|e| FramingError::Bincode(e.to_string()))?;
    if consumed != claimed {
        return Err(FramingError::Bincode(format!(
            "decoded {consumed} bytes but frame claimed {claimed} — trailing bytes not allowed"
        )));
    }
    Ok(WireFrame::Message(Box::new(msg)))
}

/// Like `Read::read_exact` but maps `UnexpectedEof` to [`FramingError::UnexpectedEof`]
/// instead of a generic I/O error.
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), FramingError> {
    reader.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FramingError::UnexpectedEof
        } else {
            FramingError::Io(e)
        }
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The bincode discriminant (first byte of the encoded payload) of a client
    /// message. Encodes directly rather than through [`encode`], which refuses the
    /// payload-free placeholders whose tags this file must still pin.
    fn client_tag(msg: &ClientMessage) -> u8 {
        *bincode::serde::encode_to_vec(msg, bincode::config::standard())
            .unwrap()
            .first()
            .expect("encoded ClientMessage must start with a discriminant byte")
    }

    /// The bincode discriminant of a server message.
    fn server_tag(msg: &ServerMessage) -> u8 {
        *bincode::serde::encode_to_vec(msg, bincode::config::standard())
            .unwrap()
            .first()
            .expect("encoded ServerMessage must start with a discriminant byte")
    }

    /// Length-prefix a payload the way herdr frames a message.
    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(payload);
        framed
    }

    /// Frame an arbitrary tag byte plus body, bypassing the enums entirely — how a
    /// herdr frame muxrd does not mirror arrives on the wire.
    fn frame_raw(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut payload = vec![tag];
        payload.extend_from_slice(body);
        frame(&payload)
    }

    /// Frame a [`ServerMessage`] the way herdr would.
    fn frame_server(msg: &ServerMessage) -> Vec<u8> {
        frame(&bincode::serde::encode_to_vec(msg, bincode::config::standard()).unwrap())
    }

    /// Unwrap a decoded frame, failing the test on an unexpected skip.
    fn expect_message(f: WireFrame) -> ServerMessage {
        match f {
            WireFrame::Message(msg) => *msg,
            WireFrame::Unknown { tag, len } => {
                panic!("expected a decoded message, got Unknown {{ tag: {tag}, len: {len} }}")
            }
        }
    }

    // ── Discriminant-order guards ─────────────────────────────────────────────
    //
    // These are the load-bearing wire constants. A round-trip test proves nothing
    // here: the whole defect class is a message that round-trips perfectly against
    // the WRONG tag, which is exactly how protocol 22 broke muxrd. So every tag of
    // both enums is asserted numerically, in herdr v0.9.0's declaration order.

    #[test]
    fn client_message_discriminants_match_protocol_22() {
        assert_eq!(
            client_tag(&ClientMessage::TerminalHello {
                version: HERDR_MIN_PROTOCOL,
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
            }),
            0,
            "TerminalHello must be discriminant 0"
        );
        assert_eq!(
            client_tag(&ClientMessage::Input { data: Vec::new() }),
            1,
            "Input must be 1"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClipboardImage {
                target: ClientClipboardImageTarget::DirectTerminal,
                extension: String::new(),
                data: Vec::new(),
            }),
            2,
            "ClipboardImage must be 2"
        );
        assert_eq!(
            client_tag(&ClientMessage::Resize {
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0,
                pixel_mouse: false,
            }),
            3,
            "Resize must be 3"
        );
        assert_eq!(client_tag(&ClientMessage::Detach), 4, "Detach must be 4");
        assert_eq!(
            client_tag(&ClientMessage::AttachTerminal {
                terminal_id: String::new(),
                takeover: false,
            }),
            5,
            "AttachTerminal must be 5"
        );
        assert_eq!(
            client_tag(&ClientMessage::AttachScroll {
                source: AttachScrollSource::Wheel,
                direction: AttachScrollDirection::Up,
                lines: 1,
                column: None,
                row: None,
                modifiers: 0,
            }),
            6,
            "AttachScroll must be 6"
        );
        assert_eq!(
            client_tag(&ClientMessage::ObserveTerminal {
                target: String::new()
            }),
            7,
            "ObserveTerminal must be 7 (protocol 17's batched-input tag)"
        );
        assert_eq!(
            client_tag(&ClientMessage::ControlTerminal {
                target: String::new(),
                takeover: false,
            }),
            8,
            "ControlTerminal must be 8"
        );
        assert_eq!(
            client_tag(&ClientMessage::GraphicsTransmissionResult {
                transfer_id: 0,
                image_id: 0,
                success: false,
            }),
            9,
            "GraphicsTransmissionResult must be 9"
        );
        assert_eq!(
            client_tag(&ClientMessage::GraphicsTransmissionStarted {
                transfer_id: 0,
                image_id: 0,
            }),
            10,
            "GraphicsTransmissionStarted must be 10"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClientShellHello {
                version: HERDR_MIN_PROTOCOL,
                cell_width_px: 0,
                cell_height_px: 0,
                surface_size: ClientSurfaceSize { cols: 80, rows: 24 },
                pixel_mouse: false,
                direct_graphics: false,
                endpoint_keybindings: false,
                mouse_capture: false,
            }),
            11,
            "ClientShellHello must be 11"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClientShellResize {
                cell_width_px: 0,
                cell_height_px: 0,
                surface_size: ClientSurfaceSize { cols: 80, rows: 24 },
                pixel_mouse: false,
            }),
            12,
            "ClientShellResize must be 12"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClientShellPaneInput),
            13,
            "the ClientShellPaneInput placeholder must hold tag 13"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClientShellPopupInput),
            14,
            "the ClientShellPopupInput placeholder must hold tag 14"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClientShellEndpointRequest {
                boot_id: String::new(),
                request: String::new(),
            }),
            15,
            "ClientShellEndpointRequest must be 15"
        );
        assert_eq!(
            client_tag(&ClientMessage::AttachMouse {
                kind: ClientMouseKind::Down(ClientMouseButton::Left),
                position: ClientMousePosition::Cell { column: 0, row: 0 },
                geometry: None,
                modifiers: 0,
                lines: 1,
            }),
            16,
            "AttachMouse must be 16"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClientShellHostTheme {
                update: ClientHostThemeUpdate::Appearance(ClientHostAppearance::Dark),
            }),
            17,
            "ClientShellHostTheme must be 17"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClientShellFocus { focused: true }),
            18,
            "ClientShellFocus must be 18"
        );
        assert_eq!(
            client_tag(&ClientMessage::ClientShellMouseCapture { enabled: true }),
            19,
            "ClientShellMouseCapture must be 19"
        );
        assert_eq!(
            client_tag(&ClientMessage::EndpointControl {
                kind: String::new(),
                data: String::new(),
            }),
            20,
            "EndpointControl must be 20"
        );
    }

    #[test]
    fn server_message_discriminants_match_protocol_22() {
        assert_eq!(
            server_tag(&ServerMessage::Welcome {
                version: HERDR_MIN_PROTOCOL,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            }),
            0,
            "Welcome must be discriminant 0"
        );
        assert_eq!(
            server_tag(&ServerMessage::Terminal(TerminalFrame {
                seq: 0,
                width: 0,
                height: 0,
                full: false,
                bytes: Vec::new(),
            })),
            1,
            "Terminal must be 1 — herdr 0.8.0 deleted the semantic-frame variant \
             that used to sit here, moving Terminal down from 2"
        );
        assert_eq!(
            server_tag(&ServerMessage::Graphics { bytes: Vec::new() }),
            2,
            "Graphics must be 2"
        );
        assert_eq!(
            server_tag(&ServerMessage::ServerShutdown { reason: None }),
            3,
            "ServerShutdown must be 3"
        );
        assert_eq!(
            server_tag(&ServerMessage::Notify {
                kind: NotifyKind::Sound,
                message: String::new(),
                body: None,
            }),
            4,
            "Notify must be 4"
        );
        assert_eq!(
            server_tag(&ServerMessage::Clipboard {
                data: String::new()
            }),
            5,
            "Clipboard must be 5"
        );
        assert_eq!(
            server_tag(&ServerMessage::WindowTitle { title: None }),
            6,
            "WindowTitle must be 6"
        );
        assert_eq!(
            server_tag(&ServerMessage::ReloadSoundConfig),
            7,
            "ReloadSoundConfig must be 7"
        );
        assert_eq!(
            server_tag(&ServerMessage::MouseCapture {
                enabled: false,
                sgr_pixels: false,
            }),
            8,
            "MouseCapture must be 8"
        );
        assert_eq!(
            server_tag(&ServerMessage::TerminalBell { count: 1 }),
            9,
            "TerminalBell must be 9"
        );
        assert_eq!(
            server_tag(&ServerMessage::GraphicsFile {
                path: String::new(),
                expected_len: 0,
                image_id: 0,
                transfer_id: 0,
                leading: Vec::new(),
                control: String::new(),
                surface_asset: None,
            }),
            10,
            "GraphicsFile must be 10"
        );
        assert_eq!(
            server_tag(&ServerMessage::GraphicsTransmissionRetired {
                transfer_id: 0,
                image_id: 0,
            }),
            11,
            "GraphicsTransmissionRetired must be 11"
        );
        assert_eq!(
            server_tag(&ServerMessage::ClientShellSnapshot),
            12,
            "the ClientShellSnapshot placeholder must hold tag 12"
        );
        assert_eq!(
            server_tag(&ServerMessage::PaneSurface),
            13,
            "the PaneSurface placeholder must hold tag 13"
        );
        assert_eq!(
            server_tag(&ServerMessage::SemanticNotification),
            14,
            "the SemanticNotification placeholder must hold tag 14"
        );
        assert_eq!(
            server_tag(&ServerMessage::ClientShellError {
                message: String::new()
            }),
            15,
            "ClientShellError must be 15"
        );
        assert_eq!(
            server_tag(&ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 0,
                modify_other_keys_level: 0,
            }),
            16,
            "DirectTerminalKeyboardProtocol must be 16"
        );
        assert_eq!(
            server_tag(&ServerMessage::ClientShellKeyboardReportAll { enabled: false }),
            17,
            "ClientShellKeyboardReportAll must be 17"
        );
        assert_eq!(
            server_tag(&ServerMessage::ClientShellEndpointResponseChunk {
                boot_id: String::new(),
                request_id: String::new(),
                final_chunk: false,
                data: Vec::new(),
            }),
            18,
            "ClientShellEndpointResponseChunk must be 18"
        );
        assert_eq!(
            server_tag(&ServerMessage::PaneSurfacePatch),
            19,
            "the PaneSurfacePatch placeholder must hold tag 19"
        );
        assert_eq!(
            server_tag(&ServerMessage::EndpointControl {
                kind: String::new(),
                data: String::new(),
            }),
            20,
            "EndpointControl must be 20"
        );
        assert_eq!(
            HIGHEST_KNOWN_SERVER_TAG, 20,
            "the skip ceiling must be herdr 0.9.0's highest declared server tag"
        );
    }

    // ── Client round-trips: the retyped and re-fielded messages ───────────────

    /// The renamed handshake, including the `pixel_mouse` flag that replaced the
    /// three fields herdr 0.9.0 dropped.
    #[test]
    fn terminal_hello_round_trip() {
        let msg = ClientMessage::TerminalHello {
            version: HERDR_MAX_TESTED_PROTOCOL,
            cols: 120,
            rows: 40,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        };

        let framed = encode(&msg).unwrap();
        assert!(
            framed.len() > 4,
            "frame must be longer than just the length prefix"
        );
        let claimed = u32::from_le_bytes([framed[0], framed[1], framed[2], framed[3]]) as usize;
        assert_eq!(
            claimed,
            framed.len() - 4,
            "length prefix must be consistent"
        );

        let payload = &framed[4..];
        let (decoded, consumed): (ClientMessage, _) =
            bincode::serde::decode_from_slice(payload, bincode::config::standard()).unwrap();
        assert_eq!(
            consumed,
            payload.len(),
            "decoder must consume exactly the payload — herdr rejects trailing bytes"
        );
        assert_eq!(msg, decoded);
    }

    /// `Resize` gained a fifth field; a frame one field short is a decode failure
    /// on herdr's side, not a defaulted value, so the round-trip must carry it.
    #[test]
    fn resize_round_trip_carries_pixel_mouse() {
        let msg = ClientMessage::Resize {
            cols: 200,
            rows: 50,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        };
        let framed = encode(&msg).unwrap();
        let payload = &framed[4..];
        let (decoded, consumed): (ClientMessage, _) =
            bincode::serde::decode_from_slice(payload, bincode::config::standard()).unwrap();
        assert_eq!(consumed, payload.len());
        assert_eq!(msg, decoded);
        assert!(
            matches!(
                decoded,
                ClientMessage::Resize {
                    pixel_mouse: false,
                    ..
                }
            ),
            "muxrd advertises no pixel geometry, so pixel_mouse must survive as false"
        );
    }

    /// The two placeholders hold their tags but must never reach the wire.
    #[test]
    fn encode_refuses_payload_free_placeholders() {
        for msg in [
            ClientMessage::ClientShellPaneInput,
            ClientMessage::ClientShellPopupInput,
        ] {
            let err = encode(&msg).expect_err("a payload-free placeholder must not be encodable");
            assert!(
                matches!(err, FramingError::Bincode(ref m) if m.contains("placeholder")),
                "unexpected error for {msg:?}: {err}"
            );
        }
        // A message muxrd really sends still encodes.
        assert!(encode(&ClientMessage::Detach).is_ok());
    }

    // ── Server round-trips through the reader ─────────────────────────────────

    #[test]
    fn welcome_round_trip() {
        let msg = ServerMessage::Welcome {
            version: HERDR_MAX_TESTED_PROTOCOL,
            encoding: RenderEncoding::TerminalAnsi,
            error: None,
        };
        let framed = frame_server(&msg);
        assert_eq!(
            expect_message(read_server_message(&mut framed.as_slice()).unwrap()),
            msg
        );
    }

    /// The render payload, end to end through `read_server_message`.
    #[test]
    fn terminal_frame_decode() {
        let msg = ServerMessage::Terminal(TerminalFrame {
            seq: 42,
            width: 120,
            height: 40,
            full: true,
            bytes: b"\x1b[2J\x1b[1;1Hhello, herdr".to_vec(),
        });
        let framed = frame_server(&msg);

        let decoded = expect_message(read_server_message(&mut framed.as_slice()).unwrap());
        assert_eq!(msg, decoded);
        if let ServerMessage::Terminal(tf) = decoded {
            assert_eq!(tf.seq, 42);
            assert_eq!(tf.width, 120);
            assert_eq!(tf.height, 40);
            assert!(tf.full);
        } else {
            panic!("expected Terminal variant");
        }
    }

    /// `MouseCapture` gained `sgr_pixels` in herdr 0.8.2. Mirroring only `enabled`
    /// left the decoder one field short of the declared frame length — a hard
    /// decode failure, which is why this round-trip asserts both flags.
    #[test]
    fn mouse_capture_round_trip_carries_sgr_pixels() {
        for (enabled, sgr_pixels) in [(true, true), (true, false), (false, false)] {
            let msg = ServerMessage::MouseCapture {
                enabled,
                sgr_pixels,
            };
            let framed = frame_server(&msg);
            assert_eq!(
                expect_message(read_server_message(&mut framed.as_slice()).unwrap()),
                msg,
                "MouseCapture {{ enabled: {enabled}, sgr_pixels: {sgr_pixels} }} must round-trip"
            );
        }
    }

    // ── Skipping: newer variants and unmirrored payloads ──────────────────────

    /// A variant appended by a NEWER herdr (tag above the ceiling) must be reported
    /// as skippable, not as a decode error — treating it as an error is what
    /// silently killed the terminal relay.
    #[test]
    fn server_tag_above_the_ceiling_is_skipped_not_an_error() {
        let framed = frame_raw(HIGHEST_KNOWN_SERVER_TAG + 1, &[0xAA, 0xBB]);
        match read_server_message(&mut framed.as_slice()).unwrap() {
            WireFrame::Unknown { tag, len } => {
                assert_eq!(tag, HIGHEST_KNOWN_SERVER_TAG + 1);
                assert_eq!(len, 3, "tag byte + 2 body bytes");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// A tag herdr 0.9.0 declares but muxrd does not mirror is skipped the same
    /// way — the payload is never handed to bincode, so a client-owned shell frame
    /// cannot fail the decode and end the relay.
    #[test]
    fn unmirrored_server_tags_are_skipped_not_decoded() {
        for tag in UNMIRRORED_SERVER_TAGS {
            assert!(
                tag <= HIGHEST_KNOWN_SERVER_TAG,
                "an unmirrored tag must be one herdr 0.9.0 declares"
            );
            // A body that would decode into nothing sane — proving the payload is
            // never given to the decoder.
            let framed = frame_raw(tag, &[0xFF; 8]);
            match read_server_message(&mut framed.as_slice()).unwrap() {
                WireFrame::Unknown { tag: got, len } => {
                    assert_eq!(got, tag);
                    assert_eq!(len, 9, "tag byte + 8 body bytes");
                }
                other => panic!("expected Unknown for tag {tag}, got {other:?}"),
            }
        }
    }

    /// Skipping must consume the frame exactly, leaving the next frame decodable —
    /// otherwise the relay desynchronises. Covers both skip reasons in one stream.
    #[test]
    fn stream_stays_aligned_after_skipping() {
        let known = ServerMessage::Clipboard {
            data: "after".to_string(),
        };

        let mut stream = frame_raw(HIGHEST_KNOWN_SERVER_TAG + 5, &[1, 2, 3, 4, 5]);
        stream.extend_from_slice(&frame_raw(UNMIRRORED_SERVER_TAGS[0], &[9; 32]));
        stream.extend_from_slice(&frame_server(&known));

        let mut cursor = stream.as_slice();
        assert!(matches!(
            read_server_message(&mut cursor).unwrap(),
            WireFrame::Unknown { .. }
        ));
        assert!(matches!(
            read_server_message(&mut cursor).unwrap(),
            WireFrame::Unknown { .. }
        ));
        assert_eq!(
            expect_message(read_server_message(&mut cursor).unwrap()),
            known,
            "the frame following two skipped ones must still decode"
        );
    }
}
