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
use ratcn::{Button, Dialog, Theme, ToastPosition, ToasterWidget};

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

    // The QR matrix needs the frame (see `qr_overlay::paint_matrix`).
    qr_overlay::paint_matrix(frame, state, area);

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

/// A placeholder dialog: the title its feature will keep, one Close action, and
/// Esc bound to the same message.
///
/// The cert, tokens, devices and wizard cards each replace their own
/// `declare*` body; the id strings and the message each emits are already the
/// ones those cards inherit.
pub(crate) fn stub_dialog(
    ctx: &mut DeclareCtx<'_, AppState, UiMsg>,
    id: DialogId,
    title: &'static str,
    action_id: &'static str,
    on_close: fn() -> UiMsg,
) {
    let area = ctx.frame_area();
    let dialog = Dialog::new()
        .title(title)
        .description("Coming in the next wave.")
        .action(action_id, Button::new("Close").on_press(on_close))
        .on_dismiss(on_close);
    ctx.modal(id.id(), dialog, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::DialogId;
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
            let mut harness = Self {
                terminal: Terminal::new(TestBackend::new(80, 24)).expect("terminal"),
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
