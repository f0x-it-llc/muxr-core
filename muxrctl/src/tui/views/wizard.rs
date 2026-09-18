//! The setup wizard — a stub the wizard card fills in.

use ratcn::runtime::DeclareCtx;

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::wizard::WizardMsg;

/// The first-run setup wizard.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(ctx, DialogId::Wizard, "Setup wizard", "wizard_close", close);
}

/// What the wizard's Close action and Esc both emit.
fn close() -> UiMsg {
    UiMsg::Wizard(WizardMsg::Close)
}
