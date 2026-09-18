//! Push-device dialogs — stubs the devices card fills in.

use ratcn::runtime::DeclareCtx;

use crate::app::UiMsg;
use crate::app::state::AppState;
use crate::app::state::DialogId;
use crate::app::state::devices::DevicesMsg;

/// The device list.
pub fn declare(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(ctx, DialogId::Devices, "Devices", "devices_close", close);
}

/// The "remove this device?" confirmation.
pub fn declare_remove_confirm(ctx: &mut DeclareCtx<'_, AppState, UiMsg>) {
    super::stub_dialog(
        ctx,
        DialogId::DeviceRemoveConfirm,
        "Devices",
        "device_remove_close",
        close,
    );
}

/// What every devices dialog's Close action and Esc both emit.
fn close() -> UiMsg {
    UiMsg::Devices(DevicesMsg::Close)
}
