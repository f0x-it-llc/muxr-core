//! Tokens screen — list, create (with read-only toggle), and revoke API tokens.
//!
//! # Key bindings (Browsing mode)
//! - `j`/`↓`: move cursor down.
//! - `k`/`↑`: move cursor up.
//! - `c`: open the create form.
//! - `d`/`x`: revoke the selected token.
//! - `r`: reload the token list.
//! - `Esc`: dismiss the one-time minted-secret banner.
//!
//! # Key bindings (Creating mode)
//! - Type to set the optional token name.
//! - `Ctrl-Space`: toggle read-only.
//! - `Ctrl-E`: cycle the expiry (never → 30m → 1h → 24h → 7d).
//! - `Enter`: submit (`c`, `q`, Space etc. are literal name characters).
//! - `Esc`: cancel.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::app::AppState;
use crate::app::state::TokensFormPhase;
use crate::tui::theme::{palette, styles};

/// Render the Tokens screen.
pub fn render(frame: &mut Frame, state: &AppState, area: Rect) {
    let block = styles::panel(true).title(Span::styled(" Tokens ", styles::heading()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Vertical split:
    //  - minted-secret banner (shown when a token was just created, else 0 height)
    //  - token list
    //  - create form (shown in Creating mode)
    //  - status line
    //  - hints
    let secret_height: u16 = if state.tokens.last_minted_secret.is_some() {
        5
    } else {
        0
    };
    let form_height: u16 = if state.tokens.form_phase == TokensFormPhase::Creating {
        5
    } else {
        0
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(secret_height), // minted-secret banner
            Constraint::Min(4),                // token list
            Constraint::Length(form_height),   // create form
            Constraint::Length(1),             // status line
            Constraint::Length(2),             // hints
        ])
        .split(inner);

    if secret_height > 0 {
        render_secret_banner(frame, state, rows[0]);
    }
    render_token_list(frame, state, rows[1]);
    if form_height > 0 {
        render_create_form(frame, state, rows[2]);
    }
    render_status(frame, state, rows[3]);
    render_hints(frame, state.tokens.form_phase, rows[4]);
}

/// Render the one-time minted-secret banner.
fn render_secret_banner(frame: &mut Frame, state: &AppState, area: Rect) {
    if let Some((name, secret, _read_only)) = &state.tokens.last_minted_secret {
        let block = styles::panel(false).title(Span::styled(
            " New Token — copy now, shown once! ",
            styles::status_warn(),
        ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines = vec![
            Line::from(vec![
                Span::styled("  Name:   ", styles::muted()),
                Span::styled(name.as_str(), styles::accent()),
            ]),
            Line::from(vec![
                Span::styled("  Secret: ", styles::muted()),
                Span::styled(secret.as_str(), styles::accent_bold()),
            ]),
            Line::from(vec![
                Span::styled("  ", styles::muted()),
                Span::styled("Enter", styles::accent()),
                Span::styled(" → Show pairing QR (fullscreen)", styles::muted()),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(palette::BG_SURFACE)),
            inner,
        );
    }
}

/// Render the list of existing tokens.
fn render_token_list(frame: &mut Frame, state: &AppState, area: Rect) {
    let block = styles::panel(false).title(Span::styled(" Token List ", styles::muted()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.tokens.loading && state.tokens.tokens.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  Loading tokens…", styles::status_warn()))
                .style(Style::default().bg(palette::BG_SURFACE)),
            inner,
        );
        return;
    }

    if state.tokens.tokens.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  No tokens yet. Press c to create one.",
                    styles::muted(),
                )),
            ])
            .style(Style::default().bg(palette::BG_SURFACE)),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = state
        .tokens
        .tokens
        .iter()
        .map(|t| {
            let ro_badge = if t.read_only {
                Span::styled(" [ro]", styles::muted())
            } else {
                Span::styled(" [rw]", styles::accent())
            };
            let name_span = Span::styled(t.name.as_str(), styles::body());
            let created_span = Span::styled(format!("  {}", t.created_at), styles::muted());
            ListItem::new(Line::from(vec![
                Span::raw("  "),
                name_span,
                ro_badge,
                created_span,
            ]))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(state.tokens.cursor));

    let list = List::new(items)
        .style(Style::default().bg(palette::BG_SURFACE))
        .highlight_style(Style::default().bg(palette::BG_HOVER).fg(palette::TEAL))
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, inner, &mut list_state);
}

/// Render the create-token mini form.
fn render_create_form(frame: &mut Frame, state: &AppState, area: Rect) {
    let block = styles::panel(false).title(Span::styled(" Create Token ", styles::accent()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let ro_indicator = if state.tokens.form_read_only {
        Span::styled("read-only  [Ctrl-Space to toggle]", styles::status_warn())
    } else {
        Span::styled("read-write [Ctrl-Space to toggle]", styles::status_ok())
    };

    let expiry = state.tokens.form_expiry;
    let expiry_style = if expiry.ttl_secs().is_some() {
        styles::status_warn()
    } else {
        styles::muted()
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("  Name (optional): ", styles::muted()),
            Span::styled(state.tokens.form_name.as_str(), styles::accent()),
            Span::styled("▎", styles::accent()), // cursor indicator
        ]),
        Line::from(vec![Span::raw("  "), ro_indicator]),
        Line::from(vec![
            Span::styled("  Expires: ", styles::muted()),
            Span::styled(expiry.label(), expiry_style),
            Span::styled("  [Ctrl-E to cycle]", styles::muted()),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(palette::BG_SURFACE)),
        inner,
    );
}

/// Render the status line.
fn render_status(frame: &mut Frame, state: &AppState, area: Rect) {
    let status = &state.tokens.status;
    if status.is_empty() {
        return;
    }
    let style = if status.starts_with("Error") {
        styles::status_err()
    } else if state.tokens.loading {
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
fn render_hints(frame: &mut Frame, phase: TokensFormPhase, area: Rect) {
    let hint = match phase {
        TokensFormPhase::Browsing => Line::from(vec![
            Span::styled("c", styles::accent()),
            Span::styled(" create  ", styles::muted()),
            Span::styled("d/x", styles::accent()),
            Span::styled(" revoke  ", styles::muted()),
            Span::styled("r", styles::accent()),
            Span::styled(" refresh  ", styles::muted()),
            Span::styled("j/k", styles::accent()),
            Span::styled(" move  ", styles::muted()),
            Span::styled("Enter", styles::accent()),
            Span::styled(" show QR  ", styles::muted()),
            Span::styled("Esc", styles::accent()),
            Span::styled(" dismiss secret", styles::muted()),
        ]),
        TokensFormPhase::Creating => Line::from(vec![
            Span::styled("Enter", styles::accent()),
            Span::styled(" create  ", styles::muted()),
            Span::styled("Ctrl-Space", styles::accent()),
            Span::styled(" toggle ro  ", styles::muted()),
            Span::styled("Ctrl-E", styles::accent()),
            Span::styled(" expiry  ", styles::muted()),
            Span::styled("Esc", styles::accent()),
            Span::styled(" cancel", styles::muted()),
        ]),
    };
    frame.render_widget(Paragraph::new(hint), area);
}
