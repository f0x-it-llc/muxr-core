//! The Certificate dialogs: the overview, its explainer, and the regenerate
//! confirmation.
//!
//! The overview answers three questions in one screen — what transport the
//! daemon is actually serving, what the pairing QR will carry, and which
//! addresses the certificate claims — because every one of them changes what
//! happens when a phone scans the QR. The prose lives in `app/state/cert.rs`
//! beside the values it describes; this module only lays it out.
//!
//! Every dialog here fits an 80x24 terminal: the content strips are fixed-row
//! constants measured against a 78-column box (`Dialog` cannot measure a
//! closure, so the constants and the layout below must agree).

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratcn::runtime::DeclareCtx;
use ratcn::text_width::wrap_to_width;
use ratcn::{Button, Cycle, Dialog, ScrollArea, Theme};

use crate::app::UiMsg;
use crate::app::state::cert::{
    CERT_EXPLAIN, CertMsg, REGEN_EXPLAIN, SAN_EXPLAIN, TRUST_EXPLAIN, dns_advisory,
    resolved_trust_label, tls_mode_label,
};
use crate::app::state::{AppState, DialogId};
use crate::app::update::cert::{current_sans, planned_sans};

/// Outer width of all three dialogs, border and padding included.
const OUTER_WIDTH: u16 = 78;
/// Cells the ratcn `Dialog` spends per side on its border plus padding.
const EDGE: u16 = 2;
/// Columns the content strip gets inside [`OUTER_WIDTH`].
const INNER_WIDTH: u16 = OUTER_WIDTH - EDGE * 2;

/// The `Mode` row: the h2c label needs two wrapped rows at this width.
const MODE_ROWS: u16 = 2;
/// The `Fingerprint` label plus the 64 hex characters below it.
const FINGERPRINT_ROWS: u16 = 2;
/// SAN entries listed per column before the overflow line.
const SAN_LIST_ROWS: u16 = 4;
/// The two-column SAN box: one header row plus its entries.
const SAN_ROWS: u16 = 1 + SAN_LIST_ROWS;
/// The advertised-trust label and its cycle.
const TRUST_ROWS: u16 = 1;
/// What that choice resolves to for the QR.
const RESOLVED_ROWS: u16 = 1;
/// The DNS-host advisory, when it applies.
const ADVISORY_ROWS: u16 = 2;
/// The two muted closing lines.
const EXPLAIN_ROWS: u16 = 2;
/// Content rows without the advisory. With it the strip is 15 rows, which is
/// 15 + 2 (action row and its gap) + 4 (border and padding) = 21 of the 24 an
/// 80x24 terminal has.
const CONTENT_ROWS: u16 =
    MODE_ROWS + FINGERPRINT_ROWS + SAN_ROWS + TRUST_ROWS + RESOLVED_ROWS + EXPLAIN_ROWS;

/// Content rows of the explainer's scroll viewport.
const HELP_ROWS: u16 = 16;

/// The height budget, checked at compile time: a `Dialog` spends 4 rows on its
/// border and padding and 2 more on the action row and its gap, so 18 of an
/// 80x24 terminal's rows are left for a content strip.
const _: () = {
    assert!(CONTENT_ROWS + ADVISORY_ROWS <= 18);
    assert!(HELP_ROWS <= 18);
};

/// Width the `Mode` value wraps to, after its label column.
const MODE_LABEL: &str = "Mode  ";

// ── The certificate overview ──────────────────────────────────────────────────

/// Declare the Certificate dialog as a modal layer.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let state = ctx.state();
    let rows = CONTENT_ROWS
        + if dns_advisory(state).is_some() {
            ADVISORY_ROWS
        } else {
            0
        };
    // Regenerating while a read is in flight would race it, and the button is
    // the one action here that cannot be undone.
    let busy = state.cert.loading;

    let dialog = Dialog::new()
        .title("Certificate & pairing trust")
        .outer_width(OUTER_WIDTH)
        .content(rows, content)
        .action(
            "cert_help",
            Button::new("Help")
                .secondary()
                .on_press(|| UiMsg::Cert(CertMsg::OpenHelp)),
        )
        .action(
            "cert_regen",
            Button::new("Regenerate…")
                .destructive()
                .disabled(busy)
                .on_press(|| UiMsg::Cert(CertMsg::RegenerateRequested)),
        )
        .action(
            "cert_refresh",
            Button::new("Refresh")
                .ghost()
                .on_press(|| UiMsg::Cert(CertMsg::Refresh)),
        )
        .action("cert_close", Button::new("Close").on_press(close))
        .on_dismiss(close);
    ctx.modal(DialogId::Cert.id(), dialog, area);
}

/// The overview's content strip.
fn content(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.area();
    let theme = *ctx.theme;

    // Read the whole model first: every paint below needs `ctx` mutably.
    let state = ctx.state();
    let has_cert = state.cert.fingerprint.is_some();
    let mode = tls_mode_label(state.cert.tls_mode, has_cert);
    let fingerprint = state.cert.fingerprint.clone();
    let resolved = resolved_trust_label(state.cert.advertise_trust, state.cert.tls_mode, has_cert);
    let advisory = dns_advisory(state);
    let current = current_sans(state);
    let planned = planned_sans(state);

    let mut constraints = vec![
        Constraint::Length(MODE_ROWS),
        Constraint::Length(FINGERPRINT_ROWS),
        Constraint::Length(SAN_ROWS),
        Constraint::Length(TRUST_ROWS),
        Constraint::Length(RESOLVED_ROWS),
    ];
    if advisory.is_some() {
        constraints.push(Constraint::Length(ADVISORY_ROWS));
    }
    constraints.push(Constraint::Length(EXPLAIN_ROWS));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let muted = Style::default().fg(theme.muted_foreground);
    let body = Style::default().fg(theme.foreground);
    let accent = Style::default().fg(theme.accent);
    let warn = Style::default().fg(theme.warning);

    // Mode — what the daemon reported, hanging-indented under its label.
    let indent = " ".repeat(MODE_LABEL.len());
    let mode_lines: Vec<Line<'static>> =
        wrap(mode, area.width.saturating_sub(MODE_LABEL.len() as u16))
            .into_iter()
            .take(MODE_ROWS as usize)
            .enumerate()
            .map(|(index, chunk)| {
                if index == 0 {
                    Line::from(vec![
                        Span::styled(MODE_LABEL, muted),
                        Span::styled(chunk, body),
                    ])
                } else {
                    Line::from(Span::styled(format!("{indent}{chunk}"), body))
                }
            })
            .collect();
    ctx.paint_widget(Paragraph::new(mode_lines), rows[0]);

    // Fingerprint — in full: it is what the operator compares against the phone.
    let mut fingerprint_lines = vec![Line::from(Span::styled("Fingerprint", muted))];
    match fingerprint {
        Some(fp) => fingerprint_lines.extend(
            wrap(&fp, area.width)
                .into_iter()
                .map(|chunk| Line::from(Span::styled(chunk, accent.add_modifier(Modifier::BOLD)))),
        ),
        None => fingerprint_lines.push(Line::from(Span::styled(
            "No certificate yet — Regenerate creates one",
            warn,
        ))),
    }
    ctx.paint_widget(Paragraph::new(fingerprint_lines), rows[1]);

    // SANs — what the certificate claims now, beside what a regenerate would
    // claim. An address only the right column has is the reason to regenerate.
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[2]);
    ctx.paint_widget(
        Paragraph::new(san_column("Current", &current, &current, theme)),
        columns[0],
    );
    ctx.paint_widget(
        Paragraph::new(san_column(
            "Planned after regenerate",
            &planned,
            &current,
            theme,
        )),
        columns[1],
    );

    // Advertised trust — the one setting on this screen.
    let trust_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(18), Constraint::Min(0)])
        .split(rows[3]);
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Advertised trust:", muted))),
        trust_row[0],
    );
    ctx.component(
        "cert_trust",
        Cycle::new(["Auto", "CA", "Pin"]).selection(
            |state: &AppState| state.cert.advertise_trust.index(),
            |index| UiMsg::Cert(CertMsg::TrustChanged(index)),
        ),
        trust_row[1],
    );

    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&resolved, area.width),
            body,
        ))),
        rows[4],
    );

    let explain_row = if let Some(advisory) = advisory {
        let lines: Vec<Line<'static>> = wrap(advisory, area.width)
            .into_iter()
            .take(ADVISORY_ROWS as usize)
            .map(|chunk| Line::from(Span::styled(chunk, warn)))
            .collect();
        ctx.paint_widget(Paragraph::new(lines), rows[5]);
        rows[6]
    } else {
        rows[5]
    };

    ctx.paint_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(truncate(CERT_EXPLAIN[0], area.width), muted)),
            Line::from(Span::styled(
                "Press Help for what SANs, pinning and regeneration mean.",
                muted,
            )),
        ]),
        explain_row,
    );
}

/// One SAN column: its heading, then its entries, with anything `reference`
/// lacks called out — on the planned side that is exactly what regenerating
/// would add.
fn san_column(
    heading: &str,
    entries: &[String],
    reference: &[String],
    theme: Theme,
) -> Vec<Line<'static>> {
    let muted = Style::default().fg(theme.muted_foreground);
    let body = Style::default().fg(theme.foreground);
    let warn = Style::default().fg(theme.warning);

    let mut lines = vec![Line::from(Span::styled(
        heading.to_string(),
        muted.add_modifier(Modifier::BOLD),
    ))];
    if entries.is_empty() {
        lines.push(Line::from(Span::styled("  (none)".to_string(), muted)));
        return lines;
    }

    let shown = if entries.len() > SAN_LIST_ROWS as usize {
        SAN_LIST_ROWS as usize - 1
    } else {
        entries.len()
    };
    for entry in &entries[..shown] {
        let style = if reference.contains(entry) {
            body
        } else {
            warn
        };
        lines.push(Line::from(Span::styled(format!("  {entry}"), style)));
    }
    if shown < entries.len() {
        lines.push(Line::from(Span::styled(
            format!("  +{} more", entries.len() - shown),
            muted,
        )));
    }
    lines
}

// ── The explainer ─────────────────────────────────────────────────────────────

/// Declare the Certificate explainer as a modal layer.
pub fn declare_help(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    // The scroll area spends one column on its gutter.
    let width = INNER_WIDTH
        .min(area.width.saturating_sub(EDGE * 2))
        .saturating_sub(1);
    let lines = help_lines(width, *ctx.theme);
    let content_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);

    let dialog = Dialog::new()
        .title("About certificates, SANs and pairing trust")
        .outer_width(OUTER_WIDTH)
        .content(HELP_ROWS, move |ctx| {
            let area = ctx.area();
            ctx.component(
                "cert_help_scroll",
                ScrollArea::new(content_height)
                    .scroll(
                        |state: &AppState| {
                            u16::try_from(state.cert.help_scroll).unwrap_or(u16::MAX)
                        },
                        |offset| UiMsg::Cert(CertMsg::HelpScrolled(usize::from(offset))),
                    )
                    .content(move |ctx| {
                        let content_area = ctx.area();
                        ctx.paint_widget(Paragraph::new(lines), content_area);
                    }),
                area,
            );
        })
        .action(
            "cert_help_close",
            Button::new("Close").on_press(|| UiMsg::Cert(CertMsg::CloseHelp)),
        )
        .on_dismiss(|| UiMsg::Cert(CertMsg::CloseHelp));
    ctx.modal(DialogId::CertHelp.id(), dialog, area);
}

/// The four explanation blocks, wrapped to `width`, headings included.
fn help_lines(width: u16, theme: Theme) -> Vec<Line<'static>> {
    let heading = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(theme.foreground);

    let blocks: [(&str, &[&str]); 4] = [
        ("Certificate", CERT_EXPLAIN),
        ("Subject Alternative Names", SAN_EXPLAIN),
        ("Regenerating", REGEN_EXPLAIN),
        ("Advertised trust", TRUST_EXPLAIN),
    ];

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, (title, paragraphs)) in blocks.into_iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(title.to_string(), heading)));
        for paragraph in paragraphs {
            for chunk in wrap(paragraph, width) {
                lines.push(Line::from(Span::styled(chunk, body)));
            }
        }
    }
    lines
}

// ── The regenerate confirmation ───────────────────────────────────────────────

/// Declare the "Regenerate the certificate?" confirmation as a modal layer.
pub fn declare_regen_confirm(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let planned = planned_sans(ctx.state()).join(", ");
    let description = format!("{}\n\nPlanned SANs: {planned}", REGEN_EXPLAIN.join(" "));

    let dialog = Dialog::new()
        .title("Regenerate the certificate?")
        .outer_width(OUTER_WIDTH)
        .description(description)
        .action(
            "cert_regen_cancel",
            Button::new("Cancel")
                .secondary()
                .on_press(|| UiMsg::Cert(CertMsg::RegenerateCancelled)),
        )
        .action(
            "cert_regen_confirm",
            Button::new("Regenerate")
                .destructive()
                .on_press(|| UiMsg::Cert(CertMsg::RegenerateConfirmed)),
        )
        .on_dismiss(|| UiMsg::Cert(CertMsg::RegenerateCancelled));
    ctx.modal(DialogId::CertRegenConfirm.id(), dialog, area);
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// What the overview's Close action and Esc both emit.
fn close() -> UiMsg {
    UiMsg::Cert(CertMsg::Close)
}

/// Word-wrap `text` to `width` cells as owned lines.
fn wrap(text: &str, width: u16) -> Vec<String> {
    wrap_to_width(text, usize::from(width.max(1)))
        .into_iter()
        .map(ToString::to_string)
        .collect()
}

/// `text` cut to `width` cells, so one long line can never bleed past the box.
fn truncate(text: &str, width: u16) -> String {
    ratcn::text_width::truncate_to_width(text, usize::from(width)).to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::cert::{AdvertiseTrust, TlsMode};
    use crate::app::{Message, update};
    use crate::tui::runner::build_ratcn;
    use crate::tui::views::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratcn::runtime::{Event, EventResult, KeyCode, KeyEvent, Ratcn};

    /// The real loop in miniature — the same shape `tui/views/mod.rs` tests use:
    /// one `Ratcn`, one `AppState`, and the runner's own key routing.
    struct Harness {
        terminal: Terminal<TestBackend>,
        ratcn: Ratcn<AppState, UiMsg>,
        theme: Theme,
        state: AppState,
        actions: Vec<crate::app::action::UpdateAction>,
    }

    impl Harness {
        fn new() -> Self {
            let mut harness = Self {
                terminal: Terminal::new(TestBackend::new(80, 24)).expect("terminal"),
                ratcn: build_ratcn(),
                theme: crate::tui::theme::muxr(),
                state: AppState::new(),
                actions: Vec::new(),
            };
            // A configured host and a cert on disk: the populated screen.
            harness.state.config.host = "192.168.1.10".to_string();
            harness.state.config.reachable_ips = vec![std::net::Ipv4Addr::new(192, 168, 1, 10)];
            harness.state.cert.fingerprint = Some("ab".repeat(32));
            harness.state.cert.sans = vec!["10.0.0.1".to_string()];
            harness.state.cert.tls_mode = Some(TlsMode::SelfSigned);
            harness.draw();
            harness
        }

        fn draw(&mut self) {
            let Self {
                terminal,
                ratcn,
                theme,
                state,
                ..
            } = self;
            terminal
                .draw(|frame| render(frame, ratcn, state, theme))
                .expect("draw");
        }

        fn key(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code);
            let message = match self.ratcn.handle_event(Event::Key(key), &self.state) {
                EventResult::Emit(msg) => Message::Ui(msg),
                EventResult::Consumed => Message::Tick,
                EventResult::Ignored => Message::Key(key),
            };
            self.actions.extend(update(&mut self.state, message));
            self.draw();
        }

        /// Tab until `id` holds focus, then press Enter on it.
        fn press(&mut self, id: &str) {
            for _ in 0..12 {
                if self
                    .state
                    .ui
                    .focus
                    .path()
                    .last()
                    .map(|child| child.as_str())
                    == Some(id)
                {
                    self.key(KeyCode::Enter);
                    return;
                }
                self.key(KeyCode::Tab);
            }
            panic!("never reached {id}; focus is {:?}", self.state.ui.focus);
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

        fn ensure_cert_requested(&self) -> bool {
            self.actions
                .iter()
                .any(|a| matches!(a, crate::app::action::UpdateAction::EnsureCert(_)))
        }
    }

    /// Open the Certificate dialog the way the dashboard does.
    fn open_cert() -> Harness {
        let mut harness = Harness::new();
        harness.key(KeyCode::Char('e'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Cert));
        // Opening dispatches three loads; pretend they have landed, so the
        // screen under test is the settled one.
        harness.state.cert.loading = false;
        harness.draw();
        harness
    }

    #[test]
    fn the_certificate_dialog_paints_mode_fingerprint_and_both_san_columns() {
        let harness = open_cert();
        let screen = harness.screen();
        for expected in [
            "Certificate & pairing trust",
            "Self-signed",
            "Fingerprint",
            "Current",
            "Planned after regenerate",
            "Advertised trust:",
            "Auto",
            "tm=pin",
            "Regenerate",
            "Help",
        ] {
            assert!(
                screen.contains(expected),
                "{expected} missing from:\n{screen}"
            );
        }
        // The full 64-hex fingerprint, not an excerpt.
        assert!(
            screen.contains(&"ab".repeat(32)),
            "fingerprint not shown in full:\n{screen}"
        );
        // The planned column carries the address the sidecar does not have yet.
        assert!(
            screen.contains("192.168.1.10"),
            "planned SAN missing from:\n{screen}"
        );
    }

    /// Assert the whole dialog box is inside the 24 rows: its titled top border,
    /// its action row and its bottom border all painted, in that order.
    fn assert_unclipped(screen: &str, title: &str) {
        let rows: Vec<&str> = screen.lines().collect();
        assert_eq!(rows.len(), 24, "{screen}");
        let top = rows
            .iter()
            .position(|row| row.contains(title))
            .unwrap_or_else(|| panic!("no top border:\n{screen}"));
        let bottom = rows
            .iter()
            .rposition(|row| row.contains('\u{2518}'))
            .unwrap_or_else(|| panic!("no bottom border:\n{screen}"));
        assert!(
            top < bottom,
            "the box never closes inside the frame:\n{screen}"
        );
        let actions = rows
            .iter()
            .rposition(|row| row.contains("Close"))
            .unwrap_or_else(|| panic!("no action row:\n{screen}"));
        assert!(
            top < actions && actions < bottom,
            "the action row is clipped:\n{screen}"
        );
    }

    #[test]
    fn the_dialog_fits_an_80x24_terminal() {
        let harness = open_cert();
        assert_unclipped(&harness.screen(), "Certificate & pairing trust");
    }

    #[test]
    fn the_advisory_row_still_fits_an_80x24_terminal() {
        let mut harness = Harness::new();
        // Auto + a self-signed cert + a DNS host: the tallest the dialog gets.
        harness.state.config.host = "muxr.example.com".to_string();
        harness.key(KeyCode::Char('e'));
        harness.state.cert.loading = false;
        harness.draw();
        let screen = harness.screen();
        assert!(
            screen.contains("PINNED"),
            "advisory missing from:\n{screen}"
        );
        assert_unclipped(&screen, "Certificate & pairing trust");
    }

    #[test]
    fn the_explainer_fits_an_80x24_terminal() {
        let mut harness = open_cert();
        harness.press("cert_help");
        assert_unclipped(
            &harness.screen(),
            "About certificates, SANs and pairing trust",
        );
    }

    #[test]
    fn regenerate_opens_the_confirmation_and_esc_cancels_it() {
        let mut harness = open_cert();
        harness.press("cert_regen");
        assert_eq!(
            harness.state.top_dialog(),
            Some(DialogId::CertRegenConfirm),
            "Regenerate must confirm first"
        );
        assert!(
            harness.screen().contains("Regenerate the certificate?"),
            "confirmation did not paint:\n{}",
            harness.screen()
        );
        assert!(
            !harness.ensure_cert_requested(),
            "asking must not regenerate"
        );

        harness.key(KeyCode::Esc);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Cert));
        assert!(
            !harness.ensure_cert_requested(),
            "cancelling must not regenerate"
        );
    }

    #[test]
    fn confirming_the_regeneration_requests_it() {
        let mut harness = open_cert();
        harness.press("cert_regen");
        harness.press("cert_regen_confirm");
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Cert));
        assert!(harness.ensure_cert_requested(), "EnsureCert expected");
    }

    #[test]
    fn help_paints_the_explanation_and_esc_returns_to_the_dialog() {
        let mut harness = open_cert();
        harness.press("cert_help");
        assert_eq!(harness.state.top_dialog(), Some(DialogId::CertHelp));
        let screen = harness.screen();
        assert!(
            screen.contains("About certificates, SANs and pairing trust"),
            "explainer did not paint:\n{screen}"
        );
        assert!(screen.contains("Certificate"), "heading missing:\n{screen}");

        harness.key(KeyCode::Esc);
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Cert));
    }

    #[test]
    fn the_trust_cycle_shows_the_persisted_choice() {
        let mut harness = Harness::new();
        harness.state.cert.advertise_trust = AdvertiseTrust::Pin;
        harness.key(KeyCode::Char('e'));
        let screen = harness.screen();
        assert!(
            screen.contains("Pin"),
            "cycle value missing from:\n{screen}"
        );
        assert!(
            screen.contains("tm=pin"),
            "resolved trust missing from:\n{screen}"
        );
    }

    #[test]
    fn a_missing_certificate_says_so_instead_of_showing_a_blank() {
        let mut harness = Harness::new();
        harness.state.cert.fingerprint = None;
        harness.key(KeyCode::Char('e'));
        assert!(
            harness.screen().contains("No certificate yet"),
            "missing-cert notice absent from:\n{}",
            harness.screen()
        );
    }

    #[test]
    fn san_column_flags_only_what_the_reference_lacks() {
        let theme = crate::tui::theme::muxr();
        let current = vec!["127.0.0.1".to_string()];
        let planned = vec!["127.0.0.1".to_string(), "10.0.0.1".to_string()];
        let lines = san_column("Planned after regenerate", &planned, &current, theme);
        assert_eq!(lines.len(), 3, "heading plus both entries");
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.foreground));
        assert_eq!(
            lines[2].spans[0].style.fg,
            Some(theme.warning),
            "an address the certificate lacks must stand out"
        );
    }

    #[test]
    fn san_column_summarises_an_overlong_list() {
        let theme = crate::tui::theme::muxr();
        let entries: Vec<String> = (0..9).map(|n| format!("10.0.0.{n}")).collect();
        let lines = san_column("Current", &entries, &entries, theme);
        assert_eq!(lines.len(), 1 + SAN_LIST_ROWS as usize);
        assert!(
            lines
                .last()
                .expect("overflow line")
                .spans
                .iter()
                .any(|span| span.content.contains("+6 more")),
            "overflow line missing: {lines:?}"
        );
    }
}
