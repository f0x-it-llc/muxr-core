//! Command runner: short-lived tmux invocations and their typed rows.
//!
//! Every unary operation and every layout query runs one
//! `tmux -L <name>|-S <path> <command> ...` process and parses its stdout. Layout
//! strings are never parsed.
//!
//! # Record format
//!
//! A tmux format cannot emit a separator that is guaranteed absent from titles,
//! so each record is written one field per line: a marker line `@@` followed by
//! `key=value` lines in a fixed order, with the free-text fields (pane title,
//! window name, session name, client name) last. The parser splits records on
//! the marker line and takes a value as everything after the first `=`. A pane
//! title cannot hold a newline or tab on any supported version, so a title may
//! contain `|`, `:` and `=` freely. As defence in depth the last field of a
//! record absorbs any unexpected extra lines up to the next marker. The
//! `#{s/.../.../:...}` format modifier is never used.
//!
//! # Targets
//!
//! Panes and windows are addressed by id (`%N`, `@N`). A session is addressed
//! as `=<name>` (exact match) where the command takes a session target
//! ([`session_target`]: `has-session`, `kill-session`, `rename-session`,
//! `list-windows`, `list-panes -s`, `list-clients`, `attach-session`) and as
//! `=<name>:` where it takes a window or pane target ([`session_scope`]:
//! `new-window`, `split-window`, `new-pane`); `split-window -t =name` fails
//! with "can't find pane" while `-t =name:` works.
//!
//! # Resize
//!
//! `resize-pane` moves a border by 5 cells. With a direction, `Increase` uses
//! that direction's flag and `Decrease` the opposite one. Without a direction
//! `Increase` is `-R` and `Decrease` is `-L`.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::paths::{TmuxSocket, TmuxVersion, tmux_bin};
use crate::actions::{ActionAck, ScrollDir};
use crate::multiplexer::types::{ResizeDir, ResizeKind};

/// How long one tmux invocation may run before it is killed.
const RUN_TIMEOUT: Duration = Duration::from_secs(5);
/// Bytes sent per `send-keys -H` invocation.
const HEX_CHUNK: usize = 256;
/// Cells moved per `resize-pane`.
const RESIZE_STEP: &str = "5";
/// The record marker line.
const MARKER: &str = "@@";

const FLOATING_NEEDS: &str = "floating panes need tmux 3.7 or newer";
const MOVE_NEEDS: &str = "moving a pane between tiled and floating needs tmux 3.8 or newer";

/// `=<name>`: exact-match session target for session-taking commands.
pub fn session_target(name: &str) -> String {
    format!("={name}")
}

/// `=<name>:`: exact-match session scope for window- and pane-taking commands.
pub fn session_scope(name: &str) -> String {
    format!("={name}:")
}

/// One row of `list-sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub name: String,
    pub id: u32,
    pub created_unix: u64,
    pub attached: u32,
    pub windows: u32,
}

/// One row of `list-windows`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRow {
    pub id: u32,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub bell: bool,
    /// `window_zoomed_flag`.
    pub zoomed: bool,
    pub width: u32,
    pub height: u32,
    pub panes: u32,
}

/// One row of `list-panes -s`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    pub id: u32,
    pub window_id: u32,
    pub index: u32,
    pub active: bool,
    pub title: String,
    pub current_command: String,
    pub current_path: String,
    pub dead: bool,
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    /// `pane_floating_flag` (tmux 3.7+; `false` below).
    pub floating: bool,
    /// `pane_z` (tmux 3.7+; `0` below).
    pub z: i32,
    /// `pane_command_running` (tmux 3.8+; `None` below).
    pub command_running: Option<bool>,
    pub pid: u32,
}

/// One row of `list-clients`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRow {
    pub name: String,
    pub tty: String,
    pub readonly: bool,
    pub control_mode: bool,
    pub width: u32,
    pub height: u32,
}

const SESSION_KEYS: &[&str] = &["id", "created", "attached", "windows", "name"];
const WINDOW_KEYS: &[&str] = &[
    "id", "index", "active", "bell", "zoomed", "width", "height", "panes", "name",
];
const PANE_KEYS: &[&str] = &[
    "id", "window", "index", "active", "dead", "left", "top", "width", "height", "floating", "z",
    "running", "pid", "command", "path", "title",
];
const CLIENT_KEYS: &[&str] = &["readonly", "control", "width", "height", "tty", "name"];

/// Build a `-F` template: one marker line, then one `key=#{var}` line per field.
fn template(fields: &[(&str, &str)]) -> String {
    let mut out = String::from(MARKER);
    for (key, var) in fields {
        out.push('\n');
        out.push_str(key);
        out.push_str("=#{");
        out.push_str(var);
        out.push('}');
    }
    out
}

fn session_template() -> String {
    template(&[
        ("id", "session_id"),
        ("created", "session_created"),
        ("attached", "session_attached"),
        ("windows", "session_windows"),
        ("name", "session_name"),
    ])
}

fn window_template() -> String {
    template(&[
        ("id", "window_id"),
        ("index", "window_index"),
        ("active", "window_active"),
        ("bell", "window_bell_flag"),
        ("zoomed", "window_zoomed_flag"),
        ("width", "window_width"),
        ("height", "window_height"),
        ("panes", "window_panes"),
        ("name", "window_name"),
    ])
}

fn pane_template() -> String {
    template(&[
        ("id", "pane_id"),
        ("window", "window_id"),
        ("index", "pane_index"),
        ("active", "pane_active"),
        ("dead", "pane_dead"),
        ("left", "pane_left"),
        ("top", "pane_top"),
        ("width", "pane_width"),
        ("height", "pane_height"),
        ("floating", "pane_floating_flag"),
        ("z", "pane_z"),
        ("running", "pane_command_running"),
        ("pid", "pane_pid"),
        ("command", "pane_current_command"),
        ("path", "pane_current_path"),
        ("title", "pane_title"),
    ])
}

fn client_template() -> String {
    template(&[
        ("readonly", "client_readonly"),
        ("control", "client_control_mode"),
        ("width", "client_width"),
        ("height", "client_height"),
        ("tty", "client_tty"),
        ("name", "client_name"),
    ])
}

/// Split `text` into records of `keys.len()` values each. The last value absorbs
/// any extra lines up to the next marker.
fn parse_records(text: &str, keys: &[&str]) -> Result<Vec<Vec<String>>> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in text.lines() {
        if line == MARKER {
            blocks.push(Vec::new());
        } else if let Some(block) = blocks.last_mut() {
            block.push(line);
        }
        // Text before the first marker is not part of any record.
    }
    let mut records = Vec::with_capacity(blocks.len());
    for block in blocks {
        if block.len() < keys.len() {
            bail!(
                "tmux record has {} lines, expected {}: {:?}",
                block.len(),
                keys.len(),
                block
            );
        }
        let mut values = Vec::with_capacity(keys.len());
        let last = keys.len() - 1;
        for (i, key) in keys.iter().enumerate() {
            let raw = if i == last {
                block[i..].join("\n")
            } else {
                block[i].to_owned()
            };
            let value = raw
                .strip_prefix(key)
                .and_then(|r| r.strip_prefix('='))
                .ok_or_else(|| anyhow!("tmux record line {raw:?} is not `{key}=...`"))?;
            values.push(value.to_owned());
        }
        records.push(values);
    }
    Ok(records)
}

fn flag(v: &str) -> bool {
    matches!(v.trim(), "1" | "true")
}

fn num<T: std::str::FromStr>(key: &str, v: &str) -> Result<T> {
    v.trim()
        .parse()
        .map_err(|_| anyhow!("tmux field `{key}` is not a number: {v:?}"))
}

/// Number with an empty value meaning the default.
fn num_or_default<T: std::str::FromStr + Default>(key: &str, v: &str) -> Result<T> {
    if v.trim().is_empty() {
        Ok(T::default())
    } else {
        num(key, v)
    }
}

/// Parse an id such as `$3`, `@4` or `%5` into its number.
fn id_num(key: &str, v: &str) -> Result<u32> {
    num(key, v.trim().trim_start_matches(['$', '@', '%']))
}

/// Parse `list-sessions` output produced with the session template.
pub fn parse_sessions(text: &str) -> Result<Vec<SessionRow>> {
    parse_records(text, SESSION_KEYS)?
        .into_iter()
        .map(|v| {
            Ok(SessionRow {
                id: id_num("id", &v[0])?,
                created_unix: num("created", &v[1])?,
                attached: num("attached", &v[2])?,
                windows: num("windows", &v[3])?,
                name: v[4].clone(),
            })
        })
        .collect()
}

/// Parse `list-windows` output produced with the window template.
pub fn parse_windows(text: &str) -> Result<Vec<WindowRow>> {
    parse_records(text, WINDOW_KEYS)?
        .into_iter()
        .map(|v| {
            Ok(WindowRow {
                id: id_num("id", &v[0])?,
                index: num("index", &v[1])?,
                active: flag(&v[2]),
                bell: flag(&v[3]),
                zoomed: flag(&v[4]),
                width: num("width", &v[5])?,
                height: num("height", &v[6])?,
                panes: num("panes", &v[7])?,
                name: v[8].clone(),
            })
        })
        .collect()
}

/// Parse `list-panes` output produced with the pane template.
pub fn parse_panes(text: &str) -> Result<Vec<PaneRow>> {
    parse_records(text, PANE_KEYS)?
        .into_iter()
        .map(|v| {
            Ok(PaneRow {
                id: id_num("id", &v[0])?,
                window_id: id_num("window", &v[1])?,
                index: num("index", &v[2])?,
                active: flag(&v[3]),
                dead: flag(&v[4]),
                left: num("left", &v[5])?,
                top: num("top", &v[6])?,
                width: num("width", &v[7])?,
                height: num("height", &v[8])?,
                floating: flag(&v[9]),
                z: num_or_default("z", &v[10])?,
                command_running: match v[11].trim() {
                    "" => None,
                    other => Some(flag(other)),
                },
                pid: num_or_default("pid", &v[12])?,
                current_command: v[13].clone(),
                current_path: v[14].clone(),
                title: v[15].clone(),
            })
        })
        .collect()
}

/// Parse `list-clients` output produced with the client template.
pub fn parse_clients(text: &str) -> Result<Vec<ClientRow>> {
    parse_records(text, CLIENT_KEYS)?
        .into_iter()
        .map(|v| {
            Ok(ClientRow {
                readonly: flag(&v[0]),
                control_mode: flag(&v[1]),
                width: num_or_default("width", &v[2])?,
                height: num_or_default("height", &v[3])?,
                tty: v[4].clone(),
                name: v[5].clone(),
            })
        })
        .collect()
}

fn is_no_server(stderr: &str) -> bool {
    stderr.contains("no server running")
        || stderr.contains("error connecting to")
        || stderr.contains("No such file or directory")
}

fn ack_ok(info: impl Into<String>) -> ActionAck {
    let info = info.into();
    ActionAck {
        ok: true,
        error: None,
        info: (!info.is_empty()).then_some(info),
    }
}

fn ack_err(error: impl Into<String>) -> ActionAck {
    ActionAck {
        ok: false,
        error: Some(error.into()),
        info: None,
    }
}

fn pane_arg(pane: u32) -> String {
    format!("%{pane}")
}

fn window_arg(window: u32) -> String {
    format!("@{window}")
}

/// The ephemeral tmux command runner for one server socket.
#[derive(Debug, Clone)]
pub struct Tmux {
    socket: TmuxSocket,
    version: TmuxVersion,
    bin: OsString,
}

impl Tmux {
    /// A runner for `socket` using the binary from `MUXRD_TMUX_BIN`.
    pub fn new(socket: TmuxSocket, version: TmuxVersion) -> Self {
        Self::with_bin(socket, version, tmux_bin())
    }

    /// A runner using an explicit binary instead of the environment.
    pub fn with_bin(socket: TmuxSocket, version: TmuxVersion, bin: OsString) -> Self {
        Self {
            socket,
            version,
            bin,
        }
    }

    pub fn socket(&self) -> &TmuxSocket {
        &self.socket
    }

    pub fn version(&self) -> &TmuxVersion {
        &self.version
    }

    /// Run `tmux -L <name>|-S <path> <args>` with a 5 s timeout (the child is killed on
    /// expiry). stdout and stderr are captured; decode them lossily.
    pub fn run(&self, args: &[&str]) -> Result<Output> {
        let mut child = Command::new(&self.bin)
            .args(self.socket.base_args())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {:?}", self.bin))?;
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut p) = pipe {
                    let _ = p.read_to_end(&mut buf);
                }
                buf
            })
        };
        let out_t = drain(child.stdout.take().map(|p| Box::new(p) as _));
        let err_t = drain(child.stderr.take().map(|p| Box::new(p) as _));
        let deadline = Instant::now() + RUN_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().context("wait for tmux")? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "tmux {} timed out after {RUN_TIMEOUT:?}",
                    args.first().copied().unwrap_or("")
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        Ok(Output {
            status,
            stdout: out_t.join().unwrap_or_default(),
            stderr: err_t.join().unwrap_or_default(),
        })
    }

    /// Run a command and map its exit status onto an [`ActionAck`].
    pub fn ack(&self, args: &[&str]) -> Result<ActionAck> {
        let out = self.run(args)?;
        if out.status.success() {
            Ok(ack_ok(
                String::from_utf8_lossy(&out.stdout).trim().to_owned(),
            ))
        } else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            Ok(ack_err(if err.is_empty() {
                format!("tmux exited with {}", out.status)
            } else {
                err
            }))
        }
    }

    /// Run a query and return its stdout, failing with stderr on non-zero exit.
    fn query(&self, args: &[&str]) -> Result<String> {
        let out = self.run(args)?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            Ok(stdout)
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!(
                "tmux {}: {}",
                args.first().copied().unwrap_or(""),
                err.trim()
            )
        }
    }

    /// All sessions; a missing server or socket yields an empty list.
    pub fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let fmt = session_template();
        let out = self.run(&["list-sessions", "-F", &fmt])?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if is_no_server(&err) {
                return Ok(Vec::new());
            }
            bail!("tmux list-sessions: {}", err.trim());
        }
        parse_sessions(&String::from_utf8_lossy(&out.stdout))
    }

    /// The windows of `session`.
    pub fn list_windows(&self, session: &str) -> Result<Vec<WindowRow>> {
        let fmt = window_template();
        let target = session_target(session);
        parse_windows(&self.query(&["list-windows", "-t", &target, "-F", &fmt])?)
    }

    /// Every pane of `session`.
    pub fn list_panes(&self, session: &str) -> Result<Vec<PaneRow>> {
        let fmt = pane_template();
        let target = session_target(session);
        parse_panes(&self.query(&["list-panes", "-s", "-t", &target, "-F", &fmt])?)
    }

    /// The clients attached to `session`.
    pub fn list_clients(&self, session: &str) -> Result<Vec<ClientRow>> {
        let fmt = client_template();
        let target = session_target(session);
        parse_clients(&self.query(&["list-clients", "-t", &target, "-F", &fmt])?)
    }

    /// Expand `format` against `target` (`display-message -p`).
    pub fn display(&self, target: &str, format: &str) -> Result<String> {
        let out = self.query(&["display-message", "-p", "-t", target, format])?;
        Ok(out.trim_end_matches('\n').to_owned())
    }

    pub fn new_session(&self, name: &str) -> Result<ActionAck> {
        self.ack(&["new-session", "-d", "-s", name, "-x", "80", "-y", "24"])
    }

    pub fn has_session(&self, name: &str) -> Result<ActionAck> {
        self.ack(&["has-session", "-t", &session_target(name)])
    }

    pub fn kill_session(&self, name: &str) -> Result<ActionAck> {
        self.ack(&["kill-session", "-t", &session_target(name)])
    }

    pub fn rename_session(&self, name: &str, new_name: &str) -> Result<ActionAck> {
        self.ack(&[
            "rename-session",
            "-t",
            &session_target(name),
            "--",
            new_name,
        ])
    }

    /// Send raw bytes to a pane as hex, 256 bytes per invocation. Stops at the
    /// first failing chunk and returns its ack.
    pub fn send_keys_hex(&self, pane: u32, bytes: &[u8]) -> Result<ActionAck> {
        let target = pane_arg(pane);
        let mut last = ack_ok("");
        for chunk in bytes.chunks(HEX_CHUNK) {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let mut args = vec!["send-keys", "-t", target.as_str(), "-H"];
            args.extend(hex.iter().map(String::as_str));
            last = self.ack(&args)?;
            if !last.ok {
                return Ok(last);
            }
        }
        Ok(last)
    }

    pub fn select_pane(&self, pane: u32) -> Result<ActionAck> {
        self.ack(&["select-pane", "-t", &pane_arg(pane)])
    }

    pub fn select_window(&self, window: u32) -> Result<ActionAck> {
        self.ack(&["select-window", "-t", &window_arg(window)])
    }

    pub fn kill_pane(&self, pane: u32) -> Result<ActionAck> {
        self.ack(&["kill-pane", "-t", &pane_arg(pane)])
    }

    pub fn kill_window(&self, window: u32) -> Result<ActionAck> {
        self.ack(&["kill-window", "-t", &window_arg(window)])
    }

    /// Create a window; `info` carries the new window id number.
    pub fn new_window(&self, session: &str, name: Option<&str>) -> Result<ActionAck> {
        let scope = session_scope(session);
        let mut args = vec!["new-window", "-t", scope.as_str()];
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            args.extend(["-n", name]);
        }
        args.extend(["-P", "-F", "#{window_id}"]);
        Ok(strip_id(self.ack(&args)?))
    }

    /// Split the active pane of the session's active window; `info` carries the
    /// new pane id number.
    pub fn split_window(&self, session: &str) -> Result<ActionAck> {
        let scope = session_scope(session);
        let ack = self.ack(&["split-window", "-t", &scope, "-P", "-F", "#{pane_id}"])?;
        Ok(strip_id(ack))
    }

    /// Create a floating pane (tmux 3.7+); `info` carries the new pane id number.
    pub fn new_floating_pane(&self, session: &str) -> Result<ActionAck> {
        if !self.version.at_least(3, 7) {
            return Ok(ack_err(FLOATING_NEEDS));
        }
        let scope = session_scope(session);
        let ack = self.ack(&["new-pane", "-t", &scope, "-P", "-F", "#{pane_id}"])?;
        Ok(strip_id(ack))
    }

    pub fn rename_window(&self, window: u32, name: &str) -> Result<ActionAck> {
        self.ack(&["rename-window", "-t", &window_arg(window), "--", name])
    }

    pub fn set_pane_title(&self, pane: u32, title: &str) -> Result<ActionAck> {
        self.ack(&["select-pane", "-t", &pane_arg(pane), "-T", title])
    }

    pub fn resize_pane(
        &self,
        pane: u32,
        kind: ResizeKind,
        dir: Option<ResizeDir>,
    ) -> Result<ActionAck> {
        let flag = match (kind, dir) {
            (ResizeKind::Increase, None) => "-R",
            (ResizeKind::Decrease, None) => "-L",
            (ResizeKind::Increase, Some(d)) => dir_flag(d),
            (ResizeKind::Decrease, Some(d)) => dir_flag(opposite(d)),
        };
        self.ack(&["resize-pane", "-t", &pane_arg(pane), flag, RESIZE_STEP])
    }

    pub fn zoom_toggle(&self, pane: u32) -> Result<ActionAck> {
        self.ack(&["resize-pane", "-Z", "-t", &pane_arg(pane)])
    }

    /// Turn a tiled pane into a floating one (tmux 3.8+).
    pub fn float_pane(&self, pane: u32) -> Result<ActionAck> {
        if !self.version.at_least(3, 8) {
            return Ok(ack_err(MOVE_NEEDS));
        }
        self.ack(&["break-pane", "-W", "-s", &pane_arg(pane)])
    }

    /// Move a floating pane into the tiled layout next to `onto_tiled` (3.8+).
    pub fn tile_pane(&self, floating: u32, onto_tiled: u32) -> Result<ActionAck> {
        if !self.version.at_least(3, 8) {
            return Ok(ack_err(MOVE_NEEDS));
        }
        self.ack(&[
            "join-pane",
            "-s",
            &pane_arg(floating),
            "-t",
            &pane_arg(onto_tiled),
        ])
    }

    /// Enter copy mode on the pane and scroll it.
    pub fn scroll(&self, pane: u32, dir: ScrollDir) -> Result<ActionAck> {
        let target = pane_arg(pane);
        let entered = self.ack(&["copy-mode", "-t", &target])?;
        if !entered.ok {
            return Ok(entered);
        }
        let cmd: &[&str] = match dir {
            ScrollDir::Up => &["-N", "3", "scroll-up"],
            ScrollDir::Down => &["-N", "3", "scroll-down"],
            ScrollDir::ToTop => &["history-top"],
            ScrollDir::ToBottom => &["history-bottom"],
            ScrollDir::PageUp => &["page-up"],
            ScrollDir::PageDown => &["page-down"],
            ScrollDir::HalfPageUp => &["halfpage-up"],
            ScrollDir::HalfPageDown => &["halfpage-down"],
        };
        let mut args = vec!["send-keys", "-t", target.as_str(), "-X"];
        args.extend(cmd);
        self.ack(&args)
    }
}

/// Reduce an id such as `@4` or `%7` in `info` to its number.
fn strip_id(mut ack: ActionAck) -> ActionAck {
    if ack.ok
        && let Some(info) = ack.info.as_mut()
    {
        *info = info.trim().trim_start_matches(['@', '%']).to_owned();
    }
    ack
}

fn dir_flag(dir: ResizeDir) -> &'static str {
    match dir {
        ResizeDir::Left => "-L",
        ResizeDir::Right => "-R",
        ResizeDir::Up => "-U",
        ResizeDir::Down => "-D",
    }
}

fn opposite(dir: ResizeDir) -> ResizeDir {
    match dir {
        ResizeDir::Left => ResizeDir::Right,
        ResizeDir::Right => ResizeDir::Left,
        ResizeDir::Up => ResizeDir::Down,
        ResizeDir::Down => ResizeDir::Up,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    /// Serialises fake-binary tests so no fork inherits a script still open for writing.
    static FAKE_LOCK: Mutex<()> = Mutex::new(());

    struct Fake {
        dir: PathBuf,
        tmux: Tmux,
        _guard: MutexGuard<'static, ()>,
    }

    impl Fake {
        fn new(version: &str) -> Self {
            let guard = FAKE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "muxrd-tmux-cmd-{}-{}",
                std::process::id(),
                std::thread::current()
                    .name()
                    .unwrap_or("t")
                    .replace("::", "_")
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("tmux");
            let body = format!(
                "#!/bin/sh\n\
                 d='{d}'\n\
                 printf '%s\\n' \"$@\" >> \"$d/log\"\n\
                 printf '==\\n' >> \"$d/log\"\n\
                 [ -f \"$d/out\" ] && cat \"$d/out\"\n\
                 [ -f \"$d/err\" ] && cat \"$d/err\" >&2\n\
                 if [ -f \"$d/code\" ]; then exit \"$(cat \"$d/code\")\"; fi\n\
                 exit 0\n",
                d = dir.display()
            );
            std::fs::write(&script, body).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let tmux = Tmux::with_bin(
                TmuxSocket::Named {
                    name: "muxrd-fake".into(),
                    display_path: PathBuf::from("/fake/tmux-1/muxrd-fake"),
                },
                TmuxVersion::parse(&format!("tmux {version}")).unwrap(),
                script.into_os_string(),
            );
            Self {
                dir,
                tmux,
                _guard: guard,
            }
        }

        fn set(&self, name: &str, content: &str) {
            std::fs::write(self.dir.join(name), content).unwrap();
        }

        /// Recorded invocations, each without the leading `-L <name>`.
        fn calls(&self) -> Vec<Vec<String>> {
            let log = std::fs::read_to_string(self.dir.join("log")).unwrap_or_default();
            let mut calls = Vec::new();
            let mut cur: Vec<String> = Vec::new();
            for line in log.lines() {
                if line == "==" {
                    assert_eq!(&cur[..2], ["-L", "muxrd-fake"]);
                    calls.push(cur[2..].to_vec());
                    cur = Vec::new();
                } else {
                    cur.push(line.to_owned());
                }
            }
            calls
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn target_helpers() {
        assert_eq!(session_target("dev"), "=dev");
        assert_eq!(session_scope("dev"), "=dev:");
    }

    #[test]
    fn templates_have_marker_and_ordered_keys() {
        let t = pane_template();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "@@");
        assert_eq!(lines.len(), PANE_KEYS.len() + 1);
        for (line, key) in lines[1..].iter().zip(PANE_KEYS) {
            assert!(line.starts_with(&format!("{key}=#{{")), "{line}");
        }
        assert!(!t.contains("#{s/"));
        assert!(!t.contains("pane_zoomed_flag"));
        assert!(!t.contains("pane_last_output_time"));
    }

    #[test]
    fn sessions_parse() {
        let text = "@@\nid=$3\ncreated=1700000000\nattached=1\nwindows=2\nname=work\n\
                    @@\nid=$4\ncreated=1700000100\nattached=0\nwindows=1\nname=a=b|c:d\n";
        let rows = parse_sessions(text).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            SessionRow {
                name: "work".into(),
                id: 3,
                created_unix: 1_700_000_000,
                attached: 1,
                windows: 2
            }
        );
        assert_eq!(rows[1].name, "a=b|c:d");
        assert_eq!(rows[1].id, 4);
    }

    #[test]
    fn windows_parse_with_space_in_name() {
        let text = "@@\nid=@2\nindex=1\nactive=1\nbell=0\nzoomed=1\nwidth=80\nheight=24\npanes=3\nname=a b\n";
        let rows = parse_windows(text).unwrap();
        assert_eq!(
            rows,
            vec![WindowRow {
                id: 2,
                index: 1,
                name: "a b".into(),
                active: true,
                bell: false,
                zoomed: true,
                width: 80,
                height: 24,
                panes: 3
            }]
        );
    }

    fn pane_record(floating: &str, z: &str, running: &str, title: &str) -> String {
        format!(
            "@@\nid=%5\nwindow=@2\nindex=0\nactive=1\ndead=0\nleft=0\ntop=0\nwidth=80\nheight=24\n\
             floating={floating}\nz={z}\nrunning={running}\npid=1234\ncommand=bash\n\
             path=/home/a b\ntitle={title}\n"
        )
    }

    #[test]
    fn pane_parses_title_with_separators() {
        let rows = parse_panes(&pane_record("0", "0", "", "a|b:c=d")).unwrap();
        assert_eq!(rows[0].title, "a|b:c=d");
        assert_eq!(rows[0].current_path, "/home/a b");
        assert_eq!(rows[0].id, 5);
        assert_eq!(rows[0].window_id, 2);
        assert_eq!(rows[0].pid, 1234);
    }

    #[test]
    fn pane_3_5a_empty_floating_fields() {
        let rows = parse_panes(&pane_record("", "", "", "t")).unwrap();
        assert!(!rows[0].floating);
        assert_eq!(rows[0].z, 0);
        assert_eq!(rows[0].command_running, None);
    }

    #[test]
    fn pane_3_8_command_running() {
        let rows = parse_panes(&pane_record("1", "2", "1", "t")).unwrap();
        assert!(rows[0].floating);
        assert_eq!(rows[0].z, 2);
        assert_eq!(rows[0].command_running, Some(true));
        let rows = parse_panes(&pane_record("0", "0", "0", "t")).unwrap();
        assert_eq!(rows[0].command_running, Some(false));
    }

    #[test]
    fn last_field_absorbs_stray_lines() {
        let text = format!(
            "{}{}",
            pane_record("0", "0", "", "first\nstray=1\nmore"),
            pane_record("0", "0", "", "next")
        );
        let rows = parse_panes(&text).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "first\nstray=1\nmore");
        assert_eq!(rows[1].title, "next");
    }

    #[test]
    fn short_or_misordered_record_is_an_error() {
        assert!(parse_sessions("@@\nid=$1\n").is_err());
        assert!(parse_sessions("@@\nname=x\ncreated=1\nattached=0\nwindows=1\nid=$1\n").is_err());
    }

    #[test]
    fn clients_parse() {
        let text = "@@\nreadonly=1\ncontrol=0\nwidth=100\nheight=40\ntty=/dev/pts/3\nname=/dev/pts/3\n\
                    @@\nreadonly=0\ncontrol=1\nwidth=\nheight=\ntty=\nname=client-1\n";
        let rows = parse_clients(text).unwrap();
        assert!(rows[0].readonly && !rows[0].control_mode);
        assert_eq!((rows[0].width, rows[0].height), (100, 40));
        assert!(rows[1].control_mode);
        assert_eq!(rows[1].name, "client-1");
        assert_eq!(rows[1].width, 0);
    }

    #[test]
    fn ack_maps_exit_status() {
        let f = Fake::new("3.8");
        f.set("out", "  hello \n");
        let ok = f.tmux.ack(&["x"]).unwrap();
        assert!(ok.ok);
        assert_eq!(ok.info.as_deref(), Some("hello"));
        assert!(ok.error.is_none());

        f.set("out", "");
        f.set("err", "can't find session: x\n");
        f.set("code", "1");
        let bad = f.tmux.ack(&["x"]).unwrap();
        assert!(!bad.ok);
        assert_eq!(bad.error.as_deref(), Some("can't find session: x"));
        assert!(bad.info.is_none());
    }

    #[test]
    fn list_sessions_without_server_is_empty() {
        let f = Fake::new("3.8");
        f.set("err", "no server running on /tmp/x\n");
        f.set("code", "1");
        assert!(f.tmux.list_sessions().unwrap().is_empty());
        f.set(
            "err",
            "error connecting to /tmp/x (No such file or directory)\n",
        );
        assert!(f.tmux.list_sessions().unwrap().is_empty());
        f.set("err", "something else\n");
        assert!(f.tmux.list_sessions().is_err());
    }

    #[test]
    fn session_vs_scope_targets() {
        let f = Fake::new("3.8");
        f.tmux.has_session("s").unwrap();
        f.tmux.kill_session("s").unwrap();
        f.tmux.rename_session("s", "t").unwrap();
        f.tmux.new_window("s", Some("w")).unwrap();
        f.tmux.split_window("s").unwrap();
        f.tmux.new_floating_pane("s").unwrap();
        f.tmux.list_windows("s").unwrap();
        f.tmux.list_panes("s").unwrap();
        f.tmux.list_clients("s").unwrap();
        let calls = f.calls();
        let target_of = |i: usize| {
            let c = &calls[i];
            c[c.iter().position(|a| a == "-t").unwrap() + 1].clone()
        };
        for i in [0, 1, 2, 6, 7, 8] {
            assert_eq!(target_of(i), "=s", "{:?}", calls[i]);
        }
        for i in [3, 4, 5] {
            assert_eq!(target_of(i), "=s:", "{:?}", calls[i]);
        }
        assert_eq!(calls[2], v(&["rename-session", "-t", "=s", "--", "t"]));
        assert_eq!(calls[7][..3], v(&["list-panes", "-s", "-t"])[..]);
    }

    #[test]
    fn new_session_form_and_ids_in_info() {
        let f = Fake::new("3.8");
        f.tmux.new_session("main").unwrap();
        f.set("out", "@12\n");
        let w = f.tmux.new_window("main", None).unwrap();
        assert_eq!(w.info.as_deref(), Some("12"));
        f.set("out", "%34\n");
        let p = f.tmux.split_window("main").unwrap();
        assert_eq!(p.info.as_deref(), Some("34"));
        let calls = f.calls();
        assert_eq!(
            calls[0],
            v(&["new-session", "-d", "-s", "main", "-x", "80", "-y", "24"])
        );
        assert_eq!(
            calls[1],
            v(&["new-window", "-t", "=main:", "-P", "-F", "#{window_id}"])
        );
        assert_eq!(
            calls[2],
            v(&["split-window", "-t", "=main:", "-P", "-F", "#{pane_id}"])
        );
    }

    #[test]
    fn send_keys_hex_chunks_at_256() {
        let f = Fake::new("3.8");
        let bytes: Vec<u8> = (0..600u32).map(|i| (i % 256) as u8).collect();
        assert!(f.tmux.send_keys_hex(7, &bytes).unwrap().ok);
        let calls = f.calls();
        assert_eq!(calls.len(), 3);
        for (c, n) in calls.iter().zip([256, 256, 88]) {
            assert_eq!(c[..4], v(&["send-keys", "-t", "%7", "-H"])[..]);
            assert_eq!(c.len() - 4, n);
        }
        assert_eq!(calls[0][4], "00");
        assert_eq!(calls[0][4 + 255], "ff");
        assert_eq!(calls[2][4], "00");
    }

    #[test]
    fn version_gates() {
        for (ver, floating, moving) in [
            ("3.5a", false, false),
            ("3.7c", true, false),
            ("3.8", true, true),
        ] {
            let f = Fake::new(ver);
            let fl = f.tmux.new_floating_pane("s").unwrap();
            assert_eq!(fl.ok, floating, "{ver}");
            if !floating {
                assert_eq!(fl.error.as_deref(), Some(FLOATING_NEEDS));
            }
            let fp = f.tmux.float_pane(3).unwrap();
            let tp = f.tmux.tile_pane(3, 4).unwrap();
            assert_eq!(fp.ok, moving, "{ver}");
            assert_eq!(tp.ok, moving, "{ver}");
            if !moving {
                assert_eq!(fp.error.as_deref(), Some(MOVE_NEEDS));
            }
            let calls = f.calls();
            let mut it = calls.iter();
            if floating {
                assert_eq!(
                    it.next().unwrap(),
                    &v(&["new-pane", "-t", "=s:", "-P", "-F", "#{pane_id}"])
                );
            }
            if moving {
                assert_eq!(it.next().unwrap(), &v(&["break-pane", "-W", "-s", "%3"]));
                assert_eq!(
                    it.next().unwrap(),
                    &v(&["join-pane", "-s", "%3", "-t", "%4"])
                );
            }
            assert!(it.next().is_none(), "{ver}: {calls:?}");
        }
    }

    #[test]
    fn resize_and_misc_forms() {
        let f = Fake::new("3.8");
        f.tmux.resize_pane(1, ResizeKind::Increase, None).unwrap();
        f.tmux.resize_pane(1, ResizeKind::Decrease, None).unwrap();
        f.tmux
            .resize_pane(1, ResizeKind::Increase, Some(ResizeDir::Up))
            .unwrap();
        f.tmux
            .resize_pane(1, ResizeKind::Decrease, Some(ResizeDir::Up))
            .unwrap();
        f.tmux.zoom_toggle(1).unwrap();
        f.tmux.set_pane_title(1, "a b").unwrap();
        f.tmux.rename_window(2, "x").unwrap();
        let c = f.calls();
        assert_eq!(c[0], v(&["resize-pane", "-t", "%1", "-R", "5"]));
        assert_eq!(c[1], v(&["resize-pane", "-t", "%1", "-L", "5"]));
        assert_eq!(c[2], v(&["resize-pane", "-t", "%1", "-U", "5"]));
        assert_eq!(c[3], v(&["resize-pane", "-t", "%1", "-D", "5"]));
        assert_eq!(c[4], v(&["resize-pane", "-Z", "-t", "%1"]));
        assert_eq!(c[5], v(&["select-pane", "-t", "%1", "-T", "a b"]));
        assert_eq!(c[6], v(&["rename-window", "-t", "@2", "--", "x"]));
    }

    #[test]
    fn scroll_enters_copy_mode_then_sends_command() {
        let f = Fake::new("3.8");
        let dirs = [
            (ScrollDir::Up, v(&["-N", "3", "scroll-up"])),
            (ScrollDir::Down, v(&["-N", "3", "scroll-down"])),
            (ScrollDir::ToTop, v(&["history-top"])),
            (ScrollDir::ToBottom, v(&["history-bottom"])),
            (ScrollDir::PageUp, v(&["page-up"])),
            (ScrollDir::PageDown, v(&["page-down"])),
            (ScrollDir::HalfPageUp, v(&["halfpage-up"])),
            (ScrollDir::HalfPageDown, v(&["halfpage-down"])),
        ];
        for (dir, _) in &dirs {
            assert!(f.tmux.scroll(9, *dir).unwrap().ok);
        }
        let calls = f.calls();
        for (i, (_, tail)) in dirs.iter().enumerate() {
            assert_eq!(calls[2 * i], v(&["copy-mode", "-t", "%9"]));
            let mut want = v(&["send-keys", "-t", "%9", "-X"]);
            want.extend(tail.clone());
            assert_eq!(calls[2 * i + 1], want);
        }
    }

    #[test]
    fn display_trims_trailing_newline() {
        let f = Fake::new("3.8");
        f.set("out", "hello world\n");
        assert_eq!(f.tmux.display("%1", "#{x}").unwrap(), "hello world");
        assert_eq!(
            f.calls()[0],
            v(&["display-message", "-p", "-t", "%1", "#{x}"])
        );
    }

    #[test]
    fn missing_binary_is_an_error() {
        let t = Tmux::with_bin(
            TmuxSocket::Path(PathBuf::from("/fake/sock")),
            TmuxVersion::parse("tmux 3.8").unwrap(),
            "/nonexistent/tmux-bin".into(),
        );
        assert!(t.run(&["list-sessions"]).is_err());
    }

    #[test]
    fn an_explicit_socket_path_is_passed_with_dash_s() {
        let f = Fake::new("3.8");
        let t = Tmux::with_bin(
            TmuxSocket::Path(PathBuf::from("/fake/sock")),
            TmuxVersion::parse("tmux 3.8").unwrap(),
            f.dir.join("tmux").into_os_string(),
        );
        t.run(&["list-sessions"]).unwrap();
        let log = std::fs::read_to_string(f.dir.join("log")).unwrap();
        let argv: Vec<&str> = log.lines().take_while(|l| *l != "==").collect();
        assert_eq!(argv, ["-S", "/fake/sock", "list-sessions"]);
    }
}
