//! The Config dialog: the bind-address form.
//!
//! Host and port are [`TextInput`]s bound straight to `state.config`; the
//! reachable-IP picker is a ratcn [`Select`] whose open state, cursor and
//! selection all live in the same struct. Everything below them is explanatory
//! paint.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratcn::runtime::DeclareCtx;
use ratcn::{Button, Dialog, ListItem, Select};

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::config::{BIND_EXPLAIN, ConfigMsg};

use crate::tui::components::TextInput;

/// Rows the content strip occupies. `Dialog::content` cannot measure a closure,
/// so this must match the layout below exactly.
const CONTENT_ROWS: u16 = 1 + 3 + 1 + 3 + 1 + 1 + BIND_EXPLAIN_ROWS + 1;
const BIND_EXPLAIN_ROWS: u16 = 3;

/// Declare the Config dialog as a modal layer.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let dialog = Dialog::new()
        .title("Configure bind address")
        .outer_width(78)
        .content(CONTENT_ROWS, content)
        .action(
            "cfg_save",
            Button::new("Save").on_press(|| UiMsg::Config(ConfigMsg::Save)),
        )
        .action(
            "cfg_cancel",
            Button::new("Cancel")
                .secondary()
                .on_press(|| UiMsg::Config(ConfigMsg::Cancel)),
        )
        .on_dismiss(|| UiMsg::Config(ConfigMsg::Cancel));
    ctx.modal(DialogId::Config.id(), dialog, area);
}

/// The dialog's content strip: two fields, the IP picker, the explainer, the error.
fn content(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                 // host label
            Constraint::Length(3),                 // host field
            Constraint::Length(1),                 // port label
            Constraint::Length(3),                 // port field
            Constraint::Length(1),                 // ip picker
            Constraint::Length(1),                 // gap
            Constraint::Length(BIND_EXPLAIN_ROWS), // explainer
            Constraint::Length(1),                 // error
        ])
        .split(ctx.area());

    let theme = *ctx.theme;
    let muted = Style::default().fg(theme.muted_foreground);
    // While the write is in flight the values are already on their way to disk:
    // inert fields keep an edit from being silently dropped by the reload.
    let saving = ctx.state().config.pending_save;

    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Bind host", muted))),
        rows[0],
    );
    ctx.component(
        "cfg_host",
        TextInput::new()
            .value(
                |state: &AppState| state.config.host.clone(),
                |value| UiMsg::Config(ConfigMsg::HostChanged(value)),
            )
            .placeholder("0.0.0.0")
            .disabled(saving)
            .on_submit(|| UiMsg::Config(ConfigMsg::Save)),
        rows[1],
    );

    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Port", muted))),
        rows[2],
    );
    ctx.component(
        "cfg_port",
        TextInput::new()
            .value(
                |state: &AppState| state.config.port.clone(),
                |value| UiMsg::Config(ConfigMsg::PortChanged(value)),
            )
            .placeholder("50051")
            .digits_only(true)
            .max_len(5)
            .disabled(saving)
            .on_submit(|| UiMsg::Config(ConfigMsg::Save)),
        rows[3],
    );

    let items: Vec<ListItem<std::net::Ipv4Addr>> = ctx
        .state()
        .config
        .reachable_ips
        .iter()
        .map(|ip| ListItem::new(*ip, ip.to_string()))
        .collect();
    ctx.component(
        "cfg_ip",
        Select::new(items)
            .placeholder("Pick a reachable IP…")
            .open(
                |state: &AppState| state.config.ip_open,
                |open| UiMsg::Config(ConfigMsg::IpOpenChanged(open)),
            )
            .item_focus(
                |state: &AppState| state.config.ip_cursor,
                |ip| UiMsg::Config(ConfigMsg::IpFocused(ip)),
            )
            .selection(
                // The trigger shows the bind host whenever it is one of the
                // reachable addresses, so the picker and the host field can
                // never disagree about what is selected.
                |state: &AppState| state.config.host.parse::<std::net::Ipv4Addr>().ok(),
                |ip| UiMsg::Config(ConfigMsg::IpPicked(ip)),
            ),
        rows[4],
    );

    let explain: Vec<Line<'static>> = BIND_EXPLAIN
        .iter()
        .map(|line| Line::from(Span::styled((*line).to_string(), muted)))
        .collect();
    ctx.paint_widget(Paragraph::new(explain), rows[6]);

    if let Some(error) = ctx.state().config.error.clone() {
        ctx.paint_widget(
            Paragraph::new(Line::from(Span::styled(
                error,
                Style::default().fg(theme.destructive),
            ))),
            rows[7],
        );
    }
}
