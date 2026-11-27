//! A wrapper that keeps a control's hands off a dialog's letters.
//!
//! ratcn's linear navigation reads `h`/`l` (and `j`/`k`) as previous/next, so a
//! focused [`Cycle`](ratcn::Cycle) steps its setting on a letter the dialog's
//! shortcut table means for something else. [`LettersPass`] hands every
//! unmodified letter straight back as ignored, without asking the control, so
//! it bubbles to the modal root's [`DialogShortcuts`](super::DialogShortcuts)
//! instead. Arrows, Enter, Space, the mouse and every chord still reach the
//! control unchanged.
//!
//! Only wrap a control whose dialog has letter shortcuts and which does not
//! need letters itself: a text field types them, and a list wants `j`/`k`.

use ratatui::layout::Rect;
use ratcn::runtime::{
    Component, DeclareCtx, Event, EventCtx, EventResult, KeyCode, PaintCtx, ScopeOptions,
};

use crate::app::UiMsg;
use crate::app::state::AppState;

/// A control that never sees an unmodified letter.
pub struct LettersPass<C> {
    inner: C,
}

impl<C> LettersPass<C> {
    /// Wrap `inner`.
    pub const fn new(inner: C) -> Self {
        Self { inner }
    }
}

/// Whether `event` is a plain letter key, no modifier held.
fn is_letter(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key) if !key.modifiers.any()
            && matches!(key.code, KeyCode::Char(c) if c.is_alphabetic())
    )
}

impl<C: Component<AppState, UiMsg>> Component<AppState, UiMsg> for LettersPass<C> {
    fn scope_options(&self, state: &AppState) -> ScopeOptions {
        self.inner.scope_options(state)
    }

    fn interaction_area(&self, area: Rect, state: &AppState) -> Rect {
        self.inner.interaction_area(area, state)
    }

    fn declare(&mut self, ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
        self.inner.declare(ctx);
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_, AppState>) {
        self.inner.paint(ctx);
    }

    fn handle_event(
        &mut self,
        event: &Event,
        state: &AppState,
        ctx: &mut EventCtx<'_>,
    ) -> EventResult<UiMsg> {
        if is_letter(event) {
            return EventResult::Ignored;
        }
        self.inner.handle_event(event, state, ctx)
    }

    fn reveal_in_viewport(
        &mut self,
        target: Rect,
        state: &AppState,
        ctx: &mut EventCtx<'_>,
    ) -> bool {
        self.inner.reveal_in_viewport(target, state, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratcn::runtime::{KeyEvent, Modifiers};

    #[test]
    fn only_unmodified_letters_count_as_letters() {
        let plain = |code| Event::Key(KeyEvent::new(code));
        assert!(is_letter(&plain(KeyCode::Char('h'))));
        assert!(is_letter(&plain(KeyCode::Char('l'))));
        assert!(!is_letter(&plain(KeyCode::Char('1'))));
        assert!(!is_letter(&plain(KeyCode::Char(' '))));
        assert!(!is_letter(&plain(KeyCode::Right)));
        assert!(!is_letter(&plain(KeyCode::Enter)));
        let ctrl_h = Event::Key(KeyEvent {
            code: KeyCode::Char('h'),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        });
        assert!(!is_letter(&ctrl_h));
    }
}
