//! Status parser: the per-pane agent status scanner and its state machine.
//!
//! The module is pure: no I/O and no threads. The watcher feeds it the
//! octal-unescaped output bytes of a pane and cheap per-pane facts, and
//! turns the results into agent rows.
//!
//! Reports follow the program-status OSC 7501 protocol (revision 0.4,
//! 2026-10-10): `ESC ] 7501 ; pairs ST` where `ST` is `ESC \` or BEL,
//! `pairs := pair (":" pair)*`, `pair := key "=" value`. A body of `?` is a
//! capability probe.
//!
//! One-root-record decision: a pane shows one status, so only the root
//! record (a report without an `id`) updates [`PaneStatus`]. Records that
//! carry an `id` describe sub-tasks and are parsed but ignored.
//!
//! Panes that never send a report fall back to [`heuristic`], which infers a
//! status from the foreground command, the pane title and output recency.

use std::time::{Duration, Instant};

use crate::multiplexer::types::AgentStatus;

/// Largest report body kept; longer reports are discarded.
const MAX_REPORT_BYTES: usize = 4096;
/// Introducer bytes of a status report.
const PREFIX: &[u8] = b"\x1b]7501;";
const MAX_KEY: usize = 16;
const MAX_APP: usize = 32;
const MAX_ID: usize = 128;
const MAX_TITLE: usize = 192;
const MAX_MSG: usize = 2048;

/// Commands the heuristic path recognises as agents.
pub const KNOWN_AGENTS: [&str; 8] = [
    "claude",
    "codex",
    "gemini",
    "opencode",
    "aider",
    "pi",
    "amp",
    "cursor-agent",
];

/// Title fragments that mean the agent is waiting for the user.
const BLOCKED_MARKERS: [&str; 5] = ["Do you want", "Allow", "(y/n)", "Waiting for", "needs your"];

/// Window within which recent output counts as work.
const WORKING_WINDOW: Duration = Duration::from_secs(5);

/// The `state` of a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}

/// The `kind` of a blocked report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Permission,
    Question,
    Auth,
}

impl BlockKind {
    fn label(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }
}

/// One parsed report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// The `?` capability probe.
    Probe,
    /// A status report.
    Status {
        state: ReportState,
        /// Honoured only together with `state=blocked`.
        kind: Option<BlockKind>,
        progress: Option<u8>,
        app: Option<String>,
        title: Option<String>,
        msg: Option<String>,
        id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScanState {
    /// Matching the introducer; `matched` bytes of it seen.
    Prefix { matched: usize },
    /// Inside a body.
    Body { esc: bool, overflow: bool },
}

/// Per-pane scanner that finds reports across chunk boundaries.
#[derive(Debug, Clone)]
pub struct Osc7501Scanner {
    state: ScanState,
    buf: Vec<u8>,
}

impl Default for Osc7501Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Osc7501Scanner {
    pub fn new() -> Self {
        Self {
            state: ScanState::Prefix { matched: 0 },
            buf: Vec::new(),
        }
    }

    /// Consume `bytes`, returning every complete, valid report in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Report> {
        let mut out = Vec::new();
        for &b in bytes {
            match self.state {
                ScanState::Prefix { matched } => {
                    self.state = if b == PREFIX[matched] {
                        if matched + 1 == PREFIX.len() {
                            self.buf.clear();
                            ScanState::Body {
                                esc: false,
                                overflow: false,
                            }
                        } else {
                            ScanState::Prefix {
                                matched: matched + 1,
                            }
                        }
                    } else if b == 0x1b {
                        ScanState::Prefix { matched: 1 }
                    } else {
                        ScanState::Prefix { matched: 0 }
                    };
                }
                ScanState::Body { esc, overflow } => {
                    let terminated = b == 0x07 || (esc && b == b'\\');
                    if terminated {
                        if !overflow && let Some(r) = parse_body(&self.buf) {
                            out.push(r);
                        }
                        self.buf.clear();
                        self.state = ScanState::Prefix { matched: 0 };
                    } else if esc && b != 0x1b {
                        // An ESC that does not start ST aborts the report.
                        self.buf.clear();
                        self.state = ScanState::Prefix { matched: 0 };
                    } else if b == 0x1b {
                        self.state = ScanState::Body {
                            esc: true,
                            overflow,
                        };
                    } else if overflow || self.buf.len() >= MAX_REPORT_BYTES {
                        self.buf.clear();
                        self.state = ScanState::Body {
                            esc: false,
                            overflow: true,
                        };
                    } else {
                        self.buf.push(b);
                        self.state = ScanState::Body {
                            esc: false,
                            overflow: false,
                        };
                    }
                }
            }
        }
        out
    }
}

fn parse_body(body: &[u8]) -> Option<Report> {
    if body == b"?" {
        return Some(Report::Probe);
    }
    let text = std::str::from_utf8(body).ok()?;
    let mut state = None;
    let mut kind = None;
    let mut progress = None;
    let mut app = None;
    let mut title = None;
    let mut msg = None;
    let mut id = None;
    for pair in text.split(':') {
        let (key, value) = pair.split_once('=')?;
        if key.is_empty()
            || key.len() > MAX_KEY
            || !key.bytes().all(|c| c.is_ascii_lowercase())
            || !value.bytes().all(valid_value_byte)
        {
            return None;
        }
        match key {
            "state" => {
                state = match value {
                    "idle" => Some(ReportState::Idle),
                    "working" => Some(ReportState::Working),
                    "done" => Some(ReportState::Done),
                    "blocked" => Some(ReportState::Blocked),
                    "error" => Some(ReportState::Error),
                    "clear" => Some(ReportState::Clear),
                    _ => None,
                };
            }
            "kind" => {
                kind = match value {
                    "permission" => Some(BlockKind::Permission),
                    "question" => Some(BlockKind::Question),
                    "auth" => Some(BlockKind::Auth),
                    _ => None,
                };
            }
            "progress" => {
                progress = value.parse::<u8>().ok().filter(|p| *p <= 100);
            }
            "app" => {
                if value.len() > MAX_APP {
                    return None;
                }
                app = Some(value.to_string());
            }
            "id" => {
                if value.len() > MAX_ID {
                    return None;
                }
                id = Some(value.to_string());
            }
            "title" => title = Some(decode_text(value, MAX_TITLE)?),
            "msg" => msg = Some(decode_text(value, MAX_MSG)?),
            _ => {}
        }
    }
    let state = state?;
    Some(Report::Status {
        state,
        kind: if state == ReportState::Blocked {
            kind
        } else {
            None
        },
        progress,
        app,
        title,
        msg,
        id,
    })
}

fn valid_value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

/// Decode base64 text into UTF-8 with no control characters and at most
/// `max` decoded bytes.
fn decode_text(value: &str, max: usize) -> Option<String> {
    let bytes = decode_base64(value)?;
    if bytes.len() > max {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text
        .chars()
        .any(|c| matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}'))
    {
        return None;
    }
    Some(text)
}

/// Standard-alphabet base64 decoder; trailing `=` padding is optional.
pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let data = input.trim_end_matches('=');
    if input.len() - data.len() > 2 || data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in data.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Per-pane state machine fed by [`Report`]s.
#[derive(Debug, Clone, Default)]
pub struct PaneStatus {
    status: Option<AgentStatus>,
    app: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    has_report: bool,
}

impl PaneStatus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a report. Returns the new status when the pane's status,
    /// summary, title or kind changed, so the caller refreshes its row.
    /// Probes and sub-records (reports with an `id`) change nothing.
    pub fn apply(&mut self, r: &Report, _now: Instant) -> Option<AgentStatus> {
        let Report::Status {
            state,
            kind,
            progress,
            app,
            title,
            msg,
            id,
        } = r
        else {
            return None;
        };
        if id.is_some() {
            return None;
        }
        let before = (
            self.status,
            self.app.clone(),
            self.title.clone(),
            self.summary.clone(),
        );
        if *state == ReportState::Clear {
            self.app = None;
            self.title = None;
        }
        if app.is_some() {
            self.app = app.clone();
        }
        if title.is_some() {
            self.title = title.clone();
        }
        let (status, summary) = match state {
            ReportState::Working => (
                AgentStatus::Working,
                match (msg, progress) {
                    (Some(m), Some(p)) => Some(format!("{m} ({p}%)")),
                    (Some(m), None) => Some(m.clone()),
                    (None, Some(p)) => Some(format!("working {p}%")),
                    (None, None) => None,
                },
            ),
            ReportState::Blocked => (
                AgentStatus::Blocked,
                match (kind, msg) {
                    (Some(k), Some(m)) => Some(format!("{}: {m}", k.label())),
                    (Some(k), None) => Some(k.label().to_string()),
                    (None, Some(m)) => Some(m.clone()),
                    (None, None) => None,
                },
            ),
            ReportState::Done => (AgentStatus::Done, msg.clone()),
            ReportState::Error => (
                AgentStatus::Blocked,
                Some(format!("error: {}", msg.as_deref().unwrap_or("error"))),
            ),
            ReportState::Idle | ReportState::Clear => (AgentStatus::Idle, None),
        };
        self.status = Some(status);
        self.summary = summary;
        self.has_report = true;
        let after = (
            self.status,
            self.app.clone(),
            self.title.clone(),
            self.summary.clone(),
        );
        (before != after).then_some(status)
    }

    /// Current status; `Unknown` until a report has been applied.
    pub fn status(&self) -> AgentStatus {
        self.status.unwrap_or(AgentStatus::Unknown)
    }

    /// The reporting application, used as the agent kind.
    pub fn kind(&self) -> Option<&str> {
        self.app.as_deref()
    }

    /// Human-readable kind; the same as [`Self::kind`].
    pub fn display_kind(&self) -> Option<&str> {
        self.app.as_deref()
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Whether any root status report has been applied to this pane.
    pub fn has_report(&self) -> bool {
        self.has_report
    }
}

/// Base name of a command path.
fn command_name(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

/// Display kind for an inferred status: `"<cmd> (inferred)"`.
pub fn inferred_display_kind(cmd: &str) -> String {
    format!("{} (inferred)", command_name(cmd))
}

/// Infer a status for a pane that never sent a report.
///
/// `None` when `cmd` is not a known agent. `last_output_age` is `None` when
/// the pane has printed nothing since the watcher started. A `Working`
/// status whose command has stopped becomes `Done`, and stays `Done` while
/// the command stays stopped.
pub fn heuristic(
    cmd: &str,
    title: &str,
    last_output_age: Option<Duration>,
    command_running: Option<bool>,
    prev: Option<AgentStatus>,
) -> Option<AgentStatus> {
    if !KNOWN_AGENTS.contains(&command_name(cmd)) {
        return None;
    }
    if BLOCKED_MARKERS.iter().any(|m| title.contains(m)) {
        return Some(AgentStatus::Blocked);
    }
    if command_running == Some(false)
        && matches!(prev, Some(AgentStatus::Working | AgentStatus::Done))
    {
        return Some(AgentStatus::Done);
    }
    match last_output_age {
        Some(age) if age < WORKING_WINDOW => Some(AgentStatus::Working),
        _ => Some(AgentStatus::Idle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    fn st(body: &str) -> Vec<u8> {
        format!("\x1b]7501;{body}\x1b\\").into_bytes()
    }

    fn scan(bytes: &[u8]) -> Vec<Report> {
        Osc7501Scanner::new().feed(bytes)
    }

    fn status_of(r: &Report) -> ReportState {
        match r {
            Report::Status { state, .. } => *state,
            Report::Probe => panic!("probe"),
        }
    }

    #[test]
    fn base64_decoder_matches_reference() {
        for s in [
            "",
            "a",
            "ab",
            "abc",
            "abcd",
            "héllo wörld",
            "Apply 3 to add",
        ] {
            let enc = b64(s);
            assert_eq!(decode_base64(&enc).unwrap(), s.as_bytes(), "{enc}");
            let bare = enc.trim_end_matches('=');
            assert_eq!(decode_base64(bare).unwrap(), s.as_bytes(), "{bare}");
        }
        assert!(decode_base64("a").is_none());
        assert!(decode_base64("ab!d").is_none());
        assert!(decode_base64("ab===").is_none());
    }

    #[test]
    fn working_report_with_msg() {
        let r = scan(&st(&format!(
            "state=working:app=brew:msg={}",
            b64("Fetching")
        )));
        assert_eq!(r.len(), 1);
        match &r[0] {
            Report::Status {
                state, app, msg, ..
            } => {
                assert_eq!(*state, ReportState::Working);
                assert_eq!(app.as_deref(), Some("brew"));
                assert_eq!(msg.as_deref(), Some("Fetching"));
            }
            Report::Probe => panic!(),
        }
    }

    #[test]
    fn blocked_auth_maps_to_prefixed_summary() {
        let r = scan(&st(&format!(
            "state=blocked:kind=auth:app=brew:msg={}",
            b64("Sign in")
        )));
        let mut p = PaneStatus::new();
        assert_eq!(p.apply(&r[0], Instant::now()), Some(AgentStatus::Blocked));
        assert_eq!(p.summary(), Some("auth: Sign in"));
        assert_eq!(p.kind(), Some("brew"));
    }

    #[test]
    fn blocked_permission_spec_fixture() {
        let r = scan(&st(
            "state=blocked:kind=permission:app=terraform:msg=QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/",
        ));
        assert_eq!(r.len(), 1);
        let mut p = PaneStatus::new();
        p.apply(&r[0], Instant::now());
        assert_eq!(p.status(), AgentStatus::Blocked);
        assert!(
            p.summary()
                .unwrap()
                .starts_with("permission: Apply 3 to add")
        );
        assert_eq!(p.kind(), Some("terraform"));
    }

    #[test]
    fn codex_forms() {
        let r = scan(&[st("state=working:app=codex"), st("state=clear")].concat());
        assert_eq!(r.len(), 2);
        let mut p = PaneStatus::new();
        let now = Instant::now();
        assert_eq!(p.apply(&r[0], now), Some(AgentStatus::Working));
        assert_eq!(p.kind(), Some("codex"));
        assert_eq!(p.apply(&r[1], now), Some(AgentStatus::Idle));
        assert_eq!(p.kind(), None);
        assert!(p.has_report());
    }

    #[test]
    fn terminators_bel_and_st() {
        let r = scan(b"junk\x1b]7501;state=idle\x07more\x1b]7501;state=done\x1b\\x");
        assert_eq!(r.len(), 2);
        assert_eq!(status_of(&r[0]), ReportState::Idle);
        assert_eq!(status_of(&r[1]), ReportState::Done);
    }

    #[test]
    fn split_at_every_offset() {
        let bytes = [
            b"ab".as_slice(),
            &st(&format!("state=working:msg={}", b64("hi"))),
            b"\x1b]7501;state=idle\x07",
        ]
        .concat();
        let whole = scan(&bytes);
        assert_eq!(whole.len(), 2);
        for i in 0..=bytes.len() {
            let mut s = Osc7501Scanner::new();
            let mut got = s.feed(&bytes[..i]);
            got.extend(s.feed(&bytes[i..]));
            assert_eq!(got, whole, "split at {i}");
        }
    }

    #[test]
    fn oversize_report_dropped_and_scanner_recovers() {
        let big = format!("state=working:app={}", "a".repeat(5000));
        let bytes = [st(&big), st("state=done")].concat();
        let r = scan(&bytes);
        assert_eq!(r.len(), 1);
        assert_eq!(status_of(&r[0]), ReportState::Done);
        let mut s = Osc7501Scanner::new();
        s.feed(b"\x1b]7501;state=working:");
        s.feed(&vec![b'a'; 20_000]);
        assert!(s.buf.len() <= MAX_REPORT_BYTES);
    }

    #[test]
    fn control_char_drops_whole_report() {
        let r = scan(&st(&format!("state=working:msg={}", b64("a\u{1}b"))));
        assert!(r.is_empty());
        let r = scan(&st(&format!("state=working:title={}", b64("a\u{85}b"))));
        assert!(r.is_empty());
        let r = scan(&st(&format!("state=working:msg={}", b64("a\x7fb"))));
        assert!(r.is_empty());
    }

    #[test]
    fn length_limits() {
        let ok = b64(&"x".repeat(2048));
        assert_eq!(scan(&st(&format!("state=idle:msg={ok}"))).len(), 1);
        let bad = b64(&"x".repeat(2049));
        assert!(scan(&st(&format!("state=idle:msg={bad}"))).is_empty());
        let bad = b64(&"x".repeat(193));
        assert!(scan(&st(&format!("state=idle:title={bad}"))).is_empty());
        assert!(scan(&st(&format!("state=idle:app={}", "a".repeat(33)))).is_empty());
        assert!(scan(&st(&format!("state=idle:{}=1", "a".repeat(17)))).is_empty());
        assert!(scan(&st(&format!("state=idle:id={}", "a".repeat(129)))).is_empty());
    }

    #[test]
    fn unknown_key_ignored_and_last_wins() {
        let r = scan(&st("state=working:bogus=1:state=done:state=blocked"));
        assert_eq!(r.len(), 1);
        assert_eq!(status_of(&r[0]), ReportState::Blocked);
    }

    #[test]
    fn missing_or_unknown_state_ignored() {
        assert!(scan(&st("app=brew")).is_empty());
        assert!(scan(&st("state=dancing")).is_empty());
        assert!(scan(&st("state=idle:novalue")).is_empty());
        assert!(scan(&st("state=idle:Bad=1")).is_empty());
        assert!(scan(&st("state=idle:msg=a b")).is_empty());
    }

    #[test]
    fn kind_only_on_blocked() {
        let r = scan(&st("state=working:kind=auth"));
        match &r[0] {
            Report::Status { kind, .. } => assert_eq!(*kind, None),
            Report::Probe => panic!(),
        }
    }

    #[test]
    fn sub_record_ignored_for_pane_status() {
        let r = scan(&[st("state=working:app=x"), st("state=blocked:id=a/b")].concat());
        assert_eq!(r.len(), 2);
        let mut p = PaneStatus::new();
        let now = Instant::now();
        assert!(p.apply(&r[0], now).is_some());
        assert_eq!(p.apply(&r[1], now), None);
        assert_eq!(p.status(), AgentStatus::Working);
    }

    #[test]
    fn probe_recognised() {
        assert_eq!(scan(&st("?")), vec![Report::Probe]);
        let mut p = PaneStatus::new();
        assert_eq!(p.apply(&Report::Probe, Instant::now()), None);
        assert!(!p.has_report());
    }

    #[test]
    fn interrupted_sequence_does_not_poison_scanner() {
        let r = scan(b"\x1b]7501;state=wor\x1b[31mred\x1b]7501;state=done\x07");
        assert_eq!(r.len(), 1);
        assert_eq!(status_of(&r[0]), ReportState::Done);
    }

    fn apply_one(body: &str) -> (PaneStatus, Option<AgentStatus>) {
        let r = scan(&st(body));
        let mut p = PaneStatus::new();
        let out = p.apply(&r[0], Instant::now());
        (p, out)
    }

    #[test]
    fn mapping_table() {
        let cases = [
            ("state=idle", AgentStatus::Idle, None),
            (
                "state=working:progress=42",
                AgentStatus::Working,
                Some("working 42%"),
            ),
            (
                "state=blocked:kind=question",
                AgentStatus::Blocked,
                Some("question"),
            ),
            ("state=done", AgentStatus::Done, None),
            ("state=error", AgentStatus::Blocked, Some("error: error")),
            ("state=clear", AgentStatus::Idle, None),
        ];
        for (body, want, summary) in cases {
            let (p, out) = apply_one(body);
            assert_eq!(out, Some(want), "{body}");
            assert_eq!(p.status(), want, "{body}");
            assert_eq!(p.summary(), summary, "{body}");
            assert_ne!(p.status(), AgentStatus::Unknown);
        }
        let (p, _) = apply_one(&format!("state=error:msg={}", b64("boom")));
        assert_eq!(p.summary(), Some("error: boom"));
    }

    #[test]
    fn repeated_identical_report_is_not_a_change() {
        let r = scan(&st("state=working:app=x"));
        let mut p = PaneStatus::new();
        let now = Instant::now();
        assert!(p.apply(&r[0], now).is_some());
        assert_eq!(p.apply(&r[0], now), None);
    }

    #[test]
    fn heuristic_non_agent_is_none() {
        for cmd in ["zsh", "vim", "bash", ""] {
            assert_eq!(heuristic(cmd, "", None, None, None), None, "{cmd}");
        }
    }

    #[test]
    fn heuristic_tables_for_each_agent() {
        let secs = Duration::from_secs;
        for cmd in KNOWN_AGENTS {
            assert_eq!(
                heuristic(cmd, "", Some(secs(1)), None, None),
                Some(AgentStatus::Working),
                "{cmd}"
            );
            assert_eq!(
                heuristic(cmd, "", Some(secs(30)), None, None),
                Some(AgentStatus::Idle),
                "{cmd}"
            );
            assert_eq!(
                heuristic(cmd, "", None, None, None),
                Some(AgentStatus::Idle),
                "{cmd}"
            );
            assert_eq!(
                heuristic(cmd, "Do you want to proceed?", Some(secs(1)), None, None),
                Some(AgentStatus::Blocked),
                "{cmd}"
            );
            assert_eq!(
                heuristic(
                    cmd,
                    "",
                    Some(secs(30)),
                    Some(false),
                    Some(AgentStatus::Working)
                ),
                Some(AgentStatus::Done),
                "{cmd}"
            );
            assert_eq!(inferred_display_kind(cmd), format!("{cmd} (inferred)"));
        }
        assert_eq!(
            heuristic("/usr/bin/claude", "", Some(secs(1)), None, None),
            Some(AgentStatus::Working)
        );
    }

    #[test]
    fn heuristic_blocked_markers_and_done_rules() {
        let old = Some(Duration::from_secs(60));
        for t in [
            "Do you want to",
            "Allow tool",
            "ok? (y/n)",
            "Waiting for input",
            "Claude needs your attention",
        ] {
            assert_eq!(
                heuristic("claude", t, old, None, None),
                Some(AgentStatus::Blocked),
                "{t}"
            );
        }
        // Not running without a prior Working does not produce Done.
        assert_eq!(
            heuristic("codex", "", old, Some(false), Some(AgentStatus::Idle)),
            Some(AgentStatus::Idle)
        );
        // Done holds while the command stays stopped, then output resumes.
        assert_eq!(
            heuristic("codex", "", old, Some(false), Some(AgentStatus::Done)),
            Some(AgentStatus::Done)
        );
        assert_eq!(
            heuristic(
                "codex",
                "",
                Some(Duration::from_secs(1)),
                Some(true),
                Some(AgentStatus::Done)
            ),
            Some(AgentStatus::Working)
        );
    }
}
