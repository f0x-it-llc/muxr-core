//! QR overlay — the fullscreen pairing QR for an already-minted token.
//!
//! ## Phase machine
//!
//! ```text
//! Generating ──(TokenQrReady)──► Showing ──(client count rises)──► Connected
//!             └──(TokenQrFailed)──► Failed
//! ```
//!
//! Declared as a `modal_scope` so the layer traps input and dims the dashboard
//! behind it. Everything but the QR matrix itself is painted from the layer's
//! own `PaintCtx`; the matrix needs a [`Frame`] (see [`paint_matrix`]), which
//! only the caller of [`super::render`] holds, so it is painted over the layer
//! immediately afterwards — the same way the toast stack is.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratcn::runtime::{DeclareCtx, PaintCtx, ScopeOptions};

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::tokens::{QrOverlay, QrOverlayPhase};
use crate::tui::theme::{palette, styles};
use crate::tui::widgets::qr::QrWidget;

/// Rows the caption under the QR occupies.
const INFO_ROWS: u16 = 5;

/// Declare the QR overlay as a modal layer.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    ctx.modal_scope(
        DialogId::Qr.id(),
        area,
        ScopeOptions::default(),
        move |ctx| {
            let area = ctx.area();
            ctx.paint(move |ctx| paint_chrome(ctx, area));
        },
    );
}

/// Split the overlay into its phase body and the bottom strip.
fn split(area: Rect) -> (Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // phase body
            Constraint::Length(1), // bottom strip (Esc + ro)
        ])
        .split(area);
    (rows[0], rows[1])
}

/// Split the `Showing` body into the QR region and its caption.
fn split_showing(body: Rect) -> (Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(13), Constraint::Length(INFO_ROWS)])
        .split(body);
    (rows[0], rows[1])
}

/// Paint everything except the QR matrix: the opaque base, the phase body, the
/// caption, and the bottom strip.
fn paint_chrome(ctx: &mut PaintCtx<'_, AppState>, area: Rect) {
    let Some(overlay) = ctx.state().tokens.qr_overlay.as_ref() else {
        return;
    };
    // Wipe the dashboard drawn underneath FIRST. `Clear` resets each cell's
    // symbol (a plain `Block`/`set_style` only recolors the background and
    // leaves the glyphs in place — they then bleed through the overlay margins
    // and read as a "transparent" background). After clearing, paint a solid
    // opaque base so the whole overlay is one flat colour.
    ctx.widget(Clear, area);
    ctx.widget(
        Block::default().style(Style::default().bg(palette::BG_BASE)),
        area,
    );

    let (body, strip) = split(area);
    match &overlay.phase {
        QrOverlayPhase::Generating => paint_generating(ctx, body),
        QrOverlayPhase::Showing {
            host,
            port,
            fingerprint_short,
            ..
        } => {
            let (_, info) = split_showing(body);
            paint_info_panel(ctx, host, *port, fingerprint_short, info);
        }
        QrOverlayPhase::Connected => paint_connected(ctx, body),
        QrOverlayPhase::Failed { err } => paint_failed(ctx, err, body),
    }
    paint_bottom_strip(ctx, overlay, strip);
}

/// Paint the QR matrix over the declared overlay.
///
/// [`QrWidget::render`] takes a [`Frame`], which a `PaintCtx` cannot supply, so
/// this runs from [`super::render`] right after the ratcn pass — the matrix is
/// the topmost thing on screen, which is also what the overlay wants.
pub fn paint_matrix(frame: &mut Frame, state: &AppState, area: Rect) {
    let Some(overlay) = state.tokens.qr_overlay.as_ref() else {
        return;
    };
    let QrOverlayPhase::Showing { uri, .. } = &overlay.phase else {
        return;
    };
    let (body, _) = split(area);
    let (qr_area, _) = split_showing(body);
    // Falls back to printing the raw URI when the terminal is too small.
    QrWidget::new(uri.as_str()).render(frame, qr_area);
}

/// Generating phase: show a progress message.
fn paint_generating(ctx: &mut PaintCtx<'_, AppState>, area: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Generating pairing code…",
            styles::status_warn(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Reading cert fingerprint, building QR…",
            styles::muted(),
        )),
    ];
    ctx.widget(
        Paragraph::new(lines).style(Style::default().bg(palette::BG_SURFACE)),
        area,
    );
}

/// Info caption (centered, below the QR): host:port, cert fingerprint, scan prompt.
fn paint_info_panel(
    ctx: &mut PaintCtx<'_, AppState>,
    host: &str,
    port: u16,
    fingerprint_short: &str,
    area: Rect,
) {
    let info_lines = vec![
        Line::from(vec![
            Span::styled("Server: ", styles::muted()),
            Span::styled(format!("{host}:{port}"), styles::accent()),
            Span::styled("   Cert: ", styles::muted()),
            Span::styled(fingerprint_short.to_string(), styles::body()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Scan with the Muxr app to connect…",
            styles::muted(),
        )),
    ];
    ctx.widget(
        Paragraph::new(info_lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(palette::BG_BASE)),
        area,
    );
}

/// Connected phase: show a heuristic "a client connected" message.
///
/// NOTE: this is inferred from a rise in the attached-client count, not from
/// verified per-token authentication — the copy is deliberately honest about it.
fn paint_connected(ctx: &mut PaintCtx<'_, AppState>, area: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  ✓ A client connected (attached-client count rose).",
            Style::default().fg(palette::TEAL),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Verify it's your phone, then continue.",
            styles::muted(),
        )),
        Line::from(Span::styled(
            "  Press Esc to close the overlay.",
            styles::muted(),
        )),
    ];
    ctx.widget(
        Paragraph::new(lines)
            .alignment(Alignment::Left)
            .style(Style::default().bg(palette::BG_SURFACE)),
        area,
    );
}

/// Failed phase: show the error.
fn paint_failed(ctx: &mut PaintCtx<'_, AppState>, err: &str, area: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  QR generation failed:",
            styles::status_err(),
        )),
        Line::from(Span::styled(format!("  {err}"), styles::body())),
        Line::from(""),
        Line::from(Span::styled("  Press Esc to close.", styles::muted())),
    ];
    ctx.widget(
        Paragraph::new(lines).style(Style::default().bg(palette::BG_SURFACE)),
        area,
    );
}

/// Bottom strip: `Esc close · ro=<on|off> · <token_name>`.
fn paint_bottom_strip(ctx: &mut PaintCtx<'_, AppState>, overlay: &QrOverlay, area: Rect) {
    let ro_span = if overlay.read_only {
        Span::styled("on", styles::status_warn())
    } else {
        Span::styled("off", styles::status_ok())
    };

    let line = Line::from(vec![
        Span::styled(" Esc", styles::accent()),
        Span::styled(" close  ·  ro=", styles::muted()),
        ro_span,
        Span::styled("  ·  ", styles::muted()),
        Span::styled(overlay.token_name.clone(), styles::accent()),
    ]);
    ctx.widget(Paragraph::new(line), area);
}
