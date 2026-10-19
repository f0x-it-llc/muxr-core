//! Rendering: one dashboard, every action a ratcn dialog on top of it.
//!
//! [`render`] is the single entry point the runner draws through. It paints the
//! background, runs one ratcn declaration pass (the dashboard plus exactly the
//! dialogs [`crate::app::state::UiState::modals`] says are open, in stack
//! order), and finally paints the toast stack and the key-hint footer
//! *outside* that pass so the modal backdrop never dims them.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratcn::runtime::{DeclareCtx, Ratcn};
use ratcn::{Theme, ToastPosition, ToasterWidget};

use crate::app::UiMsg;
use crate::app::state::{AppState, DialogId};

pub mod cert_dialog;
pub mod config_dialog;
pub mod dashboard;
pub mod devices_dialog;
pub mod qr_overlay;
pub mod server_dialog;
pub mod tokens_dialog;
pub mod wizard;

/// Draw one frame.
pub fn render(
    frame: &mut Frame,
    ratcn: &mut Ratcn<AppState, UiMsg>,
    state: &AppState,
    theme: &Theme,
) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.background)),
        area,
    );

    ratcn.render(frame, area, state, theme, |ctx| {
        dashboard::declare(ctx);
        // Exactly the open ids, in stack order: `Ratcn::modals` is bound, so a
        // render that declares anything else is a panic rather than a drift.
        for id in state.ui.modals.ids() {
            declare_dialog(ctx, DialogId::parse(id.as_str()));
        }
    });

    // The QR matrix needs the frame (see `qr_overlay::paint_matrix`), so it is
    // painted after the ratcn pass — but only for the layer on top, and only
    // where that layer put it.
    match state.top_dialog() {
        // The Tokens flow's fullscreen layer: the matrix lands in the frame it
        // laid its own chrome out in.
        Some(DialogId::Qr) => qr_overlay::paint_matrix(frame, state, area),
        // The wizard's Pair step: a rect inside its 72-cell panel, which the
        // wizard owns, measures and paints itself.
        Some(DialogId::Wizard) => wizard::paint_pair_matrix(frame, state, area),
        // Anything else — the wizard on another step, a dialog stacked over
        // either layer, a lingering overlay behind the dashboard — gets no
        // matrix, which is what keeps it off the panel it would cover.
        _ => {}
    }

    // Toasts sit outside the ratcn pass so an open dialog's backdrop never
    // dims them.
    frame.render_widget(
        ToasterWidget::new(&state.ui.toasts, state.ui.now)
            .themed(theme)
            .position(ToastPosition::TopRight),
        area,
    );

    // The footer is painted post-pass for the same reason: `dim_background`
    // would otherwise grey out the very hints the open dialog needs.
    paint_footer(frame, state, theme, area);
}

/// The key-hint footer on the frame's last row, built from the same table the
/// shortcuts read. `Qr` and `Wizard` paint nothing here: their layers own the
/// bottom row and carry their own strips.
fn paint_footer(frame: &mut Frame, state: &AppState, theme: &Theme, area: Rect) {
    let pairs = crate::app::shortcuts::hint(state.top_dialog());
    if pairs.is_empty() || area.height == 0 {
        return;
    }
    let key_style = Style::default().fg(theme.primary);
    let label_style = Style::default().fg(theme.muted_foreground);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut left = usize::from(area.width);
    for (i, (key, label)) in pairs.into_iter().enumerate() {
        // Each piece is cut to what is left, so a narrow terminal loses the
        // tail of the line rather than wrapping or bleeding past the frame.
        for (text, style) in [
            (if i == 0 { "" } else { "  " }, label_style),
            (key, key_style),
            (" ", label_style),
            (label, label_style),
        ] {
            let text = ratcn::text_width::truncate_to_width(text, left);
            left -= ratcn::text_width::display_width(text);
            if !text.is_empty() {
                spans.push(Span::styled(text.to_string(), style));
            }
        }
    }
    let row = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    frame.render_widget(Paragraph::new(Line::from(spans)), row);
}

/// Declare the dialog an open modal id names.
fn declare_dialog(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, id: Option<DialogId>) {
    let Some(id) = id else {
        // Only `DialogId::id()` strings ever reach the modal stack, so this is
        // unreachable; declaring nothing keeps a future stray id from panicking
        // the render.
        return;
    };
    match id {
        DialogId::Config => config_dialog::declare(ctx),
        DialogId::StopServer => server_dialog::declare_stop_confirm(ctx),
        DialogId::Cert => cert_dialog::declare(ctx),
        DialogId::CertHelp => cert_dialog::declare_help(ctx),
        DialogId::CertRegenConfirm => cert_dialog::declare_regen_confirm(ctx),
        DialogId::Tokens => tokens_dialog::declare(ctx),
        DialogId::TokenCreate => tokens_dialog::declare_create(ctx),
        DialogId::TokenMinted => tokens_dialog::declare_minted(ctx),
        DialogId::TokenRevokeConfirm => tokens_dialog::declare_revoke_confirm(ctx),
        DialogId::Qr => qr_overlay::declare(ctx),
        DialogId::Devices => devices_dialog::declare(ctx),
        DialogId::DeviceRemoveConfirm => devices_dialog::declare_remove_confirm(ctx),
        DialogId::Wizard => wizard::declare(ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::DialogId;
    use crate::app::state::tokens::{QrOverlay, QrOverlayPhase, TokensMsg};
    use crate::app::{Message, update};
    use crate::tui::runner::{EventSource, InputPump, build_ratcn, route_event};
    use crossterm::event::Event as CtEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratcn::runtime::{
        Event, EventResult, FocusState, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent,
        MouseKind,
    };
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    /// A scripted terminal: everything queued is ready at once.
    struct Script(VecDeque<CtEvent>);

    impl EventSource for Script {
        fn poll(&mut self, _timeout: Duration) -> anyhow::Result<bool> {
            Ok(!self.0.is_empty())
        }

        fn read(&mut self) -> anyhow::Result<CtEvent> {
            Ok(self.0.pop_front().expect("polled ready"))
        }
    }

    /// The real loop in miniature: one `Ratcn`, one `AppState`, and the runner's
    /// own key routing — ratcn first, the reducer only for what it ignored.
    struct Harness {
        terminal: Terminal<TestBackend>,
        ratcn: Ratcn<AppState, UiMsg>,
        theme: Theme,
        state: AppState,
    }

    impl Harness {
        fn new() -> Self {
            Self::sized(80, 24)
        }

        fn sized(width: u16, height: u16) -> Self {
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

        /// Feed one key exactly as `runner::next_message` would, then redraw so
        /// the retained surface matches the new state.
        fn key(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code);
            let message = match self.ratcn.handle_event(Event::Key(key), &self.state) {
                EventResult::Emit(msg) => Message::Ui(msg),
                EventResult::Consumed => Message::Tick,
                EventResult::Ignored => Message::Key(key),
            };
            let _ = update(&mut self.state, message);
            self.draw();
        }

        /// Feed `events` as one terminal burst, iterating exactly as `run()`
        /// does: fill the pump, route ONE event with the current state, reduce,
        /// redraw, repeat until the queue is empty. Returns how many `Tick`s
        /// the routing produced (the heartbeat is the clock's job, not the
        /// burst's, so this must be zero).
        fn burst(&mut self, events: Vec<CtEvent>) -> usize {
            let mut source = Script(events.into());
            let mut pump = InputPump::new(Instant::now());
            let mut ticks = 0;
            loop {
                pump.fill(&mut source).expect("fill");
                let Some(event) = pump.next() else { break };
                if let Some(message) = route_event(&mut self.ratcn, &self.state, event) {
                    ticks += usize::from(matches!(message, Message::Tick));
                    let _ = update(&mut self.state, message);
                }
                self.draw();
            }
            ticks
        }

        /// Feed one pointer event through the same Emit/Consumed/Ignored mapping
        /// as [`Self::key`] (an ignored pointer event is a plain tick).
        fn mouse(&mut self, kind: MouseKind, column: u16, row: u16) {
            let event = Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: Modifiers::NONE,
            });
            let message = match self.ratcn.handle_event(event, &self.state) {
                EventResult::Emit(msg) => Message::Ui(msg),
                EventResult::Consumed | EventResult::Ignored => Message::Tick,
            };
            let _ = update(&mut self.state, message);
            self.draw();
        }

        /// A left click: ratcn synthesises the `Click` on the `Up`, as in its
        /// own pointer tests.
        fn click(&mut self, column: u16, row: u16) {
            self.mouse(MouseKind::Down(MouseButton::Left), column, row);
            self.mouse(MouseKind::Up(MouseButton::Left), column, row);
        }

        /// The cell of the first character of `label` on `row_marker`'s row
        /// (the row containing both), so a section title with the same word is
        /// not matched. Columns count cells, not bytes.
        fn locate(&self, row_marker: &str, label: &str) -> (u16, u16) {
            let screen = self.screen();
            for (y, line) in screen.lines().enumerate() {
                if line.contains(row_marker)
                    && let Some(byte) = line.find(label)
                {
                    let x = line[..byte].chars().count();
                    return (x as u16, y as u16);
                }
            }
            panic!("{label:?} not found on a row with {row_marker:?}:\n{screen}");
        }

        /// Apply a message the way the runner would after an async task answered.
        fn feed(&mut self, message: Message) {
            let _ = update(&mut self.state, message);
            self.draw();
        }

        /// Open the Tokens dialog with `names` loaded and the first one focused
        /// (the async load never answers in the harness).
        fn tokens_dialog_with(&mut self, names: &[&str]) {
            self.key(KeyCode::Char('t'));
            let records = names
                .iter()
                .map(|name| crate::server::tokens::TokenRecord {
                    name: (*name).to_string(),
                    created_at: "2026-01-01".to_string(),
                    read_only: false,
                })
                .collect();
            self.feed(Message::TokensLoaded(records));
            if let Some(first) = names.first() {
                self.feed(Message::Ui(UiMsg::Tokens(TokensMsg::Focused(
                    (*first).to_string(),
                ))));
            }
        }

        /// The background colour of the first cell of `label` on the screen.
        fn bg_of(&self, label: &str) -> ratatui::style::Color {
            let (x, y) = self.locate(label, label);
            self.terminal.backend().buffer()[(x, y)].bg
        }

        fn last_row(&self) -> String {
            self.screen().lines().last().unwrap_or_default().to_string()
        }

        fn type_str(&mut self, text: &str) {
            for c in text.chars() {
                self.key(KeyCode::Char(c));
            }
        }

        /// The whole frame as text, one line per row.
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

        /// The bounding box of every white cell — the QR block paints its quiet
        /// zone white and nothing else in the app does, so this is where (and
        /// whether) the matrix landed.
        fn white_box(&self) -> Option<ratatui::layout::Rect> {
            let buffer = self.terminal.backend().buffer();
            let area = buffer.area;
            let mut hit: Option<(u16, u16, u16, u16)> = None;
            for y in 0..area.height {
                for x in 0..area.width {
                    if buffer[(x, y)].bg != ratatui::style::Color::White {
                        continue;
                    }
                    hit = Some(match hit {
                        None => (x, y, x, y),
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                    });
                }
            }
            hit.map(|(x0, y0, x1, y1)| ratatui::layout::Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
        }

        /// A `Showing` overlay, the way the Tokens flow leaves one.
        fn showing_overlay(&mut self) {
            self.state.tokens.qr_overlay = Some(QrOverlay {
                phase: QrOverlayPhase::Showing {
                    uri: "muxr://pair?v=2&h=10.0.0.5&p=50051&t=abcdefghijklmnopqrst&ro=1&tm=pin"
                        .to_string(),
                    host: "10.0.0.5".to_string(),
                    port: 50051,
                    fingerprint_short: "ab:cd:ef…".to_string(),
                },
                seq: 1,
                baseline_clients: 0,
                token_name: "phone".to_string(),
                read_only: true,
                tick_counter: 0,
            });
        }
    }

    /// The Tokens flow's fullscreen layer keeps the whole frame: `render`'s gate
    /// hands `paint_matrix` the frame area exactly as before the wizard existed.
    #[test]
    fn the_qr_dialog_still_gets_the_matrix_over_the_whole_frame() {
        let mut harness = Harness::sized(100, 44);
        harness.showing_overlay();
        harness.state.open_dialog(DialogId::Qr);
        harness.draw();

        let painted = harness
            .white_box()
            .expect("the fullscreen matrix did not paint");
        assert!(
            painted.width >= 41 && painted.height >= 19,
            "the fullscreen layer painted a {painted:?} block"
        );
        assert!(
            harness.screen().contains("Scan with the Muxr app"),
            "the layer lost its caption:\n{}",
            harness.screen()
        );
    }

    /// Only the topmost layer gets a matrix: one stacked over the QR layer is
    /// not painted over.
    #[test]
    fn a_dialog_stacked_over_the_qr_layer_gets_no_matrix() {
        let mut harness = Harness::sized(100, 44);
        harness.showing_overlay();
        harness.state.open_dialog(DialogId::Qr);
        harness.state.open_dialog(DialogId::TokenMinted);
        harness.draw();
        assert!(
            harness.white_box().is_none(),
            "the matrix painted over the dialog on top of it"
        );
    }

    #[test]
    fn dashboard_renders_every_section_and_button() {
        let harness = Harness::new();
        let screen = harness.screen();
        for section in ["Daemon", "Network", "Certificate", "Tokens", "Devices"] {
            assert!(
                screen.contains(section),
                "section {section} missing from:\n{screen}"
            );
        }
        for label in [
            "Start",
            "Configure",
            "Certificate",
            "Tokens",
            "Devices",
            "Setup wizard",
        ] {
            assert!(
                screen.contains(label),
                "button {label} missing from:\n{screen}"
            );
        }
    }

    #[test]
    fn c_opens_the_config_dialog_and_esc_closes_it() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('c'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Config));
        assert!(
            harness.screen().contains("Configure bind address"),
            "dialog did not paint:\n{}",
            harness.screen()
        );

        harness.key(KeyCode::Esc);
        assert_eq!(harness.state.top_dialog(), None);
        assert_eq!(harness.state.ui.modals.ids().len(), 0);
        assert_eq!(
            harness.state.ui.focus,
            FocusState::default(),
            "closing must restore the focus the dialog saved"
        );
    }

    #[test]
    fn typing_into_the_host_field_edits_the_model() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('c'));
        // The modal takes default focus, which resolves to its first focusable
        // leaf — the host field.
        harness.type_str("10.0.0.5");
        assert_eq!(harness.state.config.host, "10.0.0.5");
    }

    #[test]
    fn q_quits_on_the_dashboard_but_is_a_literal_in_the_host_field() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('c'));
        harness.type_str("q");
        assert_eq!(harness.state.config.host, "q");
        assert!(
            !harness.state.should_quit,
            "q must be typed, not a shortcut"
        );

        harness.key(KeyCode::Esc);
        harness.key(KeyCode::Char('q'));
        assert!(harness.state.should_quit);
    }

    fn ct_key(c: char) -> CtEvent {
        CtEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    #[test]
    fn a_burst_of_characters_all_land_in_the_focused_field() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('c'));
        harness.burst(vec![ct_key('1'), ct_key('9'), ct_key('2')]);
        assert_eq!(harness.state.config.host, "192");

        harness.burst("168.1.10xyz".chars().map(ct_key).collect());
        assert_eq!(harness.state.config.host, "192168.1.10xyz");
    }

    #[test]
    fn type_ahead_survives_a_surface_change_inside_one_burst() {
        let mut harness = Harness::new();
        harness.tokens_dialog_with(&["phone"]);
        harness.burst(vec![ct_key('x'), ct_key('y')]);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
        assert!(harness.state.tokens.loading, "the revoke path did not run");
    }

    #[test]
    fn a_paste_is_one_atomic_edit_into_the_focused_field() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('c'));
        harness.burst(vec![CtEvent::Paste("10.0.0.5".into())]);
        assert_eq!(harness.state.config.host, "10.0.0.5");
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Config));
        assert!(!harness.state.should_quit);
    }

    #[test]
    fn a_paste_cannot_fire_a_shortcut() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('t'));
        harness.burst(vec![CtEvent::Paste("n".into())]);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
        assert_eq!(harness.state.ui.modals.ids().len(), 1);
    }

    #[test]
    fn a_flood_of_moves_then_a_key_opens_the_dialog_without_ticks() {
        let mut harness = Harness::new();
        let mut events: Vec<CtEvent> = (0..300u16)
            .map(|i| {
                CtEvent::Mouse(crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Moved,
                    column: i % 80,
                    row: i % 24,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                })
            })
            .collect();
        events.push(ct_key('t'));
        let ticks = harness.burst(events);
        assert_eq!(ticks, 0);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
    }

    const BUTTON_ROW: &str = "Setup wizard";

    #[test]
    fn clicking_the_tokens_button_opens_the_tokens_dialog() {
        let mut harness = Harness::new();
        let (x, y) = harness.locate(BUTTON_ROW, "Tokens");
        harness.click(x, y);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
    }

    #[test]
    fn clicking_create_in_the_tokens_dialog_opens_the_create_form() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('t'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
        let screen = harness.screen();
        let (x, y) = screen
            .lines()
            .enumerate()
            .find_map(|(y, line)| {
                line.find("Create…")
                    .map(|b| (line[..b].chars().count() as u16, y as u16))
            })
            .unwrap_or_else(|| panic!("Create… not painted:\n{screen}"));
        harness.click(x, y);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::TokenCreate));
    }

    #[test]
    fn a_click_under_an_open_dialog_does_not_reach_the_dashboard() {
        let mut harness = Harness::new();
        let (x, y) = harness.locate(BUTTON_ROW, "Devices");
        harness.key(KeyCode::Char('t'));
        let before: Vec<_> = harness.state.ui.modals.ids().cloned().collect();
        harness.click(x, y);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
        let after: Vec<_> = harness.state.ui.modals.ids().cloned().collect();
        assert_eq!(after, before);
    }

    #[test]
    fn hovering_a_button_changes_no_state() {
        let mut harness = Harness::new();
        let (x, y) = harness.locate(BUTTON_ROW, "Tokens");
        harness.mouse(MouseKind::Moved, x, y);
        assert_eq!(harness.state.top_dialog(), None);
        assert!(!harness.state.should_quit);
    }

    #[test]
    fn n_in_the_tokens_dialog_opens_the_create_form() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('t'));
        harness.key(KeyCode::Char('n'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::TokenCreate));
    }

    #[test]
    fn letters_typed_into_the_token_name_are_text_not_shortcuts() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('t'));
        harness.key(KeyCode::Char('n'));
        harness.type_str("phone");
        assert_eq!(harness.state.tokens.form_name, "phone");
        assert_eq!(harness.state.top_dialog(), Some(DialogId::TokenCreate));
    }

    #[test]
    fn x_then_y_revokes_the_focused_token() {
        let mut harness = Harness::new();
        harness.tokens_dialog_with(&["phone"]);
        harness.key(KeyCode::Char('x'));
        assert_eq!(
            harness.state.top_dialog(),
            Some(DialogId::TokenRevokeConfirm)
        );
        harness.key(KeyCode::Char('y'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
        assert!(harness.state.tokens.loading, "the revoke path did not run");
    }

    #[test]
    fn s_without_a_held_secret_does_nothing() {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('t'));
        harness.key(KeyCode::Char('s'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
        assert_eq!(harness.state.ui.modals.ids().len(), 1);
    }

    #[test]
    fn x_without_a_focused_row_does_nothing() {
        let mut harness = Harness::new();
        harness.tokens_dialog_with(&[]);
        harness.key(KeyCode::Char('x'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Tokens));
    }

    #[test]
    fn the_footer_names_the_dialog_keys() {
        let mut harness = Harness::new();
        let dashboard = harness.last_row();
        assert!(dashboard.contains("t tokens") && dashboard.contains("q quit"));

        harness.key(KeyCode::Char('t'));
        let row = harness.last_row();
        assert!(row.contains("n create"), "{row:?}");
        assert!(row.contains("Tab moves"), "{row:?}");

        harness.key(KeyCode::Esc);
        let row = harness.last_row();
        assert!(
            row.contains("t tokens") && row.contains("q quit"),
            "{row:?}"
        );
    }

    #[test]
    fn the_footer_is_cut_to_a_narrow_frame() {
        let mut harness = Harness::sized(20, 24);
        harness.key(KeyCode::Char('t'));
        assert!(harness.last_row().starts_with("Tab moves"));
    }

    #[test]
    fn y_confirms_the_stop_dialog() {
        let mut harness = Harness::new();
        harness.state.server.status = Some(crate::app::state::ServerInfo {
            version: "0".into(),
            bind_addr: "0.0.0.0:50051".into(),
            pid: 1,
            uptime_secs: 1,
            client_count: 0,
            notify_relay_url: None,
            push_device_count: 0,
        });
        harness.draw();
        harness.key(KeyCode::Char('s'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::StopServer));
        harness.key(KeyCode::Char('y'));
        assert_eq!(harness.state.top_dialog(), None);
        assert!(harness.state.server.loading, "the stop path did not run");
    }

    #[test]
    fn the_focused_action_is_the_only_teal_one() {
        use crate::tui::theme::palette;
        let mut harness = Harness::new();
        harness.tokens_dialog_with(&["phone"]);
        // The list holds the focus: no action is lit.
        assert_ne!(harness.bg_of("Create…"), palette::TEAL);
        assert_ne!(harness.bg_of("Close"), palette::TEAL);

        harness.key(KeyCode::Tab);
        assert_eq!(harness.bg_of("Create…"), palette::TEAL);
        assert_ne!(harness.bg_of("Close"), palette::TEAL);

        harness.key(KeyCode::BackTab);
        assert_ne!(harness.bg_of("Create…"), palette::TEAL);
        assert_ne!(harness.bg_of("Close"), palette::TEAL);
    }
}
