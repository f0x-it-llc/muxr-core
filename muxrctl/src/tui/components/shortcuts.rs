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
    fn prepare(&mut self, state: &AppState) {
        self.inner.prepare(state);
    }

    fn scope_options(&self) -> ScopeOptions {
        self.inner.scope_options()
    }

    fn interaction_area(&self, area: Rect) -> Rect {
        self.inner.interaction_area(area)
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
        let result = self.inner.handle_event(event, state, ctx);
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

    fn reveal_in_viewport(&mut self, target: Rect, state: &AppState, ctx: &mut EventCtx<'_>) {
        self.inner.reveal_in_viewport(target, state, ctx);
    }
}
