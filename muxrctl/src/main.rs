//! muxrctl — install / configure / pair TUI for muxrd.
//!
//! A standalone ratatui control panel that drives muxrd's library + CLI
//! surface (config / cert / tokens / control / pairing). This binary wires up
//! logging, the terminal, the panic hook, the tokio runtime, and hands off to
//! the event loop in [`tui::runner`].
//!
//! Architecture (TEA, single crate):
//! - [`app`] — pure state / message / update / action (no ratatui).
//! - [`tui`] — terminal lifecycle, event loop, theme, screen rendering.

mod app;
mod pairing;
mod server;
mod tui;

use anyhow::Result;

use app::AppState;

fn main() -> Result<()> {
    // Handle --version / --help BEFORE any terminal init so they never touch the
    // alternate screen (and so install scripts can probe the binary safely).
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("muxrctl {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "muxrctl {} — install / configure / pair TUI for muxrd\n\n\
                     USAGE:\n    muxrctl [--version] [--help]\n\n\
                     Run with no arguments to launch the control panel.",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            _ => {}
        }
    }

    // Logging to stderr. The TUI owns stdout (alternate screen), so env_logger's
    // default stderr target does not corrupt the rendered frame. Quiet by
    // default; raise with e.g. `RUST_LOG=muxrctl=debug`.
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Warn)
        .init();

    // Build the multi-thread runtime explicitly so async UpdateAction tasks
    // (later waves: spawn server, mint token, poll status) have an executor.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(run())
}

/// Initialize the terminal, install the panic hook, run the event loop, and
/// restore the terminal on exit (panic restoration is handled by the hook).
async fn run() -> Result<()> {
    let mut terminal = tui::terminal::init();

    // Install AFTER init so our restore hook wraps ratatui's (see terminal.rs).
    tui::terminal::install_panic_hook();

    let mut state = AppState::new();
    let result = tui::runner::run(&mut terminal, &mut state);

    // Restore on normal exit too (the panic hook covers the panic path).
    tui::terminal::restore();

    result
}
