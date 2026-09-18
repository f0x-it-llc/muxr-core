//! The dashboard — muxrctl's only screen.
//!
//! Five bordered sections stacked over a button row and a key-hint footer.
//! Everything else the control panel does is a dialog declared on top of this
//! (see [`super::render`]).
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │ muxrctl · muxrd control panel                               │
//! │ ┌ Daemon ──────────────────────────────────────────────────┐│
//! │ ┌ Network ─────────────────────────────────────────────────┐│
//! │ ┌ Certificate ─────────────────────────────────────────────┐│
//! │ ┌ Tokens ──────────────────────────────────────────────────┐│
//! │ ┌ Devices ─────────────────────────────────────────────────┐│
//! │ [Start] [Configure] [Certificate] [Tokens] [Devices] [Setup…]│
//! │ s start · c configure · … · q quit                          │
//! └─────────────────────────────────────────────────────────────┘
//! ```

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratcn::runtime::DeclareCtx;
use ratcn::{Button, Theme};

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::server::ServerMsg;
use crate::app::state::{DialogId, ServerInfo};

/// Rows each section body occupies, borders included.
const DAEMON_ROWS: u16 = 8;
const SECTION_ROWS: u16 = 4;

/// Declare the dashboard into the base layer.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // title
            Constraint::Length(DAEMON_ROWS),  // Daemon
            Constraint::Length(SECTION_ROWS), // Network
            Constraint::Length(SECTION_ROWS), // Certificate
            Constraint::Length(SECTION_ROWS), // Tokens
            Constraint::Length(SECTION_ROWS), // Devices
            Constraint::Min(0),               // slack
            Constraint::Length(1),            // button row
            Constraint::Length(1),            // key hints
        ])
        .split(area);

    let theme = *ctx.theme;
    let state = ctx.state();

    let title = Line::from(vec![
        Span::styled(
            "muxrctl",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " · muxrd control panel",
            Style::default().fg(theme.muted_foreground),
        ),
    ]);
    ctx.paint_widget(Paragraph::new(title), rows[0]);

    section(ctx, "Daemon", daemon_lines(state, &theme), rows[1]);
    section(ctx, "Network", network_lines(state, &theme), rows[2]);
    section(ctx, "Certificate", cert_lines(state, &theme), rows[3]);
    section(ctx, "Tokens", tokens_lines(state, &theme), rows[4]);
    section(ctx, "Devices", devices_lines(state, &theme), rows[5]);

    buttons(ctx, rows[7]);
    ctx.paint_widget(Paragraph::new(hints(&theme)), rows[8]);
}

/// Paint one bordered section with `title` and `lines` inside it.
fn section(
    ctx: &mut DeclareCtx<'_, AppState, UiMsg>,
    title: &'static str,
    lines: Vec<Line<'static>>,
    area: Rect,
) {
    let theme = *ctx.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.surface))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD),
        ));
    ctx.paint_widget(Paragraph::new(lines).block(block), area);
}

/// The button row: one stable id per action, each emitting its own message.
fn buttons(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, area: Rect) {
    let running = ctx.state().server.is_running();
    let server_button = if running {
        Button::new("Stop")
            .destructive()
            .on_press(|| UiMsg::Server(ServerMsg::StopRequested))
    } else {
        Button::new("Start").on_press(|| UiMsg::Server(ServerMsg::Start))
    };
    let rest = [
        (
            "btn_config",
            Button::new("Configure")
                .secondary()
                .on_press(|| UiMsg::Open(DialogId::Config)),
        ),
        (
            "btn_cert",
            Button::new("Certificate")
                .secondary()
                .on_press(|| UiMsg::Open(DialogId::Cert)),
        ),
        (
            "btn_tokens",
            Button::new("Tokens")
                .secondary()
                .on_press(|| UiMsg::Open(DialogId::Tokens)),
        ),
        (
            "btn_devices",
            Button::new("Devices")
                .secondary()
                .on_press(|| UiMsg::Open(DialogId::Devices)),
        ),
        (
            "btn_wizard",
            Button::new("Setup wizard")
                .secondary()
                .on_press(|| UiMsg::Open(DialogId::Wizard)),
        ),
    ];

    let mut widths = vec![Constraint::Length(server_button.width())];
    widths.extend(rest.iter().map(|(_, b)| Constraint::Length(b.width() + 1)));
    widths.push(Constraint::Min(0));
    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(widths)
        .split(area);

    ctx.component("btn_server", server_button, cells[0]);
    for (i, (id, button)) in rest.into_iter().enumerate() {
        ctx.component(id, button, cells[i + 1]);
    }
}

/// The Daemon section: running detail, the stopped prompt, or the first-load
/// banner.
fn daemon_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    let srv = &state.server;
    if is_first_load(srv.loading, srv.status.is_none(), srv.stopped) {
        return vec![field("Status", "Querying…", theme.warning, theme)];
    }
    match srv.status.as_ref() {
        Some(info) => running_lines(info, theme),
        None => vec![
            field("Status", "○ Stopped", theme.destructive, theme),
            Line::from(Span::styled(
                "  press s to start",
                Style::default().fg(theme.muted_foreground),
            )),
        ],
    }
}

/// The five detail rows of a running daemon.
fn running_lines(info: &ServerInfo, theme: &Theme) -> Vec<Line<'static>> {
    vec![
        field("Status", "● Running", theme.primary, theme),
        field("Version", info.version.clone(), theme.foreground, theme),
        field("Bind", info.bind_addr.clone(), theme.accent, theme),
        field("PID", info.pid.to_string(), theme.foreground, theme),
        field(
            "Uptime",
            format_uptime(info.uptime_secs),
            theme.foreground,
            theme,
        ),
        field(
            "Clients",
            info.client_count.to_string(),
            if info.client_count > 0 {
                theme.primary
            } else {
                theme.muted_foreground
            },
            theme,
        ),
    ]
}

/// The Network section: the configured bind address and the reachable IPs.
fn network_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    let cfg = &state.config;
    if cfg.host.is_empty() {
        return vec![field(
            "Bind",
            loading_or("(not yet loaded)", cfg.loading),
            theme.muted_foreground,
            theme,
        )];
    }
    let ips = if cfg.reachable_ips.is_empty() {
        "(none discovered)".to_string()
    } else {
        cfg.reachable_ips
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    vec![
        field("Bind", cfg.bind_addr(), theme.accent, theme),
        field("Reachable", ips, theme.foreground, theme),
    ]
}

/// The Certificate section: mode, short fingerprint, SAN count, advertised trust.
fn cert_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    let cert = &state.cert;
    let mode = match cert.tls_mode {
        Some(mode) => mode.label().to_string(),
        None => "daemon stopped — mode known at start".to_string(),
    };
    let fingerprint = match cert.fingerprint.as_deref() {
        Some(fp) => short_fingerprint(fp),
        None => loading_or("no certificate yet", cert.loading),
    };
    vec![
        field("Mode", mode, theme.foreground, theme),
        field("Fingerprint", fingerprint, theme.accent, theme),
        field(
            "SANs",
            format!(
                "{} · trust {}",
                cert.sans.len(),
                cert.advertise_trust.label()
            ),
            theme.foreground,
            theme,
        ),
    ]
}

/// The Tokens section: how many tokens, split by read-write / read-only.
fn tokens_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    let (rw, ro) = state.tokens.rw_ro_split();
    let count = if state.tokens.tokens.is_empty() {
        loading_or("none", state.tokens.loading)
    } else {
        format!("{} ({rw} rw · {ro} ro)", state.tokens.tokens.len())
    };
    vec![field("Tokens", count, theme.foreground, theme)]
}

/// The Devices section: how many push devices, and the relay they use.
fn devices_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    let devices = &state.devices;
    let count = if devices.devices.is_empty() {
        loading_or("none", devices.loading)
    } else {
        devices.devices.len().to_string()
    };
    let relay = devices
        .relay_url
        .clone()
        .unwrap_or_else(|| "push disabled".to_string());
    vec![
        field("Devices", count, theme.foreground, theme),
        field("Relay", relay, theme.foreground, theme),
    ]
}

/// One `label: value` row inside a section.
fn field(
    label: &'static str,
    value: impl Into<String>,
    value_color: ratatui::style::Color,
    theme: &Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<12}"),
            Style::default().fg(theme.muted_foreground),
        ),
        Span::styled(value.into(), Style::default().fg(value_color)),
    ])
}

/// The footer's key hints.
fn hints(theme: &Theme) -> Line<'static> {
    let accent = Style::default().fg(theme.primary);
    let muted = Style::default().fg(theme.muted_foreground);
    Line::from(vec![
        Span::styled("s", accent),
        Span::styled(" start/stop  ", muted),
        Span::styled("c", accent),
        Span::styled(" config  ", muted),
        Span::styled("e", accent),
        Span::styled(" cert  ", muted),
        Span::styled("t", accent),
        Span::styled(" tokens  ", muted),
        Span::styled("d", accent),
        Span::styled(" devices  ", muted),
        Span::styled("w", accent),
        Span::styled(" wizard  ", muted),
        Span::styled("r", accent),
        Span::styled(" refresh  ", muted),
        Span::styled("q", accent),
        Span::styled(" quit", muted),
    ])
}

/// `"…"` while a first load is in flight, `fallback` otherwise.
fn loading_or(fallback: &str, loading: bool) -> String {
    if loading {
        "…".to_string()
    } else {
        fallback.to_string()
    }
}

/// First 16 hex chars of a fingerprint, elided.
fn short_fingerprint(fp: &str) -> String {
    if fp.len() > 16 {
        format!("{}…", &fp[..16])
    } else {
        fp.to_string()
    }
}

/// Returns `true` while the very first status load is still in progress —
/// i.e. we have never received a result yet (`status` is `None`) and we do not
/// yet know whether the daemon is stopped (`stopped == false`).
///
/// Once `stopped` is set to `true` by a `StatusLoaded(None)` result, background
/// polls that set `loading = true` every ~1 s must **not** show the "Querying…"
/// banner, because the daemon is known-stopped and the panel should stay steady.
pub(crate) fn is_first_load(loading: bool, status_is_none: bool, stopped: bool) -> bool {
    loading && status_is_none && !stopped
}

/// Format an uptime duration in seconds as a human-readable string.
fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m {}s", secs % 60);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h {}m", mins % 60);
    }
    let days = hours / 24;
    format!("{days}d {}h", hours % 24)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_first_load predicate ───────────────────────────────────────────────

    /// The "Querying…" banner must only appear on the very first load cycle,
    /// before stopped/running is known. Once `stopped` is `true`, background
    /// polls must not trigger the banner even when `loading == true`.
    #[test]
    fn first_load_true_only_when_loading_and_no_status_and_not_stopped() {
        assert!(is_first_load(true, true, false));
        assert!(!is_first_load(true, true, true));
        assert!(!is_first_load(false, true, false));
        assert!(!is_first_load(true, false, false));
        assert!(!is_first_load(false, false, false));
    }

    // ── format_uptime ─────────────────────────────────────────────────────────

    #[test]
    fn format_uptime_seconds() {
        assert_eq!(format_uptime(0), "0s");
        assert_eq!(format_uptime(59), "59s");
    }

    #[test]
    fn format_uptime_minutes() {
        assert_eq!(format_uptime(60), "1m 0s");
        assert_eq!(format_uptime(90), "1m 30s");
        assert_eq!(format_uptime(3599), "59m 59s");
    }

    #[test]
    fn format_uptime_hours() {
        assert_eq!(format_uptime(3600), "1h 0m");
        assert_eq!(format_uptime(7384), "2h 3m");
    }

    #[test]
    fn format_uptime_days() {
        assert_eq!(format_uptime(86400), "1d 0h");
        assert_eq!(format_uptime(90000), "1d 1h");
    }

    #[test]
    fn short_fingerprint_elides_past_16_chars() {
        assert_eq!(short_fingerprint("abcd"), "abcd");
        assert_eq!(
            short_fingerprint("0123456789abcdef0123"),
            "0123456789abcdef…"
        );
    }
}
