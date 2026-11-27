//! A single-line text input: the one interactive ratcn component this crate
//! writes itself. It predates ratcn's own `Input`, which this crate does not
//! use.
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
//!
//! The cursor is the one exception, because it means nothing to the app. It
//! lives in ratcn's per-component transient storage as a [`Caret`]: event
//! handling writes it through `EventCtx::transient`, and
//! [`declare`](Component::declare) reads it back through
//! `DeclareCtx::transient` for the same instance's
//! [`paint`](Component::paint), which has no transient access of its own.

use std::{
    fmt,
    hash::{DefaultHasher, Hash, Hasher},
    rc::Rc,
};

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
/// on the cursor cell while focused is [`cursor`](Self::cursor).
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
    /// Background of the caret painted on the cursor cell while focused (the
    /// cell after the value when the cursor is at the end).
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
/// focusable and answers no events.
///
/// Keys, while the field can act:
///
/// - Left / Right move one character; Home / End and Ctrl-A / Ctrl-E move to
///   the start / end.
/// - Alt-Left / Alt-Right and Alt-B / Alt-F move one word, a word being a
///   maximal run of non-whitespace.
/// - A character is inserted at the cursor; Backspace deletes before it,
///   Delete at it; Ctrl-W deletes the word before it, Ctrl-U everything
///   before it and Ctrl-K everything from it.
/// - A bracketed paste is inserted at the cursor as one edit, control
///   characters dropped and the rest truncated to fit.
///
/// Every edit emits the whole new value as [`EventResult::Emit`]. A key from
/// that list that changes nothing (Left at the start, a rejected character)
/// is [`EventResult::Consumed`], so it never falls through. Enter emits
/// [`on_submit`](Self::on_submit) when one is bound, and is otherwise left
/// [`EventResult::Ignored`] so an enclosing dialog can handle it. Tab,
/// Shift+Tab, Esc and every other chord, Ctrl-C included, are always left
/// `Ignored` — this component never intercepts navigation or the quit chord.
pub struct TextInput<S, M> {
    value: Option<(ReadValueFn<S>, OnChangeFn<M>)>,
    placeholder: String,
    digits_only: bool,
    max_len: Option<usize>,
    disabled: bool,
    on_submit: Option<OnSubmitFn<M>>,
    bordered: bool,
    style: Option<StyleFn>,
    /// The cursor as this frame's [`declare`](Component::declare) read it
    /// from transient storage, for [`paint`](Component::paint).
    caret: Caret,
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
            caret: Caret::default(),
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

    /// Insert `input` at character `cursor` of `current` as one edit.
    ///
    /// Control characters (newlines, tabs, ESC, DEL, …) are dropped first,
    /// then whatever [`accepts`](Self::accepts) rejects; what is left is
    /// truncated to the room [`max_len`](Self::max_len) still allows rather
    /// than rejected. The cursor lands after the inserted text.
    fn insert(&self, current: &str, cursor: usize, input: &str) -> Outcome {
        let room = self.max_len.map_or(usize::MAX, |max_len| {
            max_len.saturating_sub(current.chars().count())
        });
        let accepted: String = input
            .chars()
            .filter(|c| !c.is_control() && self.accepts(*c))
            .take(room)
            .collect();
        if accepted.is_empty() {
            return Outcome::Move(cursor);
        }
        let mut next = current.to_owned();
        next.insert_str(byte_at(current, cursor), &accepted);
        Outcome::Edit(next, cursor + accepted.chars().count())
    }

    /// What `command` does to `current` with the cursor at character
    /// `cursor` (already clamped to it).
    fn apply(&self, command: Command<'_>, current: &str, cursor: usize) -> Outcome {
        let len = current.chars().count();
        match command {
            Command::Insert(c) => {
                let mut buf = [0_u8; 4];
                self.insert(current, cursor, c.encode_utf8(&mut buf))
            }
            Command::Paste(text) => self.insert(current, cursor, text),
            Command::Left => Outcome::Move(cursor.saturating_sub(1)),
            Command::Right => Outcome::Move((cursor + 1).min(len)),
            Command::Home => Outcome::Move(0),
            Command::End => Outcome::Move(len),
            Command::WordLeft => Outcome::Move(word_start_before(current, cursor)),
            Command::WordRight => Outcome::Move(word_end_after(current, cursor)),
            Command::Backspace => delete(current, cursor.saturating_sub(1), cursor),
            Command::Delete => delete(current, cursor, (cursor + 1).min(len)),
            Command::DeleteWordBefore => {
                delete(current, word_start_before(current, cursor), cursor)
            }
            Command::DeleteToStart => delete(current, 0, cursor),
            Command::DeleteToEnd => delete(current, cursor, len),
            Command::Submit => Outcome::Submit,
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
}

/// Where the cursor sits in one field, kept in ratcn's per-component
/// transient storage because the component itself is rebuilt every frame.
///
/// `index` counts characters, never bytes. It is trusted only against the
/// value it was placed in, which `placed_in` fingerprints: a field never
/// touched, or one whose bound value was replaced from outside (a form
/// reset, a programmatic set), resolves to the end of the value instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Caret {
    index: usize,
    placed_in: Option<u64>,
}

impl Caret {
    /// A cursor at character `index` of `value`.
    fn at(index: usize, value: &str) -> Self {
        Self {
            index,
            placed_in: Some(fingerprint(value)),
        }
    }

    /// The cursor's character index in `value`, clamped to its length, or
    /// the end of `value` when this cursor was placed in some other value.
    fn resolve(self, value: &str) -> usize {
        let len = value.chars().count();
        if self.placed_in == Some(fingerprint(value)) {
            self.index.min(len)
        } else {
            len
        }
    }
}

/// A fingerprint of `value`, so [`Caret`] can tell whether the bound value
/// changed underneath it without keeping a second copy of it.
fn fingerprint(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// What a key or a paste asks of the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command<'a> {
    Insert(char),
    Paste(&'a str),
    Left,
    Right,
    Home,
    End,
    WordLeft,
    WordRight,
    Backspace,
    Delete,
    DeleteWordBefore,
    DeleteToStart,
    DeleteToEnd,
    Submit,
}

impl Command<'_> {
    /// The command `key` names, or `None` for every key the field leaves
    /// [`EventResult::Ignored`]: Tab, BackTab, Esc, Up/Down, and any Ctrl or
    /// Alt chord not listed here — Ctrl-C and any Ctrl+Alt chord included,
    /// so the quit path still receives them.
    fn from_key(key: KeyEvent) -> Option<Self> {
        let KeyEvent { code, modifiers } = key;
        let command = match (modifiers.ctrl, modifiers.alt, code) {
            (false, false, KeyCode::Char(c)) => Self::Insert(c),
            (false, false, KeyCode::Left) => Self::Left,
            (false, false, KeyCode::Right) => Self::Right,
            (false, false, KeyCode::Home) | (true, false, KeyCode::Char('a')) => Self::Home,
            (false, false, KeyCode::End) | (true, false, KeyCode::Char('e')) => Self::End,
            (false, false, KeyCode::Backspace) => Self::Backspace,
            (false, false, KeyCode::Delete) => Self::Delete,
            (false, false, KeyCode::Enter) => Self::Submit,
            (true, false, KeyCode::Char('w')) => Self::DeleteWordBefore,
            (true, false, KeyCode::Char('u')) => Self::DeleteToStart,
            (true, false, KeyCode::Char('k')) => Self::DeleteToEnd,
            (false, true, KeyCode::Left | KeyCode::Char('b')) => Self::WordLeft,
            (false, true, KeyCode::Right | KeyCode::Char('f')) => Self::WordRight,
            _ => return None,
        };
        Some(command)
    }
}

/// What a [`Command`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// The value became the string, with the cursor at the character index.
    Edit(String, usize),
    /// The value is unchanged and the cursor is at the character index,
    /// which may be where it already was.
    Move(usize),
    /// Enter: hand over to [`TextInput::on_submit`].
    Submit,
}

/// The byte offset of character `index` in `text`, or `text.len()` at or
/// past its end.
fn byte_at(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// `text` with characters `start..end` removed and the cursor at `start`,
/// or a plain [`Outcome::Move`] to `end` when the range is empty — so a
/// deletion with nothing to delete leaves the cursor where it was.
fn delete(text: &str, start: usize, end: usize) -> Outcome {
    if start >= end {
        return Outcome::Move(end);
    }
    let mut next = text.to_owned();
    next.replace_range(byte_at(text, start)..byte_at(text, end), "");
    Outcome::Edit(next, start)
}

/// The start of the word before character `cursor`: back over any
/// whitespace, then over the run of non-whitespace before it.
fn word_start_before(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = cursor.min(chars.len());
    while index > 0 && chars[index - 1].is_whitespace() {
        index -= 1;
    }
    while index > 0 && !chars[index - 1].is_whitespace() {
        index -= 1;
    }
    index
}

/// The end of the word after character `cursor`: forward over any
/// whitespace, then over the run of non-whitespace after it.
fn word_end_after(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut index = cursor.min(chars.len());
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    while index < chars.len() && !chars[index].is_whitespace() {
        index += 1;
    }
    index
}

impl<S: 'static, M: 'static> Component<S, M> for TextInput<S, M> {
    fn declare(&mut self, ctx: &mut DeclareCtx<'_, S, M>) {
        // A text input paints itself; it declares no children. `paint` has
        // no transient access, so the cursor event handling stored is read
        // here and handed to this same instance's paint.
        self.caret = *ctx.transient::<Caret>();
    }

    fn paint(&mut self, ctx: &mut PaintCtx<'_, S>) {
        let style = resolve_style(self.style.as_deref(), ctx.theme, TextInputStyle::from_theme);
        let value = self
            .value
            .as_ref()
            .map_or_else(String::new, |(read, _)| read(ctx.state()));
        let widget = TextInputWidget {
            cursor: self.caret.resolve(&value),
            value: &value,
            placeholder: &self.placeholder,
            focused: ctx.focused(),
            disabled: self.disabled,
            bordered: self.bordered,
            style,
        };
        ctx.widget(widget, ctx.area());
    }

    fn handle_event(&mut self, event: &Event, state: &S, ctx: &mut EventCtx<'_>) -> EventResult<M> {
        if !self.can_act() {
            return EventResult::Ignored;
        }
        let Some((read, on_change)) = &self.value else {
            return EventResult::Ignored;
        };
        let command = match event {
            Event::Key(key) => match Command::from_key(*key) {
                Some(command) => command,
                None => return EventResult::Ignored,
            },
            Event::Paste(text) => Command::Paste(text),
            _ => return EventResult::Ignored,
        };
        // Read fresh, not from declaration: see `value`.
        let current = read(state);
        let caret = ctx.transient::<Caret>();
        let cursor = caret.resolve(&current);
        match self.apply(command, &current, cursor) {
            Outcome::Edit(next, index) => {
                *caret = Caret::at(index, &next);
                EventResult::Emit(on_change(next))
            }
            Outcome::Move(index) => {
                *caret = Caret::at(index, &current);
                EventResult::Consumed
            }
            Outcome::Submit => self.submit(),
        }
    }

    fn scope_options(&self, _state: &S) -> ScopeOptions {
        ScopeOptions::default().focusable(self.can_act())
    }

    fn interaction_area(&self, area: Rect, _state: &S) -> Rect {
        geometry::fixed_height(area, self.height())
    }
}

/// A text input that only draws — an ordinary ratatui [`Widget`] with no
/// focus, events, or state.
struct TextInputWidget<'a> {
    value: &'a str,
    /// The cursor's character index in `value`; clamped here as well.
    cursor: usize,
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

        // The placeholder keeps the look it always had: the caret sits after
        // it, and a placeholder too wide for the field shows its tail.
        let widths: Vec<usize> = content.chars().map(char_width).collect();
        let cursor = if is_placeholder {
            widths.len()
        } else {
            self.cursor.min(widths.len())
        };
        let show_cursor = self.focused && !self.disabled;
        // The caret covers the character under the cursor, or one cell past
        // the end of the value.
        let caret_width = if show_cursor {
            widths.get(cursor).copied().unwrap_or(1).max(1)
        } else {
            0
        };
        let field_width = usize::from(text_area.width);
        let (start, end) = visible_window(&widths, cursor, caret_width, field_width);
        let shown = &content[byte_at(content, start)..byte_at(content, end)];

        Span::styled(
            shown,
            Style::default().fg(text_color).bg(self.style.background),
        )
        .render(text_area, buf);

        let offset: usize = widths[start..cursor].iter().sum();
        if show_cursor && offset < field_width {
            // Both fit in a u16: they are below the field's own u16 width.
            let x = text_area.x + offset as u16;
            let width = caret_width.min(field_width - offset) as u16;
            buf.set_style(
                Rect::new(x, text_area.y, width, 1),
                Style::default().bg(self.style.cursor),
            );
        }
    }
}

/// Terminal cells `c` takes.
fn char_width(c: char) -> usize {
    let mut buf = [0_u8; 4];
    text_width::display_width(c.encode_utf8(&mut buf))
}

/// The characters `start..end` of a value (one cell width per character in
/// `widths`) to show in a field `width` cells wide, so that character
/// `cursor` and the `caret` cells drawn on it are inside the field.
///
/// The window starts at the head whenever the cursor fits from there, and
/// otherwise as late as needed to keep the cursor on the field's last cells;
/// it then runs as far right as the field allows. A value that fits is shown
/// whole, and a cursor at the end of an overflowing value shows its tail.
fn visible_window(widths: &[usize], cursor: usize, caret: usize, width: usize) -> (usize, usize) {
    let mut start = 0;
    let mut before: usize = widths[..cursor].iter().sum();
    while start < cursor && before + caret > width {
        before -= widths[start];
        start += 1;
    }
    let mut end = start;
    let mut used = 0;
    while end < widths.len() && used + widths[end] <= width {
        used += widths[end];
        end += 1;
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, layout::Rect};
    use ratcn::{
        Button, Theme,
        runtime::{
            ChildId, DeclareCtx, Event, EventResult, FocusState, KeyCode, KeyEvent, Modifiers,
            Ratcn, TabWrap,
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
        declare: impl FnMut(&mut DeclareCtx<'_, State, Msg>),
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
            cursor: 0,
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

    // ─── Cursor and editing keys ─────────────────────────────────────────────

    /// A field driven through the runtime the way the app's loop drives it:
    /// every emitted value is written back to the model and the frame is
    /// redrawn after every event, so the cursor travels through transient
    /// storage exactly as it does in the app.
    struct Field {
        terminal: Terminal<TestBackend>,
        ratcn: Ratcn<State, Msg>,
        state: State,
        input: fn() -> TextInput<State, Msg>,
    }

    fn plain() -> TextInput<State, Msg> {
        TextInput::new().value(|state: &State| state.value.clone(), Msg::Value)
    }

    fn digits_with_room_for_four() -> TextInput<State, Msg> {
        plain().digits_only(true).max_len(4)
    }

    fn submitting() -> TextInput<State, Msg> {
        plain().on_submit(|| Msg::Value("submitted".to_owned()))
    }

    impl Field {
        fn new(value: &str) -> Self {
            Self::with(value, plain)
        }

        fn with(value: &str, input: fn() -> TextInput<State, Msg>) -> Self {
            let mut field = Self {
                terminal: Terminal::new(TestBackend::new(20, 3)).expect("terminal"),
                ratcn: ratcn_runtime(),
                state: State {
                    value: value.to_owned(),
                    ..State::default()
                },
                input,
            };
            field.draw();
            field
        }

        fn draw(&mut self) {
            let input = self.input;
            render(&mut self.terminal, &mut self.ratcn, &self.state, |ctx| {
                ctx.component(ChildId::Static("input"), input(), Rect::new(0, 0, 20, 3));
            });
        }

        fn send(&mut self, event: Event) -> EventResult<Msg> {
            let result = self.ratcn.handle_event(event, &self.state);
            match &result {
                EventResult::Emit(Msg::Value(value)) => self.state.value.clone_from(value),
                EventResult::Emit(Msg::Focus(focus)) => self.state.focus = focus.clone(),
                _ => {}
            }
            self.draw();
            result
        }

        fn press(&mut self, code: KeyCode) -> EventResult<Msg> {
            self.send(key(code))
        }

        fn press_n(&mut self, code: KeyCode, times: usize) {
            for _ in 0..times {
                self.press(code);
            }
        }

        fn chord(&mut self, code: KeyCode, ctrl: bool, alt: bool) -> EventResult<Msg> {
            self.send(Event::Key(KeyEvent {
                code,
                modifiers: Modifiers {
                    ctrl,
                    alt,
                    ..Modifiers::NONE
                },
            }))
        }

        fn ctrl(&mut self, c: char) -> EventResult<Msg> {
            self.chord(KeyCode::Char(c), true, false)
        }

        fn alt(&mut self, code: KeyCode) -> EventResult<Msg> {
            self.chord(code, false, true)
        }

        fn type_str(&mut self, text: &str) {
            for c in text.chars() {
                self.press(KeyCode::Char(c));
            }
        }

        fn paste(&mut self, text: &str) -> EventResult<Msg> {
            self.send(Event::Paste(text.to_owned()))
        }

        /// Replace the bound value from outside the field, as a form reset
        /// or a programmatic set would.
        fn set(&mut self, value: &str) {
            self.state.value = value.to_owned();
            self.draw();
        }

        /// Type `|` and return the value: where the cursor was, made visible.
        fn marked(&mut self) -> String {
            self.press(KeyCode::Char('|'));
            self.state.value.clone()
        }

        /// The field's one text row inside its border.
        fn row(&self) -> String {
            let buffer = self.terminal.backend().buffer();
            (1..19_u16).map(|x| buffer[(x, 1)].symbol()).collect()
        }

        /// The screen column the caret is painted on, if any.
        fn caret_column(&self) -> Option<u16> {
            let cursor = TextInputStyle::from_theme(&Theme::default_dark()).cursor;
            let buffer = self.terminal.backend().buffer();
            (1..19_u16).find(|&x| buffer[(x, 1)].bg == cursor)
        }
    }

    fn value(text: &str) -> EventResult<Msg> {
        EventResult::Emit(Msg::Value(text.to_owned()))
    }

    const WORDS: &str = "my  laptop key";

    #[test]
    fn a_character_typed_after_left_is_inserted_mid_value() {
        let mut field = Field::new("ac");
        assert_eq!(field.press(KeyCode::Left), EventResult::Consumed);
        assert_eq!(field.press(KeyCode::Char('b')), value("abc"));
        assert_eq!(field.marked(), "ab|c");
    }

    #[test]
    fn a_move_that_cannot_move_is_consumed() {
        let mut field = Field::new("ab");
        assert_eq!(field.press(KeyCode::Right), EventResult::Consumed);
        assert_eq!(field.press(KeyCode::End), EventResult::Consumed);
        assert_eq!(field.alt(KeyCode::Right), EventResult::Consumed);
        field.press_n(KeyCode::Left, 2);
        assert_eq!(field.press(KeyCode::Left), EventResult::Consumed);
        assert_eq!(field.press(KeyCode::Home), EventResult::Consumed);
        assert_eq!(field.alt(KeyCode::Left), EventResult::Consumed);
        assert_eq!(field.marked(), "|ab");
    }

    #[test]
    fn backspace_deletes_before_the_cursor_at_the_end_middle_and_start() {
        let mut field = Field::new("abcd");
        assert_eq!(field.press(KeyCode::Backspace), value("abc"));
        field.press(KeyCode::Left);
        assert_eq!(field.press(KeyCode::Backspace), value("ac"));
        field.press(KeyCode::Home);
        assert_eq!(field.press(KeyCode::Backspace), EventResult::Consumed);
        assert_eq!(field.marked(), "|ac");
    }

    #[test]
    fn delete_deletes_at_the_cursor_at_the_end_start_and_middle() {
        let mut field = Field::new("abcd");
        assert_eq!(field.press(KeyCode::Delete), EventResult::Consumed);
        field.press(KeyCode::Home);
        assert_eq!(field.press(KeyCode::Delete), value("bcd"));
        field.press(KeyCode::Right);
        assert_eq!(field.press(KeyCode::Delete), value("bd"));
        assert_eq!(field.marked(), "b|d");
    }

    #[test]
    fn home_end_and_ctrl_a_ctrl_e_jump_to_the_ends() {
        let mut field = Field::new("abc");
        field.press(KeyCode::Home);
        field.type_str("x");
        field.press(KeyCode::End);
        field.type_str("y");
        assert_eq!(field.state.value, "xabcy");
        assert_eq!(field.ctrl('a'), EventResult::Consumed);
        field.type_str("1");
        assert_eq!(field.ctrl('e'), EventResult::Consumed);
        field.type_str("2");
        assert_eq!(field.state.value, "1xabcy2");
    }

    #[test]
    fn alt_left_and_alt_b_move_back_one_word() {
        let cases = [
            (1, "my  laptop |key"),
            (2, "my  |laptop key"),
            (3, "|my  laptop key"),
        ];
        for code in [KeyCode::Left, KeyCode::Char('b')] {
            for (times, expected) in cases {
                let mut field = Field::new(WORDS);
                for _ in 0..times {
                    assert_eq!(field.alt(code), EventResult::Consumed);
                }
                assert_eq!(field.marked(), expected, "Alt-{code:?} x{times}");
            }
        }
    }

    #[test]
    fn alt_right_and_alt_f_move_forward_one_word() {
        let cases = [
            (1, "my|  laptop key"),
            (2, "my  laptop| key"),
            (3, "my  laptop key|"),
        ];
        for code in [KeyCode::Right, KeyCode::Char('f')] {
            for (times, expected) in cases {
                let mut field = Field::new(WORDS);
                field.press(KeyCode::Home);
                for _ in 0..times {
                    assert_eq!(field.alt(code), EventResult::Consumed);
                }
                assert_eq!(field.marked(), expected, "Alt-{code:?} x{times}");
            }
        }
    }

    #[test]
    fn ctrl_w_deletes_the_word_before_the_cursor() {
        let mut field = Field::new(WORDS);
        assert_eq!(field.ctrl('w'), value("my  laptop "));
        assert_eq!(field.ctrl('w'), value("my  "));
        assert_eq!(field.ctrl('w'), value(""));
        assert_eq!(field.ctrl('w'), EventResult::Consumed);

        // From inside a word only the part before the cursor goes.
        let mut field = Field::new(WORDS);
        field.press(KeyCode::Home);
        field.press_n(KeyCode::Right, 7);
        assert_eq!(field.ctrl('w'), value("my  top key"));
        assert_eq!(field.marked(), "my  |top key");
    }

    #[test]
    fn ctrl_k_deletes_to_the_end_and_ctrl_u_to_the_start() {
        let mut field = Field::new("hello world");
        field.press_n(KeyCode::Left, 5);
        assert_eq!(field.ctrl('k'), value("hello "));
        assert_eq!(field.ctrl('k'), EventResult::Consumed);
        field.press(KeyCode::Left);
        assert_eq!(field.ctrl('u'), value(" "));
        assert_eq!(field.ctrl('u'), EventResult::Consumed);
        assert_eq!(field.marked(), "| ");
    }

    #[test]
    fn digits_only_and_max_len_hold_for_a_mid_value_insert() {
        let mut field = Field::with("12", digits_with_room_for_four);
        field.press(KeyCode::Left);
        assert_eq!(field.press(KeyCode::Char('x')), EventResult::Consumed);
        assert_eq!(field.press(KeyCode::Char('5')), value("152"));
        assert_eq!(field.press(KeyCode::Char('6')), value("1562"));
        assert_eq!(field.press(KeyCode::Char('7')), EventResult::Consumed);
        assert_eq!(field.state.value, "1562");
    }

    #[test]
    fn a_paste_is_filtered_then_truncated_to_the_room_left() {
        let mut field = Field::with("12", digits_with_room_for_four);
        field.press(KeyCode::Home);
        assert_eq!(field.paste("a3b45"), value("3412"));
        assert_eq!(field.paste("9"), EventResult::Consumed);
    }

    #[test]
    fn a_paste_drops_control_characters_and_lands_at_the_cursor() {
        let mut field = Field::new("ab");
        field.press(KeyCode::Left);
        assert_eq!(field.paste("1\n2\t3\x1b4\x7f\r"), value("a1234b"));
        assert_eq!(field.paste("\n\t\x1b"), EventResult::Consumed);
        assert_eq!(field.marked(), "a1234|b");
    }

    #[test]
    fn multi_byte_characters_are_single_characters() {
        let mut field = Field::new("");
        field.type_str("é日x");
        assert_eq!(field.state.value, "é日x");
        field.press(KeyCode::Left);
        assert_eq!(field.press(KeyCode::Backspace), value("éx"));
        field.press(KeyCode::Home);
        field.press(KeyCode::Right);
        field.type_str("日");
        assert_eq!(field.state.value, "é日x");
        field.press(KeyCode::Home);
        assert_eq!(field.press(KeyCode::Delete), value("日x"));
        field.press(KeyCode::Right);
        assert_eq!(field.marked(), "日|x");
        assert_eq!(field.ctrl('w'), value("x"));
    }

    #[test]
    fn the_cursor_is_clamped_after_the_bound_value_shrinks() {
        let mut field = Field::new("abcdef");
        field.press_n(KeyCode::Left, 2);
        field.set("ab");
        assert_eq!(field.marked(), "ab|");

        let stale = Caret {
            index: 9,
            ..Caret::at(0, "ab")
        };
        assert_eq!(stale.resolve("ab"), 2);
    }

    #[test]
    fn a_value_replaced_from_outside_puts_the_cursor_at_the_end() {
        let mut field = Field::new("abcd");
        field.press(KeyCode::Home);
        field.set("wxyz");
        assert_eq!(field.marked(), "wxyz|");
    }

    #[test]
    fn ctrl_c_esc_and_other_chords_are_ignored() {
        let mut field = Field::new("abc");
        field.press(KeyCode::Home);
        assert_eq!(field.ctrl('c'), EventResult::Ignored);
        assert_eq!(field.press(KeyCode::Esc), EventResult::Ignored);
        assert_eq!(
            field.chord(KeyCode::Char('c'), true, true),
            EventResult::Ignored
        );
        assert_eq!(field.alt(KeyCode::Char('x')), EventResult::Ignored);
        assert_eq!(field.press(KeyCode::Up), EventResult::Ignored);
        assert_eq!(field.state.value, "abc");
        assert_eq!(field.marked(), "|abc");

        for code in [KeyCode::Tab, KeyCode::BackTab, KeyCode::Esc] {
            assert_eq!(Command::from_key(KeyEvent::new(code)), None, "{code:?}");
        }
    }

    #[test]
    fn enter_submits_only_when_a_submit_is_bound() {
        assert_eq!(Field::new("a").press(KeyCode::Enter), EventResult::Ignored);
        assert_eq!(
            Field::with("a", submitting).press(KeyCode::Enter),
            value("submitted")
        );
    }

    /// One unbordered, focused row `width` cells wide: its text, and the
    /// column the caret is painted on.
    fn painted_row(value: &str, cursor: usize, width: u16) -> (String, Option<u16>) {
        let area = Rect::new(0, 0, width, 1);
        let style = TextInputStyle::from_theme(&Theme::default_dark());
        let mut buffer = Buffer::empty(area);
        TextInputWidget {
            value,
            cursor,
            placeholder: "",
            focused: true,
            disabled: false,
            bordered: false,
            style,
        }
        .render(area, &mut buffer);
        let row = (0..width)
            .map(|x| buffer.cell((x, 0)).expect("cell").symbol())
            .collect();
        let caret = (0..width).find(|&x| buffer.cell((x, 0)).expect("cell").bg == style.cursor);
        (row, caret)
    }

    #[test]
    fn a_value_wider_than_the_field_shows_the_part_around_the_cursor() {
        let alphabet = "abcdefghijklmnopqrstuvwxyz";
        assert_eq!(
            painted_row(alphabet, 3, 10),
            ("abcdefghij".to_owned(), Some(3))
        );
        assert_eq!(
            painted_row(alphabet, 20, 10),
            ("lmnopqrstu".to_owned(), Some(9))
        );
        assert_eq!(
            painted_row(alphabet, 26, 10),
            ("rstuvwxyz ".to_owned(), Some(9))
        );
        // Two cells per character: the caret covers the one under it.
        assert_eq!(painted_row("日本語", 2, 4).1, Some(2));
    }

    #[test]
    fn moving_home_in_an_overflowing_field_scrolls_back_to_the_head() {
        let mut field = Field::new("0123456789abcdefghijklmnopqrst");
        assert_eq!(field.row(), "defghijklmnopqrst ");
        assert_eq!(field.caret_column(), Some(18));
        field.press(KeyCode::Home);
        assert_eq!(field.row(), "0123456789abcdefgh");
        assert_eq!(field.caret_column(), Some(1));
    }
}
