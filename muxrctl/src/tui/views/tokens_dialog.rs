//! Token dialogs — stubs the tokens card fills in.

use ratcn::runtime::DeclareCtx;

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::tokens::TokensMsg;

/// The token list.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(ctx, DialogId::Tokens, "Tokens", "tokens_close", close);
}

/// The create form.
pub fn declare_create(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(
        ctx,
        DialogId::TokenCreate,
        "Tokens",
        "token_create_close",
        close,
    );
}

/// The one-time minted-secret display.
pub fn declare_minted(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(
        ctx,
        DialogId::TokenMinted,
        "Tokens",
        "token_minted_close",
        close,
    );
}

/// The "revoke this token?" confirmation.
pub fn declare_revoke_confirm(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(
        ctx,
        DialogId::TokenRevokeConfirm,
        "Tokens",
        "token_revoke_close",
        close,
    );
}

/// What every token dialog's Close action and Esc both emit.
fn close() -> UiMsg {
    UiMsg::Tokens(TokensMsg::Close)
}
