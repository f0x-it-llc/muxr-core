//! Certificate dialogs — stubs the cert card fills in.

use ratcn::runtime::DeclareCtx;

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::cert::CertMsg;

/// The certificate overview.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(ctx, DialogId::Cert, "Certificate", "cert_close", close);
}

/// The certificate explainer.
pub fn declare_help(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(
        ctx,
        DialogId::CertHelp,
        "Certificate",
        "cert_help_close",
        close,
    );
}

/// The "regenerate the certificate?" confirmation.
pub fn declare_regen_confirm(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(
        ctx,
        DialogId::CertRegenConfirm,
        "Certificate",
        "cert_regen_close",
        close,
    );
}

/// What every cert dialog's Close action and Esc both emit.
fn close() -> UiMsg {
    UiMsg::Cert(CertMsg::Close)
}
