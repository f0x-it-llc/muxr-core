//! The first-run setup wizard: one modal layer, six steps, one Back/Next row.
//!
//! ```text
//! ┌ Set up muxrd ────────────────────────────────────────────────────────┐
//! │ ● ─── ● ─── ○ ─── ○ ─── ○ ─── ○   Step 2 of 6                        │
//! │ ┌ Network ─────────────────────────────────────────────────────────┐ │
//! │ │ Bind host                                   Port                 │ │
//! │ │ ┌────────────────────────────────────────┐  ┌───────┐            │ │
//! │ │ …                                                                │ │
//! │ └──────────────────────────────────────────────────────────────────┘ │
//! │ Port must be a number in 1..=65535.                                  │
//! │ [Back] [Next]                                   Esc cancel           │
//! └──────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Declared as a [`modal_scope`](DeclareCtx::modal_scope) rather than a
//! [`Dialog`](ratcn::Dialog): the step panel changes shape per step and the
//! button row is the wizard's own, so there is no chrome worth inheriting. Two
//! consequences follow from that choice, both handled here:
//!
//! - a `modal_scope` has no `on_dismiss`, and an open modal absorbs whatever
//!   nothing inside it handled, so Esc is answered by [`EscCloses`] — a wrapper
//!   every focusable control in the layer wears;
//! - the layer has to paint its own opaque base, or the dashboard shows through.
//!
//! Every control binds the state its own feature owns (`ConfigState` on the
//! Network step, `CertState` on Certificate, `TokensState` on Token) and every
//! explanation is that feature's own constant, so the wizard restates nothing.
//!
//! The Pair step paints `state.tokens.qr_overlay`'s phase and caption. The
//! matrix itself needs a [`Frame`], which a `PaintCtx` cannot supply, so
//! [`super::render`] asks this file for it after the ratcn pass —
//! [`paint_pair_matrix`], into the rect [`pair_qr_rect`] reserves inside the
//! panel, never frame-wide. The geometry lives beside the code that draws the
//! panel so the two halves cannot drift, the same reason `qr_overlay` keeps
//! `split`/`split_showing` next to its own chrome.

use std::rc::Rc;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratcn::runtime::{
    Component, DeclareCtx, Event, EventCtx, EventResult, KeyCode, PaintCtx, ScopeOptions, TabWrap,
};
use ratcn::text_width::wrap_to_width;
use ratcn::{Button, Checkbox, Cycle, ListItem, ProgressWidget, Select, Theme};

use crate::app::UiMsg;
use crate::app::state::cert::{
    CertMsg, REGEN_EXPLAIN, SAN_EXPLAIN, resolved_trust_label, tls_mode_label,
};
use crate::app::state::config::{BIND_EXPLAIN, ConfigMsg};
use crate::app::state::tokens::{
    QrOverlayPhase, TOKEN_EXPLAIN_EXPIRY, TOKEN_EXPLAIN_NAME, TOKEN_EXPLAIN_READ_ONLY,
    TOKEN_EXPLAIN_SECRET, TokenExpiryChoice, TokensMsg,
};
use crate::app::state::wizard::{WELCOME_EXPLAIN, WizardMsg, WizardStep};
use crate::app::state::{AppState, DialogId};
use crate::app::update::cert::planned_sans;
use crate::tui::components::TextInput;
use crate::tui::widgets::qr::{MIN_HEIGHT as QR_MIN_HEIGHT, MIN_WIDTH as QR_MIN_WIDTH, QrWidget};

/// The layer's fixed size. Below this the frame shows the resize hint instead
/// of a step — a wizard squeezed into fewer rows would hide the control the
/// operator is about to press.
const WIDTH: u16 = 72;
const HEIGHT: u16 = 22;

/// Rows the layer spends on chrome whatever the step: its border ring, the
/// stepper and its gap, the error line, the button row, and the step panel's
/// own border ring.
const CHROME_ROWS: u16 = 2 + 1 + 1 + 1 + 1 + 2;

/// Rows the Pair step reserves under the matrix for its caption: server line,
/// scan prompt, blank, token line, and the two the secret warning wraps to.
const PAIR_CAPTION_ROWS: u16 = 6;

/// Rows a production pairing block occupies: a version-8 matrix is 49 modules,
/// which `Dense1x2` packs into 25 rows, plus one quiet-zone row per side (see
/// [`crate::tui::widgets::qr`]).
const QR_BLOCK_ROWS: u16 = 27;

/// The height the Pair step would like: the chrome, the caption, and a region
/// the whole pairing block fits in. The step grows the layer towards this and
/// no further; a frame that cannot spare the rows gets a shorter layer and, if
/// the code still does not fit, the step's own "needs more room" copy rather
/// than a matrix nothing can scan.
const PAIR_HEIGHT: u16 = CHROME_ROWS + PAIR_CAPTION_ROWS + QR_BLOCK_ROWS;

/// Columns the outer border and its one cell of padding spend per side.
const EDGE: u16 = 2;

// ── The layer ─────────────────────────────────────────────────────────────────

/// Declare the setup wizard as a modal layer.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let frame = ctx.frame_area();
    let area = layer_area(frame, ctx.state().wizard.step);
    let too_small = frame.width < WIDTH || frame.height < HEIGHT;

    ctx.modal_scope(
        DialogId::Wizard.id(),
        area,
        ScopeOptions::default().tab_wrap(TabWrap::Wrap),
        move |ctx| {
            let area = ctx.area();
            let theme = *ctx.theme;
            let step = ctx.state().wizard.step;

            // The layer is opaque: `Clear` resets the symbols the dashboard
            // left behind, and the block paints the surface under everything.
            ctx.paint(move |ctx| {
                ctx.widget(Clear, area);
                ctx.widget(frame_block(&theme), area);
            });

            let rows = layer_rows(area);

            ctx.paint_widget(Paragraph::new(stepper_line(step, &theme)), rows[0]);
            panel(ctx, step, too_small, rows[2]);

            if let Some(error) = ctx.state().wizard.error.clone() {
                ctx.paint_widget(
                    Paragraph::new(Line::from(Span::styled(
                        truncate(&error, rows[3].width),
                        Style::default().fg(theme.destructive),
                    ))),
                    rows[3],
                );
            }

            buttons(ctx, step, rows[4]);
        },
    );
}

/// The step panel: a bordered box, ringed while the focus is inside it, with
/// the step's own scope in it.
fn panel(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, step: WizardStep, too_small: bool, area: Rect) {
    let theme = *ctx.theme;
    let focused = focus_is_inside(ctx.state(), step);
    let border = if focused { theme.ring } else { theme.border };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(" {} ", step.title()),
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD),
        ));
    let body = panel_inner(area);
    ctx.paint_widget(block, area);

    if too_small {
        ctx.paint_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Resize to continue",
                    Style::default().fg(theme.warning),
                )),
                Line::from(Span::styled(
                    format!("The setup wizard needs {WIDTH}x{HEIGHT} cells."),
                    Style::default().fg(theme.muted_foreground),
                )),
            ]),
            body,
        );
        return;
    }

    ctx.scope(step.id(), body, ScopeOptions::default(), move |ctx| {
        let body = ctx.area();
        match step {
            WizardStep::Welcome => welcome_step(ctx, body),
            WizardStep::Network => network_step(ctx, body),
            WizardStep::Certificate => certificate_step(ctx, body),
            WizardStep::Token => token_step(ctx, body),
            WizardStep::Start => start_step(ctx, body),
            WizardStep::Pair => pair_step(ctx, body),
        }
    });
}

/// The Back / Next row. The ids are stable: the reducer parks focus on
/// `wiz_next` (or `wiz_done` on the last step) after every step change, which
/// is what makes Enter alone walk the wizard.
fn buttons(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, step: WizardStep, area: Rect) {
    let theme = *ctx.theme;
    let busy = ctx.state().wizard.busy;

    let back = Button::new("Back")
        .secondary()
        .disabled(busy || step.is_first())
        .on_press(|| wizard(WizardMsg::Back));
    // The last step has no Next at all — Done is the only way forward from it.
    let forward = if step.is_last() {
        Button::new("Done").on_press(|| wizard(WizardMsg::Finish))
    } else {
        Button::new(next_label(ctx.state(), step))
            .disabled(busy)
            .on_press(|| wizard(WizardMsg::Next))
    };

    let cells = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(back.width() + 1),
            Constraint::Length(forward.width() + 1),
            Constraint::Min(0),
        ])
        .split(area);

    ctx.component("wiz_back", EscCloses::new(back), cells[0]);
    ctx.component(
        if step.is_last() {
            "wiz_done"
        } else {
            "wiz_next"
        },
        EscCloses::new(forward),
        cells[1],
    );
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled(
            "Esc cancels · Tab moves · Enter presses",
            Style::default().fg(theme.muted_foreground),
        )))
        .alignment(ratatui::layout::Alignment::Right),
        cells[2],
    );
}

/// What Next promises on this step, so the button never overstates it.
fn next_label(state: &AppState, step: WizardStep) -> &'static str {
    if state.wizard.busy {
        return "Working…";
    }
    match step {
        WizardStep::Certificate => {
            if state.cert.fingerprint.is_some() && !state.wizard.cert_regen_ack {
                "Keep"
            } else {
                "Regenerate"
            }
        }
        WizardStep::Token => "Create token",
        WizardStep::Start => {
            if state.server.status.is_some() {
                "Next"
            } else {
                "Start muxrd"
            }
        }
        _ => "Next",
    }
}

// ── Steps ─────────────────────────────────────────────────────────────────────

/// Welcome: what the wizard will do, and what it found.
fn welcome_step(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, area: Rect) {
    let theme = *ctx.theme;
    let state = ctx.state();
    let muted = Style::default().fg(theme.muted_foreground);
    let body = Style::default().fg(theme.foreground);

    let mut lines: Vec<Line<'static>> = WELCOME_EXPLAIN
        .iter()
        .map(|line| Line::from(Span::styled((*line).to_string(), muted)))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Detected — certificate: ", muted),
        Span::styled(
            if state.cert.fingerprint.is_some() {
                "present"
            } else {
                "absent"
            },
            body,
        ),
        Span::styled(", tokens: ", muted),
        Span::styled(state.tokens.tokens.len().to_string(), body),
        Span::styled(", daemon: ", muted),
        Span::styled(
            if state.server.is_running() {
                "running"
            } else {
                "stopped"
            },
            body,
        ),
    ]));
    ctx.paint_widget(Paragraph::new(lines), area);
}

/// Network: the Config dialog's three controls, bound to the same state.
fn network_step(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // labels
            Constraint::Length(3), // host + port fields
            Constraint::Length(1), // gap
            Constraint::Length(1), // reachable-IP picker
            Constraint::Length(1), // gap
            Constraint::Min(0),    // explanation
        ])
        .split(area);
    let columns = |row: Rect| {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(10), Constraint::Length(9)])
            .split(row)
    };
    let labels = columns(rows[0]);
    let fields = columns(rows[1]);

    let theme = *ctx.theme;
    let muted = Style::default().fg(theme.muted_foreground);
    let saving = ctx.state().config.pending_save;

    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Bind host", muted))),
        labels[0],
    );
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Port", muted))),
        labels[1],
    );
    ctx.component(
        "wiz_host",
        EscCloses::new(
            TextInput::new()
                .value(
                    |state: &AppState| state.config.host.clone(),
                    |value| UiMsg::Config(ConfigMsg::HostChanged(value)),
                )
                .placeholder("0.0.0.0")
                .disabled(saving),
        ),
        fields[0],
    );
    ctx.component(
        "wiz_port",
        EscCloses::new(
            TextInput::new()
                .value(
                    |state: &AppState| state.config.port.clone(),
                    |value| UiMsg::Config(ConfigMsg::PortChanged(value)),
                )
                .placeholder("50051")
                .digits_only(true)
                .max_len(5)
                .disabled(saving),
        ),
        fields[1],
    );

    let items: Vec<ListItem<std::net::Ipv4Addr>> = ctx
        .state()
        .config
        .reachable_ips
        .iter()
        .map(|ip| ListItem::new(*ip, ip.to_string()))
        .collect();
    ctx.component(
        "wiz_ip",
        EscCloses::new(
            Select::new(items)
                .placeholder("Pick a reachable IP…")
                .open(
                    |state: &AppState| state.config.ip_open,
                    |open| UiMsg::Config(ConfigMsg::IpOpenChanged(open)),
                )
                .item_focus(
                    |state: &AppState| state.config.ip_cursor,
                    |ip| UiMsg::Config(ConfigMsg::IpFocused(ip)),
                )
                .selection(
                    |state: &AppState| state.config.host.parse::<std::net::Ipv4Addr>().ok(),
                    |ip| UiMsg::Config(ConfigMsg::IpPicked(ip)),
                ),
        ),
        rows[3],
    );

    paint_block(ctx, BIND_EXPLAIN, rows[5]);
}

/// Certificate: what is served now, what a regenerate would claim, and the
/// acknowledgement a regenerate costs.
fn certificate_step(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, area: Rect) {
    let theme = *ctx.theme;
    let state = ctx.state();
    let has_cert = state.cert.fingerprint.is_some();
    let mode = tls_mode_label(state.cert.tls_mode, has_cert);
    let resolved = resolved_trust_label(state.cert.advertise_trust, state.cert.tls_mode, has_cert);
    let planned = planned_sans(state).join(", ");
    let detected = if state.wizard.cert_existed {
        "A certificate was already on disk when the wizard started."
    } else {
        "No certificate was on disk when the wizard started."
    };

    // With a certificate on disk the panel also has to hold the whole cost of
    // regenerating one, which leaves room for the first SAN sentence only; with
    // none, both of the sentences the Certificate dialog opens with fit.
    let mut constraints = vec![
        Constraint::Length(1), // mode
        Constraint::Length(1), // planned SANs
        Constraint::Length(1), // advertised trust
        Constraint::Length(1), // what it resolves to
    ];
    if has_cert {
        constraints.extend([
            Constraint::Length(2), // what a SAN is
            Constraint::Length(7), // what was found, and what regenerating costs
            Constraint::Length(1), // the acknowledgement
        ]);
    } else {
        constraints.push(Constraint::Length(1)); // gap
        constraints.push(Constraint::Min(0)); // what a SAN is
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let muted = Style::default().fg(theme.muted_foreground);
    let body = Style::default().fg(theme.foreground);

    ctx.paint_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Serving: ", muted),
            Span::styled(truncate(mode, area.width.saturating_sub(9)), body),
        ])),
        rows[0],
    );
    ctx.paint_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Planned SANs: ", muted),
            Span::styled(truncate(&planned, area.width.saturating_sub(14)), body),
        ])),
        rows[1],
    );

    let trust_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(18), Constraint::Min(0)])
        .split(rows[2]);
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Advertised trust:", muted))),
        trust_row[0],
    );
    ctx.component(
        "wiz_trust",
        EscCloses::new(Cycle::new(["Auto", "CA", "Pin"]).selection(
            |state: &AppState| state.cert.advertise_trust.index(),
            |index| UiMsg::Cert(CertMsg::TrustChanged(index)),
        )),
        trust_row[1],
    );
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&resolved, area.width),
            body,
        ))),
        rows[3],
    );

    if has_cert {
        // What a SAN list is, then what was found and what replacing it costs.
        paint_block(ctx, &SAN_EXPLAIN[..1], rows[4]);
        let mut regen: Vec<&str> = vec![detected];
        regen.extend(REGEN_EXPLAIN.iter().copied());
        paint_block(ctx, &regen, rows[5]);
        ctx.component(
            "wiz_regen_ack",
            EscCloses::new(
                Checkbox::new("Regenerate anyway (every phone must re-pair)").checked(
                    |state: &AppState| state.wizard.cert_regen_ack,
                    |checked| wizard(WizardMsg::RegenAckChanged(checked)),
                ),
            ),
            rows[6],
        );
    } else {
        // What the list is, and who puts entries in it.
        paint_block(ctx, &SAN_EXPLAIN[..2], rows[5]);
    }
}

/// Token: the create form's three controls, bound to the same state.
fn token_step(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // "Name"
            Constraint::Length(3), // name field
            Constraint::Length(1), // what the name is for
            Constraint::Length(1), // read-only
            Constraint::Length(3), // what read-only means
            Constraint::Length(1), // expiry
            Constraint::Min(0),    // what expiry does
        ])
        .split(area);

    let theme = *ctx.theme;
    let muted = Style::default().fg(theme.muted_foreground);

    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Name", muted))),
        rows[0],
    );
    ctx.component(
        "wiz_tok_name",
        EscCloses::new(
            TextInput::new()
                .value(
                    |state: &AppState| state.tokens.form_name.clone(),
                    |value| UiMsg::Tokens(TokensMsg::CreateNameChanged(value)),
                )
                .placeholder("optional name")
                .max_len(64),
        ),
        rows[1],
    );
    paint_block(ctx, &[TOKEN_EXPLAIN_NAME], rows[2]);

    ctx.component(
        "wiz_tok_ro",
        EscCloses::new(Checkbox::new("Read-only").checked(
            |state: &AppState| state.tokens.form_read_only,
            |checked| UiMsg::Tokens(TokensMsg::CreateReadOnlyChanged(checked)),
        )),
        rows[3],
    );
    paint_block(ctx, &[TOKEN_EXPLAIN_READ_ONLY], rows[4]);

    let expiry_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(9), Constraint::Min(0)])
        .split(rows[5]);
    ctx.paint_widget(
        Paragraph::new(Line::from(Span::styled("Expires:", muted))),
        expiry_row[0],
    );
    ctx.component(
        "wiz_tok_expiry",
        EscCloses::new(
            Cycle::new(TokenExpiryChoice::ALL.map(TokenExpiryChoice::label)).selection(
                |state: &AppState| state.tokens.form_expiry.index(),
                |index| UiMsg::Tokens(TokensMsg::CreateExpiryChanged(index)),
            ),
        ),
        expiry_row[1],
    );
    paint_block(ctx, &[TOKEN_EXPLAIN_EXPIRY], rows[6]);
}

/// Start: whether the daemon is up, and what Next will do about it.
fn start_step(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // status
            Constraint::Length(1), // gap
            Constraint::Length(2), // what Next does, or the progress bar
            Constraint::Min(0),    // slack
        ])
        .split(area);

    let theme = *ctx.theme;
    let state = ctx.state();
    let muted = Style::default().fg(theme.muted_foreground);
    let running = state.server.status.as_ref();
    let busy = state.wizard.busy;

    let status = match running {
        Some(info) => Line::from(vec![
            Span::styled("Daemon: ", muted),
            Span::styled("● Running", Style::default().fg(theme.primary)),
            Span::styled(format!("  {}  pid {}", info.bind_addr, info.pid), muted),
        ]),
        None => Line::from(vec![
            Span::styled("Daemon: ", muted),
            Span::styled("○ Stopped", Style::default().fg(theme.destructive)),
        ]),
    };
    ctx.paint_widget(Paragraph::new(status), rows[0]);

    if busy {
        ctx.paint_widget(
            ProgressWidget::new(0.5).label("Starting…").themed(&theme),
            rows[2],
        );
        return;
    }
    let next = if running.is_some() {
        "Already running — Next goes on to pairing."
    } else {
        "Next starts muxrd with the bind address from the Network step."
    };
    paint_block(ctx, &[next], rows[2]);
}

/// Pair: the QR the wizard just asked for, and what the secret in it means.
fn pair_step(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, area: Rect) {
    let theme = *ctx.theme;
    let state = ctx.state();
    let muted = Style::default().fg(theme.muted_foreground);
    let body = Style::default().fg(theme.foreground);

    let Some(overlay) = state.tokens.qr_overlay.as_ref() else {
        let message = if state.tokens.last_minted_secret.is_some() {
            "Preparing the pairing code…"
        } else {
            "No token was minted — go Back to the Token step."
        };
        ctx.paint_widget(
            Paragraph::new(Line::from(Span::styled(
                message,
                Style::default().fg(theme.warning),
            ))),
            area,
        );
        return;
    };

    // `super::render` paints the matrix into [`pair_qr_rect`] — the top of this
    // panel — for a `Showing` overlay, so the caption moves to the rows below
    // it. The other phases paint no matrix, so they keep the whole panel.
    let showing = matches!(overlay.phase, QrOverlayPhase::Showing { .. });
    let mut lines: Vec<Line<'static>> = match &overlay.phase {
        QrOverlayPhase::Generating => vec![Line::from(Span::styled(
            "Generating the pairing code…",
            Style::default().fg(theme.warning),
        ))],
        QrOverlayPhase::Showing {
            host,
            port,
            fingerprint_short,
            ..
        } => vec![
            Line::from(vec![
                Span::styled("Server: ", muted),
                Span::styled(format!("{host}:{port}"), body),
                Span::styled("   Cert: ", muted),
                Span::styled(fingerprint_short.clone(), body),
            ]),
            Line::from(Span::styled(
                "Scan it with the Muxr app to pair this phone.",
                muted,
            )),
        ],
        QrOverlayPhase::Connected => vec![Line::from(Span::styled(
            "✓ A client connected (the attached-client count rose).",
            Style::default().fg(theme.primary),
        ))],
        QrOverlayPhase::Failed { err } => vec![
            Line::from(Span::styled(
                "The pairing code could not be built:",
                Style::default().fg(theme.destructive),
            )),
            Line::from(Span::styled(err.clone(), body)),
        ],
    };
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Token: ", muted),
        Span::styled(overlay.token_name.clone(), body),
        Span::styled(
            if overlay.read_only {
                "  (read-only)"
            } else {
                "  (read-write)"
            },
            muted,
        ),
    ]));
    for chunk in wrap(TOKEN_EXPLAIN_SECRET, area.width) {
        lines.push(Line::from(Span::styled(chunk, muted)));
    }
    if !showing {
        ctx.paint_widget(Paragraph::new(lines), area);
        return;
    }

    let (matrix_area, caption) = split_pair(area);
    if pair_qr_rect(state, ctx.frame_area()).is_none() {
        // `super::render` paints no matrix here, so say why in the wizard's own
        // words. Letting `QrWidget` fall back would print the pairing URI —
        // secret and all — across the whole frame, over this panel.
        let mut small = vec![Line::from(Span::styled(
            format!(
                "The pairing code does not fit this {}x{} panel.",
                matrix_area.width, matrix_area.height
            ),
            Style::default().fg(theme.warning),
        ))];
        for chunk in wrap(
            &format!(
                "Resize the terminal to at least {WIDTH}x{PAIR_HEIGHT} and come back to this \
                 step, or press Done and open the code from the dashboard's Tokens list."
            ),
            area.width,
        ) {
            small.push(Line::from(Span::styled(chunk, muted)));
        }
        ctx.paint_widget(Paragraph::new(small), matrix_area);
    }
    ctx.paint_widget(Paragraph::new(lines), caption);
}

// ── Shared bits ───────────────────────────────────────────────────────────────

/// Wrap `UiMsg` around a wizard message.
fn wizard(msg: WizardMsg) -> UiMsg {
    UiMsg::Wizard(msg)
}

/// What Esc emits, from anywhere inside the layer.
fn close() -> UiMsg {
    UiMsg::Wizard(WizardMsg::Close)
}

// ── Geometry ──────────────────────────────────────────────────────────────────

/// The layer's rect inside `frame`: [`WIDTH`]x[`HEIGHT`], except on the Pair
/// step, which grows towards [`PAIR_HEIGHT`] as far as the frame allows so the
/// pairing matrix has somewhere of its own to land.
fn layer_area(frame: Rect, step: WizardStep) -> Rect {
    let height = if step == WizardStep::Pair {
        frame.height.clamp(HEIGHT, PAIR_HEIGHT)
    } else {
        HEIGHT
    };
    centered(frame, WIDTH, height)
}

/// The five rows [`declare`] lays the layer's inside out into: stepper, gap,
/// step panel, error line, buttons.
fn layer_rows(area: Rect) -> Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // stepper
            Constraint::Length(1), // gap
            Constraint::Min(3),    // step panel
            Constraint::Length(1), // error
            Constraint::Length(1), // buttons
        ])
        .split(inset(area))
}

/// The step panel's inside: its rect minus the border ring [`panel`] draws.
fn panel_inner(panel: Rect) -> Rect {
    Rect::new(
        panel.x + 1,
        panel.y + 1,
        panel.width.saturating_sub(2),
        panel.height.saturating_sub(2),
    )
}

/// Split the Pair panel into the matrix region and the caption strip under it.
fn split_pair(body: Rect) -> (Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(PAIR_CAPTION_ROWS)])
        .split(body);
    (rows[0], rows[1])
}

/// Whether `area` can hold the matrix `uri` encodes to.
///
/// Both of [`QrWidget::render`]'s own conditions, asked before it is called:
/// its `MIN_*` floor, and the block this particular payload measures to. Either
/// one failing makes the widget print the raw pairing URI — secret and all —
/// across whatever rect it was handed, which is never what this layer wants.
/// Encoding the payload a second time costs microseconds once a frame, and
/// only on the Pair step.
fn fits_a_matrix(area: Rect, uri: &str) -> bool {
    area.width >= QR_MIN_WIDTH
        && area.height >= QR_MIN_HEIGHT
        && QrWidget::new(uri)
            .block_size()
            .is_some_and(|(w, h)| w <= area.width && h <= area.height)
}

/// The rect the Pair step reserves for the pairing matrix in a frame of
/// `frame`, or `None` when nothing should be painted there.
///
/// One answer, three readers: [`super::render`] gates on it, [`pair_step`]
/// paints its own "needs more room" copy when it is `None`, and
/// [`paint_pair_matrix`] draws into it. The matrix needs a [`Frame`], which a
/// `PaintCtx` cannot supply, so it is painted after the ratcn pass — and
/// painting it frame-wide would cover this layer. Owning the geometry here,
/// beside [`pair_step`], is what keeps the halves in step, exactly as
/// `qr_overlay::split`/`split_showing` do for the fullscreen layer.
pub(crate) fn pair_qr_rect(state: &AppState, frame: Rect) -> Option<Rect> {
    if state.wizard.step != WizardStep::Pair {
        return None;
    }
    // Too small for the layer at all: the panel is painting the resize hint.
    if frame.width < WIDTH || frame.height < HEIGHT {
        return None;
    }
    // Only a `Showing` overlay has a matrix to paint; the other phases are the
    // panel's own copy.
    let QrOverlayPhase::Showing { uri, .. } = &state.tokens.qr_overlay.as_ref()?.phase else {
        return None;
    };
    let body = panel_inner(layer_rows(layer_area(frame, WizardStep::Pair))[2]);
    let (matrix, _) = split_pair(body);
    fits_a_matrix(matrix, uri).then_some(matrix)
}

/// Paint the pairing matrix into the rect the Pair step reserved for it.
///
/// The wizard paints its own matrix rather than handing its rect to
/// [`super::qr_overlay::paint_matrix`]: that function takes a *layer* area and
/// splits the chrome out of it the way the fullscreen layer arranges its own,
/// so feeding it an already-split rect would carve the caption rows off twice.
/// [`pair_qr_rect`] has already established that the block fits, so
/// [`QrWidget::render`] never reaches its raw-URI fallback here.
pub(crate) fn paint_pair_matrix(frame: &mut Frame, state: &AppState, frame_area: Rect) {
    let Some(rect) = pair_qr_rect(state, frame_area) else {
        return;
    };
    let Some(QrOverlayPhase::Showing { uri, .. }) =
        state.tokens.qr_overlay.as_ref().map(|o| &o.phase)
    else {
        return;
    };
    QrWidget::new(uri.as_str()).render(frame, rect);
}

/// `size` centered inside `frame`, clamped to it.
fn centered(frame: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(frame.width);
    let h = height.min(frame.height);
    Rect::new(
        frame.x + (frame.width - w) / 2,
        frame.y + (frame.height - h) / 2,
        w,
        h,
    )
}

/// The layer's area minus its border and one cell of padding per side.
fn inset(area: Rect) -> Rect {
    Rect::new(
        area.x + EDGE,
        area.y + 1,
        area.width.saturating_sub(EDGE * 2),
        area.height.saturating_sub(2),
    )
}

/// The layer's own frame: an opaque surface with the wizard's title on it.
fn frame_block(theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.surface))
        .title(Span::styled(
            " Set up muxrd ",
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD),
        ))
}

/// The stepper: one dot per step, joined by rules, with the count beside it.
fn stepper_line(step: WizardStep, theme: &Theme) -> Line<'static> {
    let done = Style::default().fg(theme.primary);
    let todo = Style::default().fg(theme.muted_foreground);
    let mut spans = Vec::new();
    for (index, _) in WizardStep::ALL.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("───", todo));
        }
        let reached = index <= step.index();
        spans.push(Span::styled(
            if reached { "●" } else { "○" },
            if reached { done } else { todo },
        ));
    }
    spans.push(Span::styled(
        format!(
            "   Step {} of {} · {}",
            step.index() + 1,
            WizardStep::ALL.len(),
            step.title()
        ),
        todo,
    ));
    Line::from(spans)
}

/// Whether the focus path names something inside `step`'s panel.
fn focus_is_inside(state: &AppState, step: WizardStep) -> bool {
    let path = state.ui.focus.path();
    path.len() >= 2 && path[0] == DialogId::Wizard.id() && path[1] == step.id()
}

/// Paint muted explanatory prose, wrapped into the rows reserved for it.
fn paint_block(ctx: &mut DeclareCtx<'_, AppState, UiMsg>, text: &[&str], area: Rect) {
    let style = Style::default().fg(ctx.theme.muted_foreground);
    let lines: Vec<Line<'static>> = text
        .iter()
        .map(|line| Line::from(Span::styled((*line).to_string(), style)))
        .collect();
    ctx.paint_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

/// Word-wrap `text` to `width` cells as owned lines.
fn wrap(text: &str, width: u16) -> Vec<String> {
    wrap_to_width(text, usize::from(width.max(1)))
        .into_iter()
        .map(ToString::to_string)
        .collect()
}

/// `text` cut to `width` cells, so one long value cannot bleed past the panel.
fn truncate(text: &str, width: u16) -> String {
    ratcn::text_width::truncate_to_width(text, usize::from(width)).to_string()
}

// ── Esc ───────────────────────────────────────────────────────────────────────

/// A control that also answers Esc with [`WizardMsg::Close`].
///
/// A ratcn [`Dialog`](ratcn::Dialog) owns `on_dismiss`; a `modal_scope` in ratcn
/// 0.0.3 has none, and an open modal absorbs every key nothing inside it handled
/// rather than letting it bubble out to the app — so an Esc "routed through the
/// reducer" would never arrive. Wrapping each control is what makes the row's
/// "Esc cancels" true wherever the focus happens to be.
///
/// The inner control is asked first and only an Esc it *ignored* closes the
/// wizard, so a control that wants Esc for itself — an open [`Select`] panel,
/// for one — keeps it.
struct EscCloses<C> {
    inner: C,
}

impl<C> EscCloses<C> {
    const fn new(inner: C) -> Self {
        Self { inner }
    }
}

impl<C: Component<AppState, UiMsg>> Component<AppState, UiMsg> for EscCloses<C> {
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
            && key.code == KeyCode::Esc
            && !key.modifiers.any()
        {
            return EventResult::Emit(close());
        }
        result
    }

    fn reveal_in_viewport(&mut self, target: Rect, state: &AppState, ctx: &mut EventCtx<'_>) {
        self.inner.reveal_in_viewport(target, state, ctx);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::wizard::WizardStep;
    use crate::app::{Message, update};
    use crate::tui::runner::build_ratcn;
    use crate::tui::views::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratcn::Theme;
    use ratcn::runtime::{Event, EventResult, FocusState, KeyEvent, Ratcn};

    /// The runner's loop in miniature: keys go to ratcn first, the reducer only
    /// gets what it ignored — exactly `runner::next_message`.
    struct Harness {
        terminal: Terminal<TestBackend>,
        ratcn: Ratcn<AppState, UiMsg>,
        theme: Theme,
        state: AppState,
    }

    impl Harness {
        fn new(width: u16, height: u16) -> Self {
            let mut harness = Self {
                terminal: Terminal::new(TestBackend::new(width, height)).expect("terminal"),
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

        fn key(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code);
            let message = match self.ratcn.handle_event(Event::Key(key), &self.state) {
                EventResult::Emit(msg) => Message::Ui(msg),
                EventResult::Consumed => Message::Tick,
                EventResult::Ignored => Message::Key(key),
            };
            let _ = update(&mut self.state, message);
            self.draw();
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

        /// The bounding box of every white cell on screen — the QR block paints
        /// its quiet zone white and nothing else in the app does, so this is
        /// where the matrix landed.
        fn white_box(&self) -> Option<Rect> {
            let buffer = self.terminal.backend().buffer();
            let area = buffer.area;
            let mut hit: Option<(u16, u16, u16, u16)> = None;
            for y in 0..area.height {
                for x in 0..area.width {
                    if buffer[(x, y)].bg != ratatui::style::Color::White {
                        continue;
                    }
                    hit = Some(match hit {
                        None => (x, y, x, y),
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                    });
                }
            }
            hit.map(|(x0, y0, x1, y1)| Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1))
        }

        /// Put the wizard on Pair with a `Showing` overlay, the way the reducer
        /// leaves it once the daemon answered `ShowTokenQr`.
        fn pairing(&mut self) {
            self.state.wizard.step = WizardStep::Pair;
            self.state.ui.focus = FocusState::intent([DialogId::Wizard.id(), "wiz_done"]);
            self.state.tokens.qr_overlay = Some(crate::app::state::tokens::QrOverlay {
                phase: QrOverlayPhase::Showing {
                    uri: PAIR_URI.to_string(),
                    host: "10.0.0.5".to_string(),
                    port: 50051,
                    fingerprint_short: "ab:cd:ef…".to_string(),
                },
                seq: 1,
                baseline_clients: 0,
                token_name: "phone".to_string(),
                read_only: true,
                tick_counter: 0,
            });
            self.draw();
        }
    }

    /// A pairing URI the size the daemon really emits (~185 bytes), so the fit
    /// check reasons about a production block rather than a toy one.
    const PAIR_URI: &str = "muxr://pair?v=2&h=10.0.0.5&p=50051&t=Zm9vYmFyYmF6cXV1eGNvcmdlZ3JhdWx0Z2FycGx5&fp=\
         0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef&ro=1&tm=pin&n=phone";

    #[test]
    fn w_opens_the_wizard_and_it_paints_its_first_step() {
        let mut harness = Harness::new(80, 24);
        harness.key(KeyCode::Char('w'));
        assert_eq!(harness.state.top_dialog(), Some(DialogId::Wizard));
        let screen = harness.screen();
        for expected in ["Set up muxrd", "Welcome", "Step 1 of 6", "Back", "Next"] {
            assert!(
                screen.contains(expected),
                "{expected} missing from:\n{screen}"
            );
        }
    }

    #[test]
    fn esc_closes_the_wizard() {
        let mut harness = Harness::new(80, 24);
        harness.key(KeyCode::Char('w'));
        harness.key(KeyCode::Esc);
        assert_eq!(harness.state.top_dialog(), None);
        assert_eq!(harness.state.ui.modals.ids().len(), 0);
    }

    #[test]
    fn enter_alone_walks_the_wizard_from_welcome_to_the_saved_bind_address() {
        let mut harness = Harness::new(80, 24);
        harness.key(KeyCode::Char('w'));
        harness.state.config.host = "10.0.0.5".to_string();
        harness.state.config.port = "50051".to_string();

        // Enter on the parked Next walks Welcome → Network …
        harness.key(KeyCode::Enter);
        assert_eq!(harness.state.wizard.step, WizardStep::Network);
        assert!(
            harness.screen().contains("Bind host"),
            "the Network step did not paint:\n{}",
            harness.screen()
        );

        // … and again commits the form rather than typing into it.
        harness.key(KeyCode::Enter);
        assert!(harness.state.wizard.busy);
        assert_eq!(harness.state.config.bind_addr(), "10.0.0.5:50051");

        let _ = update(
            &mut harness.state,
            Message::ActionOk("Bind address saved.".to_string()),
        );
        harness.draw();
        assert_eq!(harness.state.wizard.step, WizardStep::Certificate);
        assert!(
            harness.screen().contains("Advertised trust"),
            "the Certificate step did not paint:\n{}",
            harness.screen()
        );
    }

    #[test]
    fn every_step_paints_inside_an_eighty_by_twentyfour_terminal() {
        for step in WizardStep::ALL {
            let mut harness = Harness::new(80, 24);
            harness.key(KeyCode::Char('w'));
            harness.state.wizard.step = step;
            if step == WizardStep::Certificate {
                harness.state.cert.fingerprint = Some("ab".repeat(32));
            }
            harness.draw();
            let screen = harness.screen();
            assert!(
                screen.contains(step.title()),
                "step {step:?} did not paint its title:\n{screen}"
            );
            assert!(
                !screen.contains("Resize to continue"),
                "step {step:?} asked for a resize at 80x24:\n{screen}"
            );
        }
    }

    #[test]
    fn a_small_terminal_asks_for_a_resize_instead_of_a_step() {
        let mut harness = Harness::new(60, 18);
        harness.key(KeyCode::Char('w'));
        let screen = harness.screen();
        assert!(
            screen.contains("Resize to continue"),
            "no resize hint at 60x18:\n{screen}"
        );
    }

    /// The matrix belongs to the Pair panel, not the frame: it lands inside the
    /// rect the view hands out, and the chrome around it survives the paint.
    #[test]
    fn the_pair_step_keeps_the_matrix_inside_its_own_panel() {
        let mut harness = Harness::new(100, 44);
        harness.key(KeyCode::Char('w'));
        harness.pairing();

        let rect = pair_qr_rect(&harness.state, Rect::new(0, 0, 100, 44))
            .expect("a 100x44 frame has room for the pairing matrix");
        let painted = harness.white_box().expect("the matrix did not paint");
        assert!(
            painted.x >= rect.x
                && painted.y >= rect.y
                && painted.right() <= rect.right()
                && painted.bottom() <= rect.bottom(),
            "the matrix painted at {painted:?}, outside the panel's {rect:?}"
        );

        let screen = harness.screen();
        for expected in [
            "Set up muxrd",
            "Step 6 of 6",
            "Server: 10.0.0.5:50051",
            "Done",
        ] {
            assert!(
                screen.contains(expected),
                "the matrix painted over {expected}:\n{screen}"
            );
        }
        assert!(
            !screen.contains("muxr://pair"),
            "the raw pairing URI leaked onto the frame:\n{screen}"
        );
    }

    /// Too few rows for a matrix: the panel says so itself rather than letting
    /// `QrWidget`'s fallback print the secret-bearing URI over the frame.
    #[test]
    fn a_short_frame_gets_the_wizards_own_copy_instead_of_the_raw_uri() {
        let mut harness = Harness::new(80, 24);
        harness.key(KeyCode::Char('w'));
        harness.pairing();

        assert!(pair_qr_rect(&harness.state, Rect::new(0, 0, 80, 24)).is_none());
        let screen = harness.screen();
        assert!(
            screen.contains("does not fit this"),
            "no explanation of the missing code:\n{screen}"
        );
        assert!(
            !screen.contains("muxr://pair") && !screen.contains("terminal too small"),
            "the widget fallback painted anyway:\n{screen}"
        );
        assert!(
            screen.contains("Set up muxrd") && screen.contains("Token: phone"),
            "the panel lost its own copy:\n{screen}"
        );
    }

    /// Only the Pair step reserves a rect; every other step gets no matrix at
    /// all, which is what stops a lingering overlay painting over them.
    #[test]
    fn no_other_step_reserves_a_matrix_rect() {
        let mut harness = Harness::new(100, 44);
        harness.key(KeyCode::Char('w'));
        harness.pairing();
        for step in WizardStep::ALL {
            harness.state.wizard.step = step;
            let rect = pair_qr_rect(&harness.state, Rect::new(0, 0, 100, 44));
            assert_eq!(
                rect.is_some(),
                step == WizardStep::Pair,
                "step {step:?} disagreed about owning the matrix"
            );
        }
    }

    #[test]
    fn the_last_step_offers_done_instead_of_next() {
        let mut harness = Harness::new(80, 24);
        harness.key(KeyCode::Char('w'));
        harness.state.wizard.step = WizardStep::Pair;
        // What the reducer parks on entering the last step.
        harness.state.ui.focus = FocusState::intent([DialogId::Wizard.id(), "wiz_done"]);
        harness.draw();
        let screen = harness.screen();
        assert!(screen.contains("Done"), "no Done button:\n{screen}");
        assert!(
            screen.contains("No token was minted"),
            "the Pair step must say what to do without a secret:\n{screen}"
        );

        harness.key(KeyCode::Enter);
        assert_eq!(
            harness.state.top_dialog(),
            None,
            "Done must finish the wizard"
        );
    }
}
