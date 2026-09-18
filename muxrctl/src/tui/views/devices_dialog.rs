//! The Devices dialog: registered push devices, the relay they use, and
//! Remove behind a confirmation.
//!
//! The list is keyed by device name (unique per registration), so
//! `paint_item` looks the full [`DeviceRecord`] up by that key rather than
//! carrying a second, index-based copy of the row data.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratcn::runtime::DeclareCtx;
use ratcn::{Button, Dialog, List, ListItem, ListItemState};

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::devices::{DEVICES_EXPLAIN, DevicesMsg, humanize_registered_at};
use crate::server::devices::DeviceRecord;

/// Rows the device list gets, whatever it holds.
const LIST_ROWS: u16 = 6;
/// [`DEVICES_EXPLAIN`] is two lines; kept as a literal (like the list rows
/// above it) so a layout mismatch shows up as a wrong-length array rather
/// than a silent clip. `Dialog::content` cannot measure a closure, so
/// [`CONTENT_ROWS`] must match the layout in [`content`] exactly.
const EXPLAIN_ROWS: u16 = 2;
const CONTENT_ROWS: u16 = 1 // relay line
    + 1 // registered count
    + 1 // gap
    + LIST_ROWS
    + 1 // gap
    + EXPLAIN_ROWS;

/// Declare the Devices dialog as a modal layer.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let devices = &ctx.state().devices;
    let remove_disabled = devices.focused.is_none() || devices.loading;
    let dialog = Dialog::new()
        .title("Push devices")
        .outer_width(78)
        .content(CONTENT_ROWS, content)
        .action(
            "dev_remove",
            Button::new("Remove…")
                .destructive()
                .disabled(remove_disabled)
                .on_press(|| UiMsg::Devices(DevicesMsg::RemoveRequested)),
        )
        .action(
            "dev_refresh",
            Button::new("Refresh")
                .ghost()
                .on_press(|| UiMsg::Devices(DevicesMsg::Refresh)),
        )
        .action(
            "dev_close",
            Button::new("Close").on_press(|| UiMsg::Devices(DevicesMsg::Close)),
        )
        .on_dismiss(|| UiMsg::Devices(DevicesMsg::Close));
    ctx.modal(DialogId::Devices.id(), dialog, area);
}

/// The dialog's content strip: relay status, the count, the list, and the
/// explainer.
fn content(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // relay line
            Constraint::Length(1),            // registered count
            Constraint::Length(1),            // gap
            Constraint::Length(LIST_ROWS),    // device list
            Constraint::Length(1),            // gap
            Constraint::Length(EXPLAIN_ROWS), // explainer
        ])
        .split(ctx.area());

    let theme = *ctx.theme;
    let muted = Style::default().fg(theme.muted_foreground);
    let count = ctx.state().devices.devices.len();

    let relay_line = match ctx.state().devices.relay_url.clone() {
        Some(url) => Line::from(Span::styled(format!("Relay: {url}"), muted)),
        None => Line::from(Span::styled(
            "Relay: disabled — push notifications are off (set notify_relay_url)",
            Style::default().fg(theme.warning),
        )),
    };
    ctx.paint_widget(Paragraph::new(relay_line), rows[0]);
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("Registered: {count}"),
            muted,
        ))),
        rows[1],
    );

    if count == 0 {
        ctx.paint_widget(
            Paragraph::new(Line::from(Span::styled(
                "No devices registered yet.",
                muted,
            ))),
            rows[3],
        );
    } else {
        let items: Vec<ListItem<String>> = ctx
            .state()
            .devices
            .devices
            .iter()
            .map(|d| ListItem::new(d.device_name.clone(), d.device_name.clone()))
            .collect();
        ctx.component(
            "dev_list",
            List::new(items)
                .item_focus(
                    |state: &AppState| state.devices.focused.clone(),
                    |name, _offset| UiMsg::Devices(DevicesMsg::Focused(name)),
                )
                .focus_symbol("> ")
                .paint_item(paint_device_row),
            rows[3],
        );
    }

    let explain: Vec<Line<'static>> = DEVICES_EXPLAIN
        .iter()
        .map(|line| Line::from(Span::styled((*line).to_string(), muted)))
        .collect();
    ctx.paint_widget(Paragraph::new(explain), rows[5]);
}

/// One device row: `name  [platform]  <age>  <handle_prefix>…`.
fn paint_device_row(state: &AppState, row: ListItemState<'_, String>) -> Line<'static> {
    match find_device(&state.devices.devices, row.value) {
        Some(d) => Line::from(format!(
            "{}  [{}]  {}  {}…",
            d.device_name,
            d.platform,
            humanize_registered_at(d.registered_at),
            d.handle_prefix,
        )),
        None => Line::from(row.label.to_string()),
    }
}

fn find_device<'a>(devices: &'a [DeviceRecord], name: &str) -> Option<&'a DeviceRecord> {
    devices.iter().find(|d| d.device_name == name)
}

/// The "remove this device?" confirmation.
pub fn declare_remove_confirm(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let name = ctx
        .state()
        .devices
        .focused
        .clone()
        .unwrap_or_else(|| "this device".to_string());
    let dialog = Dialog::new()
        .title(format!("Remove '{name}'?"))
        .description("It stops receiving push notifications until the app registers again.")
        .action(
            "device_remove_cancel",
            Button::new("Cancel")
                .secondary()
                .on_press(|| UiMsg::Devices(DevicesMsg::RemoveCancelled)),
        )
        .action(
            "device_remove_confirm",
            Button::new("Remove")
                .destructive()
                .on_press(|| UiMsg::Devices(DevicesMsg::RemoveConfirmed)),
        )
        .on_dismiss(|| UiMsg::Devices(DevicesMsg::RemoveCancelled));
    ctx.modal(DialogId::DeviceRemoveConfirm.id(), dialog, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::Message;
    use crate::app::update::update;
    use crate::tui::runner::build_ratcn;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratcn::Theme;
    use ratcn::runtime::{Event, EventResult, KeyCode, KeyEvent, Ratcn};

    fn device_record(name: &str, platform: &str) -> DeviceRecord {
        DeviceRecord {
            device_name: name.to_string(),
            platform: platform.to_string(),
            registered_at: 0,
            handle_prefix: "abcd1234".to_string(),
        }
    }

    /// The real loop in miniature — the same shape as `tui::views::tests::Harness`,
    /// duplicated here rather than shared because that harness is private to its
    /// module: one `Ratcn`, one `AppState`, and the runner's own key routing.
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
                .draw(|frame| crate::tui::views::render(frame, ratcn, state, theme))
                .expect("draw");
        }

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
    fn devices_dialog_shows_a_device_row_and_the_relay_line() {
        let mut harness = Harness::new();
        harness.state.devices.devices = vec![device_record("pixel", "android")];
        harness.state.devices.relay_url = Some("https://relay.example".to_string());
        harness.state.open_dialog(DialogId::Devices);
        harness.draw();

        let screen = harness.screen();
        assert!(screen.contains("pixel"), "device name missing:\n{screen}");
        assert!(
            screen.contains("[android]"),
            "platform badge missing:\n{screen}"
        );
        assert!(
            screen.contains("Relay: https://relay.example"),
            "relay line missing:\n{screen}"
        );
    }

    #[test]
    fn enter_on_remove_opens_the_confirm_and_esc_cancels_without_removing() {
        let mut harness = Harness::new();
        harness.state.devices.devices = vec![device_record("pixel", "android")];
        harness.state.open_dialog(DialogId::Devices);
        harness.draw();

        // Move the list cursor onto the only row, then tab to the Remove
        // action and press it.
        harness.key(KeyCode::Down);
        assert_eq!(harness.state.devices.focused.as_deref(), Some("pixel"));
        harness.key(KeyCode::Tab);
        harness.key(KeyCode::Enter);
        assert_eq!(
            harness.state.top_dialog(),
            Some(DialogId::DeviceRemoveConfirm)
        );

        harness.key(KeyCode::Esc);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Devices));
        assert!(
            !harness.state.devices.loading,
            "Esc must not have confirmed the removal"
        );
        assert_eq!(
            harness.state.devices.devices.len(),
            1,
            "cancelling must not remove anything"
        );
    }
}
