//! Screen rendering. Dispatches on [`crate::app::state::Screen`].
//!
//! The dashboard module owns the shared chrome (title bar, tab list, footer).
//! Config, Cert, Server, Tokens, and Devices screens render their own body panels.
//! The QR overlay is a fullscreen layer that paints over the active screen
//! when `state.qr_overlay` is `Some`.

use ratatui::Frame;

use crate::app::AppState;
use crate::app::state::Screen;

pub mod cert;
pub mod config;
pub mod dashboard;
pub mod devices;
pub mod qr_overlay;
pub mod server;
pub mod tokens;

/// Top-level render entry point: draws the full frame for the current state.
///
/// The shared chrome (title bar + tab list + footer) always comes from the
/// dashboard module. Each screen then renders its own body into the body area.
/// After the base chrome is drawn, if a QR overlay is open it is painted
/// fullscreen over the entire frame.
pub fn render(frame: &mut Frame, state: &AppState) {
    dashboard::render(frame, state, frame.area());
    if let Some(ref ov) = state.qr_overlay {
        qr_overlay::render(frame, ov, frame.area());
    }
}

/// Render the body panel for the active screen.
///
/// Called by [`dashboard::render_body`] with the body area after the chrome
/// (tab list column) has been laid out.
pub fn render_screen_body(frame: &mut Frame, state: &AppState, area: ratatui::layout::Rect) {
    match state.screen {
        Screen::Config => config::render(frame, state, area),
        Screen::Cert => cert::render(frame, state, area),
        Screen::Server => server::render(frame, state, area),
        Screen::Tokens => tokens::render(frame, state, area),
        Screen::Devices => devices::render(frame, state, area),
        Screen::Dashboard => dashboard::render_overview(frame, state, area),
    }
}
