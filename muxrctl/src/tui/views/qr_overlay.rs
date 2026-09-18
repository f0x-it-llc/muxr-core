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
//! immediately afterwards — the same way the toast stack is. The two halves
//! share [`split`] and [`split_showing`], so the matrix always lands exactly
//! where the chrome left room for it.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratcn::Button;
use ratcn::runtime::{
    Component, DeclareCtx, Event, EventCtx, EventResult, KeyCode, PaintCtx, ScopeOptions,
};

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::tokens::{QrOverlay, QrOverlayPhase, TokensMsg};
use crate::tui::theme::{palette, styles};
use crate::tui::widgets::qr::QrWidget;

/// Rows the caption under the QR occupies.
const INFO_ROWS: u16 = 5;

/// Columns reserved for the Close control at the right of the bottom strip:
/// the label plus ratcn's two cells of button padding on each side.
const CLOSE_WIDTH: u16 = 9;

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
            // Declared after the chrome so it paints over the strip, and so the
            // layer has a focus target at all — a modal scope with nothing
            // focusable inside parks focus outside itself, and then no key
            // reaches the layer.
            let (_, close_area) = split_strip(split(area).1);
            ctx.component("qr_close", CloseControl::new(), close_area);
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

/// Split the bottom strip into its caption and the Close control.
fn split_strip(strip: Rect) -> (Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(CLOSE_WIDTH)])
        .split(strip);
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
    paint_bottom_strip(ctx, overlay, split_strip(strip).0);
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

// ── The Close control ─────────────────────────────────────────────────────────

/// The layer's Close button, which also answers Esc.
///
/// A ratcn [`Dialog`](ratcn::Dialog) owns `on_dismiss`/`dismiss_key`; a
/// `modal_scope` in ratcn 0.0.3 has neither, and an open modal turns every key
/// nothing inside it handled into `Consumed` rather than letting it bubble out
/// to the app. So an Esc routed "through the reducer" would never arrive: the
/// only place it can be answered is a component inside the layer. Answering it
/// here, beside the Enter/Space the button already answers, is what makes the
/// strip's "Esc close" true — and both emit [`TokensMsg::QrClose`], which never
/// revokes the token being shown.
struct CloseControl {
    button: Button<UiMsg>,
}

impl CloseControl {
    fn new() -> Self {
        Self {
            button: Button::new("Close")
                .ghost()
                .on_press(|| UiMsg::Tokens(TokensMsg::QrClose)),
        }
    }
}

impl Component<AppState, UiMsg> for CloseControl {
    fn declare(&mut self, ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
        Component::<AppState, UiMsg>::declare(&mut self.button, ctx);
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_, AppState>) {
        Component::<AppState, UiMsg>::paint(&mut self.button, ctx);
    }

    fn handle_event(
        &mut self,
        event: &Event,
        state: &AppState,
        ctx: &mut EventCtx<'_>,
    ) -> EventResult<UiMsg> {
        if let Event::Key(key) = event
            && key.code == KeyCode::Esc
            && !key.modifiers.any()
        {
            return EventResult::Emit(UiMsg::Tokens(TokensMsg::QrClose));
        }
        Component::<AppState, UiMsg>::handle_event(&mut self.button, event, state, ctx)
    }

    fn scope_options(&self) -> ScopeOptions {
        Component::<AppState, UiMsg>::scope_options(&self.button)
    }

    fn interaction_area(&self, area: Rect) -> Rect {
        Component::<AppState, UiMsg>::interaction_area(&self.button, area)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::tokens::QrOverlay;
    use crate::app::{Message, update};
    use crate::tui::runner::build_ratcn;
    use crate::tui::views::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratcn::Theme;
    use ratcn::runtime::{Event, KeyEvent, Ratcn};

    struct Harness {
        terminal: Terminal<TestBackend>,
        ratcn: Ratcn<AppState, UiMsg>,
        theme: Theme,
        state: AppState,
    }

    impl Harness {
        fn new(width: u16, height: u16) -> Self {
            let mut harness = Self {
                terminal: Terminal::new(TestBackend::new(width, height)).expect("terminal"),
                ratcn: build_ratcn(),
                theme: crate::tui::theme::muxr(),
                state: AppState::new(),
            };
            harness.draw();
            harness
        }

        fn draw(&mut self) {
            let Self {
                terminal,
                ratcn,
                theme,
                state,
            } = self;
            terminal
                .draw(|frame| render(frame, ratcn, state, theme))
                .expect("draw");
        }

        fn screen(&self) -> String {
            let buffer = self.terminal.backend().buffer();
            let area = buffer.area;
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// Open the layer directly in `phase`, the way the reducer would.
        fn open(&mut self, phase: QrOverlayPhase) {
            self.state.tokens.qr_overlay = Some(QrOverlay {
                phase,
                seq: 1,
                baseline_clients: 0,
                token_name: "phone".to_string(),
                read_only: true,
                tick_counter: 0,
            });
            self.state.open_dialog(DialogId::Qr);
            self.draw();
        }
    }

    fn showing() -> QrOverlayPhase {
        QrOverlayPhase::Showing {
            uri: "muxr://pair?v=2&h=10.0.0.1&p=50051&t=abcdefghijklmnop&ro=1&tm=pin".to_string(),
            host: "10.0.0.1".to_string(),
            port: 50051,
            fingerprint_short: "ab:cd:ef…".to_string(),
        }
    }

    /// The QR block does not fit a 40×12 terminal, so the widget prints its
    /// fallback instead — which is the whole reason the matrix is painted from
    /// the frame rather than the layer's canvas.
    #[test]
    fn the_layer_falls_back_to_text_on_a_small_terminal() {
        let mut harness = Harness::new(40, 12);
        harness.open(showing());
        let screen = harness.screen();
        assert!(
            screen.contains("terminal too small"),
            "expected the QR fallback:\n{screen}"
        );
    }

    #[test]
    fn the_bottom_strip_reports_the_token_and_its_access() {
        let mut harness = Harness::new(80, 24);
        harness.open(showing());
        let screen = harness.screen();
        assert!(screen.contains("Esc close"), "no dismiss hint:\n{screen}");
        assert!(screen.contains("ro=on"), "no access flag:\n{screen}");
        assert!(screen.contains("phone"), "no token name:\n{screen}");
        assert!(screen.contains("Close"), "no close control:\n{screen}");
    }

    #[test]
    fn the_generating_phase_says_what_it_is_doing() {
        let mut harness = Harness::new(80, 24);
        harness.open(QrOverlayPhase::Generating);
        assert!(
            harness.screen().contains("Generating pairing code"),
            "no progress copy:\n{}",
            harness.screen()
        );
    }

    /// Esc is answered inside the layer (see [`CloseControl`]) and closes it
    /// without revoking the token it showed.
    #[test]
    fn esc_closes_the_layer_and_revokes_nothing() {
        let mut harness = Harness::new(80, 24);
        harness.open(showing());
        let key = KeyEvent::new(KeyCode::Esc);
        let result = harness.ratcn.handle_event(Event::Key(key), &harness.state);
        let EventResult::Emit(msg) = result else {
            panic!("Esc must emit from inside the layer, got {result:?}");
        };
        let actions = update(&mut harness.state, Message::Ui(msg));
        assert!(harness.state.tokens.qr_overlay.is_none());
        assert_eq!(harness.state.top_dialog(), None);
        assert!(
            actions
                .iter()
                .all(|action| !matches!(action, crate::app::UpdateAction::RevokeToken(_)))
        );
    }
}
