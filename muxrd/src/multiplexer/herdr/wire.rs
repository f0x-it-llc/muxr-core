//! Independently-authored types matching herdr's public v0.7.1 wire/JSON protocol for interop.
//! Not derived from herdr's AGPL source; herdr runs as a separate, unmodified, user-installed
//! binary driven over its public sockets.
//!
//! # Binary wire protocol — terminal relay socket
//!
//! herdr uses bincode 2 (`bincode::config::standard()`) with a 4-byte little-endian length
//! prefix for every frame.  The enum discriminants are positional (0-based declaration order) and
//! MUST match herdr's exact `ClientMessage`/`ServerMessage` declaration order or the server will
//! reject / misinterpret messages.
//!
//! # Version handling — discovered, never pinned
//!
//! herdr enforces **strict equality** on the `Hello`/`Welcome` protocol version: it rejects a
//! client that is older *or* newer than itself.  muxrd therefore discovers the server's version
//! over the stable JSON-API socket (`HerdrControl::ping`) and echoes it back, rather than
//! compiling one in — a pinned constant is guaranteed to break on some herdr release, and did
//! (muxrd targeted 14 while herdr shipped 16 in v0.7.2 and 17 in v0.7.5, breaking every attach).
//!
//! herdr has so far only **appended** variants (`ObserveTerminal`/`ControlTerminal` to
//! `ClientMessage`, `PrefixInputSource` to `ServerMessage`, all in v0.7.2), so the tags below stay
//! valid across 14–17.  A frame carrying a tag beyond [`HIGHEST_KNOWN_SERVER_TAG`] is skipped via
//! its length prefix ([`WireFrame::Unknown`]) instead of being treated as stream corruption.
//!
//! The tag values below were verified against herdr's own wire-tag ordering test:
//!
//! | Variant | Tag |
//! |---|---|
//! | `ClientMessage::Hello` | 0 |
//! | `ClientMessage::Input` | 1 |
//! | `ClientMessage::ClipboardImage` | 2 |
//! | `ClientMessage::Resize` | 3 |
//! | `ClientMessage::Detach` | 4 |
//! | `ClientMessage::AttachTerminal` | 5 |
//! | `ClientMessage::AttachScroll` | 6 |
//! | `ClientMessage::InputEvents` | 7 |
//!
//! | Variant | Tag |
//! |---|---|
//! | `ServerMessage::Welcome` | 0 |
//! | `ServerMessage::Frame` | 1 |
//! | `ServerMessage::Terminal` | 2 |
//! | `ServerMessage::Graphics` | 3 |
//! | `ServerMessage::ServerShutdown` | 4 |
//! | `ServerMessage::Notify` | 5 |
//! | `ServerMessage::Clipboard` | 6 |
//! | `ServerMessage::WindowTitle` | 7 |
//! | `ServerMessage::ReloadSoundConfig` | 8 |
//! | `ServerMessage::MouseCapture` | 9 |

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

// ─── Protocol constants ───────────────────────────────────────────────────────

/// Lowest herdr wire protocol version muxrd knows how to speak (herdr v0.7.1).
///
/// muxrd **does not pin** a protocol version: herdr enforces strict equality on the
/// `Hello`/`Welcome` handshake and rejects clients that are older *or* newer than
/// itself, so any compiled-in constant breaks on some herdr release.  The version
/// actually sent is discovered per connection via the JSON-API `ping`
/// (`HerdrControl::ping`).  This floor exists only to catch an implausible
/// downgrade below the layout this module was written against.
pub const HERDR_MIN_PROTOCOL: u32 = 14;

/// Highest herdr wire protocol version muxrd has been **tested** against (v0.7.5).
///
/// This is a diagnostic tripwire, **not a gate** — muxrd still attaches to a server
/// reporting a higher protocol, it just logs a prominent warning first.  herdr's
/// changes from 14 → 17 were purely additive (variants appended to
/// `ClientMessage`/`ServerMessage`, none removed or reordered), so the layout below
/// stayed valid; the warning is what tells us to re-verify if that ever stops
/// being true.  Bump this after testing a newer herdr.
///
/// **Paired with the dev rig's pin:** `docker/Dockerfile`'s `ARG HERDR_VERSION`
/// (and compose's `${HERDR_VERSION:-…}` fallbacks) name the release that ships
/// this protocol — 0.7.5. Move both in the same commit, or the rig stops testing
/// what this constant claims.
pub const HERDR_MAX_TESTED_PROTOCOL: u32 = 17;

/// Highest `ServerMessage` bincode tag this module can decode ([`ServerMessage`]
/// declares tags 0..=9).  A frame carrying a higher tag is a variant herdr appended
/// in a newer release; it is skipped rather than treated as a corrupt stream.
const HIGHEST_KNOWN_SERVER_TAG: u8 = 9;

/// Maximum frame payload we accept from the server (2 MiB, matching herdr's
/// `MAX_FRAME_SIZE`).  Frames larger than this are rejected before allocation.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

// ─── Enums shared between client and server ───────────────────────────────────

/// Render encoding negotiated during the `Hello`/`Welcome` handshake.
/// We always request `TerminalAnsi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderEncoding {
    /// Full semantic `FrameData` values (local/default in herdr TUI clients).
    SemanticFrame,
    /// Pre-diffed terminal ANSI byte streams — what muxrd requests.
    TerminalAnsi,
}

/// Keybinding profile the client requests at connection time.
/// We always send `Server`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKeybindings {
    /// Use the server's own keybinding config.
    Server,
    /// Use the client's own normalized `[keys]` TOML section.
    Local { keys_toml: String },
}

/// Client behaviour mode requested at connection time.
/// We always use `TerminalAttach`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientLaunchMode {
    /// Full app client (renders full herdr UI).
    App,
    /// Direct terminal-attach client — muxrd uses this to relay a single pane.
    TerminalAttach,
}

// ─── ClientMessage supporting types ──────────────────────────────────────────

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

/// Key event kind (press / repeat / release) for structured input events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKeyKind {
    Press,
    Repeat,
    Release,
}

/// Key code for structured input events from platform clients.
///
/// Declaration order matches herdr's `ClientKeyCode` exactly; the bincode
/// discriminant of each variant must be preserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Esc,
    Char(char),
    F(u8),
    Null,
}

/// Mouse button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMouseButton {
    Left,
    Right,
    Middle,
}

/// Mouse event kind.  Declaration order matches herdr's `ClientMouseKind`.
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

/// Structured input event from a platform client that does not expose
/// Unix-style raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientInputEvent {
    Key {
        code: ClientKeyCode,
        /// Crossterm-compatible modifier bitmask.
        modifiers: u8,
        kind: ClientKeyKind,
    },
    Mouse {
        kind: ClientMouseKind,
        column: u16,
        row: u16,
        /// Crossterm-compatible modifier bitmask.
        modifiers: u8,
    },
    Paste {
        text: String,
    },
    FocusGained,
    FocusLost,
}

// ─── ClientMessage ────────────────────────────────────────────────────────────

/// Messages sent from muxrd → herdr over the wire relay socket.
///
/// **Declaration order is wire-critical.** bincode `standard()` encodes the
/// variant index sequentially from 0; any reordering breaks the protocol.
/// Protocol-14 tag assignments are listed in this file's module-level doc
/// and confirmed via herdr's upstream wire-tag test.
///
/// Variants we do not send (`ClipboardImage`, `AttachScroll`, `InputEvents`)
/// are included so the discriminants of later variants remain correct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Handshake — must be the first message on a new connection.  Tag = 0.
    Hello {
        version: u32,
        cols: u16,
        rows: u16,
        /// Physical pixel width of a terminal cell (0 = Kitty graphics disabled).
        cell_width_px: u32,
        /// Physical pixel height of a terminal cell (0 = Kitty graphics disabled).
        cell_height_px: u32,
        requested_encoding: RenderEncoding,
        keybindings: ClientKeybindings,
        launch_mode: ClientLaunchMode,
    },

    /// Raw input bytes from the client's stdin.  Tag = 1.
    Input { data: Vec<u8> },

    /// Image bytes for remote clipboard paste bridging.  Tag = 2.
    /// muxrd does not send this variant; it is present only to keep tags aligned.
    ClipboardImage { extension: String, data: Vec<u8> },

    /// Terminal resize notification.  Tag = 3.
    Resize {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    },

    /// Graceful disconnect.  Tag = 4.
    Detach,

    /// Switch this connection into direct terminal-attach mode.  Tag = 5.
    AttachTerminal {
        /// herdr `terminal_id` — the attach key returned by the JSON-API.
        terminal_id: String,
        /// Replace an existing writable owner for this terminal.
        takeover: bool,
    },

    /// Scrollback scroll in direct-attach mode.  Tag = 6.
    /// Sent by the relay's `send_mouse` (mouse-input capability): wheel events
    /// scroll the attached pane's scrollback, or reach a mouse-capturing app
    /// via the wheel source + position.
    AttachScroll {
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        /// Crossterm-compatible modifier bitmask.
        modifiers: u8,
    },

    /// Structured input events from platform clients.  Tag = 7.
    /// muxrd uses raw `Input` bytes instead; this variant maintains tag alignment.
    InputEvents { events: Vec<ClientInputEvent> },
}

// ─── ServerMessage supporting types ──────────────────────────────────────────

/// A single rendered cell in a semantic `FrameData`.
///
/// Included to keep `ServerMessage::Frame` (tag = 1) bincode-correct.  We
/// never consume `Frame` at runtime — only `Terminal` (tag = 2) is rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellData {
    /// Grapheme cluster displayed in this cell.
    pub symbol: String,
    /// Foreground colour as a packed u32 (herdr's `color_to_u32` encoding).
    pub fg: u32,
    /// Background colour as a packed u32.
    pub bg: u32,
    /// Style modifier bitmask (bold, italic, …).
    pub modifier: u16,
    /// Whether this cell should be skipped during diff-based rendering.
    pub skip: bool,
    /// Index into `FrameData::hyperlinks`, if any.
    pub hyperlink: Option<u32>,
}

/// Cursor position and shape within a rendered frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    /// Column (0-based).
    pub x: u16,
    /// Row (0-based).
    pub y: u16,
    pub visible: bool,
    /// DECSCUSR shape parameter (0 = terminal default).
    #[serde(default)]
    pub shape: u8,
}

/// Semantic rendered frame (herdr `SemanticFrame` encoding).
///
/// Included to keep `ServerMessage::Frame` (discriminant 1) structurally
/// correct even though we negotiate `TerminalAnsi` and only receive `Terminal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameData {
    /// Cells in row-major order; length = `width * height`.
    pub cells: Vec<CellData>,
    pub width: u16,
    pub height: u16,
    pub cursor: Option<CursorState>,
    /// OSC 8 hyperlink URIs referenced by cells.
    pub hyperlinks: Vec<String>,
    /// Kitty graphics protocol bytes to apply after the text frame.
    pub graphics: Vec<u8>,
}

/// Terminal ANSI bytes delivered by the server for `TerminalAnsi` clients.
/// This is the primary render payload consumed by muxrd.
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

// ─── ServerMessage ────────────────────────────────────────────────────────────

/// Messages sent from herdr → muxrd over the wire relay socket.
///
/// **Declaration order is wire-critical** — discriminants must match herdr's
/// `ServerMessage` exactly.  See this file's module-level doc for the full
/// tag table (Welcome=0 … MouseCapture=9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Handshake response.  Tag = 0.
    Welcome {
        version: u32,
        encoding: RenderEncoding,
        /// If `Some`, the handshake failed; muxrd must surface this error and close.
        error: Option<String>,
    },

    /// Semantic rendered frame (only when encoding is `SemanticFrame`).  Tag = 1.
    Frame(FrameData),

    /// Terminal ANSI bytes (only when encoding is `TerminalAnsi`).  Tag = 2.
    Terminal(TerminalFrame),

    /// Client-local Kitty graphics bytes.  Tag = 3.
    Graphics { bytes: Vec<u8> },

    /// Server shutting down; muxrd should close the connection.  Tag = 4.
    ServerShutdown { reason: Option<String> },

    /// Notification event (sound / toast).  Tag = 5.
    Notify {
        kind: NotifyKind,
        message: String,
        body: Option<String>,
    },

    /// OSC 52 clipboard data forwarded from a PTY.  Tag = 6.
    Clipboard { data: String },

    /// Set the client's outer terminal window title.  Tag = 7.
    WindowTitle { title: Option<String> },

    /// Reload client-local sound config.  Tag = 8.
    ReloadSoundConfig,

    /// Whether the client should capture host mouse input.  Tag = 9.
    MouseCapture { enabled: bool },
}

// ─── Read results ─────────────────────────────────────────────────────────────

/// One frame read off the wire.
///
/// herdr **appends** variants to `ServerMessage` between releases (`PrefixInputSource`
/// arrived in v0.7.2).  Because every frame carries a 4-byte little-endian length
/// prefix, a frame whose tag we do not know can be consumed and discarded without
/// desynchronising the stream — so an unrecognised variant is reported as
/// [`WireFrame::Unknown`] rather than as a decode error.  Treating it as an error
/// would silently kill the terminal relay on any future additive herdr change.
#[derive(Debug)]
pub enum WireFrame {
    /// A frame decoded into a variant this module knows.
    Message(Box<ServerMessage>),
    /// A well-framed message carrying a tag from a newer herdr; safely skipped.
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
pub fn encode(msg: &ClientMessage) -> Result<Vec<u8>, FramingError> {
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
/// `bincode::config::standard()`.  Returns [`FramingError::UnexpectedEof`] on
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
    // (0..=250) is a single leading byte — so an appended variant from a newer
    // herdr is identifiable without a decode attempt. The whole frame has already
    // been consumed, so skipping it leaves the stream perfectly aligned.
    let Some(&tag) = payload.first() else {
        return Err(FramingError::Bincode(
            "empty frame payload — no variant tag".to_string(),
        ));
    };
    if tag > HIGHEST_KNOWN_SERVER_TAG {
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

    // ── Discriminant-order guard ──────────────────────────────────────────────

    /// Verify that the bincode discriminant (first byte of encoded message)
    /// matches herdr's protocol-14 tag table.  These values are load-bearing
    /// wire constants; any regression here means the server will reject or
    /// misinterpret our messages.
    #[test]
    fn client_message_discriminants_match_protocol_14() {
        fn tag(msg: &ClientMessage) -> u8 {
            *bincode::serde::encode_to_vec(msg, bincode::config::standard())
                .unwrap()
                .first()
                .expect("encoded ClientMessage must start with a discriminant byte")
        }

        assert_eq!(
            tag(&ClientMessage::Hello {
                version: HERDR_MIN_PROTOCOL,
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0,
                requested_encoding: RenderEncoding::TerminalAnsi,
                keybindings: ClientKeybindings::Server,
                launch_mode: ClientLaunchMode::TerminalAttach,
            }),
            0,
            "Hello must be discriminant 0"
        );
        assert_eq!(
            tag(&ClientMessage::Input { data: Vec::new() }),
            1,
            "Input must be 1"
        );
        assert_eq!(
            tag(&ClientMessage::ClipboardImage {
                extension: String::new(),
                data: Vec::new()
            }),
            2,
            "ClipboardImage must be 2"
        );
        assert_eq!(
            tag(&ClientMessage::Resize {
                cols: 80,
                rows: 24,
                cell_width_px: 0,
                cell_height_px: 0
            }),
            3,
            "Resize must be 3"
        );
        assert_eq!(tag(&ClientMessage::Detach), 4, "Detach must be 4");
        assert_eq!(
            tag(&ClientMessage::AttachTerminal {
                terminal_id: String::new(),
                takeover: false,
            }),
            5,
            "AttachTerminal must be 5"
        );
        assert_eq!(
            tag(&ClientMessage::AttachScroll {
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
            tag(&ClientMessage::InputEvents { events: Vec::new() }),
            7,
            "InputEvents must be 7"
        );
    }

    #[test]
    fn server_message_discriminants_match_protocol_14() {
        fn tag(msg: &ServerMessage) -> u8 {
            *bincode::serde::encode_to_vec(msg, bincode::config::standard())
                .unwrap()
                .first()
                .expect("encoded ServerMessage must start with a discriminant byte")
        }

        assert_eq!(
            tag(&ServerMessage::Welcome {
                version: HERDR_MIN_PROTOCOL,
                encoding: RenderEncoding::TerminalAnsi,
                error: None,
            }),
            0,
            "Welcome must be discriminant 0"
        );
        assert_eq!(
            tag(&ServerMessage::Frame(FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            })),
            1,
            "Frame must be 1"
        );
        assert_eq!(
            tag(&ServerMessage::Terminal(TerminalFrame {
                seq: 0,
                width: 0,
                height: 0,
                full: false,
                bytes: Vec::new(),
            })),
            2,
            "Terminal must be 2"
        );
        assert_eq!(
            tag(&ServerMessage::Graphics { bytes: Vec::new() }),
            3,
            "Graphics must be 3"
        );
        assert_eq!(
            tag(&ServerMessage::ServerShutdown { reason: None }),
            4,
            "ServerShutdown must be 4"
        );
        assert_eq!(
            tag(&ServerMessage::Notify {
                kind: NotifyKind::Sound,
                message: String::new(),
                body: None,
            }),
            5,
            "Notify must be 5"
        );
        assert_eq!(
            tag(&ServerMessage::Clipboard {
                data: String::new()
            }),
            6,
            "Clipboard must be 6"
        );
        assert_eq!(
            tag(&ServerMessage::WindowTitle { title: None }),
            7,
            "WindowTitle must be 7"
        );
        assert_eq!(
            tag(&ServerMessage::ReloadSoundConfig),
            8,
            "ReloadSoundConfig must be 8"
        );
        assert_eq!(
            tag(&ServerMessage::MouseCapture { enabled: false }),
            9,
            "MouseCapture must be 9"
        );
    }

    // ── Hello round-trip ─────────────────────────────────────────────────────

    /// Encode a `Hello` ClientMessage, frame it, decode the payload back —
    /// verifies encode + bincode round-trip.
    #[test]
    fn hello_round_trip() {
        let msg = ClientMessage::Hello {
            version: HERDR_MIN_PROTOCOL,
            cols: 120,
            rows: 40,
            cell_width_px: 0,
            cell_height_px: 0,
            requested_encoding: RenderEncoding::TerminalAnsi,
            keybindings: ClientKeybindings::Server,
            launch_mode: ClientLaunchMode::TerminalAttach,
        };

        let frame = encode(&msg).unwrap();
        assert!(
            frame.len() > 4,
            "frame must be longer than just the length prefix"
        );

        // Verify the 4-byte LE length prefix is consistent.
        let claimed = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(claimed, frame.len() - 4);

        // Decode the payload back to a ClientMessage.
        let payload = &frame[4..];
        let (decoded, consumed): (ClientMessage, _) =
            bincode::serde::decode_from_slice(payload, bincode::config::standard()).unwrap();
        assert_eq!(
            consumed,
            payload.len(),
            "decoder must consume exactly the payload"
        );
        assert_eq!(msg, decoded);
    }

    // ── TerminalFrame decode ──────────────────────────────────────────────────

    /// Encode a `ServerMessage::Terminal`, frame it, then decode via
    /// `read_server_message` — confirms the reader path end-to-end.
    #[test]
    fn terminal_frame_decode() {
        let frame = ServerMessage::Terminal(TerminalFrame {
            seq: 42,
            width: 120,
            height: 40,
            full: true,
            bytes: b"\x1b[2J\x1b[1;1Hhello, herdr".to_vec(),
        });

        // Manually build the framed bytes (the same as `encode` does for ClientMessage).
        let payload = bincode::serde::encode_to_vec(&frame, bincode::config::standard()).unwrap();
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);

        let decoded = expect_message(read_server_message(&mut framed.as_slice()).unwrap());
        assert_eq!(frame, decoded);

        if let ServerMessage::Terminal(tf) = decoded {
            assert_eq!(tf.seq, 42);
            assert_eq!(tf.width, 120);
            assert_eq!(tf.height, 40);
            assert!(tf.full);
        } else {
            panic!("expected Terminal variant, got {decoded:?}");
        }
    }

    // ── Welcome decode ────────────────────────────────────────────────────────

    #[test]
    fn welcome_round_trip() {
        let msg = ServerMessage::Welcome {
            version: HERDR_MIN_PROTOCOL,
            encoding: RenderEncoding::TerminalAnsi,
            error: None,
        };
        let payload = bincode::serde::encode_to_vec(&msg, bincode::config::standard()).unwrap();
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);

        let decoded = expect_message(read_server_message(&mut framed.as_slice()).unwrap());
        assert_eq!(msg, decoded);
    }

    // ── Forward compatibility with newer herdr releases ───────────────────────

    /// Frame an arbitrary tag byte plus payload the way herdr frames a message.
    fn frame_raw(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut payload = vec![tag];
        payload.extend_from_slice(body);
        let mut framed = Vec::with_capacity(4 + payload.len());
        framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        framed.extend_from_slice(&payload);
        framed
    }

    /// A variant appended by a newer herdr (e.g. `PrefixInputSource`, added in
    /// v0.7.2 at tag 10) must be reported as skippable — NOT as a decode error.
    /// Treating it as an error is what silently killed the terminal relay.
    #[test]
    fn unknown_server_tag_is_skipped_not_an_error() {
        let framed = frame_raw(HIGHEST_KNOWN_SERVER_TAG + 1, &[0xAA, 0xBB]);
        match read_server_message(&mut framed.as_slice()).unwrap() {
            WireFrame::Unknown { tag, len } => {
                assert_eq!(tag, HIGHEST_KNOWN_SERVER_TAG + 1);
                assert_eq!(len, 3, "tag byte + 2 body bytes");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// Skipping an unknown frame must consume it exactly, leaving the next frame
    /// on the stream decodable — otherwise the relay desynchronises.
    #[test]
    fn stream_stays_aligned_after_skipping_unknown_frame() {
        let known = ServerMessage::Clipboard {
            data: "after".to_string(),
        };
        let known_payload =
            bincode::serde::encode_to_vec(&known, bincode::config::standard()).unwrap();

        let mut stream = frame_raw(HIGHEST_KNOWN_SERVER_TAG + 5, &[1, 2, 3, 4, 5]);
        stream.extend_from_slice(&(known_payload.len() as u32).to_le_bytes());
        stream.extend_from_slice(&known_payload);

        let mut cursor = stream.as_slice();
        assert!(matches!(
            read_server_message(&mut cursor).unwrap(),
            WireFrame::Unknown { .. }
        ));
        assert_eq!(
            expect_message(read_server_message(&mut cursor).unwrap()),
            known,
            "the frame following an unknown one must still decode"
        );
    }

    /// Unwrap a decoded frame, failing the test on an unexpected skip.
    fn expect_message(frame: WireFrame) -> ServerMessage {
        match frame {
            WireFrame::Message(msg) => *msg,
            WireFrame::Unknown { tag, len } => {
                panic!("expected a decoded message, got Unknown {{ tag: {tag}, len: {len} }}")
            }
        }
    }
}
