//! A single-line text input: the one interactive ratcn component this crate
//! writes itself, because `ratcn` 0.0.3 ships no text entry control.
//!
//! Follows the shape of ratcn's own components (see `ratcn`'s `checkbox.rs`
//! and the `Component` trait docs): a controlled value bound through
//! [`TextInput::value`], read fresh from app state on every
//! [`handle_event`](Component::handle_event) call rather than copied at
//! declaration, so two keystrokes answered before the next render still
//! compose into one edit.
//!
//! State is app-owned, exactly like every other ratcn control: this
//! component reads the current value through a binding and emits a message
//! with the whole new value on every edit; nothing here mutates app state
//! directly.

use std::{fmt, rc::Rc};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::Span,
    widgets::{Block, Widget},
};
use ratcn::{
    Theme, geometry,
    runtime::{
        Component, DeclareCtx, Event, EventCtx, EventResult, KeyCode, KeyEvent, PaintCtx,
        ScopeOptions,
    },
    text_width,
    theme::resolve_style,
};

/// A text input's colors.
///
/// The field and, when [`bordered`](TextInput::bordered), the box around it
/// carry [`background`](Self::background). The value carries
/// [`foreground`](Self::foreground); the placeholder shown in its place while
/// the value is empty carries [`placeholder_foreground`](Self::placeholder_foreground).
/// The border is [`border`](Self::border) at rest and
/// [`border_focused`](Self::border_focused) while focused; the caret painted
/// at the end of the value while focused is [`cursor`](Self::cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInputStyle {
    /// Background of the field, and of the border box when bordered.
    pub background: Color,
    /// Color of the entered value.
    pub foreground: Color,
    /// Color of the placeholder shown while the value is empty.
    pub placeholder_foreground: Color,
    /// Border color at rest.
    pub border: Color,
    /// Border color while focused.
    pub border_focused: Color,
    /// Background of the one-cell caret painted at the end of the value
    /// while focused.
    pub cursor: Color,
}

impl TextInputStyle {
    /// Colors derived from a theme: the field's own well for the background,
    /// the ring for the focused border, and the theme's dedicated
    /// [`Theme::cursor`] role for the caret.
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            background: theme.field,
            foreground: theme.foreground,
            placeholder_foreground: theme.muted_foreground,
            border: theme.border,
            border_focused: theme.ring,
            cursor: theme.cursor,
        }
    }
}

type ReadValueFn<S> = Rc<dyn Fn(&S) -> String>;
type OnChangeFn<M> = Rc<dyn Fn(String) -> M>;
type OnSubmitFn<M> = Rc<dyn Fn() -> M>;
type StyleFn = Rc<dyn Fn(&Theme) -> TextInputStyle>;

/// A single-line, app-owned text field.
///
/// The value lives in app state and arrives through [`value`](Self::value);
/// without that binding the input paints its placeholder but is not
/// focusable and answers no events. Typing, Backspace/Delete, and a
/// bracketed paste all edit the bound value and emit
/// [`EventResult::Emit`]; Enter emits [`on_submit`](Self::on_submit) when one
/// is bound, and is otherwise left [`EventResult::Ignored`] so an enclosing
/// dialog can handle it. Tab, Shift+Tab and Esc are always left `Ignored` —
/// this component never intercepts navigation.
pub struct TextInput<S, M> {
    value: Option<(ReadValueFn<S>, OnChangeFn<M>)>,
    placeholder: String,
    digits_only: bool,
    max_len: Option<usize>,
    disabled: bool,
    on_submit: Option<OnSubmitFn<M>>,
    bordered: bool,
    style: Option<StyleFn>,
    /// The bound value, resolved once per declaration for [`paint`](Component::paint).
    resolved_value: String,
}

impl<S, M> fmt::Debug for TextInput<S, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextInput")
            .field("placeholder", &self.placeholder)
            .field("value", &self.value.is_some())
            .field("digits_only", &self.digits_only)
            .field("max_len", &self.max_len)
            .field("disabled", &self.disabled)
            .field("on_submit", &self.on_submit.is_some())
            .field("bordered", &self.bordered)
            .field("style", &self.style.is_some())
            .finish_non_exhaustive()
    }
}

impl<S, M> TextInput<S, M> {
    /// An empty, bordered text input with no binding yet. Call
    /// [`value`](Self::value) to make it live.
    #[must_use]
    pub fn new() -> Self {
        Self {
            value: None,
            placeholder: String::new(),
            digits_only: false,
            max_len: None,
            disabled: false,
            on_submit: None,
            bordered: true,
            style: None,
            resolved_value: String::new(),
        }
    }

    /// Bind the value and the message that reports an edit.
    ///
    /// `read` runs against current app state inside every
    /// [`handle_event`](Component::handle_event) call, not once at
    /// declaration — so a second keystroke answered before the next render
    /// still reads the value the first keystroke's message already produced.
    /// `on_change` receives the whole new value, never a delta. Without this
    /// binding the input paints its placeholder but is not focusable and
    /// answers no events.
    #[must_use]
    pub fn value(
        mut self,
        read: impl Fn(&S) -> String + 'static,
        on_change: impl Fn(String) -> M + 'static,
    ) -> Self {
        self.value = Some((Rc::new(read), Rc::new(on_change)));
        self
    }

    /// Text shown, in [`TextInputStyle::placeholder_foreground`], while the
    /// bound value is empty.
    #[must_use]
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Reject any typed or pasted character that is not an ASCII digit.
    #[must_use]
    pub const fn digits_only(mut self, digits_only: bool) -> Self {
        self.digits_only = digits_only;
        self
    }

    /// Reject a character once the value already holds `max_len` of them.
    #[must_use]
    pub const fn max_len(mut self, max_len: usize) -> Self {
        self.max_len = Some(max_len);
        self
    }

    /// Paint muted and answer no event, including navigation. Defaults to
    /// `false`.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// The message Enter emits. Without this, Enter is
    /// [`EventResult::Ignored`] rather than consumed, so a dialog wrapping
    /// this input can commit on it instead.
    #[must_use]
    pub fn on_submit(mut self, on_submit: impl Fn() -> M + 'static) -> Self {
        self.on_submit = Some(Rc::new(on_submit));
        self
    }

    /// Draw a `Block::bordered()` box around the one text row. Defaults to
    /// `true`; `false` paints the row alone on [`TextInputStyle::background`].
    #[must_use]
    #[allow(dead_code)] // no dialog declares a borderless field yet.
    pub const fn bordered(mut self, bordered: bool) -> Self {
        self.bordered = bordered;
        self
    }

    /// Supply exact colors, taking precedence over the theme.
    #[must_use]
    #[allow(dead_code)] // no dialog overrides the theme's field colors yet.
    pub fn style(mut self, style: impl Fn(&Theme) -> TextInputStyle + 'static) -> Self {
        self.style = Some(Rc::new(style));
        self
    }

    fn is_bound(&self) -> bool {
        self.value.is_some()
    }

    fn can_act(&self) -> bool {
        !self.disabled && self.is_bound()
    }

    /// Rows this component occupies: three when [`bordered`](Self::bordered)
    /// (border, text, border), one otherwise.
    const fn height(&self) -> u16 {
        if self.bordered { 3 } else { 1 }
    }

    /// Whether `c` passes [`digits_only`](Self::digits_only).
    fn accepts(&self, c: char) -> bool {
        !self.digits_only || c.is_ascii_digit()
    }

    /// `current` with every character of `input` appended that
    /// [`accepts`](Self::accepts) and keeps the result within
    /// [`max_len`](Self::max_len), or `None` when nothing was appended.
    fn append(&self, current: &str, input: &str) -> Option<String> {
        let mut next = current.to_owned();
        let mut changed = false;
        for c in input.chars() {
            if !self.accepts(c) {
                continue;
            }
            if let Some(max_len) = self.max_len
                && next.chars().count() >= max_len
            {
                continue;
            }
            next.push(c);
            changed = true;
        }
        changed.then_some(next)
    }

    /// `current` with its last character removed, or `None` if it was
    /// already empty. `Delete` answers the same way — the cursor is always
    /// at the end, so there is nothing after it to remove.
    fn backspace(current: &str) -> Option<String> {
        if current.is_empty() {
            return None;
        }
        let mut next = current.to_owned();
        next.pop();
        Some(next)
    }

    /// Read the bound value fresh from `state`, apply `edit` to it, and emit
    /// the result — or answer [`EventResult::Consumed`] when `edit` reports
    /// no change, and [`EventResult::Ignored`] when nothing is bound.
    fn edit(&self, state: &S, edit: impl FnOnce(&str) -> Option<String>) -> EventResult<M> {
        let Some((read, on_change)) = &self.value else {
            return EventResult::Ignored;
        };
        let current = read(state);
        match edit(&current) {
            Some(next) => EventResult::Emit(on_change(next)),
            None => EventResult::Consumed,
        }
    }

    /// The message [`on_submit`](Self::on_submit) emits, or
    /// [`EventResult::Ignored`] when none was bound.
    fn submit(&self) -> EventResult<M> {
        self.on_submit
            .as_ref()
            .map_or(EventResult::Ignored, |on_submit| {
                EventResult::Emit(on_submit())
            })
    }

    /// The keys this input answers. Ctrl/Alt chords, Tab/BackTab/Esc, and any
    /// code not named below are left [`EventResult::Ignored`] for the app or
    /// an enclosing dialog to handle.
    fn handle_key(&self, key: KeyEvent, state: &S) -> EventResult<M> {
        if key.modifiers.ctrl || key.modifiers.alt {
            return EventResult::Ignored;
        }
        match key.code {
            KeyCode::Char(c) => {
                let mut buf = [0_u8; 4];
                let input = c.encode_utf8(&mut buf);
                self.edit(state, |current| self.append(current, input))
            }
            KeyCode::Backspace | KeyCode::Delete => self.edit(state, Self::backspace),
            KeyCode::Home | KeyCode::End | KeyCode::Left | KeyCode::Right => EventResult::Consumed,
            KeyCode::Enter => self.submit(),
            _ => EventResult::Ignored,
        }
    }
}

impl<S: 'static, M: 'static> Component<S, M> for TextInput<S, M> {
    fn prepare(&mut self, state: &S) {
        self.resolved_value = self
            .value
            .as_ref()
            .map_or_else(String::new, |(read, _)| read(state));
    }

    fn declare(&mut self, _ctx: &mut DeclareCtx<'_, S, M>) {
        // A text input paints itself; it declares no children.
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_, S>) {
        let style = resolve_style(self.style.as_deref(), ctx.theme, TextInputStyle::from_theme);
        let widget = TextInputWidget {
            value: &self.resolved_value,
            placeholder: &self.placeholder,
            focused: ctx.focused(),
            disabled: self.disabled,
            bordered: self.bordered,
            style,
        };
        ctx.widget(widget, ctx.area());
    }

    fn handle_event(
        &mut self,
        event: &Event,
        state: &S,
        _ctx: &mut EventCtx<'_>,
    ) -> EventResult<M> {
        if !self.can_act() {
            return EventResult::Ignored;
        }
        match event {
            Event::Key(key) => self.handle_key(*key, state),
            Event::Paste(text) => self.edit(state, |current| self.append(current, text)),
            _ => EventResult::Ignored,
        }
    }

    fn scope_options(&self) -> ScopeOptions {
        ScopeOptions::default().focusable(self.can_act())
    }

    fn interaction_area(&self, area: Rect) -> Rect {
        geometry::fixed_height(area, self.height())
    }
}

/// A text input that only draws — an ordinary ratatui [`Widget`] with no
/// focus, events, or state.
struct TextInputWidget<'a> {
    value: &'a str,
    placeholder: &'a str,
    focused: bool,
    disabled: bool,
    bordered: bool,
    style: TextInputStyle,
}

impl Widget for TextInputWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let border_color = if self.focused {
            self.style.border_focused
        } else {
            self.style.border
        };
        let text_area = if self.bordered {
            let block = Block::bordered()
                .border_style(Style::default().fg(border_color))
                .style(Style::default().bg(self.style.background));
            let inner = block.inner(area);
            block.render(area, buf);
            geometry::fixed_height(inner, 1)
        } else {
            let row = geometry::fixed_height(area, 1);
            buf.set_style(row, Style::default().bg(self.style.background));
            row
        };
        if text_area.width == 0 {
            return;
        }

        let is_placeholder = self.value.is_empty();
        let content = if is_placeholder {
            self.placeholder
        } else {
            self.value
        };
        let text_color = if is_placeholder || self.disabled {
            self.style.placeholder_foreground
        } else {
            self.style.foreground
        };

        // The cursor lives at the end of the value, so once it overflows the
        // area what stays visible is the tail, not the head.
        let show_cursor = self.focused && !self.disabled;
        let reserve = u16::from(show_cursor);
        let content_width = text_area.width.saturating_sub(reserve);
        let shown = tail_to_width(content, usize::from(content_width));
        let shown_width = text_width::display_width_u16(shown);

        if content_width > 0 {
            Span::styled(
                shown,
                Style::default().fg(text_color).bg(self.style.background),
            )
            .render(Rect::new(text_area.x, text_area.y, content_width, 1), buf);
        }

        if show_cursor {
            let cursor_x = text_area.x.saturating_add(shown_width.min(content_width));
            if cursor_x < text_area.x.saturating_add(text_area.width) {
                buf.set_style(
                    Rect::new(cursor_x, text_area.y, 1, 1),
                    Style::default().bg(self.style.cursor),
                );
            }
        }
    }
}

/// The longest suffix of `text` that fits in `width` cells, cut on a `char`
/// boundary — the tail-keeping companion to
/// `ratcn::text_width::truncate_to_width`, which keeps the head. A text
/// input's cursor is always at the end of the value, so once the value
/// outgrows its area the tail, not the head, is what stays visible.
fn tail_to_width(text: &str, width: usize) -> &str {
    if width == 0 || text.is_empty() {
        return "";
    }
    if text_width::display_width(text) <= width {
        return text;
    }
    let mut start = text.len();
    let mut used = 0_usize;
    for (idx, ch) in text.char_indices().rev() {
        let mut buf = [0_u8; 4];
        let cell_width = text_width::display_width(ch.encode_utf8(&mut buf));
        if used + cell_width > width {
            break;
        }
        used += cell_width;
        start = idx;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};
    use ratcn::{
        Button, Theme,
        runtime::{
            ChildId, DeclareCtx, Event, EventResult, FocusState, KeyCode, KeyEvent, Ratcn, TabWrap,
        },
    };

    use super::*;

    #[derive(Default)]
    struct State {
        focus: FocusState,
        value: String,
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Msg {
        Focus(FocusState),
        Value(String),
    }

    fn ratcn_runtime() -> Ratcn<State, Msg> {
        Ratcn::new()
            .focus(|state: &State| &state.focus, Msg::Focus)
            .tab_wrap(TabWrap::Wrap)
    }

    fn render(
        terminal: &mut Terminal<TestBackend>,
        ratcn: &mut Ratcn<State, Msg>,
        state: &State,
        declare: impl FnOnce(&mut DeclareCtx<'_, State, Msg>),
    ) {
        let theme = Theme::default_dark();
        terminal
            .draw(|frame| ratcn.render(frame, frame.area(), state, &theme, declare))
            .expect("draw");
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code))
    }

    fn declare_input(ctx: &mut DeclareCtx<'_, State, Msg>) {
        ctx.component(
            ChildId::Static("input"),
            TextInput::new().value(|state: &State| state.value.clone(), Msg::Value),
            Rect::new(0, 0, 20, 3),
        );
    }

    /// The whole point of a controlled binding: two keystrokes answered
    /// before the next render still compose, because each reads the state
    /// argument `handle_event` was called with rather than a value copied at
    /// declaration.
    #[test]
    fn two_keystrokes_before_a_redraw_compose() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).expect("terminal");
        let mut ratcn = ratcn_runtime();
        let state = State::default();
        render(&mut terminal, &mut ratcn, &state, declare_input);

        let EventResult::Emit(Msg::Value(after_a)) =
            ratcn.handle_event(key(KeyCode::Char('a')), &state)
        else {
            panic!("'a' must be emitted as a value change");
        };
        assert_eq!(after_a, "a");

        let state_after_a = State {
            value: after_a,
            ..State::default()
        };
        let EventResult::Emit(Msg::Value(after_b)) =
            ratcn.handle_event(key(KeyCode::Char('b')), &state_after_a)
        else {
            panic!("'b' must be emitted as a value change");
        };
        assert_eq!(after_b, "ab");
    }

    #[test]
    fn digits_only_drops_non_digit_characters() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).expect("terminal");
        let mut ratcn = ratcn_runtime();
        let state = State::default();
        render(&mut terminal, &mut ratcn, &state, |ctx| {
            ctx.component(
                ChildId::Static("input"),
                TextInput::new()
                    .digits_only(true)
                    .value(|state: &State| state.value.clone(), Msg::Value),
                Rect::new(0, 0, 20, 3),
            );
        });

        assert_eq!(
            ratcn.handle_event(key(KeyCode::Char('x')), &state),
            EventResult::Consumed
        );
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).expect("terminal");
        let mut ratcn = ratcn_runtime();
        let state = State {
            value: "ab".to_owned(),
            ..State::default()
        };
        render(&mut terminal, &mut ratcn, &state, declare_input);

        assert_eq!(
            ratcn.handle_event(key(KeyCode::Backspace), &state),
            EventResult::Emit(Msg::Value("a".to_owned()))
        );
    }

    /// The empty-value row paints the placeholder in its own muted color,
    /// never the value's.
    #[test]
    fn placeholder_is_painted_when_empty() {
        let area = Rect::new(0, 0, 20, 1);
        let theme = Theme::default_dark();
        let style = TextInputStyle::from_theme(&theme);
        let mut buffer = Buffer::empty(area);
        TextInputWidget {
            value: "",
            placeholder: "hostname",
            focused: false,
            disabled: false,
            bordered: false,
            style,
        }
        .render(area, &mut buffer);

        let row: String = (0..8u16)
            .map(|column| buffer.cell((column, 0)).expect("cell").symbol())
            .collect();
        assert_eq!(row, "hostname");
        assert_eq!(
            buffer.cell((0, 0)).expect("cell").fg,
            style.placeholder_foreground
        );
    }

    /// Disabled is the loudest state: not focusable at all. The input is
    /// declared *before* the button, so if it were wrongly focusable the
    /// runtime's default focus (the first focusable leaf in declaration
    /// order) would land there instead, and Enter would answer `Ignored`
    /// rather than reach the button.
    #[test]
    fn a_disabled_input_is_not_focusable() {
        let mut terminal = Terminal::new(TestBackend::new(30, 6)).expect("terminal");
        let mut ratcn = ratcn_runtime();
        let state = State::default();
        render(&mut terminal, &mut ratcn, &state, |ctx| {
            ctx.component(
                ChildId::Static("input"),
                TextInput::new()
                    .disabled(true)
                    .value(|state: &State| state.value.clone(), Msg::Value),
                Rect::new(2, 1, 20, 3),
            );
            ctx.component(
                ChildId::Static("button"),
                Button::new("Save").on_press(|| Msg::Value("saved".to_owned())),
                Rect::new(2, 5, 10, 1),
            );
        });

        assert_eq!(
            ratcn.handle_event(key(KeyCode::Enter), &state),
            EventResult::Emit(Msg::Value("saved".to_owned()))
        );
    }
}
