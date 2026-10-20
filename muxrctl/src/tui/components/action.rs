//! One look for every dialog action: quiet at rest, loud when focused.
//!
//! ratcn's stock fills step the focused button only a shade away from its
//! neighbours, so after a Tab nothing on screen said the focus had moved. Rest
//! is therefore a muted fill and focus the solid primary fill with its
//! contrast label, which makes the focused action the one bright element in a
//! dialog. Fills (not borders) keep the button one row tall, so no dialog's
//! measured height changes. Hover gets the same treatment one step lighter.

use ratatui::style::Color;
use ratcn::{Button, ButtonStyle, ButtonVariant, Theme};

use crate::app::UiMsg;

type ActionButton = Button<UiMsg>;

/// A dialog action in the shared look.
pub fn action(button: ActionButton) -> ActionButton {
    button.style(|theme| styled(theme, false))
}

/// A destructive dialog action: the same look, but the label stays red at rest.
pub fn destructive(button: ActionButton) -> ActionButton {
    button.style(|theme| styled(theme, true))
}

fn styled(theme: &Theme, destructive: bool) -> ButtonStyle {
    let variant = if destructive {
        ButtonVariant::Destructive
    } else {
        ButtonVariant::Secondary
    };
    // Disabled keeps ratcn's own dimming for the variant.
    let mut style = ButtonStyle::from_theme(theme, variant);
    style.foreground = if destructive {
        theme.destructive
    } else {
        theme.foreground
    };
    style.background = theme.secondary;
    style.border = None;
    style.focused_foreground = theme.primary_foreground;
    style.focused_background = theme.primary;
    style.focused_border = None;
    style.hovered_foreground = style.foreground;
    style.hovered_background = lighten(theme.secondary);
    style.hovered_border = None;
    style
}

/// One step toward white: a hover must read lighter than rest on the dark
/// theme, without ever matching the focused fill.
fn lighten(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => {
            let up = |c: u8| c.saturating_add((255 - c) / 6);
            Color::Rgb(up(r), up(g), up(b))
        }
        other => other,
    }
}
