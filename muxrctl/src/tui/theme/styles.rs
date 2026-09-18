//! Semantic style builders for the muxrctl theme.
//!
//! Render code calls these instead of constructing `Style` from raw palette
//! constants, so emphasis conventions (DIM muted text, status colors) stay
//! consistent.
//!
//! New render code should read the active [`ratcn::Theme`] (`ctx.theme`)
//! instead: these builders survive only for the QR overlay, whose painting is
//! carried over verbatim from the pre-ratcn screens.

use ratatui::style::{Modifier, Style};

use super::palette;

/// Muted, dimmed secondary text.
pub fn muted() -> Style {
    Style::default()
        .fg(palette::MUTED)
        .add_modifier(Modifier::DIM)
}

/// Teal accent text.
pub fn accent() -> Style {
    Style::default().fg(palette::TEAL)
}

/// Plain body text in the primary foreground.
pub fn body() -> Style {
    Style::default().fg(palette::FG)
}

/// Success status text.
pub fn status_ok() -> Style {
    Style::default().fg(palette::GREEN)
}

/// Warning status text.
pub fn status_warn() -> Style {
    Style::default().fg(palette::YELLOW)
}

/// Error status text.
pub fn status_err() -> Style {
    Style::default().fg(palette::RED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muted_is_dim() {
        assert_eq!(muted().fg, Some(palette::MUTED));
        assert!(muted().add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn accent_uses_teal() {
        assert_eq!(accent().fg, Some(palette::TEAL));
        assert_eq!(body().fg, Some(palette::FG));
    }

    #[test]
    fn status_styles_have_colors() {
        assert_eq!(status_ok().fg, Some(palette::GREEN));
        assert_eq!(status_warn().fg, Some(palette::YELLOW));
        assert_eq!(status_err().fg, Some(palette::RED));
    }
}
