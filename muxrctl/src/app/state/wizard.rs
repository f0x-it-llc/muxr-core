//! Setup-wizard state: the step machine, the flags the reducer walks it with,
//! and the words the Welcome step shows.
//!
//! Plain data only — the wizard's behaviour lives in `app/update/wizard.rs` and
//! its layout in `tui/views/wizard.rs`, per `docs/CORE_CODE_STANDARDS.md`
//! § muxrctl keeps its TEA split.
//!
//! The wizard owns no copy about tokens or certificates of its own: every
//! explanation it paints is one of the constants those features already
//! declare (`state::cert::{SAN_EXPLAIN, REGEN_EXPLAIN}`,
//! `state::tokens::TOKEN_EXPLAIN_*`, `state::config::BIND_EXPLAIN`), so a
//! sentence can never drift between the wizard and the dialog that owns it.

// ── Steps ─────────────────────────────────────────────────────────────────────

/// The wizard's six steps, in the order they are walked.
///
/// The order is the one statement of the flow: [`ALL`](WizardStep::ALL) is what
/// the stepper paints, [`index`](WizardStep::index) is the position in it, and
/// [`next`](WizardStep::next) / [`previous`](WizardStep::previous) move through
/// it and clamp at the ends rather than wrapping — a wizard that wrapped from
/// the last step back to the first would re-run the whole setup on one extra
/// Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WizardStep {
    /// What the wizard is about to do.
    #[default]
    Welcome,
    /// The bind address the phone will dial.
    Network,
    /// Keep or regenerate the TLS certificate.
    Certificate,
    /// Mint the pairing token.
    Token,
    /// Start the daemon.
    Start,
    /// Show the pairing QR.
    Pair,
}

impl WizardStep {
    /// Every step, in walk order.
    pub const ALL: [Self; 6] = [
        Self::Welcome,
        Self::Network,
        Self::Certificate,
        Self::Token,
        Self::Start,
        Self::Pair,
    ];

    /// Position in [`ALL`](Self::ALL) — what the stepper line counts from.
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|step| *step == self).unwrap_or(0)
    }

    /// The next step, clamped at the last one.
    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1).min(Self::ALL.len() - 1)]
    }

    /// The previous step, clamped at the first one.
    pub fn previous(self) -> Self {
        Self::ALL[self.index().saturating_sub(1)]
    }

    /// Whether this is the first step (Back does nothing here).
    pub const fn is_first(self) -> bool {
        matches!(self, Self::Welcome)
    }

    /// Whether this is the last step (there is no Next here, only Done).
    pub const fn is_last(self) -> bool {
        matches!(self, Self::Pair)
    }

    /// The scope id this step's panel is declared under.
    ///
    /// Stable: the focus paths the reducer parks on and the scope the view
    /// declares are the same strings, so they cannot drift.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Welcome => "wiz_welcome",
            Self::Network => "wiz_network",
            Self::Certificate => "wiz_cert",
            Self::Token => "wiz_token",
            Self::Start => "wiz_start",
            Self::Pair => "wiz_pair",
        }
    }

    /// The step panel's title.
    pub const fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Network => "Network",
            Self::Certificate => "Certificate",
            Self::Token => "Pairing token",
            Self::Start => "Daemon",
            Self::Pair => "Pair your phone",
        }
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

/// State for the first-run setup wizard.
#[derive(Debug, Clone, Default)]
pub struct WizardState {
    /// The step currently shown.
    pub step: WizardStep,
    /// Whether first-run detection has already run this session, so the wizard
    /// auto-opens at most once.
    pub first_run_checked: bool,
    /// True between dispatching a step's action and the result that advances
    /// it. Blocks Next and Back, and disables both buttons.
    pub busy: bool,
    /// The failure the current step is showing, if any.
    pub error: Option<String>,
    /// Whether a certificate already existed when the wizard reached the
    /// Network step — what the Certificate step describes as "detected".
    pub cert_existed: bool,
    /// The operator's acknowledgement that regenerating an existing
    /// certificate costs every phone its pairing.
    pub cert_regen_ack: bool,
    /// Whether a `CertInfoLoaded` has been seen this session.
    pub cert_info_seen: bool,
    /// Whether a `TokensLoaded` has been seen this session.
    pub tokens_seen: bool,
    /// One-shot hand-off from `on_tokens_loaded` to the `maybe_auto_open` call
    /// `app/update/mod.rs` makes immediately after it.
    ///
    /// `maybe_auto_open` is the single hook for both loads (it runs after
    /// `CertInfoLoaded` *and* after `TokensLoaded`), so it cannot tell from the
    /// state alone which one it is trailing. `on_tokens_loaded` — which only
    /// the tokens arm calls — sets this, and `maybe_auto_open` takes it: a call
    /// that finds it set is the tokens one, a call that does not is the cert
    /// one. Without it, a tokens load arriving before the first cert read would
    /// be counted as both and the wizard could open against a certificate it
    /// has not read yet.
    pub tokens_mark_pending: bool,
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Everything the wizard can ask for.
#[derive(Debug, Clone)]
pub enum WizardMsg {
    /// Close the wizard, wherever it is (Esc and the dashboard both emit this).
    Close,
    /// Commit the current step and move on.
    Next,
    /// Go back one step, with no side effects.
    Back,
    /// The "regenerate anyway" acknowledgement was toggled.
    RegenAckChanged(bool),
    /// The last step's Done: close the wizard and refresh the dashboard.
    Finish,
}

// ── Explanation ───────────────────────────────────────────────────────────────

/// What the wizard is about to do, shown on the Welcome step.
///
/// Every step it names is reachable afterwards from the dashboard, which is the
/// last line's point: nothing here is a one-time decision the operator is
/// locked into.
pub const WELCOME_EXPLAIN: &[&str] = &[
    "This sets up muxrd end to end, one step at a time:",
    "  1. pick the address the phone will dial,",
    "  2. create or review the TLS certificate,",
    "  3. mint a pairing token,",
    "  4. start the daemon,",
    "  5. show the pairing QR.",
    "Every step can be redone later from the dashboard.",
];

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_index_round_trips_over_all() {
        for (index, step) in WizardStep::ALL.iter().enumerate() {
            assert_eq!(step.index(), index, "{step:?}");
        }
    }

    #[test]
    fn next_and_previous_clamp_at_the_ends() {
        assert_eq!(WizardStep::Welcome.previous(), WizardStep::Welcome);
        assert_eq!(WizardStep::Pair.next(), WizardStep::Pair);
        assert_eq!(WizardStep::Welcome.next(), WizardStep::Network);
        assert_eq!(WizardStep::Pair.previous(), WizardStep::Start);
    }

    #[test]
    fn walking_forward_then_back_returns_every_step() {
        let mut step = WizardStep::Welcome;
        for expected in WizardStep::ALL.iter().skip(1) {
            step = step.next();
            assert_eq!(step, *expected);
        }
        for expected in WizardStep::ALL.iter().rev().skip(1) {
            step = step.previous();
            assert_eq!(step, *expected);
        }
    }

    #[test]
    fn only_the_ends_report_themselves_as_ends() {
        for step in WizardStep::ALL {
            assert_eq!(step.is_first(), step == WizardStep::Welcome, "{step:?}");
            assert_eq!(step.is_last(), step == WizardStep::Pair, "{step:?}");
        }
    }

    #[test]
    fn step_ids_are_distinct_and_spelled_exactly() {
        let ids: Vec<&str> = WizardStep::ALL.iter().map(|s| s.id()).collect();
        assert_eq!(
            ids,
            [
                "wiz_welcome",
                "wiz_network",
                "wiz_cert",
                "wiz_token",
                "wiz_start",
                "wiz_pair",
            ]
        );
    }

    #[test]
    fn step_titles_are_present_and_distinct() {
        let mut titles: Vec<&str> = WizardStep::ALL.iter().map(|s| s.title()).collect();
        assert!(titles.iter().all(|t| !t.is_empty()));
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), WizardStep::ALL.len());
    }

    #[test]
    fn a_fresh_wizard_starts_on_welcome_and_idle() {
        let state = WizardState::default();
        assert_eq!(state.step, WizardStep::Welcome);
        assert!(!state.first_run_checked);
        assert!(!state.busy);
        assert!(state.error.is_none());
    }

    /// The Welcome copy promises the operator nothing is final; it also must not
    /// restate token expiry, which only `TOKEN_EXPLAIN_EXPIRY` is allowed to
    /// describe (expiry gates new logins, it never tears down a live session).
    #[test]
    fn welcome_explain_says_steps_can_be_redone_and_avoids_expiry_claims() {
        assert!(!WELCOME_EXPLAIN.is_empty());
        assert!(WELCOME_EXPLAIN.iter().all(|line| !line.is_empty()));
        let joined = WELCOME_EXPLAIN.join(" ").to_lowercase();
        assert!(joined.contains("redone later"));
        assert!(
            !joined.contains("expire"),
            "the wizard must not restate token expiry: {joined}"
        );
    }
}
