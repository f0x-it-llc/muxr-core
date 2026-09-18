//! Theme: centralized color palette and semantic style builders.
//!
//! Render code imports from here (`theme::styles::*`, `theme::palette::*`)
//! rather than hard-coding colors, so the design language lives in one place.

pub mod palette;
pub mod styles;

/// The muxrctl [`ratcn::Theme`], built from [`palette`] on top of
/// `ratcn::Theme::default_dark()`.
///
/// `ratcn::Theme` is `#[non_exhaustive]` with public fields, so a theme is
/// authored by starting from a preset and overriding the fields that carry
/// the muxrctl palette rather than with a struct literal.
///
/// No screen wires the ratcn runtime in yet — that lands in a later wave —
/// so nothing but this module's own test calls it; suppress the resulting
/// dead-code warning rather than dropping the function until then.
#[must_use]
#[allow(dead_code)]
pub const fn muxr() -> ratcn::Theme {
    let mut theme = ratcn::Theme::default_dark();
    theme.name = "Muxr";
    theme.foreground = palette::FG;
    theme.muted_foreground = palette::MUTED;
    theme.background = palette::BG_BASE;
    theme.surface = palette::BG_SURFACE;
    theme.field = palette::BG_RAISED;
    theme.primary = palette::TEAL;
    theme.primary_foreground = palette::BG_DEEP;
    theme.secondary = palette::BG_HOVER;
    theme.secondary_foreground = palette::FG;
    theme.accent = palette::TEAL;
    theme.destructive = palette::RED;
    theme.destructive_foreground = palette::BG_DEEP;
    theme.warning = palette::YELLOW;
    theme.border = palette::DIM;
    theme.ring = palette::TEAL;
    theme.cursor = palette::FG;
    theme
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muxr_theme_maps_the_palette() {
        let theme = muxr();
        assert_eq!(theme.background, palette::BG_BASE);
        assert_eq!(theme.primary, palette::TEAL);
    }
}
