//! Control-mode client: the `tmux -C` process, framing and notifications.
//!
//! [`ControlClient`] runs `tmux -L <name>|-S <path> -C attach-session -t =<session>` as
//! a child process, writes commands to its stdin and reads its stdout on a
//! dedicated reader thread. Every method blocks the calling thread for at most
//! the timeout it is given; the watcher owns one client per session and drives
//! it from its own thread.
//!
//! ## `-C`, not `-CC`
//! `-CC` is the variant meant for a terminal emulator: it puts the tty into raw
//! mode and wraps the whole stream in a DCS sequence. A client that talks to
//! tmux over pipes wants neither, so this one uses a single `-C` and reads the
//! protocol as plain `\n`-terminated lines.
//!
//! ## Framing and correlation
//! tmux answers every command it reads from the client with a block
//! `%begin <time> <number> <flags>` ... `%end|%error <time> <number> <flags>`
//! whose body is the command's output. Notifications (`%output`,
//! `%window-add`, ...) arrive between blocks, never inside one (tmux.1 CONTROL
//! MODE), so while a command waits for its reply the notifications that arrive
//! are queued, not dropped. A block whose flags have bit 0 set came from a line
//! this client wrote; flag 0 blocks (the command-line `attach-session`, hooks)
//! are not replies and are discarded, except that an unsolicited `%error` body
//! is kept to explain a following `%exit` (an unknown session, for example).
//!
//! One written line does not always produce one block: a comment-only line
//! produces none, and `a ; b` produces two. [`ControlClient::command`]
//! therefore follows every line with `display-message -p <token>`, where the
//! token carries a per-client random nonce and the call number, and collects
//! the blocks up to that sentinel: `ok` is false when any of them ended in
//! `%error`, and `lines` concatenates their bodies. A call that timed out is
//! skipped cleanly by the next one, because its sentinel names an older call.
//! An empty line makes tmux detach a control client, so a blank command is
//! never written: only its sentinel is.
//!
//! ## One control client per session
//! A control client only receives `%output` for panes in windows of its own
//! attached session (tmux 3.8 `control.c:627`), so watching every session takes
//! one client per session; this type attaches to exactly one and never
//! switches.
//!
//! ## Liveness and shutdown
//! A reader error, end of output or `%exit` marks the client dead: queued
//! notifications (the `%exit` itself included) can still be drained, after
//! which [`ControlClient::next_notification`] errors, and [`ControlClient::command`]
//! errors at once. The client never reconnects; the caller decides whether to
//! spawn a new one.
//!
//! Writes never block the caller: a writer thread owns tmux's stdin and drains
//! a bounded outbox ([`MAX_OUTBOX_BYTES`]). A command waits for outbox space
//! only until its own deadline, and an outbox that stays full (tmux no longer
//! reading) marks the client dead. Dropping the client queues `detach-client`
//! if there is room, closes the outbox (the writer then closes stdin), waits up
//! to one second for tmux to exit and then kills the child; it never waits on
//! the pipe.
//!
//! ## Not for pane-controlled text
//! The block framing has no escaping: a body line that repeats the block's own
//! `%end`/`%error` guard closes it early, and the rest of the body is then
//! parsed as notifications. Never use [`ControlClient::command`] to read text
//! a pane controls (`capture-pane`, titles, user options over several lines);
//! run those through the tmux command line instead.
//!
//! ## Bounded memory
//! At most [`DEFAULT_QUEUE_CAP`] notifications and [`MAX_QUEUE_BYTES`] of them
//! (payload plus per-item overhead) are queued; past either limit the oldest
//! pane output is dropped first (then the oldest notification of any kind) and
//! counted in [`ControlClient::dropped_notifications`]. A line longer than
//! [`MAX_LINE_BYTES`] is discarded whole and counted in
//! [`ControlClient::oversized_lines`], a reply body is capped at
//! [`MAX_REPLY_BYTES`] (each line also costs its `String` overhead), and a
//! command line at [`MAX_COMMAND_BYTES`], so a pane that floods output cannot
//! grow muxrd without limit.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::paths::{TmuxSocket, tmux_bin};

/// Default cap on queued notifications.
pub const DEFAULT_QUEUE_CAP: usize = 4096;
/// Longest protocol line kept; longer lines are discarded and counted.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Cap on the total body bytes of one reply, counting [`LINE_OVERHEAD`] per
/// line; further lines are dropped.
pub const MAX_REPLY_BYTES: usize = 16 * 1024 * 1024;
/// Cap on the queued notifications' bytes (payload plus per-item overhead).
pub const MAX_QUEUE_BYTES: usize = 16 * 1024 * 1024;
/// Longest command line [`ControlClient::command`] accepts.
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
/// Cap on bytes written but not yet taken by tmux's stdin.
pub const MAX_OUTBOX_BYTES: usize = 256 * 1024;
/// What one stored reply line costs beyond its text.
const LINE_OVERHEAD: usize = std::mem::size_of::<String>();
/// How long [`ControlClient::spawn`] waits for the attach to be confirmed.
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout used by the convenience wrappers ([`ControlClient::resume_pane`] and
/// friends).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Grace period between closing stdin and killing the child on drop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
/// How long the writer, after a failed write, waits for the reader to record
/// why tmux went away.
const DEATH_SETTLE: Duration = Duration::from_millis(250);
/// Bytes of the child's stderr kept for error messages.
const STDERR_KEEP: usize = 4096;
/// Prefix of the sentinel line that closes every [`ControlClient::command`].
const SYNC_PREFIX: &str = "muxr-sync-";

/// A control-client flag set with `refresh-client -f`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientFlag {
    /// The client's size does not constrain the session's windows.
    IgnoreSize,
    /// No `%output` (or `%extended-output`) at all.
    NoOutput,
    /// Pause a pane whose output is this many seconds behind; output is then
    /// delivered as `%extended-output` only.
    PauseAfter(u32),
    /// The client may only run read-only commands.
    ReadOnly,
}

impl ClientFlag {
    /// The flag as `refresh-client -f` spells it.
    pub fn as_arg(&self) -> String {
        match self {
            Self::IgnoreSize => "ignore-size".to_owned(),
            Self::NoOutput => "no-output".to_owned(),
            Self::PauseAfter(secs) => format!("pause-after={secs}"),
            Self::ReadOnly => "read-only".to_owned(),
        }
    }
}

/// The answer to one [`ControlClient::command`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandReply {
    /// False when tmux closed a block of this command with `%error`.
    pub ok: bool,
    /// The block body (every block's body, in order, for a multi-command line).
    pub lines: Vec<String>,
}

/// A tmux control-mode notification. Ids drop their sigil: `%12` is pane 12,
/// `@3` window 3, `$0` session 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// `%output %<pane> <data>`: pane output, already unescaped.
    Output { pane: u32, bytes: Vec<u8> },
    /// `%extended-output %<pane> <age> : <data>`: pane output with the
    /// milliseconds it spent buffered. A client with `pause-after` set gets
    /// all pane output in this form and none as `%output`.
    ExtendedOutput {
        pane: u32,
        age_ms: u64,
        bytes: Vec<u8>,
    },
    /// `%pause %<pane>`: output for the pane stopped until it is resumed.
    Pause { pane: u32 },
    /// `%continue %<pane>`: a paused pane resumed.
    Continue { pane: u32 },
    /// `%layout-change @<window> ...`.
    LayoutChange { window: u32 },
    /// `%window-add @<window>`.
    WindowAdd { window: u32 },
    /// `%window-close @<window>`.
    WindowClose { window: u32 },
    /// `%window-renamed @<window> <name>`.
    WindowRenamed { window: u32, name: String },
    /// `%window-pane-changed @<window> %<pane>`: the active pane changed.
    WindowPaneChanged { window: u32, pane: u32 },
    /// `%session-changed $<session> <name>`: this client's session.
    SessionChanged { session: u32, name: String },
    /// `%session-renamed [$<session>] <name>`: this client's session.
    SessionRenamed { name: String },
    /// `%sessions-changed`: a session was created or destroyed.
    SessionsChanged,
    /// `%session-window-changed $<session> @<window>`.
    SessionWindowChanged { session: u32, window: u32 },
    /// `%pane-mode-changed %<pane>`.
    PaneModeChanged { pane: u32 },
    /// `%subscription-changed <name> $s @w <index> %p ... : <value>`; `pane`
    /// is `None` for a session or window subscription (`-`).
    SubscriptionChanged {
        name: String,
        pane: Option<u32>,
        value: String,
    },
    /// `%client-detached <client>`: some client of the server detached (this
    /// client's own detach ends the stream with [`Notification::Exit`]).
    ClientDetached,
    /// `%exit [reason]`: tmux is closing this client.
    Exit { reason: Option<String> },
    /// `%message <text>`.
    Message(String),
    /// Any other line, kept raw (`%unlinked-window-*`, `%paste-buffer-*`,
    /// `%config-error`, `%client-session-changed`, malformed lines).
    Unknown(String),
}

impl Notification {
    fn is_output(&self) -> bool {
        matches!(self, Self::Output { .. } | Self::ExtendedOutput { .. })
    }

    /// Bytes this notification holds while queued, overhead included.
    fn cost(&self) -> usize {
        let payload = match self {
            Self::Output { bytes, .. } | Self::ExtendedOutput { bytes, .. } => bytes.len(),
            Self::WindowRenamed { name, .. }
            | Self::SessionChanged { name, .. }
            | Self::SessionRenamed { name } => name.len(),
            Self::SubscriptionChanged { name, value, .. } => name.len() + value.len(),
            Self::Exit { reason } => reason.as_ref().map_or(0, String::len),
            Self::Message(text) | Self::Unknown(text) => text.len(),
            Self::Pause { .. }
            | Self::Continue { .. }
            | Self::LayoutChange { .. }
            | Self::WindowAdd { .. }
            | Self::WindowClose { .. }
            | Self::WindowPaneChanged { .. }
            | Self::SessionsChanged
            | Self::SessionWindowChanged { .. }
            | Self::PaneModeChanged { .. }
            | Self::ClientDetached => 0,
        };
        std::mem::size_of::<Self>() + payload
    }
}

/// Decode a `%output` value: `\ooo` (exactly three octal digits, at most
/// `\377`) becomes that byte, every other byte passes through, and a backslash
/// that does not start such an escape is kept literally.
pub fn unescape_output(s: &str) -> Vec<u8> {
    unescape_bytes(s.as_bytes())
}

fn unescape_bytes(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\'
            && let Some(value) = octal_escape(&s[i + 1..])
        {
            out.push(value);
            i += 4;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

fn octal_escape(digits: &[u8]) -> Option<u8> {
    let digits = digits.get(..3)?;
    let mut value: u32 = 0;
    for &d in digits {
        if !(b'0'..=b'7').contains(&d) {
            return None;
        }
        value = value * 8 + u32::from(d - b'0');
    }
    u8::try_from(value).ok()
}

// ─── Line parsing ────────────────────────────────────────────────────────────

/// Split at the first space: `(word, rest)`, `rest` empty when there is none.
fn split_word(s: &[u8]) -> (&[u8], &[u8]) {
    match s.iter().position(|&b| b == b' ') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, &[]),
    }
}

fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// `%12` / `@3` / `$0` with the given sigil.
fn parse_id(s: &[u8], sigil: u8) -> Option<u32> {
    let (&first, digits) = s.split_first()?;
    if first != sigil {
        return None;
    }
    u32::try_from(parse_u64(digits)?).ok()
}

fn lossy(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

/// Locate the ` : ` (or leading `: `) that separates fields from a value;
/// returns `(fields, value)`.
fn split_value(s: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(value) = s.strip_prefix(b": ") {
        return Some((&[], value));
    }
    if s == b":" {
        return Some((&[], &[]));
    }
    if let Some(i) = s.windows(3).position(|w| w == b" : ") {
        return Some((&s[..i], &s[i + 3..]));
    }
    s.strip_suffix(b" :").map(|fields| (fields, &[][..]))
}

fn parse_notification(line: &[u8]) -> Notification {
    parse_known(line).unwrap_or_else(|| Notification::Unknown(lossy(line)))
}

fn parse_known(line: &[u8]) -> Option<Notification> {
    let (name, rest) = split_word(line);
    let one_id = |sigil| parse_id(split_word(rest).0, sigil);
    Some(match name {
        b"%output" => {
            let (pane, data) = split_word(rest);
            Notification::Output {
                pane: parse_id(pane, b'%')?,
                bytes: unescape_bytes(data),
            }
        }
        b"%extended-output" => {
            let (pane, rest) = split_word(rest);
            let (age, rest) = split_word(rest);
            let (_, data) = split_value(rest)?;
            Notification::ExtendedOutput {
                pane: parse_id(pane, b'%')?,
                age_ms: parse_u64(age)?,
                bytes: unescape_bytes(data),
            }
        }
        b"%pause" => Notification::Pause {
            pane: one_id(b'%')?,
        },
        b"%continue" => Notification::Continue {
            pane: one_id(b'%')?,
        },
        b"%layout-change" => Notification::LayoutChange {
            window: one_id(b'@')?,
        },
        b"%window-add" => Notification::WindowAdd {
            window: one_id(b'@')?,
        },
        b"%window-close" => Notification::WindowClose {
            window: one_id(b'@')?,
        },
        b"%window-renamed" => {
            let (window, name) = split_word(rest);
            Notification::WindowRenamed {
                window: parse_id(window, b'@')?,
                name: lossy(name),
            }
        }
        b"%window-pane-changed" => {
            let (window, rest) = split_word(rest);
            Notification::WindowPaneChanged {
                window: parse_id(window, b'@')?,
                pane: parse_id(split_word(rest).0, b'%')?,
            }
        }
        b"%session-changed" => {
            let (session, name) = split_word(rest);
            Notification::SessionChanged {
                session: parse_id(session, b'$')?,
                name: lossy(name),
            }
        }
        b"%session-renamed" => {
            let (first, tail) = split_word(rest);
            let name = if parse_id(first, b'$').is_some() {
                tail
            } else {
                rest
            };
            Notification::SessionRenamed { name: lossy(name) }
        }
        b"%sessions-changed" => Notification::SessionsChanged,
        b"%session-window-changed" => {
            let (session, rest) = split_word(rest);
            Notification::SessionWindowChanged {
                session: parse_id(session, b'$')?,
                window: parse_id(split_word(rest).0, b'@')?,
            }
        }
        b"%pane-mode-changed" => Notification::PaneModeChanged {
            pane: one_id(b'%')?,
        },
        b"%subscription-changed" => {
            let (sub, rest) = split_word(rest);
            let (fields, value) = split_value(rest)?;
            let pane = fields
                .split(|&b| b == b' ')
                .nth(3)
                .and_then(|p| parse_id(p, b'%'));
            Notification::SubscriptionChanged {
                name: lossy(sub),
                pane,
                value: lossy(value),
            }
        }
        b"%client-detached" => Notification::ClientDetached,
        b"%exit" => Notification::Exit {
            reason: (!rest.is_empty()).then(|| lossy(rest)),
        },
        b"%message" => Notification::Message(lossy(rest)),
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardKind {
    Begin,
    End,
    Error,
}

/// The `<time> <number> <flags>` triple of a guard line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Guard {
    time: u64,
    number: u64,
    flags: u64,
}

fn parse_guard(line: &[u8]) -> Option<(GuardKind, Guard)> {
    let (name, rest) = split_word(line);
    let kind = match name {
        b"%begin" => GuardKind::Begin,
        b"%end" => GuardKind::End,
        b"%error" => GuardKind::Error,
        _ => return None,
    };
    let mut fields = rest.split(|&b| b == b' ');
    let guard = Guard {
        time: parse_u64(fields.next()?)?,
        number: parse_u64(fields.next()?)?,
        flags: parse_u64(fields.next()?)?,
    };
    Some((kind, guard))
}

/// What one protocol line amounted to.
#[derive(Debug, PartialEq, Eq)]
enum Frame {
    Notification(Notification),
    /// A closed block; `solicited` when it answers a line this client wrote.
    Reply {
        solicited: bool,
        reply: CommandReply,
    },
}

#[derive(Debug)]
struct OpenBlock {
    guard: Guard,
    reply: CommandReply,
    bytes: usize,
    truncated: usize,
}

/// The `%begin`/`%end` state machine over protocol lines.
#[derive(Debug)]
struct Framer {
    open: Option<OpenBlock>,
    max_reply_bytes: usize,
}

impl Framer {
    fn new(max_reply_bytes: usize) -> Self {
        Self {
            open: None,
            max_reply_bytes,
        }
    }

    fn feed(&mut self, line: &[u8]) -> Option<Frame> {
        if let Some(open) = self.open.as_mut() {
            // A body line that merely looks like a guard cannot also repeat the
            // block's exact time, number and flags.
            if let Some((kind, guard)) = parse_guard(line)
                && kind != GuardKind::Begin
                && guard == open.guard
            {
                let mut block = self.open.take()?;
                if block.truncated > 0 {
                    log::warn!(
                        "tmux control: reply {} exceeded {} bytes; dropped {} line(s)",
                        guard.number,
                        self.max_reply_bytes,
                        block.truncated
                    );
                }
                block.reply.ok = kind == GuardKind::End;
                return Some(Frame::Reply {
                    solicited: guard.flags & 1 != 0,
                    reply: block.reply,
                });
            }
            let cost = line.len() + LINE_OVERHEAD;
            if open.bytes + cost > self.max_reply_bytes {
                open.truncated += 1;
            } else {
                open.bytes += cost;
                open.reply.lines.push(lossy(line));
            }
            return None;
        }
        match parse_guard(line) {
            Some((GuardKind::Begin, guard)) => {
                self.open = Some(OpenBlock {
                    guard,
                    reply: CommandReply::default(),
                    bytes: 0,
                    truncated: 0,
                });
                None
            }
            Some(_) => {
                log::debug!("tmux control: stray block end {:?}", lossy(line));
                None
            }
            None => Some(Frame::Notification(parse_notification(line))),
        }
    }
}

/// Outcome of [`read_line_capped`].
#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    Line,
    Oversized,
    Eof,
}

/// Read one `\n`-terminated line into `buf` (without the `\n`), discarding a
/// line longer than `max` whole. A final unterminated line counts as a line.
fn read_line_capped<R: BufRead>(r: &mut R, buf: &mut Vec<u8>, max: usize) -> io::Result<LineRead> {
    buf.clear();
    let mut oversized = false;
    let mut seen = false;
    loop {
        let available = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok(match (seen, oversized) {
                (false, _) => LineRead::Eof,
                (true, true) => LineRead::Oversized,
                (true, false) => LineRead::Line,
            });
        }
        seen = true;
        let (chunk, used, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (&available[..i], i + 1, true),
            None => (available, available.len(), false),
        };
        if !oversized {
            if buf.len() + chunk.len() > max {
                oversized = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        r.consume(used);
        if done {
            return Ok(if oversized {
                LineRead::Oversized
            } else {
                LineRead::Line
            });
        }
    }
}

// ─── Shared state between the client and its reader thread ──────────────────

#[derive(Debug)]
struct State {
    notifications: VecDeque<Notification>,
    replies: VecDeque<CommandReply>,
    queue_cap: usize,
    queue_bytes_cap: usize,
    /// Sum of [`Notification::cost`] over `notifications`.
    queue_bytes: usize,
    dropped: u64,
    oversized: u64,
    dead: Option<String>,
    unsolicited_error: Option<String>,
    stderr: Vec<u8>,
}

impl State {
    fn push_notification(&mut self, notification: Notification) {
        let cost = notification.cost();
        while !self.notifications.is_empty()
            && (self.notifications.len() >= self.queue_cap
                || self.queue_bytes + cost > self.queue_bytes_cap)
        {
            let evicted = match self.notifications.iter().position(Notification::is_output) {
                Some(i) => self.notifications.remove(i),
                None => self.notifications.pop_front(),
            };
            if let Some(evicted) = evicted {
                self.queue_bytes -= evicted.cost();
            }
            if self.dropped == 0 {
                log::warn!(
                    "tmux control: notification queue full ({} items, {} bytes); \
                     dropping oldest output",
                    self.queue_cap,
                    self.queue_bytes_cap
                );
            }
            self.dropped += 1;
        }
        self.queue_bytes += cost;
        self.notifications.push_back(notification);
    }

    fn pop_notification(&mut self) -> Option<Notification> {
        let notification = self.notifications.pop_front()?;
        self.queue_bytes -= notification.cost();
        Some(notification)
    }

    fn mark_dead(&mut self, reason: String) {
        if self.dead.is_none() {
            self.dead = Some(reason);
        }
    }

    /// Why the client is dead, with whatever tmux said about it.
    fn death(&self) -> Option<String> {
        let mut reason = self.dead.clone()?;
        if let Some(error) = &self.unsolicited_error {
            reason.push_str(": ");
            reason.push_str(error);
        }
        let stderr = String::from_utf8_lossy(&self.stderr);
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            reason.push_str(" (stderr: ");
            reason.push_str(stderr);
            reason.push(')');
        }
        Some(reason)
    }
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn new(queue_cap: usize, queue_bytes_cap: usize) -> Self {
        Self {
            state: Mutex::new(State {
                notifications: VecDeque::new(),
                replies: VecDeque::new(),
                queue_cap: queue_cap.max(1),
                queue_bytes_cap,
                queue_bytes: 0,
                dropped: 0,
                oversized: 0,
                dead: None,
                unsolicited_error: None,
                stderr: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.lock());
        self.changed.notify_all();
    }

    /// Wait for a change until `deadline` (`None`: forever). Returns `None`
    /// once the deadline has passed.
    fn wait<'a>(
        &self,
        guard: MutexGuard<'a, State>,
        deadline: Option<Instant>,
    ) -> Option<MutexGuard<'a, State>> {
        match deadline {
            None => Some(
                self.changed
                    .wait(guard)
                    .unwrap_or_else(PoisonError::into_inner),
            ),
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    return None;
                }
                let (guard, _) = self
                    .changed
                    .wait_timeout(guard, deadline - now)
                    .unwrap_or_else(PoisonError::into_inner);
                Some(guard)
            }
        }
    }

    fn apply(&self, frame: Frame) {
        self.update(|state| match frame {
            Frame::Notification(notification) => {
                if let Notification::Exit { reason } = &notification {
                    state.mark_dead(match reason {
                        Some(reason) => format!("tmux control client exited ({reason})"),
                        None => "tmux control client exited".to_owned(),
                    });
                }
                state.push_notification(notification);
            }
            Frame::Reply {
                solicited: true,
                reply,
            } => state.replies.push_back(reply),
            Frame::Reply {
                solicited: false,
                reply,
            } => {
                if !reply.ok {
                    let text = reply.lines.join("; ");
                    log::debug!("tmux control: unsolicited error: {text}");
                    state.unsolicited_error = Some(text);
                }
            }
        });
    }
}

fn reader_loop(stdout: impl Read, shared: &Shared, max_line: usize, max_reply: usize) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut framer = Framer::new(max_reply);
    let mut line = Vec::new();
    let reason = loop {
        match read_line_capped(&mut reader, &mut line, max_line) {
            Ok(LineRead::Line) => {
                if let Some(frame) = framer.feed(&line) {
                    shared.apply(frame);
                }
            }
            Ok(LineRead::Oversized) => {
                log::warn!("tmux control: discarded a line longer than {max_line} bytes");
                shared.update(|s| s.oversized += 1);
            }
            Ok(LineRead::Eof) => break "tmux control client closed its output".to_owned(),
            Err(e) => break format!("read from tmux control client: {e}"),
        }
    };
    shared.update(|s| s.mark_dead(reason));
}

fn stderr_loop(stderr: impl Read, shared: &Shared) {
    let mut kept = Vec::new();
    let mut rest = stderr;
    let _ = (&mut rest).take(STDERR_KEEP as u64).read_to_end(&mut kept);
    if !kept.is_empty() {
        shared.update(|s| s.stderr = kept);
    }
    let _ = io::copy(&mut rest, &mut io::sink());
}

// ─── The writer thread and its bounded outbox ────────────────────────────────

#[derive(Debug, Default)]
struct OutboxState {
    queue: VecDeque<Vec<u8>>,
    /// Bytes queued and not yet taken by the writer.
    bytes: usize,
    closed: bool,
}

/// How long [`Outbox::push`] may wait for room.
#[derive(Debug, Clone, Copy)]
enum Room {
    /// Not at all.
    Now,
    /// Until the deadline (`None`: no deadline).
    Until(Option<Instant>),
}

#[derive(Debug, PartialEq, Eq)]
enum Push {
    Queued,
    Full,
    Closed,
}

/// Bytes waiting for tmux's stdin, capped at `cap`.
#[derive(Debug)]
struct Outbox {
    state: Mutex<OutboxState>,
    changed: Condvar,
    cap: usize,
}

impl Outbox {
    fn new(cap: usize) -> Self {
        Self {
            state: Mutex::new(OutboxState::default()),
            changed: Condvar::new(),
            cap,
        }
    }

    fn lock(&self) -> MutexGuard<'_, OutboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue `payload`; a payload larger than the cap still fits an empty
    /// outbox.
    fn push(&self, payload: Vec<u8>, room: Room) -> Push {
        let mut state = self.lock();
        loop {
            if state.closed {
                return Push::Closed;
            }
            if state.queue.is_empty() || state.bytes + payload.len() <= self.cap {
                state.bytes += payload.len();
                state.queue.push_back(payload);
                self.changed.notify_all();
                return Push::Queued;
            }
            state = match room {
                Room::Now => return Push::Full,
                Room::Until(None) => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Room::Until(Some(deadline)) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Push::Full;
                    }
                    self.changed
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    /// Refuse further pushes; the writer drains what is queued, then stops.
    fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }

    /// Close and discard everything queued (the pipe is broken).
    fn fail(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.queue.clear();
        state.bytes = 0;
        drop(state);
        self.changed.notify_all();
    }

    /// The next payload for the writer; `None` once closed and drained.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        loop {
            if let Some(payload) = state.queue.pop_front() {
                state.bytes -= payload.len();
                self.changed.notify_all();
                return Some(payload);
            }
            if state.closed {
                return None;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Own tmux's stdin: write what the outbox hands over, and close stdin (by
/// returning) once the outbox is closed and drained or a write fails. Only
/// this thread ever blocks on the pipe.
fn writer_loop(mut stdin: impl Write, outbox: &Outbox, shared: &Shared) {
    while let Some(payload) = outbox.pop() {
        if let Err(e) = stdin.write_all(&payload).and_then(|()| stdin.flush()) {
            outbox.fail();
            // Let the reader record what tmux printed before it went away.
            let deadline = Instant::now() + DEATH_SETTLE;
            let mut state = shared.lock();
            while state.dead.is_none() {
                match shared.wait(state, Some(deadline)) {
                    Some(next) => state = next,
                    None => {
                        state = shared.lock();
                        break;
                    }
                }
            }
            state.mark_dead(format!("write to tmux control client: {e}"));
            drop(state);
            shared.changed.notify_all();
            return;
        }
    }
}

// ─── Command lines ───────────────────────────────────────────────────────────

/// Single-quote `s` for the tmux command parser (`'` becomes `'\''`).
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn flags_line(flags: &[ClientFlag]) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = flags.iter().map(ClientFlag::as_arg).collect();
    format!("refresh-client -f {}", joined.join(","))
}

fn resume_line(pane: u32) -> String {
    format!("refresh-client -A {}", quote(&format!("%{pane}:continue")))
}

fn size_line(rows: u16, cols: u16) -> String {
    format!("refresh-client -C {cols}x{rows}")
}

fn subscribe_line(name: &str, target: &str, format: &str) -> Result<String> {
    validate_subscription_name(name)?;
    if target.contains(':') {
        bail!("tmux subscription target {target:?} must not contain ':'");
    }
    Ok(format!(
        "refresh-client -B {}",
        quote(&format!("{name}:{target}:{format}"))
    ))
}

fn unsubscribe_line(name: &str) -> Result<String> {
    validate_subscription_name(name)?;
    Ok(format!("refresh-client -B {}", quote(name)))
}

fn validate_subscription_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains(':') || name.contains(char::is_whitespace) {
        bail!("tmux subscription name {name:?} must be non-empty, without ':' or whitespace");
    }
    Ok(())
}

fn validate_line(line: &str) -> Result<()> {
    if line.len() > MAX_COMMAND_BYTES {
        bail!(
            "tmux control command is {} bytes, over the {MAX_COMMAND_BYTES}-byte limit",
            line.len()
        );
    }
    if line.contains(['\n', '\0']) {
        bail!(
            "tmux control command must be one line without NUL: {:?}",
            truncate_for_message(line)
        );
    }
    Ok(())
}

/// At most 200 bytes of `line` (on a char boundary), for error messages.
fn truncate_for_message(line: &str) -> &str {
    let mut end = line.len().min(200);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// The `tmux -L <name>|-S <path> -C attach-session -t =<session>` invocation.
fn attach_command(socket: &TmuxSocket, session: &str) -> Command {
    let mut cmd = Command::new(tmux_bin());
    cmd.args(socket.base_args())
        .arg("-C")
        .arg("attach-session")
        .arg("-t")
        .arg(format!("={session}"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    cmd
}

/// Tunables; production uses [`Options::default`].
#[derive(Debug, Clone, Copy)]
struct Options {
    queue_cap: usize,
    queue_bytes: usize,
    max_line: usize,
    max_reply: usize,
    outbox_cap: usize,
    /// Fixed sentinel nonce (tests); random when `None`.
    nonce: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            queue_cap: DEFAULT_QUEUE_CAP,
            queue_bytes: MAX_QUEUE_BYTES,
            max_line: MAX_LINE_BYTES,
            max_reply: MAX_REPLY_BYTES,
            outbox_cap: MAX_OUTBOX_BYTES,
            nonce: None,
        }
    }
}

// ─── The client ──────────────────────────────────────────────────────────────

/// A blocking client for one tmux control-mode connection to one session.
#[derive(Debug)]
pub struct ControlClient {
    child: Child,
    outbox: Arc<Outbox>,
    shared: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    session: String,
    nonce: u64,
    next_call: u64,
}

impl ControlClient {
    /// Attach a control client to `session` on `socket`, set `flags` with
    /// `refresh-client -f` and wait (up to [`SPAWN_TIMEOUT`]) for tmux to
    /// answer, so a missing session or server fails here.
    pub fn spawn(socket: &TmuxSocket, session: &str, flags: &[ClientFlag]) -> Result<Self> {
        if session.is_empty() {
            bail!("tmux control client: empty session name");
        }
        let context = || {
            format!(
                "attach tmux control client to session {session:?} on {}",
                socket.display_path().display()
            )
        };
        let mut client =
            Self::spawn_command(attach_command(socket, session), session, Options::default())
                .with_context(context)?;
        let reply = client
            .command(&flags_line(flags), SPAWN_TIMEOUT)
            .with_context(context)?;
        if !reply.ok {
            return Err(anyhow!(
                "refresh-client -f refused: {}",
                reply.lines.join("; ")
            ))
            .with_context(context);
        }
        Ok(client)
    }

    fn spawn_command(mut cmd: Command, session: &str, options: Options) -> Result<Self> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawn tmux control client")?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("tmux control client pipes unavailable");
        };
        let shared = Arc::new(Shared::new(options.queue_cap, options.queue_bytes));
        let outbox = Arc::new(Outbox::new(options.outbox_cap));
        let reader_shared = Arc::clone(&shared);
        let reader = thread::Builder::new()
            .name("tmux-control".into())
            .spawn(move || {
                reader_loop(stdout, &reader_shared, options.max_line, options.max_reply);
            });
        let reader = match reader {
            Ok(handle) => handle,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("spawn tmux control reader thread");
            }
        };
        let writer_shared = Arc::clone(&shared);
        let writer_outbox = Arc::clone(&outbox);
        let writer = thread::Builder::new()
            .name("tmux-control-in".into())
            .spawn(move || writer_loop(stdin, &writer_outbox, &writer_shared));
        let writer = match writer {
            Ok(handle) => handle,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e).context("spawn tmux control writer thread");
            }
        };
        let stderr_shared = Arc::clone(&shared);
        if let Err(e) = thread::Builder::new()
            .name("tmux-control-err".into())
            .spawn(move || stderr_loop(stderr, &stderr_shared))
        {
            log::warn!("tmux control: no stderr thread ({e}); stderr is not drained");
        }
        Ok(Self {
            child,
            outbox,
            shared,
            reader: Some(reader),
            writer: Some(writer),
            session: session.to_owned(),
            nonce: options.nonce.unwrap_or_else(rand::random),
            next_call: 0,
        })
    }

    /// The session this client is attached to.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// False once tmux exited, closed its output or the reader failed.
    pub fn is_alive(&self) -> bool {
        self.shared.lock().dead.is_none()
    }

    /// Why the client died, `None` while it is alive.
    pub fn death_reason(&self) -> Option<String> {
        self.shared.lock().death()
    }

    /// Notifications dropped because the queue was full.
    pub fn dropped_notifications(&self) -> u64 {
        self.shared.lock().dropped
    }

    /// Protocol lines discarded for exceeding [`MAX_LINE_BYTES`].
    pub fn oversized_lines(&self) -> u64 {
        self.shared.lock().oversized
    }

    fn dead_error(&self, what: &str) -> Option<anyhow::Error> {
        let reason = self.shared.lock().death()?;
        Some(anyhow!(
            "{what}: tmux control client for session {:?} is dead: {reason}",
            self.session
        ))
    }

    fn sync_token(&self, call: u64) -> String {
        format!("{SYNC_PREFIX}{:016x}-{call}", self.nonce)
    }

    /// The call number a sentinel block closes, `None` for any other block.
    fn sync_call(&self, reply: &CommandReply) -> Option<u64> {
        let [line] = reply.lines.as_slice() else {
            return None;
        };
        let rest = line
            .strip_prefix(SYNC_PREFIX)?
            .strip_prefix(&format!("{:016x}-", self.nonce))?;
        rest.parse().ok()
    }

    /// Queue `payload` for tmux's stdin, waiting for outbox room no later than
    /// `deadline`. An outbox still full at the deadline means tmux stopped
    /// reading, which marks the client dead.
    fn enqueue(
        &self,
        payload: String,
        deadline: Option<Instant>,
        timeout: Duration,
        what: &str,
    ) -> Result<()> {
        match self
            .outbox
            .push(payload.into_bytes(), Room::Until(deadline))
        {
            Push::Queued => Ok(()),
            Push::Full => {
                let reason = format!(
                    "tmux stopped reading its input ({} bytes pending)",
                    self.outbox.cap
                );
                self.shared.update(|s| s.mark_dead(reason));
                Err(anyhow!(
                    "{what}: no room to write within {timeout:?}: tmux control client \
                     for session {:?} is not reading its input",
                    self.session
                ))
            }
            Push::Closed => Err(self
                .dead_error(what)
                .unwrap_or_else(|| anyhow!("{what}: tmux control client input is closed"))),
        }
    }

    /// Run one command line and wait up to `timeout` for its reply. Commands
    /// are serialised; notifications arriving meanwhile stay queued for
    /// [`Self::next_notification`]. The whole call, queueing the write
    /// included, is bounded by `timeout`.
    pub fn command(&mut self, line: &str, timeout: Duration) -> Result<CommandReply> {
        let deadline = Instant::now().checked_add(timeout);
        let what = format!("tmux command {:?}", truncate_for_message(line));
        validate_line(line)?;
        if let Some(err) = self.dead_error(&what) {
            return Err(err);
        }
        let call = self.next_call;
        self.next_call += 1;
        let sentinel = format!("display-message -p {}\n", self.sync_token(call));
        // An empty line would detach this client; a blank one is skipped.
        let payload = if line.trim().is_empty() {
            sentinel
        } else {
            format!("{line}\n{sentinel}")
        };
        self.enqueue(payload, deadline, timeout, &what)?;

        let mut reply = CommandReply {
            ok: true,
            lines: Vec::new(),
        };
        let shared = Arc::clone(&self.shared);
        let mut state = shared.lock();
        loop {
            while let Some(block) = state.replies.pop_front() {
                match self.sync_call(&block) {
                    Some(n) if n == call => return Ok(reply),
                    Some(n) if n < call => {
                        // The tail of an earlier call that timed out.
                        reply = CommandReply {
                            ok: true,
                            lines: Vec::new(),
                        };
                    }
                    Some(n) => log::warn!("tmux control: sentinel for future call {n} ignored"),
                    None => {
                        reply.ok &= block.ok;
                        reply.lines.extend(block.lines);
                    }
                }
            }
            if let Some(reason) = state.death() {
                bail!(
                    "{what}: tmux control client for session {:?} died before replying: {reason}",
                    self.session
                );
            }
            match shared.wait(state, deadline) {
                Some(next) => state = next,
                None => bail!("{what} timed out after {timeout:?}"),
            }
        }
    }

    /// The next notification, waiting up to `timeout`; `Ok(None)` on timeout.
    /// Queued notifications are returned even after the client died; once the
    /// queue is empty a dead client returns an error.
    pub fn next_notification(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.shared.lock();
        loop {
            if let Some(notification) = state.pop_notification() {
                return Ok(Some(notification));
            }
            if let Some(reason) = state.death() {
                bail!(
                    "tmux control client for session {:?} is dead: {reason}",
                    self.session
                );
            }
            match self.shared.wait(state, deadline) {
                Some(next) => state = next,
                None => return Ok(None),
            }
        }
    }

    fn command_ok(&mut self, line: &str) -> Result<()> {
        let reply = self.command(line, COMMAND_TIMEOUT)?;
        if !reply.ok {
            bail!("tmux command {line:?} failed: {}", reply.lines.join("; "));
        }
        Ok(())
    }

    /// Resume a pane paused by `pause-after` (`refresh-client -A '%N:continue'`).
    pub fn resume_pane(&mut self, pane: u32) -> Result<()> {
        self.command_ok(&resume_line(pane))
    }

    /// Set the client size (`refresh-client -C <cols>x<rows>`).
    pub fn set_size(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.command_ok(&size_line(rows, cols))
    }

    /// Subscribe to `format` evaluated for `target` (`%N`, `@N`, `%*`, `@*` or
    /// empty for the session); changes arrive as
    /// [`Notification::SubscriptionChanged`] named `name`.
    pub fn subscribe(&mut self, name: &str, target: &str, format: &str) -> Result<()> {
        let line = subscribe_line(name, target, format)?;
        self.command_ok(&line)
    }

    /// Drop the subscription `name`.
    pub fn unsubscribe(&mut self, name: &str) -> Result<()> {
        let line = unsubscribe_line(name)?;
        self.command_ok(&line)
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        // Never wait on the pipe: queue the detach only if there is room now,
        // then close the outbox so the writer closes stdin once it drains.
        if self.is_alive() {
            let _ = self.outbox.push(b"detach-client\n".to_vec(), Room::Now);
        }
        self.outbox.close();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    log::debug!(
                        "tmux control: client for {:?} did not exit; killing it",
                        self.session
                    );
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // The reader ends at end of output and the writer once stdin is closed
        // or broken; only join them when that already happened, so a pipe held
        // open elsewhere cannot hang the drop.
        for handle in [self.reader.take(), self.writer.take()]
            .into_iter()
            .flatten()
        {
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: u64 = 0x5eed;

    fn feed_all(lines: &str) -> Vec<Frame> {
        let mut framer = Framer::new(MAX_REPLY_BYTES);
        lines
            .split('\n')
            .filter(|l| !l.is_empty())
            .filter_map(|l| framer.feed(l.as_bytes()))
            .collect()
    }

    fn note(line: &str) -> Notification {
        parse_notification(line.as_bytes())
    }

    /// The sentinel block tmux would print for `call`.
    fn sync_block(number: u64, call: u64) -> String {
        format!(
            "%begin 1793546791 {number} 1\n{SYNC_PREFIX}{NONCE:016x}-{call}\n%end 1793546791 {number} 1\n"
        )
    }

    /// A child that prints `head`, optionally sleeps and prints `tail`, then
    /// consumes stdin until it is closed, keeping its stdout open (fd 3) so
    /// the client stays alive meanwhile.
    fn fake(head: &str, tail: Option<&str>, options: Options) -> ControlClient {
        let mut cmd = Command::new("sh");
        let script = match tail {
            Some(_) => {
                r#"printf '%s' "$HEAD"; sleep 0.4; printf '%s' "$TAIL"; exec 3>&1 cat >/dev/null"#
            }
            None => r#"printf '%s' "$HEAD"; exec 3>&1 cat >/dev/null"#,
        };
        cmd.arg("-c")
            .arg(script)
            .env("HEAD", head)
            .env("TAIL", tail.unwrap_or(""));
        ControlClient::spawn_command(
            cmd,
            "probe",
            Options {
                nonce: Some(NONCE),
                ..options
            },
        )
        .expect("spawn fake control client")
    }

    // ── Fixtures captured from tmux 3.7c and 3.8 ─────────────────────────────

    #[test]
    fn attach_transcript_yields_unsolicited_block_then_session_changed() {
        let frames =
            feed_all("%begin 1793546791 354 0\n%end 1793546791 354 0\n%session-changed $0 probe\n");
        assert_eq!(
            frames,
            vec![
                Frame::Reply {
                    solicited: false,
                    reply: CommandReply {
                        ok: true,
                        lines: vec![]
                    },
                },
                Frame::Notification(Notification::SessionChanged {
                    session: 0,
                    name: "probe".into()
                }),
            ]
        );
    }

    #[test]
    fn layout_change_names_the_window() {
        assert_eq!(
            note(
                "%layout-change @0 1f13,100x29,0,0[100x29,0,0,0,50x7,4,2,1]<50x7,4,2,1> 1f13,... *"
            ),
            Notification::LayoutChange { window: 0 }
        );
    }

    #[test]
    fn output_with_st_terminated_osc_is_unescaped() {
        assert_eq!(
            note(r"%output %0 \033]7501;state=blocked:kind=permission:app=probe\033\134"),
            Notification::Output {
                pane: 0,
                bytes: b"\x1b]7501;state=blocked:kind=permission:app=probe\x1b\\".to_vec()
            }
        );
    }

    #[test]
    fn output_with_bel_terminated_osc_is_unescaped() {
        assert_eq!(
            note(r"%output %0 \033]7501;state=clear\007"),
            Notification::Output {
                pane: 0,
                bytes: b"\x1b]7501;state=clear\x07".to_vec()
            }
        );
    }

    #[test]
    fn extended_output_of_a_pause_after_client() {
        assert_eq!(
            note(
                r"%extended-output %0 0 : \015\012\033]7501;state=blocked:kind=permission:app=probe\033\134"
            ),
            Notification::ExtendedOutput {
                pane: 0,
                age_ms: 0,
                bytes: b"\r\n\x1b]7501;state=blocked:kind=permission:app=probe\x1b\\".to_vec()
            }
        );
        assert_eq!(
            note("%extended-output %12 1500 future-arg : a : b"),
            Notification::ExtendedOutput {
                pane: 12,
                age_ms: 1500,
                bytes: b"a : b".to_vec()
            }
        );
    }

    #[test]
    fn subscription_changed_for_a_pane() {
        assert_eq!(
            note("%subscription-changed t $0 @0 0 %0 : host|bash"),
            Notification::SubscriptionChanged {
                name: "t".into(),
                pane: Some(0),
                value: "host|bash".into()
            }
        );
        assert_eq!(
            note("%subscription-changed title $0 @0 0 %0 : ed@precision:/tmp|"),
            Notification::SubscriptionChanged {
                name: "title".into(),
                pane: Some(0),
                value: "ed@precision:/tmp|".into()
            }
        );
    }

    #[test]
    fn subscription_changed_for_the_session_has_no_pane() {
        assert_eq!(
            note("%subscription-changed sess $0 - - - : probe"),
            Notification::SubscriptionChanged {
                name: "sess".into(),
                pane: None,
                value: "probe".into()
            }
        );
        assert_eq!(
            note("%subscription-changed e $0 @1 2 %3 : "),
            Notification::SubscriptionChanged {
                name: "e".into(),
                pane: Some(3),
                value: String::new()
            }
        );
    }

    #[test]
    fn exit_with_and_without_reason() {
        assert_eq!(note("%exit"), Notification::Exit { reason: None });
        assert_eq!(
            note("%exit server exited"),
            Notification::Exit {
                reason: Some("server exited".into())
            }
        );
    }

    #[test]
    fn typed_notifications() {
        let cases = [
            ("%pause %4", Notification::Pause { pane: 4 }),
            ("%continue %4", Notification::Continue { pane: 4 }),
            ("%window-add @2", Notification::WindowAdd { window: 2 }),
            ("%window-close @2", Notification::WindowClose { window: 2 }),
            (
                "%window-renamed @0 my win",
                Notification::WindowRenamed {
                    window: 0,
                    name: "my win".into(),
                },
            ),
            (
                "%window-pane-changed @1 %7",
                Notification::WindowPaneChanged { window: 1, pane: 7 },
            ),
            (
                "%session-renamed $0 probe2",
                Notification::SessionRenamed {
                    name: "probe2".into(),
                },
            ),
            (
                "%session-renamed bare",
                Notification::SessionRenamed {
                    name: "bare".into(),
                },
            ),
            ("%sessions-changed", Notification::SessionsChanged),
            (
                "%session-window-changed $1 @5",
                Notification::SessionWindowChanged {
                    session: 1,
                    window: 5,
                },
            ),
            (
                "%pane-mode-changed %3",
                Notification::PaneModeChanged { pane: 3 },
            ),
            ("%client-detached /dev/pts/3", Notification::ClientDetached),
            (
                "%message hello there",
                Notification::Message("hello there".into()),
            ),
        ];
        for (line, want) in cases {
            assert_eq!(note(line), want, "{line}");
        }
    }

    #[test]
    fn unknown_and_malformed_lines_never_panic() {
        for line in [
            "",
            "%",
            "%output",
            "%output %",
            "%output %x data",
            "%output @0 data",
            "%extended-output %0",
            "%extended-output %0 x : a",
            "%extended-output %0 0 no-separator",
            "%window-add",
            "%window-add %1",
            "%window-pane-changed @1",
            "%session-changed probe",
            "%subscription-changed t $0 @0 0 %0 no value",
            "%unlinked-window-renamed @1 zsh",
            "%paste-buffer-changed buffer0",
            "%config-error /x:1: oops",
            "%client-session-changed /dev/pts/1 $1 other",
            "%layout-change @99999999999 x",
            "plain text",
        ] {
            assert_eq!(note(line), Notification::Unknown(line.into()), "{line:?}");
        }
        let raw = b"%output %0 \xff\xfe";
        assert_eq!(
            parse_notification(raw),
            Notification::Output {
                pane: 0,
                bytes: vec![0xff, 0xfe]
            }
        );
        assert!(matches!(
            parse_notification(b"%weird \xff"),
            Notification::Unknown(_)
        ));
    }

    #[test]
    fn body_lines_that_look_like_guards_stay_in_the_block() {
        let frames =
            feed_all("%begin 5 10 1\n%end 5 9 1\n%begin 5 10 1\n%output %0 x\n%error 5 10 1\n");
        assert_eq!(
            frames,
            vec![Frame::Reply {
                solicited: true,
                reply: CommandReply {
                    ok: false,
                    lines: vec![
                        "%end 5 9 1".into(),
                        "%begin 5 10 1".into(),
                        "%output %0 x".into()
                    ]
                }
            }]
        );
    }

    // ── Octal unescape ───────────────────────────────────────────────────────

    /// tmux's own escaping: control bytes and backslash as `\ooo`.
    fn tmux_escape(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in bytes {
            if b < b' ' || b == b'\\' {
                out.extend_from_slice(format!("\\{b:03o}").as_bytes());
            } else {
                out.push(b);
            }
        }
        out
    }

    #[test]
    fn unescape_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(unescape_bytes(&tmux_escape(&all)), all);
        for b in 0..=255u8 {
            let escaped = format!("\\{b:03o}");
            assert_eq!(unescape_output(&escaped), vec![b], "{escaped}");
            assert_eq!(unescape_bytes(&tmux_escape(&[b])), vec![b], "byte {b}");
        }
        let everything: String = all.iter().map(|b| format!("\\{b:03o}")).collect();
        assert_eq!(unescape_output(&everything), all);
    }

    #[test]
    fn unescape_keeps_non_escapes_literally() {
        assert_eq!(unescape_output(r"a\"), b"a\\".to_vec());
        assert_eq!(unescape_output(r"\03"), b"\\03".to_vec());
        assert_eq!(unescape_output(r"\089"), b"\\089".to_vec());
        assert_eq!(unescape_output(r"\777"), b"\\777".to_vec());
        assert_eq!(unescape_output(r"\n\0333"), b"\\n\x1b3".to_vec());
        assert_eq!(unescape_output(r"\134\134"), b"\\\\".to_vec());
        assert_eq!(unescape_output("é"), "é".as_bytes().to_vec());
    }

    // ── Line reading ─────────────────────────────────────────────────────────

    #[test]
    fn capped_reader_discards_long_lines_whole() {
        let input = format!("short\n{}\nnext\ntail", "x".repeat(100));
        let mut r = BufReader::with_capacity(7, input.as_bytes());
        let mut buf = Vec::new();
        let mut got = Vec::new();
        loop {
            match read_line_capped(&mut r, &mut buf, 16).unwrap() {
                LineRead::Eof => break,
                LineRead::Oversized => got.push("<oversized>".to_owned()),
                LineRead::Line => got.push(lossy(&buf)),
            }
        }
        assert_eq!(got, ["short", "<oversized>", "next", "tail"]);
    }

    // ── Command lines ────────────────────────────────────────────────────────

    #[test]
    fn command_lines_are_quoted_for_tmux() {
        assert_eq!(resume_line(7), "refresh-client -A '%7:continue'");
        assert_eq!(size_line(24, 80), "refresh-client -C 80x24");
        assert_eq!(
            subscribe_line("t", "%0", "#{pane_title}").unwrap(),
            "refresh-client -B 't:%0:#{pane_title}'"
        );
        assert_eq!(
            subscribe_line("q", "", "it's").unwrap(),
            r"refresh-client -B 'q::it'\''s'"
        );
        assert_eq!(unsubscribe_line("t").unwrap(), "refresh-client -B 't'");
        assert!(subscribe_line("a:b", "%0", "x").is_err());
        assert!(subscribe_line("a b", "%0", "x").is_err());
        assert!(subscribe_line("", "%0", "x").is_err());
        assert!(subscribe_line("a", "%0:1", "x").is_err());
        assert_eq!(
            flags_line(&[
                ClientFlag::IgnoreSize,
                ClientFlag::NoOutput,
                ClientFlag::PauseAfter(2),
                ClientFlag::ReadOnly
            ]),
            "refresh-client -f ignore-size,no-output,pause-after=2,read-only"
        );
        assert_eq!(flags_line(&[]), "");
    }

    #[test]
    fn attach_command_targets_the_exact_session() {
        let args = |socket: &TmuxSocket| {
            attach_command(socket, "probe")
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let named = TmuxSocket::Named {
            name: "default".into(),
            display_path: "/tmp/tmux-1/default".into(),
        };
        assert_eq!(
            args(&named),
            ["-L", "default", "-C", "attach-session", "-t", "=probe"]
        );
        let path = TmuxSocket::Path("/tmp/t.sock".into());
        assert_eq!(
            args(&path),
            ["-S", "/tmp/t.sock", "-C", "attach-session", "-t", "=probe"]
        );
    }

    // ── The client over a fake child ─────────────────────────────────────────

    #[test]
    fn back_to_back_commands_correlate_around_a_notification() {
        let transcript = format!(
            "%begin 1793546791 354 0\n%end 1793546791 354 0\n%session-changed $0 probe\n\
             %begin 1793546791 355 1\na\n%end 1793546791 355 1\n{}\
             %window-add @1\n\
             %begin 1793546791 357 1\nb\nc\n%end 1793546791 357 1\n{}",
            sync_block(356, 0),
            sync_block(358, 1)
        );
        let mut client = fake(&transcript, None, Options::default());
        let first = client.command("display -p a", COMMAND_TIMEOUT).unwrap();
        assert_eq!(
            first,
            CommandReply {
                ok: true,
                lines: vec!["a".into()]
            }
        );
        let second = client.command("display -p b", COMMAND_TIMEOUT).unwrap();
        assert_eq!(second.lines, ["b", "c"]);
        assert!(second.ok);
        assert_eq!(
            client.next_notification(COMMAND_TIMEOUT).unwrap(),
            Some(Notification::SessionChanged {
                session: 0,
                name: "probe".into()
            })
        );
        assert_eq!(
            client.next_notification(COMMAND_TIMEOUT).unwrap(),
            Some(Notification::WindowAdd { window: 1 })
        );
        assert_eq!(
            client.next_notification(Duration::from_millis(50)).unwrap(),
            None
        );
        assert!(client.is_alive());
    }

    #[test]
    fn error_block_makes_the_reply_not_ok() {
        let transcript = format!(
            "%begin 1795757939 296 1\nparse error: unknown command: bogus-cmd\n\
             %error 1795757939 296 1\n{}",
            sync_block(297, 0)
        );
        let mut client = fake(&transcript, None, Options::default());
        let reply = client.command("bogus-cmd", COMMAND_TIMEOUT).unwrap();
        assert_eq!(
            reply,
            CommandReply {
                ok: false,
                lines: vec!["parse error: unknown command: bogus-cmd".into()]
            }
        );
    }

    #[test]
    fn multi_block_and_blockless_lines_correlate() {
        let transcript = format!(
            "%begin 1 10 1\na\n%end 1 10 1\n%begin 1 11 1\nb\n%end 1 11 1\n{}{}",
            sync_block(12, 0),
            sync_block(13, 1)
        );
        let mut client = fake(&transcript, None, Options::default());
        let reply = client.command("display -p a ; display -p b", COMMAND_TIMEOUT);
        assert_eq!(reply.unwrap().lines, ["a", "b"]);
        let reply = client.command("# only a comment", COMMAND_TIMEOUT).unwrap();
        assert!(reply.ok && reply.lines.is_empty());
    }

    #[test]
    fn a_timed_out_call_is_skipped_by_the_next() {
        let tail = format!(
            "%begin 1 20 1\nlate\n%end 1 20 1\n{}%begin 1 22 1\none\n%end 1 22 1\n{}",
            sync_block(21, 0),
            sync_block(23, 1)
        );
        let mut client = fake("", Some(&tail), Options::default());
        let err = client
            .command("slow", Duration::from_millis(50))
            .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        let reply = client.command("fast", COMMAND_TIMEOUT).unwrap();
        assert_eq!(reply.lines, ["one"]);
    }

    #[test]
    fn exit_marks_dead_after_draining_the_queue() {
        let transcript = "%begin 1 290 0\ncan't find session: nosuch\n%error 1 290 0\n\
                          %window-add @3\n%exit\n";
        let mut client = fake(transcript, None, Options::default());
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        while client.is_alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!client.is_alive());
        let reason = client.death_reason().unwrap();
        assert!(reason.contains("can't find session: nosuch"), "{reason}");
        let err = client.command("display -p x", COMMAND_TIMEOUT).unwrap_err();
        assert!(format!("{err:#}").contains("is dead"), "{err:#}");
        assert_eq!(
            client.next_notification(COMMAND_TIMEOUT).unwrap(),
            Some(Notification::WindowAdd { window: 3 })
        );
        assert_eq!(
            client.next_notification(COMMAND_TIMEOUT).unwrap(),
            Some(Notification::Exit { reason: None })
        );
        assert!(client.next_notification(COMMAND_TIMEOUT).is_err());
    }

    #[test]
    fn end_of_output_fails_a_waiting_command() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 0");
        let mut client = ControlClient::spawn_command(cmd, "probe", Options::default()).unwrap();
        let err = client.command("display -p x", COMMAND_TIMEOUT).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("closed its output") || text.contains("write to tmux"),
            "{text}"
        );
        assert!(client.next_notification(COMMAND_TIMEOUT).is_err());
        assert!(!client.is_alive());
    }

    #[test]
    fn full_queue_drops_oldest_output_first() {
        let transcript = "%output %0 1\n%window-add @1\n%output %0 2\n%output %0 3\n\
                          %window-add @2\n%output %0 4\n%window-add @3\n";
        let mut client = fake(
            transcript,
            None,
            Options {
                queue_cap: 4,
                ..Options::default()
            },
        );
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        while client.dropped_notifications() < 3 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(client.dropped_notifications(), 3);
        let mut got = Vec::new();
        while let Some(n) = client.next_notification(Duration::from_millis(50)).unwrap() {
            got.push(n);
        }
        assert_eq!(
            got,
            vec![
                Notification::WindowAdd { window: 1 },
                Notification::WindowAdd { window: 2 },
                Notification::Output {
                    pane: 0,
                    bytes: b"4".to_vec()
                },
                Notification::WindowAdd { window: 3 },
            ]
        );
    }

    #[test]
    fn oversized_lines_are_counted_and_skipped() {
        let transcript = format!("%output %0 {}\n%window-add @9\n", "y".repeat(200));
        let mut client = fake(
            &transcript,
            None,
            Options {
                max_line: 64,
                ..Options::default()
            },
        );
        assert_eq!(
            client.next_notification(COMMAND_TIMEOUT).unwrap(),
            Some(Notification::WindowAdd { window: 9 })
        );
        assert_eq!(client.oversized_lines(), 1);
    }

    #[test]
    fn drop_kills_a_child_that_ignores_detach() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 30");
        let client = ControlClient::spawn_command(cmd, "probe", Options::default()).unwrap();
        let started = Instant::now();
        drop(client);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn rejects_multi_line_commands() {
        let mut client = fake("", None, Options::default());
        assert!(client.command("a\nb", COMMAND_TIMEOUT).is_err());
        assert!(client.command("a\0b", COMMAND_TIMEOUT).is_err());
    }

    #[test]
    fn blank_commands_write_only_the_sentinel() {
        // `cat` echoes what the client writes back as protocol lines.
        let client = ControlClient::spawn_command(
            Command::new("cat"),
            "probe",
            Options {
                nonce: Some(NONCE),
                ..Options::default()
            },
        );
        let mut client = client.unwrap();
        assert!(client.command("  ", Duration::from_millis(50)).is_err());
        assert_eq!(
            client.next_notification(COMMAND_TIMEOUT).unwrap(),
            Some(Notification::Unknown(format!(
                "display-message -p {SYNC_PREFIX}{NONCE:016x}-0"
            )))
        );
        assert_eq!(
            client.next_notification(Duration::from_millis(50)).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_overlong_commands() {
        let mut client = fake("", None, Options::default());
        let line = format!("display -p {}", "x".repeat(MAX_COMMAND_BYTES));
        let err = client.command(&line, COMMAND_TIMEOUT).unwrap_err();
        assert!(format!("{err:#}").contains("byte limit"), "{err:#}");
        assert!(client.is_alive());
    }

    #[test]
    fn a_child_that_never_reads_stdin_cannot_block_commands_or_drop() {
        // `sleep` keeps stdout open and never reads stdin, so the pipe fills.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 30");
        let mut client = ControlClient::spawn_command(
            cmd,
            "probe",
            Options {
                outbox_cap: 8 * 1024,
                ..Options::default()
            },
        )
        .unwrap();
        let line = format!("display -p {}", "x".repeat(60 * 1024));
        let timeout = Duration::from_millis(100);
        let mut stalled = None;
        for _ in 0..32 {
            let started = Instant::now();
            let err = client.command(&line, timeout).unwrap_err();
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "command took {:?}",
                started.elapsed()
            );
            let text = format!("{err:#}");
            if !text.contains("timed out") {
                stalled = Some(text);
                break;
            }
        }
        let stalled = stalled.expect("the outbox never filled");
        assert!(stalled.contains("not reading its input"), "{stalled}");
        assert!(!client.is_alive());
        let started = Instant::now();
        let err = client.command("display -p x", timeout).unwrap_err();
        assert!(format!("{err:#}").contains("is dead"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(1));

        let started = Instant::now();
        drop(client);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "drop took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn reply_cap_counts_per_line_overhead() {
        let mut framer = Framer::new(10 * LINE_OVERHEAD);
        assert_eq!(framer.feed(b"%begin 1 2 1"), None);
        for _ in 0..1000 {
            assert_eq!(framer.feed(b""), None);
        }
        let Some(Frame::Reply { reply, .. }) = framer.feed(b"%end 1 2 1") else {
            panic!("block did not close");
        };
        assert_eq!(reply.lines.len(), 10);
    }

    #[test]
    fn queue_byte_budget_evicts_oldest_output_first() {
        let out = |n: u8| Notification::Output {
            pane: 0,
            bytes: vec![n; 10],
        };
        let item = std::mem::size_of::<Notification>();
        let budget = 2 * (item + 10) + item;
        let shared = Shared::new(DEFAULT_QUEUE_CAP, budget);
        let mut state = shared.lock();
        state.push_notification(out(1));
        state.push_notification(Notification::WindowAdd { window: 1 });
        state.push_notification(out(2));
        assert_eq!((state.dropped, state.queue_bytes), (0, budget));
        state.push_notification(out(3));
        assert_eq!((state.dropped, state.queue_bytes), (1, budget));
        let left: Vec<_> = state.notifications.iter().cloned().collect();
        assert_eq!(
            left,
            vec![Notification::WindowAdd { window: 1 }, out(2), out(3)]
        );
        // A notification bigger than the whole budget still gets in alone.
        state.push_notification(Notification::Output {
            pane: 0,
            bytes: vec![0; budget],
        });
        assert_eq!(state.notifications.len(), 1);
        assert_eq!(state.dropped, 4);
        assert!(state.pop_notification().is_some());
        assert_eq!(state.queue_bytes, 0);
    }
}
