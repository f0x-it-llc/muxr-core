//! The daemon's one dialog: the stop confirmation.

use ratcn::runtime::DeclareCtx;
use ratcn::{Button, Dialog};

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::server::ServerMsg;
use crate::tui::components::DialogShortcuts;
use crate::tui::components::action::{action, destructive};

/// Declare the "Stop muxrd?" confirmation as a modal layer.
pub fn declare_stop_confirm(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    let area = ctx.frame_area();
    let dialog = Dialog::new()
        .title("Stop muxrd?")
        .description("Every attached phone is disconnected until you start it again.")
        .action(
            "stop_cancel",
            action(
                Button::new("Cancel")
                    .secondary()
                    .on_press(|| UiMsg::Server(ServerMsg::StopCancelled)),
            ),
        )
        .action(
            "stop_confirm",
            destructive(
                Button::new("Stop")
                    .destructive()
                    .on_press(|| UiMsg::Server(ServerMsg::StopConfirmed)),
            ),
        )
        .on_dismiss(|| UiMsg::Server(ServerMsg::StopCancelled));
    ctx.modal(
        DialogId::StopServer.id(),
        DialogShortcuts::new(DialogId::StopServer, dialog),
        area,
    );
}
