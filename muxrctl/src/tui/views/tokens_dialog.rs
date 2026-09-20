//! The token dialogs: the list, the create form, the one-time minted secret,
//! and the revoke confirmation.
//!
//! Four modals over one feature, each a ratcn [`Dialog`] keyed by its
//! [`DialogId`]. The list never commits on Enter — moving the cursor only
//! reports which token is focused, and every action is a button, so a stray
//! Enter can never revoke anything. The pairing QR is its own fullscreen layer
//! ([`super::qr_overlay`]).

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratcn::runtime::DeclareCtx;
use ratcn::{Button, Checkbox, Cycle, Dialog, List, ListItem};

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::tokens::{
    TOKEN_EXPLAIN_EXPIRY, TOKEN_EXPLAIN_NAME, TOKEN_EXPLAIN_READ_ONLY, TOKEN_EXPLAIN_SECRET,
    TokenExpiryChoice, TokensMsg,
};
use crate::tui::components::TextInput;

/// Outer width every token dialog is laid out against, matching the Config
/// dialog so the stack does not jump as dialogs open on top of each other.
const OUTER_WIDTH: u16 = 78;

/// Rows the token list occupies inside the Tokens dialog.
const LIST_ROWS: u16 = 8;

/// Rows the Tokens dialog's content strip occupies. `Dialog::content` cannot
/// measure a closure, so this must match [`list_content`]'s layout exactly.
const LIST_CONTENT_ROWS: u16 = LIST_ROWS + 1 + 1;

/// Rows the create form occupies — see [`create_content`]'s layout.
const CREATE_CONTENT_ROWS: u16 = 1 + 3 + 1 + 1 + 1 + 2 + 1 + 1 + 3;

/// Rows the minted-secret body occupies — see [`minted_content`]'s layout.
const MINTED_CONTENT_ROWS: u16 = 1 + 1 + 1 + 1 + 1 + 1 + 2;

/// Column the read-only badge starts at in a list row.
const NAME_COLUMN: usize = 28;

// ── The token list ────────────────────────────────────────────────────────────

/// Declare the Tokens dialog: the list plus every token action.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let loading = ctx.state().tokens.loading;
    // A QR can only be built from a secret muxrctl still holds in memory; once
    // it is gone the token is a hash in the DB and nothing can re-derive it.
    let has_secret = ctx.state().tokens.last_minted_secret.is_some();
    let has_focus = ctx.state().tokens.focused_record().is_some();

    let dialog = Dialog::new()
        .title("Tokens")
        .outer_width(OUTER_WIDTH)
        .content(LIST_CONTENT_ROWS, list_content)
        .action(
            "tok_create",
            Button::new("Create…").on_press(|| tokens(TokensMsg::CreateRequested)),
        )
        .action(
            "tok_qr",
            Button::new("Show QR")
                .disabled(!has_secret)
                .on_press(|| tokens(TokensMsg::ShowQrForMinted)),
        )
        .action(
            "tok_revoke",
            Button::new("Revoke…")
                .destructive()
                .disabled(!has_focus || loading)
                .on_press(|| tokens(TokensMsg::RevokeRequested)),
        )
        .action(
            "tok_refresh",
            Button::new("Refresh")
                .ghost()
                .disabled(loading)
                .on_press(|| tokens(TokensMsg::Refresh)),
        )
        .action(
            "tok_close",
            Button::new("Close").on_press(|| tokens(TokensMsg::Close)),
        )
        .on_dismiss(|| tokens(TokensMsg::Close));
    ctx.modal(DialogId::Tokens.id(), dialog, area);
}

/// The Tokens dialog's content strip: the list and the pairing hint.
fn list_content(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(LIST_ROWS), // list (or its empty state)
            Constraint::Length(1),         // gap
            Constraint::Length(1),         // pairing hint
        ])
        .split(ctx.area());

    let muted = muted_style(ctx);

    if ctx.state().tokens.tokens.is_empty() {
        let message = if ctx.state().tokens.loading {
            "Loading…"
        } else {
            "No tokens yet — Create one."
        };
        ctx.paint_widget(
            Paragraph::new(Line::from(Span::styled(message, muted))),
            rows[0],
        );
    } else {
        let items: Vec<ListItem<String>> = ctx
            .state()
            .tokens
            .tokens
            .iter()
            .map(|record| ListItem::new(record.name.clone(), record.name.clone()))
            .collect();
        ctx.component(
            "tok_list",
            List::new(items)
                // Cursor only: no `selection` binding, so Enter on a row commits
                // nothing. Revoking is a button behind a confirmation, never a
                // keystroke on whatever the cursor happened to be over.
                .item_focus(
                    |state: &AppState| state.tokens.focused.clone(),
                    |name, _| tokens(TokensMsg::Focused(name)),
                )
                .paint_item(|state: &AppState, row| row_line(state, row.value))
                .focus_symbol("> "),
            rows[0],
        );
    }

    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled(
            "A freshly created token can be shown as a pairing QR from here.",
            muted,
        ))),
        rows[2],
    );
}

/// One list row: `name  [rw|ro]  created`.
fn row_line(state: &AppState, name: &str) -> Line<'static> {
    let Some(record) = state.tokens.tokens.iter().find(|t| t.name == name) else {
        return Line::from(name.to_string());
    };
    let badge = if record.read_only { "[ro]" } else { "[rw]" };
    let mut label = record.name.clone();
    while label.chars().count() < NAME_COLUMN {
        label.push(' ');
    }
    Line::from(format!("{label}{badge}  {}", record.created_at))
}

// ── Create ────────────────────────────────────────────────────────────────────

/// Declare the create form.
pub fn declare_create(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let loading = ctx.state().tokens.loading;
    let dialog = Dialog::new()
        .title("Create token")
        .outer_width(OUTER_WIDTH)
        .content(CREATE_CONTENT_ROWS, create_content)
        .action(
            "tok_create_ok",
            Button::new("Create")
                .disabled(loading)
                .on_press(|| tokens(TokensMsg::CreateSubmit)),
        )
        .action(
            "tok_create_cancel",
            Button::new("Cancel")
                .secondary()
                .on_press(|| tokens(TokensMsg::CreateCancel)),
        )
        .on_dismiss(|| tokens(TokensMsg::CreateCancel));
    ctx.modal(DialogId::TokenCreate.id(), dialog, area);
}

/// The create form's content strip: three controls, each with its explanation.
fn create_content(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // "Name" label
            Constraint::Length(3), // name field
            Constraint::Length(1), // name explanation
            Constraint::Length(1), // gap
            Constraint::Length(1), // read-only checkbox
            Constraint::Length(2), // read-only explanation
            Constraint::Length(1), // gap
            Constraint::Length(1), // expiry cycle
            Constraint::Length(3), // expiry explanation (three wrapped rows)
        ])
        .split(ctx.area());

    let muted = muted_style(ctx);

    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Name", muted))),
        rows[0],
    );
    ctx.component(
        "tok_name",
        TextInput::new()
            .value(
                |state: &AppState| state.tokens.form_name.clone(),
                |value| tokens(TokensMsg::CreateNameChanged(value)),
            )
            .placeholder("optional name")
            .max_len(64)
            .on_submit(|| tokens(TokensMsg::CreateSubmit)),
        rows[1],
    );
    paint_explain(ctx, TOKEN_EXPLAIN_NAME, muted, rows[2]);

    ctx.component(
        "tok_ro",
        Checkbox::new("Read-only").checked(
            |state: &AppState| state.tokens.form_read_only,
            |checked| tokens(TokensMsg::CreateReadOnlyChanged(checked)),
        ),
        rows[4],
    );
    paint_explain(ctx, TOKEN_EXPLAIN_READ_ONLY, muted, rows[5]);

    let [label_area, cycle_area] =
        Layout::horizontal([Constraint::Length(9), Constraint::Min(0)]).areas(rows[7]);
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Expires:", muted))),
        label_area,
    );
    ctx.component(
        "tok_expiry",
        Cycle::new(TokenExpiryChoice::ALL.map(TokenExpiryChoice::label)).selection(
            |state: &AppState| state.tokens.form_expiry.index(),
            |index| tokens(TokensMsg::CreateExpiryChanged(index)),
        ),
        cycle_area,
    );
    paint_explain(ctx, TOKEN_EXPLAIN_EXPIRY, muted, rows[8]);
}

// ── The one-time secret ───────────────────────────────────────────────────────

/// Declare the minted-secret dialog — the only place the plaintext is shown.
pub fn declare_minted(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let dialog = Dialog::new()
        .title("New token — copy now, shown once")
        .outer_width(OUTER_WIDTH)
        .content(MINTED_CONTENT_ROWS, minted_content)
        .action(
            "tok_minted_qr",
            Button::new("Show pairing QR").on_press(|| tokens(TokensMsg::MintedShowQr)),
        )
        .action(
            "tok_minted_done",
            Button::new("Done")
                .secondary()
                .on_press(|| tokens(TokensMsg::MintedDone)),
        )
        .on_dismiss(|| tokens(TokensMsg::MintedDone));
    ctx.modal(DialogId::TokenMinted.id(), dialog, area);
}

/// The minted-secret body: name, the plaintext, the read-only badge, the warning.
fn minted_content(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // name
            Constraint::Length(1), // gap
            Constraint::Length(1), // secret
            Constraint::Length(1), // gap
            Constraint::Length(1), // access badge
            Constraint::Length(1), // gap
            Constraint::Length(2), // explanation
        ])
        .split(ctx.area());

    let theme = *ctx.theme;
    let muted = Style::default().fg(theme.muted_foreground);
    let accent = Style::default().fg(theme.accent);

    let Some((name, secret, read_only)) = ctx.state().tokens.last_minted_secret.clone() else {
        // Only reachable if the secret were dropped while the dialog is up; the
        // reducer keeps it until the next create, revoke or reload.
        ctx.paint_widget(
            Paragraph::new(Line::from(Span::styled(
                "The secret is no longer available.",
                muted,
            ))),
            rows[0],
        );
        return;
    };

    ctx.paint_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Name: ", muted),
            Span::raw(name),
        ])),
        rows[0],
    );
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled(secret, accent))),
        rows[2],
    );
    ctx.paint_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Access: ", muted),
            Span::raw(if read_only { "read-only" } else { "read-write" }),
        ])),
        rows[4],
    );
    paint_explain(ctx, TOKEN_EXPLAIN_SECRET, muted, rows[6]);
}

// ── Revoke confirmation ───────────────────────────────────────────────────────

/// Declare the "revoke this token?" confirmation.
pub fn declare_revoke_confirm(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let name = ctx
        .state()
        .tokens
        .focused
        .clone()
        .unwrap_or_else(|| "token".to_string());
    let dialog = Dialog::new()
        .title(format!("Revoke '{name}'?"))
        .description("Any phone using it is disconnected and must pair again.")
        .action(
            "tok_revoke_cancel",
            Button::new("Cancel")
                .secondary()
                .on_press(|| tokens(TokensMsg::RevokeCancelled)),
        )
        .action(
            "tok_revoke_ok",
            Button::new("Revoke")
                .destructive()
                .on_press(|| tokens(TokensMsg::RevokeConfirmed)),
        )
        .on_dismiss(|| tokens(TokensMsg::RevokeCancelled));
    ctx.modal(DialogId::TokenRevokeConfirm.id(), dialog, area);
}

// ── Shared bits ───────────────────────────────────────────────────────────────

/// Wrap `UiMsg` around a token message — every control in this file emits one.
fn tokens(msg: TokensMsg) -> UiMsg {
    UiMsg::Tokens(msg)
}

/// The theme's muted text style.
fn muted_style(ctx: &DeclareCtx<'_, AppState, UiMsg>) -> Style {
    Style::default().fg(ctx.theme.muted_foreground)
}

/// Paint one explanation, wrapped into the rows reserved for it.
fn paint_explain(
    ctx: &mut DeclareCtx<'_, AppState, UiMsg>,
    text: &'static str,
    style: Style,
    area: Rect,
) {
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled(text, style)))
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Left),
        area,
    );
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::app::state::tokens::TokensMsg;
    use crate::app::state::{AppState, DialogId};
    use crate::app::{Message, UiMsg, update};
    use crate::server::tokens::TokenRecord;
    use crate::tui::runner::build_ratcn;
    use crate::tui::views::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratcn::Theme;
    use ratcn::runtime::{Event, EventResult, KeyCode, KeyEvent, Ratcn};

    /// The runner's loop in miniature: one `Ratcn`, one `AppState`, keys routed
    /// through ratcn first and the reducer only for what it ignored.
    struct Harness {
        terminal: Terminal<TestBackend>,
        ratcn: Ratcn<AppState, UiMsg>,
        theme: Theme,
        state: AppState,
    }

    impl Harness {
        fn new() -> Self {
            let mut harness = Self {
                terminal: Terminal::new(TestBackend::new(80, 24)).expect("terminal"),
                ratcn: build_ratcn(),
                theme: crate::tui::theme::muxr(),
                state: AppState::new(),
            };
            harness.draw();
            harness
        }

        fn draw(&mut self) {
            let Self {
                terminal,
                ratcn,
                theme,
                state,
            } = self;
            terminal
                .draw(|frame| render(frame, ratcn, state, theme))
                .expect("draw");
        }

        fn apply(&mut self, message: Message) {
            let _ = update(&mut self.state, message);
            self.draw();
        }

        fn key(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code);
            let message = match self.ratcn.handle_event(Event::Key(key), &self.state) {
                EventResult::Emit(msg) => Message::Ui(msg),
                EventResult::Consumed => Message::Tick,
                EventResult::Ignored => Message::Key(key),
            };
            self.apply(message);
        }

        fn type_str(&mut self, text: &str) {
            for c in text.chars() {
                self.key(KeyCode::Char(c));
            }
        }

        fn screen(&self) -> String {
            let buffer = self.terminal.backend().buffer();
            let area = buffer.area;
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    fn record(name: &str, read_only: bool) -> TokenRecord {
        TokenRecord {
            name: name.to_string(),
            created_at: "2026-01-01 00:00:00".to_string(),
            read_only,
        }
    }

    #[test]
    fn the_token_list_shows_each_token_with_its_access_badge() {
        let mut harness = Harness::new();
        harness.apply(Message::Ui(UiMsg::Open(DialogId::Tokens)));
        harness.apply(Message::TokensLoaded(vec![
            record("phone", true),
            record("laptop", false),
        ]));
        let screen = harness.screen();
        assert!(screen.contains("Tokens"), "no dialog:\n{screen}");
        assert!(screen.contains("phone"), "missing token:\n{screen}");
        assert!(screen.contains("[ro]"), "missing badge:\n{screen}");
        assert!(screen.contains("[rw]"), "missing badge:\n{screen}");
        assert!(screen.contains("Revoke"), "missing action:\n{screen}");
    }

    #[test]
    fn an_empty_list_says_so_instead_of_showing_nothing() {
        let mut harness = Harness::new();
        harness.apply(Message::Ui(UiMsg::Open(DialogId::Tokens)));
        harness.apply(Message::TokensLoaded(Vec::new()));
        assert!(
            harness.screen().contains("No tokens yet"),
            "no empty state:\n{}",
            harness.screen()
        );
    }

    #[test]
    fn typing_in_the_create_dialog_reaches_the_form() {
        let mut harness = Harness::new();
        harness.apply(Message::Ui(UiMsg::Tokens(TokensMsg::CreateRequested)));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::TokenCreate));
        // The modal takes default focus, which resolves to its first focusable
        // leaf — the name field.
        harness.type_str("pixel");
        assert_eq!(harness.state.tokens.form_name, "pixel");
        let screen = harness.screen();
        assert!(screen.contains("Read-only"), "no checkbox:\n{screen}");
        assert!(screen.contains("never"), "no expiry cycle:\n{screen}");
    }

    /// The expiry explanation wraps to three rows at this dialog's inner width,
    /// so its slot and [`CREATE_CONTENT_ROWS`] have to carry the extra row — and
    /// the whole dialog still has to fit the 80x24 floor. Asserting the tail of
    /// the sentence *and* both actions proves the third row is painted and the
    /// dialog was not pushed off the bottom of the frame.
    #[test]
    fn the_create_dialog_fits_80x24_with_the_full_expiry_explanation() {
        let mut harness = Harness::new();
        harness.apply(Message::Ui(UiMsg::Tokens(TokensMsg::CreateRequested)));
        let screen = harness.screen();
        assert!(
            screen.contains("can no longer pair a new"),
            "expiry explanation truncated:\n{screen}"
        );
        // The sentence wraps mid-clause, so the tail lands alone on the third
        // row — a line of its own is the proof that the extra row is painted.
        assert!(
            screen.lines().any(|line| line
                .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                .starts_with("revoked.")),
            "third explanation row missing:\n{screen}"
        );
        assert!(
            screen.contains("Cancel"),
            "dialog footer pushed off screen:\n{screen}"
        );
    }

    #[test]
    fn the_minted_dialog_shows_the_secret_once() {
        let mut harness = Harness::new();
        harness.apply(Message::Ui(UiMsg::Tokens(TokensMsg::CreateRequested)));
        harness.apply(Message::TokenCreated {
            token: "s3cret-plaintext".to_string(),
            name: "pixel".to_string(),
            read_only: true,
        });
        assert_eq!(harness.state.top_dialog(), Some(DialogId::TokenMinted));
        let screen = harness.screen();
        assert!(screen.contains("s3cret-plaintext"), "no secret:\n{screen}");
        assert!(screen.contains("shown once"), "no warning:\n{screen}");
        assert!(screen.contains("read-only"), "no access badge:\n{screen}");
    }

    #[test]
    fn the_revoke_confirmation_names_the_focused_token() {
        let mut harness = Harness::new();
        harness.apply(Message::Ui(UiMsg::Open(DialogId::Tokens)));
        harness.apply(Message::TokensLoaded(vec![record("phone", false)]));
        harness.apply(Message::Ui(UiMsg::Tokens(TokensMsg::RevokeRequested)));
        assert_eq!(
            harness.state.top_dialog(),
            Some(DialogId::TokenRevokeConfirm)
        );
        let screen = harness.screen();
        assert!(screen.contains("Revoke 'phone'?"), "no title:\n{screen}");
        assert!(screen.contains("pair again"), "no consequence:\n{screen}");
    }

    /// Esc on the list is the dialog's own dismissal, and it must not revoke.
    #[test]
    fn esc_closes_the_token_list() {
        let mut harness = Harness::new();
        harness.apply(Message::Ui(UiMsg::Open(DialogId::Tokens)));
        harness.apply(Message::TokensLoaded(vec![record("phone", false)]));
        harness.key(KeyCode::Esc);
        assert_eq!(harness.state.top_dialog(), None);
        assert_eq!(harness.state.tokens.tokens.len(), 1);
    }
}
