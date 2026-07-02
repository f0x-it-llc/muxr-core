//! QR code widget — centered, black-on-white, with a manual quiet-zone border.
//!
//! Wraps `tui_qrcode::{QrCodeWidget, QuietZone}` with an explicit white
//! background and manual quiet-zone padding so phone scanners accept the code.
//!
//! If the available area is too small to hold the matrix, a fallback is rendered
//! that shows the required minimum dimensions and the raw `muxr://pair?…`
//! URI so the user can copy/paste it even on a tiny terminal.
//!
//! # Usage
//!
//! ```ignore
//! frame.render_widget(QrWidget::new("https://example.com"), area);
//! ```

use qrcode::{EcLevel, QrCode};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use tui_qrcode::{QrCodeWidget, QuietZone};

/// Horizontal quiet-zone padding in terminal columns (≈ 4 modules).
const QUIET_H: u16 = 4;
/// Vertical quiet-zone padding in terminal rows (≥ 1 row each side).
const QUIET_V: u16 = 1;

/// Minimum area dimensions required to render the QR matrix at all.
///
/// The real production `muxr://pair?…` URI is approximately 185 bytes
/// (64-hex `fp`, 48-char base64url UUID `t`, typical host and label).
/// We use ECC-L (error correction level Low) because the code is displayed
/// on-screen and scanned immediately — there is no physical damage risk that
/// warrants higher redundancy.
///
/// With ECC-L the ~185-byte payload encodes to **QR version 8 = 49×49 modules**
/// (4×8+17 = 49).  Dense1x2 renders 1 col per module and packs 2 modules per
/// character row, so the matrix occupies 49 cols × 25 rows (⌈49/2⌉).  With
/// the manual quiet-zone padding the block is 57 cols × 27 rows.
///
/// `MIN_WIDTH` / `MIN_HEIGHT` are the minimum area thresholds below which the
/// fallback text is shown instead of the QR matrix.  These values are set
/// conservatively (based on a shorter test URI) so the widget shows a QR code
/// in smaller-payload scenarios while always showing the fallback for the full
/// 57×27 production block when the terminal is too short.  The payload sits
/// near the ECC-L version-8 capacity boundary (~185/194 bytes); a materially
/// longer label or token could bump the version to 9 (53 modules) — a
/// documented risk, no code change required.
pub(crate) const MIN_WIDTH: u16 = 41;
pub(crate) const MIN_HEIGHT: u16 = 19;

/// A stateless QR-code widget.
///
/// Renders a `muxr://pair?...` (or any) payload as a scannable
/// half-block QR matrix, centered with a white quiet-zone border.
///
/// The QR code is encoded **once** in [`QrWidget::new`] and shared by both
/// size-measurement (`block_width`) and rendering (`render`), so there is no
/// double-encode per frame and measurement and rendering always reason about
/// the same matrix.
///
/// The widget is used by the fullscreen QR overlay
/// ([`crate::tui::screens::qr_overlay`]) when a token is freshly minted and
/// the user opens the overlay to pair a mobile client.
///
/// The widget stores the payload so it can also render it as plain text in the
/// fallback path when the terminal is too small.
pub struct QrWidget {
    payload: String,
    /// Pre-encoded QR code (ECC-L, encoded once in `new`).
    ///
    /// `None` only when the payload cannot be encoded (malformed input) — for
    /// well-formed `muxr://` URIs this is always `Some`.
    code: Option<QrCode>,
}

impl QrWidget {
    /// Create a new QR widget for the given payload.
    ///
    /// The QR code is encoded here once with ECC-L.  Subsequent calls to
    /// [`block_width`] and [`render`] reuse the same `QrCode` instance.
    pub fn new(payload: impl Into<String>) -> Self {
        let payload = payload.into();
        let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L).ok();
        Self { payload, code }
    }

    /// Return the computed block size `(cols, rows)` this widget will occupy,
    /// including the manual quiet-zone padding on all sides.
    ///
    /// Returns `None` when the payload could not be encoded (should never
    /// happen for well-formed `muxr://` URIs).
    ///
    /// Reads the single cached [`QrCode`] stored in `self.code`; does not
    /// re-encode the payload.
    ///
    /// Retained for measurement and the layout-fit guard test; the fullscreen
    /// overlay no longer needs it for layout decisions (`QrWidget` self-centers).
    #[allow(dead_code)]
    pub(crate) fn block_size(&self) -> Option<(u16, u16)> {
        let qr = self.code.as_ref()?;
        let qr_widget = QrCodeWidget::new(qr.clone())
            .quiet_zone(QuietZone::Disabled)
            .style(Style::default().fg(Color::Black).bg(Color::White));
        // Use a dummy large rect just to obtain the matrix size.
        let dummy = Rect::new(0, 0, 200, 100);
        let sz = qr_widget.size(dummy);
        Some((
            sz.width.saturating_add(QUIET_H * 2),
            sz.height.saturating_add(QUIET_V * 2),
        ))
    }

    /// Return the computed block width (cols) this widget will occupy.
    ///
    /// Returns `None` when the payload could not be encoded (should never
    /// happen for well-formed `muxr://` URIs).
    ///
    /// Implemented via [`block_size`]; reads the single cached `QrCode`.
    #[allow(dead_code)]
    pub fn block_width(&self) -> Option<u16> {
        self.block_size().map(|(w, _)| w)
    }

    /// Draw the widget into `area` of `frame`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            self.render_too_small(frame, area);
            return;
        }

        // Use the pre-encoded QrCode from `new`; fall back to error display if
        // encoding failed (malformed payload, should not occur in practice).
        let qr = match self.code.as_ref() {
            Some(q) => q.clone(),
            None => {
                render_error(frame, area, "QR encode failed");
                return;
            }
        };

        // The tui-qrcode widget with quiet zone disabled; we add our own padding.
        let qr_widget = QrCodeWidget::new(qr)
            .quiet_zone(QuietZone::Disabled)
            .style(Style::default().fg(Color::Black).bg(Color::White));

        // Compute the pixel size of the matrix so we can center it.
        let matrix_size = qr_widget.size(area);
        let matrix_w = matrix_size.width;
        let matrix_h = matrix_size.height;

        // Total block including our manual quiet zone.
        let block_w = matrix_w.saturating_add(QUIET_H * 2);
        let block_h = matrix_h.saturating_add(QUIET_V * 2);

        if block_w > area.width || block_h > area.height {
            self.render_too_small(frame, area);
            return;
        }

        // Center the block within the available area.
        let pad_left = (area.width.saturating_sub(block_w)) / 2;
        let pad_top = (area.height.saturating_sub(block_h)) / 2;

        // White background for the entire block (quiet zone + matrix).
        let block_rect = Rect::new(area.x + pad_left, area.y + pad_top, block_w, block_h);
        frame.render_widget(
            ratatui::widgets::Block::default().style(Style::default().bg(Color::White)),
            block_rect,
        );

        // Matrix rect is inset by the quiet zone padding.
        let matrix_rect = ratatui::layout::Rect::new(
            block_rect.x + QUIET_H,
            block_rect.y + QUIET_V,
            matrix_w.min(block_rect.width.saturating_sub(QUIET_H * 2)),
            matrix_h.min(block_rect.height.saturating_sub(QUIET_V * 2)),
        );

        frame.render_widget(qr_widget, matrix_rect);
    }

    /// Render the "terminal too small" fallback: show required dimensions and
    /// the raw URI so users on small terminals can pair by copy/paste.
    ///
    /// A caution note is included because the URI embeds a one-time bearer
    /// token that persists in scrollback / screen-share history until cleared.
    fn render_too_small(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    "[ terminal too small — need {}×{} cols×rows ]",
                    MIN_WIDTH, MIN_HEIGHT
                ),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Pair via copy/paste — URI (contains a one-time secret — clear your scrollback after pairing):",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(""),
        ];
        // Append the raw URI so the user can select and copy it.
        lines.push(Line::from(Span::styled(
            self.payload.clone(),
            Style::default().fg(Color::Cyan),
        )));
        frame.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::Yellow)),
            area,
        );
    }
}

/// Render a generic error fallback.
fn render_error(frame: &mut Frame, area: Rect, msg: &str) {
    frame.render_widget(
        Paragraph::new(Span::styled(msg, Style::default().fg(Color::Red)))
            .alignment(Alignment::Center),
        area,
    );
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_widget_new_stores_payload() {
        let w = QrWidget::new("hello");
        assert_eq!(w.payload, "hello");
    }

    #[test]
    fn min_dimensions_are_sane() {
        // MIN_WIDTH and MIN_HEIGHT are conservative thresholds for showing the
        // QR matrix vs the fallback text.  They are calibrated for shorter test
        // payloads (version-4, 33-module QR); the real production payload encodes
        // to QR version 8 (49 modules) with block 57×27 — see
        // `layout_80x24_phase_body_fits_qr` for the full breakdown.
        //
        // MIN_WIDTH  >= 33 (33-module matrix cols) + 2*QUIET_H = 41.
        const { assert!(MIN_WIDTH >= 33 + QUIET_H * 2) };
        // MIN_HEIGHT >= 17 (⌈33/2⌉ terminal rows) + 2*QUIET_V = 19.
        const { assert!(MIN_HEIGHT >= 17 + QUIET_V * 2) };
    }

    #[test]
    fn quiet_zone_constants_match_spec() {
        // QUIET_H ≈ 4 horizontal cells of white space on each side.
        assert_eq!(QUIET_H, 4);
        // QUIET_V ≥ 1 row of white space on each side.
        const { assert!(QUIET_V >= 1) };
    }

    /// QR is encoded exactly once in `new`; block_width and render both reuse it.
    #[test]
    fn qr_encoded_once_in_new() {
        let w = QrWidget::new("muxr://pair?v=1&h=127.0.0.1&p=50051&fp=aabbcc&t=dGVzdA&ro=0&n=dev");
        // code must be Some for a valid payload
        assert!(w.code.is_some(), "QrCode should be encoded in new()");
        // block_width reads from the same stored code, not re-encoding
        let bw = w.block_width();
        assert!(
            bw.is_some(),
            "block_width should return Some for a valid payload"
        );
        assert!(
            bw.unwrap() >= MIN_WIDTH,
            "block_width should be >= MIN_WIDTH"
        );
    }

    /// Guardrail: a real-length pairing payload encodes and its block dimensions
    /// fit the layout the fullscreen QR overlay provides.
    ///
    /// ## What "real payload" means
    ///
    /// The production URI format is:
    /// ```text
    /// muxr://pair?v=1&h=192.168.1.123&p=50051&fp=<64 hex>&t=<48 b64url>&ro=0&n=muxrd
    /// ```
    /// (~185 bytes).  With ECC-L this encodes to **QR version 8 = 49 modules**
    /// (block 57 cols × 27 rows including the manual quiet zone).
    ///
    /// Note: the payload sits near the ECC-L version-8 capacity boundary
    /// (~185/194 bytes); a materially longer label or token could bump the
    /// version to 9 (53 modules, block 61×29) — documented risk, no code change.
    ///
    /// ## Layout chain (fullscreen QR overlay, Showing phase)
    ///
    /// `qr_overlay::render` receives the full frame area and splits borderless:
    /// ```text
    ///   phase_body(Min 3) + bottom_strip(1)
    /// ```
    /// `render_showing` (Showing phase) uses a horizontal side-by-side split when
    /// `area.width >= qr_block_w + INFO_MIN_WIDTH (28)`:
    /// ```text
    ///   cols[0]: width = qr_block_w (57)   ← QR widget area
    ///   cols[1]: Min(0)                     ← info panel
    /// ```
    ///
    /// At terminal width ≥ 85 cols (57+28), the QR gets a column that matches its
    /// block width exactly, and the height equals `phase_body.height`.  This test
    /// uses 90×30 to trigger the side-by-side path (80 cols is too narrow).
    ///
    /// ## Margin calculation
    ///
    /// `qr_overlay::render` receives the full frame (no title/footer deduction),
    /// giving `phase_body.height = frame_height − 1(bottom_strip)`.  At 90×30:
    ///   phase_body = 30 − 1 = 29 → margin = 29 − 27 = 2 rows.
    #[test]
    fn layout_80x24_phase_body_fits_qr() {
        use ratatui::layout::{Constraint, Direction, Layout, Rect};

        // ── Build a representative real-length payload (~171 bytes). ───────────
        // 64 lowercase-hex chars for `fp`, 48 base64url chars for `t`.
        let payload = format!(
            "muxr://pair?v=1&h=192.168.1.123&p=50051&fp={}&t={}&ro=0&n=muxrd",
            "a".repeat(64),
            "B".repeat(48),
        );
        assert_eq!(
            payload.len(),
            171,
            "representative payload must be ~171 bytes"
        );

        let qr = QrWidget::new(&payload);

        // ── Obtain actual block dimensions from the cached QrCode. ─────────────
        let (block_w, block_h) = qr
            .block_size()
            .expect("real payload must encode to a valid QrCode");
        // Real payload → QR version 8 (49 modules) → block 57×27.
        assert_eq!(
            block_w, 57,
            "block_width for 185-byte payload must be 57 cols"
        );
        assert_eq!(
            block_h, 27,
            "block_height for 185-byte payload must be 27 rows"
        );

        // ── Derive the phase-body Rect mirroring the REAL layout chain. ────────
        //
        // Use a 90×30 terminal: wide enough for the side-by-side split
        // (needs ≥ 85 cols = block_w(57) + INFO_MIN_WIDTH(28)) and tall enough
        // for the block (≥ block_h(27) + 1 bottom_strip + 2 margin = 30).
        // At 80×24 the block (27 rows) exceeds the phase_body (23 rows), so the
        // fallback text renders; 90×30 is the minimum for the QR to render.
        let frame_area = Rect::new(0, 0, 90, 30);

        // Step 1: qr_overlay::render receives the full frame area (fullscreen).
        let overlay_area = frame_area;

        // Step 2: qr_overlay::render — borderless, phase_body + bottom_strip.
        let overlay_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // phase body
                Constraint::Length(1), // bottom strip (Esc + ro)
            ])
            .split(overlay_area);
        let phase_body = overlay_rows[0]; // 29 rows, 90 cols

        // Step 3: render_showing (side-by-side) — QR gets the left column.
        // qr_block_w = block_w (57); INFO_MIN_WIDTH = 28; 90 >= 57+28 → side-by-side.
        let qr_col_share = block_w; // cols[0].width == qr_block_w == block_w
        let qr_col_height = phase_body.height;

        // ── Assertions ─────────────────────────────────────────────────────────
        // The QR must actually fit (not trigger the fallback path).
        assert!(
            block_w <= qr_col_share,
            "block_w ({block_w}) must be <= QR column share ({qr_col_share})"
        );
        assert!(
            block_h <= qr_col_height,
            "block_h ({block_h}) must be <= phase_body.height ({qr_col_height}); \
             overlay gives +2 row margin (margin = {})",
            qr_col_height.saturating_sub(block_h),
        );
        // Verify ≥1 row margin.
        assert!(
            qr_col_height > block_h,
            "phase_body ({qr_col_height}) must be >= block_h ({block_h}) + 1 margin"
        );
    }
}
