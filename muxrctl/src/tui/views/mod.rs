//! Rendering: one dashboard, every action a ratcn dialog on top of it.
//!
//! [`render`] is the single entry point the runner draws through. It paints the
//! background, runs one ratcn declaration pass (the dashboard plus exactly the
//! dialogs [`crate::app::state::UiState::modals`] says are open, in stack
//! order), and finally paints the toast stack *outside* that pass so the modal
//! backdrop never dims it.

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::widgets::Block;
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
    use crate::app::state::tokens::{QrOverlay, QrOverlayPhase};
    use crate::app::{Message, update};
    use crate::tui::runner::build_ratcn;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratcn::runtime::{Event, EventResult, FocusState, KeyCode, KeyEvent};

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
}
