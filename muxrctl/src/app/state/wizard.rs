//! Setup-wizard state — a stub for the wizard card.
//!
//! The wizard's reducer hooks already exist in `app/update/wizard.rs` and are
//! called unconditionally from `app/update/mod.rs`, so the wizard card adds its
//! behaviour here and there without touching the routing.

/// State for the first-run setup wizard.
#[derive(Debug, Clone, Default)]
pub struct WizardState {
    /// Whether first-run detection has already run this session, so the wizard
    /// auto-opens at most once.
    pub first_run_checked: bool,
}

/// Everything the wizard can ask for.
#[derive(Debug, Clone)]
pub enum WizardMsg {
    /// Close the wizard.
    Close,
}
