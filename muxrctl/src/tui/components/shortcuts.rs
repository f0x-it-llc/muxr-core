//! The modal root that turns a dialog's ignored letters into its shortcuts.
//!
//! With a modal open the runtime converts every key nothing handled into a
//! consumed one (the modal floor), so the reducer never sees a letter and a
//! shortcut cannot live there. The modal's root component, however, receives
//! every key its children ignored, which is exactly where this wrapper sits.
//! The same trick is used by the wizard's `EscCloses`.
//!
//! The inner dialog is always asked first, so a focused text field keeps the
//! letters it types, and Tab, Enter, Space and Esc behave exactly as before.
//!
//! The modal floor swallows Ctrl-C the same way, so [`quit_on_ctrl_c`] lives
//! here too: every modal root (this wrapper, the QR layer's buttons, the
//! wizard's controls) turns a Ctrl-C its control ignored into [`UiMsg::Quit`].

use ratatui::layout::Rect;
use ratcn::runtime::{
    Component, DeclareCtx, Event, EventCtx, EventResult, KeyCode, PaintCtx, ScopeOptions,
};

use crate::app::UiMsg;
use crate::app::shortcuts;
use crate::app::state::{AppState, DialogId};

/// A dialog that also answers the letters [`shortcuts::shortcuts`] lists for
/// its [`DialogId`].
pub struct DialogShortcuts<C> {
    id: DialogId,
    inner: C,
}

impl<C> DialogShortcuts<C> {
    /// Wrap `inner`, the dialog declared under `id`.
    pub const fn new(id: DialogId, inner: C) -> Self {
        Self { id, inner }
    }
}

impl<C: Component<AppState, UiMsg>> Component<AppState, UiMsg> for DialogShortcuts<C> {
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
        let result = quit_on_ctrl_c(event, self.inner.handle_event(event, state, ctx));
        if !matches!(result, EventResult::Ignored) {
            return result;
        }
        if let Event::Key(key) = event
            && let KeyCode::Char(c) = key.code
            && !key.modifiers.any()
        {
            return shortcuts::message(self.id, c, state)
                .map_or(EventResult::Ignored, EventResult::Emit);
        }
        result
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

/// `result`, unless the control ignored a bare Ctrl-C: then [`UiMsg::Quit`].
///
/// Pass what the wrapped control answered, so a control that wants Ctrl-C for
/// itself keeps it. Alt or Shift held alongside is not the quit chord.
pub fn quit_on_ctrl_c(event: &Event, result: EventResult<UiMsg>) -> EventResult<UiMsg> {
    match (event, result) {
        (Event::Key(key), EventResult::Ignored)
            if key.modifiers.ctrl
                && !key.modifiers.alt
                && !key.modifiers.shift
                && key.code == KeyCode::Char('c') =>
        {
            EventResult::Emit(UiMsg::Quit)
        }
        (_, result) => result,
    }
}
