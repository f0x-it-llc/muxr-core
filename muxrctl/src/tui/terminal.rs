//! Terminal setup, restoration, and the panic hook.
//!
//! [`init`] enters raw mode and the alternate screen (via `ratatui::init()`)
//! and then enables two crossterm input modes: mouse capture (clicks, hover and
//! the wheel reach the event loop) and bracketed paste (a paste arrives as one
//! `Event::Paste`). Both are released in exactly two places, always BEFORE
//! `ratatui::restore()`: a clean exit ([`restore`]) and a panic (the hook from
//! [`install_panic_hook`]). Skipping either would leave the user's shell
//! receiving mouse escape sequences or paste markers as typed garbage.
//!
//! The key invariant: install the panic hook AFTER `init()` so our hook wraps
//! ratatui's and the terminal is never left in raw mode or mouse-capturing.

use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::DefaultTerminal;

/// Guards against double-installation of the panic hook. Installing twice would
/// chain duplicate `restore()` closures.
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Enter the alternate screen / raw mode and return the terminal handle.
///
/// Returns ratatui's [`DefaultTerminal`]. Callers must pair this with
/// [`restore`] (or rely on the panic hook installed by [`install_panic_hook`]).
///
/// Both input modes are best-effort: a terminal that refuses mouse capture
/// leaves the TUI keyboard-only, and one that refuses bracketed paste just
/// delivers pastes as keystrokes, rather than aborting.
pub fn init() -> DefaultTerminal {
    let terminal = ratatui::init();
    if let Err(e) = crossterm::execute!(std::io::stdout(), EnableMouseCapture) {
        log::warn!("mouse capture unavailable, continuing keyboard-only: {e}");
    }
    // Bracketed so a paste is one atomic edit and is never read as keystrokes
    // that could fire a single-letter shortcut.
    if let Err(e) = crossterm::execute!(std::io::stdout(), EnableBracketedPaste) {
        log::warn!("bracketed paste unavailable, pastes arrive as keystrokes: {e}");
    }
    terminal
}

/// Leave the alternate screen and restore the cooked terminal mode on a clean
/// exit. Best-effort and idempotent (`ratatui::restore` tolerates repeat calls).
pub fn restore() {
    release_input_modes();
    ratatui::restore();
}

/// Stop mouse capture and bracketed paste. Best effort: errors are ignored
/// because both callers are already tearing the terminal down (one of them
/// while panicking).
fn release_input_modes() {
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
}

/// Install a panic hook that restores the terminal before the panic propagates.
///
/// Wraps the existing hook so any pre-existing hook still runs after cleanup.
/// Idempotent: only the first call per process installs the hook.
///
/// MUST be called after [`init`] (`ratatui::init()`): both use the "take + wrap"
/// `set_hook` pattern, and whichever installs last wraps the other. Installing
/// ours last guarantees `restore()` runs first on panic, before any outer hook
/// writes to the (now-restored) primary screen.
pub fn install_panic_hook() {
    if HOOK_INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Best-effort cleanup — we are already panicking.
        release_input_modes();
        ratatui::restore();
        original_hook(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_panic_hook_is_idempotent() {
        HOOK_INSTALLED.store(false, Ordering::Relaxed);

        install_panic_hook();
        assert!(HOOK_INSTALLED.load(Ordering::Acquire));

        // Second call must be a no-op (flag already set).
        install_panic_hook();
        assert!(HOOK_INSTALLED.load(Ordering::Acquire));

        // Restore a benign hook so we don't leave a wrapped hook for other tests.
        let _ = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
    }
}
