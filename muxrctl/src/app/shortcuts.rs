//! Dialog letter shortcuts: the one table behind both the keys and the hints.
//!
//! Every dialog action is a button, reachable by Tab and Enter. The letters here
//! are a faster route to the same message, and the footer lists them from the
//! same rows, so for dialog letters what is advertised and what works cannot
//! drift apart. The dashboard's letters live in the reducer's `handle_key`;
//! [`DASHBOARD`] repeats them for the footer and a test checks each one is live.
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

use super::message::{CopyTarget, UiMsg};
use super::state::cert::CertMsg;
use super::state::devices::DevicesMsg;
use super::state::server::ServerMsg;
use super::state::tokens::TokensMsg;
use super::state::{AppState, DialogId};

/// One letter, the label the footer shows for it, and the message it sends.
pub struct Shortcut {
    /// The lowercase letter that triggers the row.
    pub key: char,
    /// The same letter as the footer prints it.
    key_label: &'static str,
    /// What the footer calls it.
    pub label: &'static str,
    /// The message to send, or `None` while the matching button is disabled.
    build: fn(&AppState) -> Option<UiMsg>,
}

/// A row for the one-letter `key`. The letter is derived from the label string
/// (empty is a compile error in the statics), so the two cannot disagree.
const fn row(
    key: &'static str,
    label: &'static str,
    build: fn(&AppState) -> Option<UiMsg>,
) -> Shortcut {
    Shortcut {
        key: key.as_bytes()[0] as char,
        key_label: key,
        label,
        build,
    }
}

static TOKENS: [Shortcut; 4] = [
    row("n", "create", |_| {
        Some(UiMsg::Tokens(TokensMsg::CreateRequested))
    }),
    row("s", "show QR", |s| {
        s.tokens
            .last_minted_secret
            .is_some()
            .then_some(UiMsg::Tokens(TokensMsg::ShowQrForMinted))
    }),
    row("x", "revoke", |s| {
        (s.tokens.focused_record().is_some() && !s.tokens.loading)
            .then_some(UiMsg::Tokens(TokensMsg::RevokeRequested))
    }),
    row("r", "refresh", |s| {
        (!s.tokens.loading).then_some(UiMsg::Tokens(TokensMsg::Refresh))
    }),
];

static TOKEN_MINTED: [Shortcut; 3] = [
    row("s", "show QR", |_| {
        Some(UiMsg::Tokens(TokensMsg::MintedShowQr))
    }),
    row("c", "copy token", |_| {
        Some(UiMsg::Copy(CopyTarget::MintedToken))
    }),
    row("d", "done", |_| Some(UiMsg::Tokens(TokensMsg::MintedDone))),
];

static TOKEN_REVOKE: [Shortcut; 1] = [row("y", "revoke", |_| {
    Some(UiMsg::Tokens(TokensMsg::RevokeConfirmed))
})];

static CERT: [Shortcut; 4] = [
    row("h", "help", |_| Some(UiMsg::Cert(CertMsg::OpenHelp))),
    row("g", "regenerate", |s| {
        (!s.cert.loading).then_some(UiMsg::Cert(CertMsg::RegenerateRequested))
    }),
    row("r", "refresh", |_| Some(UiMsg::Cert(CertMsg::Refresh))),
    // "copy", not "copy fingerprint": the footer is cut at 80 columns, and
    // the longer label is what would be cut. The button names the fingerprint.
    row("c", "copy", |s| {
        s.cert
            .fingerprint
            .is_some()
            .then_some(UiMsg::Copy(CopyTarget::CertFingerprint))
    }),
];

static CERT_REGEN: [Shortcut; 1] = [row("y", "regenerate", |_| {
    Some(UiMsg::Cert(CertMsg::RegenerateConfirmed))
})];

static STOP: [Shortcut; 1] = [row("y", "stop", |_| {
    Some(UiMsg::Server(ServerMsg::StopConfirmed))
})];

static DEVICES: [Shortcut; 2] = [
    row("x", "remove", |s| {
        (s.devices.focused.is_some() && !s.devices.loading)
            .then_some(UiMsg::Devices(DevicesMsg::RemoveRequested))
    }),
    row("r", "refresh", |_| {
        Some(UiMsg::Devices(DevicesMsg::Refresh))
    }),
];

static DEVICE_REMOVE: [Shortcut; 1] = [row("y", "remove", |_| {
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

/// One footer pair: the key text and what it does.
pub type HintPair = (&'static str, &'static str);

/// The generic pairs every list-style dialog ends with. Esc comes first: the
/// footer is cut from the right, so Esc outlives Tab and Enter.
const GENERIC: [HintPair; 3] = [("Esc", "closes"), ("Tab", "moves"), ("Enter", "presses")];

/// The Tokens dialog omits Tab and Enter: every one of its actions has a
/// letter, and four letters plus the list keys leave no room for them.
const TOKENS_TAIL: [HintPair; 1] = [("Esc", "closes")];

/// The text-field dialogs' trailing pairs: Enter only submits while a field has
/// focus, and presses or toggles on every other control.
const FORM: [HintPair; 3] = [
    ("Esc", "cancels"),
    ("Tab", "moves"),
    ("Enter", "submits a field, presses a button"),
];

/// The footer's (key, label) pairs for the top dialog (`None` is the
/// dashboard): the dialog's own keys first, then its enabled letters in `state`,
/// then Esc, Tab and Enter. The footer is cut from the right, so a narrow
/// footer loses Enter, then Tab, before Esc, and those before the dialog's own.
/// Empty for `Qr` and `Wizard`, whose layers paint their own strips.
pub fn hint(id: Option<DialogId>, state: &AppState) -> Vec<HintPair> {
    let Some(id) = id else {
        return DASHBOARD.to_vec();
    };
    let (lead, tail): (&[HintPair], &[HintPair]) = match id {
        DialogId::Qr | DialogId::Wizard => return Vec::new(),
        DialogId::TokenCreate => (&[("Space", "toggles")], &FORM),
        DialogId::Config => (&[], &FORM),
        DialogId::TokenRevokeConfirm
        | DialogId::CertRegenConfirm
        | DialogId::StopServer
        | DialogId::DeviceRemoveConfirm => (&[], &[("Esc", "cancels")]),
        DialogId::CertHelp => (&[("↑↓ PgUp PgDn", "scroll")], &[("Esc", "closes")]),
        DialogId::Tokens => (&[("↑↓", "select")], &TOKENS_TAIL),
        DialogId::Devices => (&[("↑↓", "select")], &GENERIC),
        DialogId::TokenMinted | DialogId::Cert => (&[], &GENERIC),
    };
    let mut pairs = lead.to_vec();
    pairs.extend(
        shortcuts(id)
            .iter()
            .filter(|s| (s.build)(state).is_some())
            .map(|s| (s.key_label, s.label)),
    );
    pairs.extend_from_slice(tail);
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tokens::TokenRecord;

    #[test]
    fn every_key_is_a_lowercase_letter_and_unique_per_dialog() {
        for id in DialogId::ALL {
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
    fn c_copies_the_minted_token_and_the_fingerprint_only_when_there_is_one() {
        let mut state = AppState::new();
        assert!(matches!(
            message(DialogId::TokenMinted, 'c', &state),
            Some(UiMsg::Copy(CopyTarget::MintedToken))
        ));
        // No certificate on disk: the Copy button is disabled, so is the letter.
        assert!(message(DialogId::Cert, 'c', &state).is_none());
        state.cert.fingerprint = Some("ab".repeat(32));
        assert!(matches!(
            message(DialogId::Cert, 'c', &state),
            Some(UiMsg::Copy(CopyTarget::CertFingerprint))
        ));
        // Copying is read-only, so a load in flight does not disable it.
        state.cert.loading = true;
        assert!(message(DialogId::Cert, 'c', &state).is_some());
    }

    #[test]
    fn the_minted_and_certificate_hints_name_the_copy_letter() {
        let state = AppState::new();
        assert!(hint(Some(DialogId::TokenMinted), &state).contains(&("c", "copy token")));
        let mut state = AppState::new();
        state.cert.fingerprint = Some("ab".repeat(32));
        assert!(hint(Some(DialogId::Cert), &state).contains(&("c", "copy")));
        assert_eq!(shortcuts(DialogId::TokenMinted).len(), 3);
        assert_eq!(shortcuts(DialogId::Cert).len(), 4);
    }

    #[test]
    fn unbound_letters_send_nothing() {
        let state = AppState::new();
        assert!(message(DialogId::Tokens, 'z', &state).is_none());
        assert!(message(DialogId::Config, 'n', &state).is_none());
    }

    /// Every row's footer key is its letter, and the letter is a-z, so no
    /// input can reach an out-of-alphabet slice.
    #[test]
    fn a_rows_footer_key_is_its_letter() {
        for id in DialogId::ALL {
            for shortcut in shortcuts(id) {
                assert_eq!(shortcut.key_label, shortcut.key.to_string(), "{id:?}");
            }
        }
    }

    /// A state where every row of every dialog is enabled.
    fn all_enabled() -> AppState {
        let mut state = AppState::new();
        state.tokens.tokens.push(TokenRecord {
            name: "phone".into(),
            created_at: "now".into(),
            read_only: false,
        });
        state.tokens.focused = Some("phone".into());
        state.tokens.last_minted_secret = Some(("phone".into(), "s".into(), false));
        state.devices.focused = Some("pixel".into());
        state.cert.fingerprint = Some("ab".repeat(32));
        state
    }

    #[test]
    fn the_tokens_hint_drops_letters_whose_buttons_are_disabled() {
        let bare = AppState::new();
        let keys = |state: &AppState| -> Vec<&str> {
            hint(Some(DialogId::Tokens), state)
                .into_iter()
                .map(|(k, _)| k)
                .collect()
        };
        let keys_bare = keys(&bare);
        assert!(!keys_bare.contains(&"s") && !keys_bare.contains(&"x"));
        assert!(keys_bare.contains(&"n") && keys_bare.contains(&"r"));
        let full = keys(&all_enabled());
        assert!(full.contains(&"s") && full.contains(&"x"));
    }

    #[test]
    fn every_hint_fits_80_columns_with_all_rows_enabled() {
        let state = all_enabled();
        for id in DialogId::ALL {
            let pairs = hint(Some(id), &state);
            let width: usize = pairs
                .iter()
                .enumerate()
                .map(|(i, (k, l))| {
                    let sep = if i == 0 { 0 } else { 2 };
                    sep + k.chars().count() + 1 + l.chars().count()
                })
                .sum();
            assert!(width <= 80, "{id:?}: {width} columns");
        }
    }

    #[test]
    fn every_dialog_hint_orders_own_keys_letters_esc_tab_enter() {
        let state = all_enabled();
        for id in DialogId::ALL {
            let pairs = hint(Some(id), &state);
            if pairs.is_empty() {
                continue;
            }
            let pos = |key: &str| pairs.iter().position(|p| p.0 == key);
            let esc = pos("Esc").expect("every dialog hint has Esc");
            let last_letter = pairs.iter().rposition(|p| p.0.len() == 1);
            if !shortcuts(id).is_empty() {
                assert!(last_letter < Some(esc), "{id:?}: letters before Esc");
            }
            for later in ["Tab", "Enter"] {
                if let Some(i) = pos(later) {
                    assert!(esc < i, "{id:?}: Esc before {later}");
                }
            }
            if let (Some(t), Some(e)) = (pos("Tab"), pos("Enter")) {
                assert!(t < e, "{id:?}: Tab before Enter");
            }
            assert!(pairs[..esc].iter().all(|p| p.0 != "Tab" && p.0 != "Enter"));
        }
        let tokens = hint(Some(DialogId::Tokens), &state);
        assert!(tokens.iter().all(|p| p.0 != "Tab" && p.0 != "Enter"));
    }

    #[test]
    fn the_text_field_hints_name_tab_enter_and_esc() {
        let state = AppState::new();
        for id in [DialogId::TokenCreate, DialogId::Config] {
            let pairs = hint(Some(id), &state);
            assert!(pairs.contains(&("Tab", "moves")), "{id:?}");
            assert!(
                pairs
                    .iter()
                    .any(|p| p.0 == "Enter" && p.1.contains("submits a field")),
                "{id:?}"
            );
            assert!(pairs.contains(&("Esc", "cancels")), "{id:?}");
        }
    }

    #[test]
    fn every_dashboard_hint_key_does_something_on_a_fresh_state() {
        for (key, label) in DASHBOARD {
            let c = key.chars().next().expect("one letter");
            let mut state = AppState::new();
            let before = format!("{state:?}");
            let actions = crate::app::update(
                &mut state,
                crate::app::Message::Key(ratcn::runtime::KeyEvent::new(
                    ratcn::runtime::KeyCode::Char(c),
                )),
            );
            assert!(
                !actions.is_empty() || format!("{state:?}") != before,
                "{key} ({label}) is advertised and does nothing"
            );
        }
    }

    #[test]
    fn the_dashboard_hint_is_the_eight_keys_in_order() {
        let keys: Vec<_> = hint(None, &AppState::new())
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, ["s", "c", "e", "t", "d", "w", "r", "q"]);
    }

    #[test]
    fn a_dialog_hint_is_its_own_keys_then_the_generic_pairs() {
        let state = AppState::new();
        let pairs = hint(Some(DialogId::Tokens), &state);
        assert_eq!(pairs[..2], [("↑↓", "select"), ("n", "create")]);
        assert_eq!(pairs[pairs.len() - 1..], TOKENS_TAIL);
        let pairs = hint(Some(DialogId::Devices), &state);
        assert_eq!(pairs[pairs.len() - 3..], GENERIC);
        assert!(hint(Some(DialogId::Qr), &state).is_empty());
        assert!(hint(Some(DialogId::Wizard), &state).is_empty());
        assert_eq!(
            hint(Some(DialogId::StopServer), &state),
            [("y", "stop"), ("Esc", "cancels")]
        );
        assert_eq!(
            hint(Some(DialogId::CertHelp), &state),
            [("↑↓ PgUp PgDn", "scroll"), ("Esc", "closes")]
        );
    }
}
