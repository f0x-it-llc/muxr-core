//! Devices screen — list registered push-notification devices, remove
//! (revoke) selected ones, and show the push relay URL + device count.
//!
//! # Key bindings
//! - `j`/`↓`: move cursor down.
//! - `k`/`↑`: move cursor up.
//! - `d`/`x`: remove the selected device.
//! - `r`: reload the device list.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::app::AppState;
use crate::tui::theme::{palette, styles};

/// Render the Devices screen.
pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let block = styles::panel(true).title(Span::styled(" Devices ", styles::heading()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Vertical split:
    //  - relay URL + device-count summary
    //  - device list
    //  - status line
    //  - hints
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // relay/summary panel (bordered, needs 2 content rows)
            Constraint::Min(4),    // device list
            Constraint::Length(1), // status line
            Constraint::Length(2), // hints
        ])
        .split(inner);

    render_summary(frame, state, rows[0]);
    render_device_list(frame, state, rows[1]);
    render_status(frame, state, rows[2]);
    render_hints(frame, rows[3]);
}

/// Render the relay URL + registered-device-count summary panel.
fn render_summary(frame: &mut Frame, state: &AppState, area: Rect) {
    let block = styles::panel(false).title(Span::styled(" Relay ", styles::muted()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let relay_line = match &state.devices.relay_url {
        Some(url) => Line::from(vec![
            Span::styled("  Relay: ", styles::muted()),
            Span::styled(url.as_str(), styles::accent()),
        ]),
        None => Line::from(vec![
            Span::styled("  Relay: ", styles::muted()),
            Span::styled("disabled", styles::status_warn()),
        ]),
    };
    let count_line = Line::from(vec![
        Span::styled("  Registered devices: ", styles::muted()),
        Span::styled(
            state.devices.devices.len().to_string(),
            styles::accent_bold(),
        ),
    ]);

    frame.render_widget(
        Paragraph::new(vec![relay_line, count_line])
            .style(Style::default().bg(palette::BG_SURFACE)),
        inner,
    );
}

/// Render the list of registered devices.
fn render_device_list(frame: &mut Frame, state: &AppState, area: Rect) {
    let block = styles::panel(false).title(Span::styled(" Device List ", styles::muted()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.devices.loading && state.devices.devices.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  Loading devices…", styles::status_warn()))
                .style(Style::default().bg(palette::BG_SURFACE)),
            inner,
        );
        return;
    }

    if state.devices.devices.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  No devices registered yet.",
                    styles::muted(),
                )),
            ])
            .style(Style::default().bg(palette::BG_SURFACE)),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = state
        .devices
        .devices
        .iter()
        .map(|d| {
            let name_span = Span::styled(d.device_name.as_str(), styles::body());
            let platform_span = Span::styled(format!("  [{}]", d.platform), styles::accent());
            let age_span = Span::styled(
                format!("  {}", humanize_registered_at(d.registered_at)),
                styles::muted(),
            );
            let handle_span = Span::styled(format!("  {}…", d.handle_prefix), styles::muted());
            ListItem::new(Line::from(vec![
                Span::raw("  "),
                name_span,
                platform_span,
                age_span,
                handle_span,
            ]))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(state.devices.cursor));

    let list = List::new(items)
        .style(Style::default().bg(palette::BG_SURFACE))
        .highlight_style(Style::default().bg(palette::BG_HOVER).fg(palette::TEAL))
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, inner, &mut list_state);
}

/// Render the status line.
fn render_status(frame: &mut Frame, state: &AppState, area: Rect) {
    let status = &state.devices.status;
    if status.is_empty() {
        return;
    }
    let style = if status.starts_with("Error") {
        styles::status_err()
    } else if state.devices.loading {
        styles::status_warn()
    } else {
        styles::status_ok()
    };
    frame.render_widget(
        Paragraph::new(Span::styled(status.as_str(), style))
            .style(Style::default().bg(palette::BG_SURFACE)),
        area,
    );
}

/// Render key-binding hints.
fn render_hints(frame: &mut Frame, area: Rect) {
    let hint = Line::from(vec![
        Span::styled("d/x", styles::accent()),
        Span::styled(" remove  ", styles::muted()),
        Span::styled("r", styles::accent()),
        Span::styled(" refresh  ", styles::muted()),
        Span::styled("j/k", styles::accent()),
        Span::styled(" move", styles::muted()),
    ]);
    frame.render_widget(Paragraph::new(hint), area);
}

/// Format a unix-epoch-seconds registration timestamp as a short "time ago"
/// string for display.
///
/// Pure (no I/O beyond reading the wall clock) so it is cheap to call every
/// render tick. A timestamp at or after "now" (clock skew, or freshly
/// registered) renders as "just now" rather than underflowing.
fn humanize_registered_at(epoch_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(epoch_secs);
    let elapsed = now.saturating_sub(epoch_secs);
    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn humanize_just_now() {
        assert_eq!(humanize_registered_at(now_secs()), "just now");
    }

    #[test]
    fn humanize_minutes_ago() {
        assert_eq!(humanize_registered_at(now_secs() - 300), "5m ago");
    }

    #[test]
    fn humanize_hours_ago() {
        assert_eq!(humanize_registered_at(now_secs() - 7_200), "2h ago");
    }

    #[test]
    fn humanize_days_ago() {
        assert_eq!(humanize_registered_at(now_secs() - 172_800), "2d ago");
    }

    #[test]
    fn humanize_future_timestamp_clamped_to_just_now() {
        // Clock skew: a registration timestamp slightly ahead of "now" must not
        // underflow (`saturating_sub`) — it renders as "just now".
        assert_eq!(humanize_registered_at(now_secs() + 10), "just now");
    }
}
