//! Dialog letter shortcuts: the one table behind both the keys and the hints.
//!
//! Every dialog action is a button, reachable by Tab and Enter. The letters here
//! are a faster route to the same message, and the footer lists them from the
//! same rows, so what is advertised and what works cannot drift apart.
//!
//! They cannot live in the reducer's `handle_key`: with a modal open the
//! runtime turns every key the dialog ignored into a consumed one, so the
//! reducer never sees it. The view layer's `DialogShortcuts` wrapper receives
//! the keys a dialog's own controls ignored and asks [`message`] instead. A
//! focused text field consumes its characters first, so a letter typed into a
//! field can never double as a shortcut.
//!
//! Each row mirrors the `.disabled(…)` predicate of the button it stands for:
//! a letter never does what its button cannot.

use super::message::UiMsg;
use super::state::cert::CertMsg;
use super::state::devices::DevicesMsg;
use super::state::server::ServerMsg;
use super::state::tokens::TokensMsg;
use super::state::{AppState, DialogId};

/// One letter, the label the footer shows for it, and the message it sends.
pub struct Shortcut {
    /// The lowercase letter that triggers the row.
    pub key: char,
    /// What the footer calls it.
    pub label: &'static str,
    /// The message to send, or `None` while the matching button is disabled.
    build: fn(&AppState) -> Option<UiMsg>,
}

const fn row(key: char, label: &'static str, build: fn(&AppState) -> Option<UiMsg>) -> Shortcut {
    Shortcut { key, label, build }
}

static TOKENS: [Shortcut; 4] = [
    row('n', "create", |_| {
        Some(UiMsg::Tokens(TokensMsg::CreateRequested))
    }),
    row('s', "show QR", |s| {
        s.tokens
            .last_minted_secret
            .is_some()
            .then_some(UiMsg::Tokens(TokensMsg::ShowQrForMinted))
    }),
    row('x', "revoke", |s| {
        (s.tokens.focused_record().is_some() && !s.tokens.loading)
            .then_some(UiMsg::Tokens(TokensMsg::RevokeRequested))
    }),
    row('r', "refresh", |s| {
        (!s.tokens.loading).then_some(UiMsg::Tokens(TokensMsg::Refresh))
    }),
];

static TOKEN_MINTED: [Shortcut; 2] = [
    row('s', "show QR", |_| {
        Some(UiMsg::Tokens(TokensMsg::MintedShowQr))
    }),
    row('d', "done", |_| Some(UiMsg::Tokens(TokensMsg::MintedDone))),
];

static TOKEN_REVOKE: [Shortcut; 1] = [row('y', "revoke", |_| {
    Some(UiMsg::Tokens(TokensMsg::RevokeConfirmed))
})];

static CERT: [Shortcut; 3] = [
    row('h', "help", |_| Some(UiMsg::Cert(CertMsg::OpenHelp))),
    row('g', "regenerate", |s| {
        (!s.cert.loading).then_some(UiMsg::Cert(CertMsg::RegenerateRequested))
    }),
    row('r', "refresh", |_| Some(UiMsg::Cert(CertMsg::Refresh))),
];

static CERT_REGEN: [Shortcut; 1] = [row('y', "regenerate", |_| {
    Some(UiMsg::Cert(CertMsg::RegenerateConfirmed))
})];

static STOP: [Shortcut; 1] = [row('y', "stop", |_| {
    Some(UiMsg::Server(ServerMsg::StopConfirmed))
})];

static DEVICES: [Shortcut; 2] = [
    row('x', "remove", |s| {
        (s.devices.focused.is_some() && !s.devices.loading)
            .then_some(UiMsg::Devices(DevicesMsg::RemoveRequested))
    }),
    row('r', "refresh", |_| {
        Some(UiMsg::Devices(DevicesMsg::Refresh))
    }),
];

static DEVICE_REMOVE: [Shortcut; 1] = [row('y', "remove", |_| {
    Some(UiMsg::Devices(DevicesMsg::RemoveConfirmed))
})];

/// The letters a dialog answers to. Text-field dialogs (`TokenCreate`,
/// `Config`) have none, and `Qr` and `Wizard` carry their own key strips.
pub fn shortcuts(id: DialogId) -> &'static [Shortcut] {
    match id {
        DialogId::Tokens => &TOKENS,
        DialogId::TokenMinted => &TOKEN_MINTED,
        DialogId::TokenRevokeConfirm => &TOKEN_REVOKE,
        DialogId::Cert => &CERT,
        DialogId::CertRegenConfirm => &CERT_REGEN,
        DialogId::StopServer => &STOP,
        DialogId::Devices => &DEVICES,
        DialogId::DeviceRemoveConfirm => &DEVICE_REMOVE,
        DialogId::TokenCreate
        | DialogId::Config
        | DialogId::CertHelp
        | DialogId::Qr
        | DialogId::Wizard => &[],
    }
}

/// The message `key` sends in dialog `id` right now, or `None` when the letter
/// is unbound or its button is disabled.
pub fn message(id: DialogId, key: char, state: &AppState) -> Option<UiMsg> {
    shortcuts(id)
        .iter()
        .find(|shortcut| shortcut.key == key)
        .and_then(|shortcut| (shortcut.build)(state))
}

/// The dashboard's own letters, which the reducer's `handle_key` honours.
const DASHBOARD: [(&str, &str); 8] = [
    ("s", "start/stop"),
    ("c", "config"),
    ("e", "cert"),
    ("t", "tokens"),
    ("d", "devices"),
    ("w", "wizard"),
    ("r", "refresh"),
    ("q", "quit"),
];

/// The footer's (key, label) pairs for the top dialog (`None` is the
/// dashboard): the generic navigation keys first, then the dialog's letters.
/// Empty for `Qr` and `Wizard`, whose layers paint their own strips.
pub fn hint(id: Option<DialogId>) -> Vec<(&'static str, &'static str)> {
    let Some(id) = id else {
        return DASHBOARD.to_vec();
    };
    let mut pairs: Vec<(&'static str, &'static str)> = match id {
        DialogId::Qr | DialogId::Wizard => return Vec::new(),
        DialogId::TokenCreate => {
            vec![
                ("Enter", "creates"),
                ("Space", "toggles"),
                ("Esc", "cancels"),
            ]
        }
        DialogId::Config => vec![("Enter", "saves"), ("Esc", "cancels")],
        DialogId::TokenRevokeConfirm
        | DialogId::CertRegenConfirm
        | DialogId::StopServer
        | DialogId::DeviceRemoveConfirm => vec![("Esc", "cancels")],
        _ => vec![("Tab", "moves"), ("Enter", "presses"), ("Esc", "closes")],
    };
    pairs.extend(shortcuts(id).iter().map(|s| (key_str(s.key), s.label)));
    pairs
}

/// A `'static` one-letter string for a shortcut key.
fn key_str(key: char) -> &'static str {
    const LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";
    let at = (key as u8 - b'a') as usize;
    &LETTERS[at..=at]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tokens::TokenRecord;

    const ALL: [DialogId; 13] = [
        DialogId::Config,
        DialogId::StopServer,
        DialogId::Cert,
        DialogId::CertHelp,
        DialogId::CertRegenConfirm,
        DialogId::Tokens,
        DialogId::TokenCreate,
        DialogId::TokenMinted,
        DialogId::TokenRevokeConfirm,
        DialogId::Qr,
        DialogId::Devices,
        DialogId::DeviceRemoveConfirm,
        DialogId::Wizard,
    ];

    #[test]
    fn every_key_is_a_lowercase_letter_and_unique_per_dialog() {
        for id in ALL {
            let mut seen = Vec::new();
            for shortcut in shortcuts(id) {
                assert!(
                    shortcut.key.is_ascii_lowercase(),
                    "{id:?}: {}",
                    shortcut.key
                );
                assert!(
                    !seen.contains(&shortcut.key),
                    "{id:?} repeats {}",
                    shortcut.key
                );
                seen.push(shortcut.key);
            }
        }
    }

    #[test]
    fn text_field_and_self_stripped_dialogs_have_no_letters() {
        for id in [
            DialogId::TokenCreate,
            DialogId::Config,
            DialogId::Qr,
            DialogId::Wizard,
            DialogId::CertHelp,
        ] {
            assert!(shortcuts(id).is_empty(), "{id:?}");
        }
    }

    #[test]
    fn a_disabled_button_has_no_letter() {
        let mut state = AppState::new();
        // No held secret, no focused row.
        assert!(message(DialogId::Tokens, 's', &state).is_none());
        assert!(message(DialogId::Tokens, 'x', &state).is_none());
        assert!(message(DialogId::Tokens, 'n', &state).is_some());
        assert!(message(DialogId::Devices, 'x', &state).is_none());

        state.tokens.tokens.push(TokenRecord {
            name: "phone".into(),
            created_at: "now".into(),
            read_only: false,
        });
        state.tokens.focused = Some("phone".into());
        state.tokens.last_minted_secret = Some(("phone".into(), "s".into(), false));
        assert!(message(DialogId::Tokens, 'x', &state).is_some());
        assert!(message(DialogId::Tokens, 's', &state).is_some());

        state.tokens.loading = true;
        assert!(message(DialogId::Tokens, 'x', &state).is_none());
        assert!(message(DialogId::Tokens, 'r', &state).is_none());

        state.cert.loading = true;
        assert!(message(DialogId::Cert, 'g', &state).is_none());
        assert!(message(DialogId::Cert, 'h', &state).is_some());
    }

    #[test]
    fn unbound_letters_send_nothing() {
        let state = AppState::new();
        assert!(message(DialogId::Tokens, 'z', &state).is_none());
        assert!(message(DialogId::Config, 'n', &state).is_none());
    }

    #[test]
    fn the_dashboard_hint_is_the_eight_keys_in_order() {
        let keys: Vec<_> = hint(None).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["s", "c", "e", "t", "d", "w", "r", "q"]);
    }

    #[test]
    fn a_dialog_hint_is_generic_pairs_then_its_letters() {
        let pairs = hint(Some(DialogId::Tokens));
        assert_eq!(
            pairs[..3],
            [("Tab", "moves"), ("Enter", "presses"), ("Esc", "closes")]
        );
        assert_eq!(pairs[3], ("n", "create"));
        assert_eq!(pairs.len(), 3 + shortcuts(DialogId::Tokens).len());
        assert!(hint(Some(DialogId::Qr)).is_empty());
        assert!(hint(Some(DialogId::Wizard)).is_empty());
        assert_eq!(
            hint(Some(DialogId::StopServer)),
            [("Esc", "cancels"), ("y", "stop")]
        );
    }
}
