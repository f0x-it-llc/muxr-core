//! The TEA update function: `(state, message) -> actions`.
//!
//! Pure with respect to I/O: it mutates [`AppState`] in place and returns the
//! side effects ([`UpdateAction`]s) the runner should perform. No ratatui,
//! terminal, or async code here — that keeps the `app/` layer unit-testable
//! without a terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::action::UpdateAction;
use super::message::Message;
use super::state::{
    AppState, ConfigField, QrOverlay, QrOverlayPhase, San, Screen, TokenExpiryChoice,
    TokensFormPhase,
};

/// Apply a [`Message`] to the [`AppState`], returning any side effects.
pub fn update(state: &mut AppState, message: Message) -> Vec<UpdateAction> {
    match message {
        Message::Key(key) => handle_key(state, key),
        Message::Tick => handle_tick(state),
        Message::Quit => {
            state.should_quit = true;
            vec![UpdateAction::Quit]
        }
        Message::NavTo(screen) => transition_to(state, screen),
        // ── Async task results ────────────────────────────────────────────────
        Message::StatusLoaded(info) => {
            let client_count = info.as_ref().map(|i| i.client_count);
            match info {
                Some(info) => {
                    state.server.status = Some(info);
                    state.server.stopped = false;
                }
                None => {
                    state.server.status = None;
                    state.server.stopped = true;
                }
            }
            state.server.loading = false;
            // Drive QR overlay connection detection — only while the overlay is
            // showing a QR. A rise in the attached-client count above the baseline
            // captured when the QR was generated promotes the overlay to Connected.
            if let Some(n) = client_count {
                check_overlay_connection(state, n);
            }
            Vec::new()
        }
        Message::ConfigLoaded(snapshot) => {
            state.config.apply_bind_addr(&snapshot.bind_addr);
            state.config.cert_dir = snapshot.cert_dir;
            state.config.reachable_ips = snapshot.reachable_ips;
            state.config.advertise_sans = snapshot.advertise_sans;
            if state.config.ip_cursor >= state.config.reachable_ips.len() {
                state.config.ip_cursor = 0;
            }
            state.config.loading = false;
            state.config.status = "Config loaded.".to_string();
            Vec::new()
        }
        Message::CertEnsured { fingerprint, sans } => {
            state.cert.fingerprint = fingerprint;
            state.cert.sans = sans;
            state.cert.loading = false;
            state.cert.status = "Certificate ready.".to_string();
            Vec::new()
        }
        // ── Dashboard cert-info (read-only passive load) ──────────────────────
        Message::CertInfoLoaded { fingerprint, sans } => {
            // Passive: populate cert fields without touching the status line.
            // (The status line is only set by `CertEnsured`, which signals active
            // regeneration. This path is a silent background read for the Dashboard.)
            state.cert.fingerprint = fingerprint.unwrap_or_default();
            state.cert.sans = sans;
            state.cert.loading = false;
            Vec::new()
        }
        Message::ActionFailed(msg) => {
            // Surface the error on the active screen's status line.
            match state.screen {
                Screen::Config => state.config.status = format!("Error: {msg}"),
                Screen::Cert => state.cert.status = format!("Error: {msg}"),
                Screen::Server => state.server.action_msg = format!("Error: {msg}"),
                Screen::Tokens => state.tokens.status = format!("Error: {msg}"),
                _ => {}
            }
            // Clear loading flags.
            state.config.loading = false;
            state.cert.loading = false;
            state.server.loading = false;
            state.tokens.loading = false;
            Vec::new()
        }
        Message::ActionOk(msg) => {
            match state.screen {
                Screen::Config => state.config.status = msg,
                Screen::Cert => state.cert.status = msg,
                Screen::Server => state.server.action_msg = msg,
                Screen::Tokens => state.tokens.status = msg,
                _ => {}
            }
            state.config.loading = false;
            state.cert.loading = false;
            state.server.loading = false;
            state.tokens.loading = false;
            Vec::new()
        }

        // ── Token screen messages ─────────────────────────────────────────────
        Message::TokensLoaded(records) => {
            state.tokens.tokens = records;
            // Clamp cursor to valid range.
            if state.tokens.cursor >= state.tokens.tokens.len() {
                state.tokens.cursor = state.tokens.tokens.len().saturating_sub(1);
            }
            state.tokens.loading = false;
            state.tokens.status = "Tokens loaded.".to_string();
            Vec::new()
        }
        Message::TokenCreated {
            token,
            name,
            read_only,
        } => {
            state.tokens.last_minted_secret = Some((name, token, read_only));
            state.tokens.loading = false;
            state.tokens.form_phase = TokensFormPhase::Browsing;
            state.tokens.form_name = String::new();
            // Trigger a refresh of the list.
            vec![UpdateAction::LoadTokens]
        }
        Message::TokensChanged => {
            state.tokens.loading = false;
            state.tokens.last_minted_secret = None;
            vec![UpdateAction::LoadTokens]
        }

        // ── Token QR overlay messages ─────────────────────────────────────────
        Message::TokenQrReady {
            uri,
            host,
            port,
            fingerprint_short,
            baseline_clients,
            seq,
        } => {
            // Accept only a current result whose seq matches the live overlay's
            // seq AND an overlay still exists. A stale result (overlay since
            // closed or superseded) is simply ignored — NOTHING was minted, so
            // there is no token to revoke.
            if let Some(overlay) = state.qr_overlay.as_mut()
                && overlay.seq == seq
            {
                overlay.baseline_clients = baseline_clients;
                overlay.phase = QrOverlayPhase::Showing {
                    uri,
                    host,
                    port,
                    fingerprint_short,
                };
            }
            Vec::new()
        }
        Message::TokenQrFailed { err, seq } => {
            if let Some(overlay) = state.qr_overlay.as_mut()
                && overlay.seq == seq
            {
                overlay.phase = QrOverlayPhase::Failed { err };
            }
            Vec::new()
        }
    }
}

/// Transition `state.screen` to `next`, running any enter side effects.
///
/// Used by every navigation path (NavTo, Tab/BackTab, arrow nav) so the
/// enter-side data loads are centralised and can never be skipped by one path.
fn transition_to(state: &mut AppState, next: Screen) -> Vec<UpdateAction> {
    let actions = on_enter_screen(state, next);
    state.screen = next;
    actions
}

/// Actions to dispatch when entering a screen (load initial data).
fn on_enter_screen(state: &mut AppState, screen: Screen) -> Vec<UpdateAction> {
    match screen {
        Screen::Config => {
            state.config.loading = true;
            state.config.status = String::new();
            vec![UpdateAction::LoadConfig]
        }
        Screen::Server => {
            state.server.loading = true;
            state.server.action_msg = String::new();
            state.server.tick_counter = 0;
            vec![UpdateAction::RefreshStatus]
        }
        Screen::Tokens => {
            state.tokens.loading = true;
            state.tokens.status = String::new();
            state.tokens.last_minted_secret = None;
            state.tokens.form_phase = TokensFormPhase::Browsing;
            state.tokens.form_name = String::new();
            vec![UpdateAction::LoadTokens]
        }
        Screen::Cert => {
            // Load config + reachable IPs so that `build_sans_from_config` has
            // real interface addresses even when the user navigates directly to
            // the Cert screen without visiting Config first.
            //
            // DECISION: we reuse `LoadConfig` (which returns `reachable_ips` via
            // `ConfigLoaded`) rather than adding a focused `LoadReachableIps`
            // action/message.  The only side-effect beyond populating IPs is that
            // `state.config.status` is overwritten with "Config loaded." and
            // `state.config.loading` is set briefly.  Because the Cert screen
            // does not render `config.status`, this is invisible to the user and
            // causes no perceptible churn.  Avoiding a new action + message keeps
            // the spine surface minimal.
            state.config.loading = true;
            vec![UpdateAction::LoadConfig]
        }
        Screen::Dashboard => {
            // Load a fresh at-a-glance snapshot of all sub-systems.
            // Flags are set so individual sub-states show their loading indicators
            // until results arrive.  Re-entering the Dashboard re-fetches
            // everything (idempotent by design).
            state.server.loading = true;
            state.config.loading = true;
            state.tokens.loading = true;
            state.cert.loading = true;
            vec![
                UpdateAction::RefreshStatus,
                UpdateAction::LoadConfig,
                UpdateAction::LoadTokens,
                UpdateAction::LoadCertInfo,
            ]
        }
    }
}

/// Tick handler — drives the live status poll for the Server/Dashboard screens and
/// the QR overlay's connection detection.
///
/// All three pollers share the single `server.loading` in-flight guard so at most
/// one `RefreshStatus` is dispatched per ~1 s window even when the overlay is up
/// over a polled screen — avoiding redundant concurrent `status()` reads.
fn handle_tick(state: &mut AppState) -> Vec<UpdateAction> {
    let mut actions = Vec::new();

    // Live status poll on the screens that show daemon status. Emit RefreshStatus
    // roughly every 20 ticks (~1 s at 50 ms/tick), guarded so only one query is in
    // flight at a time.
    if matches!(state.screen, Screen::Server | Screen::Dashboard) && !state.server.loading {
        state.server.tick_counter = state.server.tick_counter.wrapping_add(1);
        if state.server.tick_counter.is_multiple_of(20) {
            state.server.loading = true;
            actions.push(UpdateAction::RefreshStatus);
        }
    }

    // Poll status while the QR overlay is showing a code so we can detect when a
    // client connects. The overlay can be up over any screen; it shares the same
    // `server.loading` single-flight guard as the screen poll above so the two
    // never spawn concurrent status reads.
    if !state.server.loading
        && let Some(overlay) = state.qr_overlay.as_mut()
        && matches!(overlay.phase, QrOverlayPhase::Showing { .. })
    {
        overlay.tick_counter = overlay.tick_counter.wrapping_add(1);
        if overlay.tick_counter.is_multiple_of(20) {
            state.server.loading = true;
            actions.push(UpdateAction::RefreshStatus);
        }
    }

    actions
}

/// Promote the QR overlay to `Connected` if a new client appeared.
///
/// Called from the `StatusLoaded` handler — only while the overlay exists and its
/// phase is `Showing`. The rise is a heuristic (the attached-client count went
/// up), not verified per-token auth.
pub(crate) fn check_overlay_connection(state: &mut AppState, current_clients: usize) {
    if let Some(overlay) = state.qr_overlay.as_mut()
        && let QrOverlayPhase::Showing { .. } = &overlay.phase
        && current_clients > overlay.baseline_clients
    {
        overlay.phase = QrOverlayPhase::Connected;
    }
}

/// Translate a raw key event into state changes / actions.
fn handle_key(state: &mut AppState, key: KeyEvent) -> Vec<UpdateAction> {
    // Ctrl-C always quits.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.should_quit = true;
        return vec![UpdateAction::Quit];
    }

    // Overlay key interception: while the token QR overlay is up it captures all
    // input. `Esc` / `q` close it (NO revoke — the token is a real user token);
    // `Tab` / `BackTab` are swallowed (no screen nav behind the overlay);
    // everything else is a no-op. Returns early so no per-screen handler runs.
    // (`Ctrl-C` is handled above and remains the always-available quit.)
    if state.qr_overlay.is_some() {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            state.qr_overlay = None;
        }
        return Vec::new();
    }

    // `q` quits from any screen — UNLESS a text field is being edited (Config
    // host/port or the Tokens create-name form), where `q` is a literal char.
    // `Ctrl-C` (handled above) remains the always-available quit while editing.
    if key.code == KeyCode::Char('q') && !state.is_text_editing() {
        state.should_quit = true;
        return vec![UpdateAction::Quit];
    }

    // Screen-level Tab / BackTab navigation (always available).
    match key.code {
        KeyCode::Tab => {
            let next = state.screen.next();
            return transition_to(state, next);
        }
        KeyCode::BackTab => {
            let prev = state.screen.prev();
            return transition_to(state, prev);
        }
        _ => {}
    }

    // Per-screen key handling.
    match state.screen {
        Screen::Config => handle_config_key(state, key),
        Screen::Cert => handle_cert_key(state, key),
        Screen::Server => handle_server_key(state, key),
        Screen::Tokens => handle_tokens_key(state, key),
        Screen::Dashboard => handle_nav_keys(state, key),
    }
}

/// Navigation-only key handler for the Dashboard screen.
fn handle_nav_keys(state: &mut AppState, key: KeyEvent) -> Vec<UpdateAction> {
    match key.code {
        KeyCode::Right | KeyCode::Down => {
            let next = state.screen.next();
            transition_to(state, next)
        }
        KeyCode::Left | KeyCode::Up => {
            let prev = state.screen.prev();
            transition_to(state, prev)
        }
        _ => Vec::new(),
    }
}

/// Key handler for the Config screen.
///
/// Fields: host (text) → port (text) → IP picker (list).
/// - Tab / Down / Right: move to next field.
/// - BackTab / Up / Left: move to prev field (except in picker where Up scrolls).
/// - In IpPicker: Up/Down scroll, Enter selects IP into host.
///
/// While editing the **Host** or **Port** text fields, printable keys (including
/// `s`/`r`/`q`) are typed literally; save/reload move to `Ctrl-S` / `Ctrl-R` so
/// hostnames like `server.local` can be entered. In the **IpPicker** (a list,
/// not a text field), the bare `s` / `r` shortcuts still apply.
fn handle_config_key(state: &mut AppState, key: KeyEvent) -> Vec<UpdateAction> {
    // While editing a text field, Ctrl-S saves and Ctrl-R reloads.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(state.config.focused, ConfigField::Host | ConfigField::Port)
    {
        match key.code {
            KeyCode::Char('s') => return config_save(state),
            KeyCode::Char('r') => return config_reload(state),
            _ => {}
        }
    }

    match state.config.focused {
        ConfigField::Host => match key.code {
            KeyCode::Tab | KeyCode::Down => {
                state.config.focused = ConfigField::Port;
                Vec::new()
            }
            KeyCode::Up | KeyCode::BackTab => Vec::new(),
            KeyCode::Enter => config_save(state),
            KeyCode::Backspace => {
                state.config.host.pop();
                Vec::new()
            }
            KeyCode::Char(c) => {
                state.config.host.push(c);
                Vec::new()
            }
            _ => Vec::new(),
        },
        ConfigField::Port => match key.code {
            KeyCode::Tab | KeyCode::Down => {
                if !state.config.reachable_ips.is_empty() {
                    state.config.focused = ConfigField::IpPicker;
                }
                Vec::new()
            }
            KeyCode::Up | KeyCode::BackTab => {
                state.config.focused = ConfigField::Host;
                Vec::new()
            }
            KeyCode::Enter => config_save(state),
            KeyCode::Backspace => {
                state.config.port.pop();
                Vec::new()
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                state.config.port.push(c);
                Vec::new()
            }
            _ => Vec::new(),
        },
        ConfigField::IpPicker => match key.code {
            KeyCode::Down => {
                if !state.config.reachable_ips.is_empty() {
                    state.config.ip_cursor =
                        (state.config.ip_cursor + 1) % state.config.reachable_ips.len();
                }
                Vec::new()
            }
            KeyCode::Up => {
                if !state.config.reachable_ips.is_empty() {
                    let len = state.config.reachable_ips.len();
                    state.config.ip_cursor = (state.config.ip_cursor + len - 1) % len;
                }
                Vec::new()
            }
            KeyCode::Tab => {
                state.config.focused = ConfigField::Host;
                Vec::new()
            }
            KeyCode::BackTab => {
                state.config.focused = ConfigField::Port;
                Vec::new()
            }
            KeyCode::Enter => {
                // Select the highlighted IP into the host field.
                if let Some(ip) = state.config.reachable_ips.get(state.config.ip_cursor) {
                    state.config.host = ip.to_string();
                    state.config.focused = ConfigField::Port;
                }
                Vec::new()
            }
            KeyCode::Char('s') => config_save(state),
            KeyCode::Char('r') => config_reload(state),
            _ => Vec::new(),
        },
    }
}

/// Validate the current host/port and dispatch a `SaveBind`, or set a validation
/// error on the status line instead of persisting garbage.
fn config_save(state: &mut AppState) -> Vec<UpdateAction> {
    if state.config.loading {
        return Vec::new();
    }
    let host = state.config.host.trim();
    if host.is_empty() {
        state.config.status = "Error: bind host must not be empty.".to_string();
        return Vec::new();
    }
    if state.config.port.trim().parse::<u16>().is_err() {
        state.config.status = "Error: port must be a number in 1..=65535.".to_string();
        return Vec::new();
    }
    let addr = state.config.bind_addr();
    state.config.loading = true;
    state.config.status = "Saving…".to_string();
    vec![UpdateAction::SaveBind(addr)]
}

/// Dispatch a config reload.
fn config_reload(state: &mut AppState) -> Vec<UpdateAction> {
    if state.config.loading {
        return Vec::new();
    }
    state.config.loading = true;
    state.config.status = "Loading…".to_string();
    vec![UpdateAction::LoadConfig]
}

/// Key handler for the Cert screen.
///
/// - `g`: ensure / regenerate cert using SANs from the config host.
/// - `r`: reload fingerprint only.
/// - `t`: cycle the `advertise_trust` override (Auto → CA → Pin → Auto).
fn handle_cert_key(state: &mut AppState, key: KeyEvent) -> Vec<UpdateAction> {
    match key.code {
        KeyCode::Char('g') => {
            if !state.cert.loading {
                state.cert.loading = true;
                state.cert.status = "Generating cert…".to_string();
                // Derive SANs from whatever IP/host is in the config screen.
                let sans = build_sans_from_config(state);
                vec![UpdateAction::EnsureCert(sans)]
            } else {
                Vec::new()
            }
        }
        KeyCode::Char('r') => {
            if !state.cert.loading {
                state.cert.loading = true;
                state.cert.status = "Refreshing…".to_string();
                let sans = build_sans_from_config(state);
                vec![UpdateAction::EnsureCert(sans)]
            } else {
                Vec::new()
            }
        }
        // Cycle the advertise_trust override: Auto → CA → Pin → Auto.
        // Persists the new value through a synchronous runner action so the
        // setting survives restarts (Finding 3 — advertise_trust not persisted).
        KeyCode::Char('t') => {
            state.cert.advertise_trust = state.cert.advertise_trust.cycle();
            vec![UpdateAction::SaveAdvertiseTrust(state.cert.advertise_trust)]
        }
        _ => Vec::new(),
    }
}

/// Key handler for the Server panel screen.
///
/// - `s`: start daemon.
/// - `x`: stop daemon.
/// - `r`: refresh status.
fn handle_server_key(state: &mut AppState, key: KeyEvent) -> Vec<UpdateAction> {
    match key.code {
        KeyCode::Char('s') => {
            if !state.server.loading {
                state.server.loading = true;
                state.server.stopped = false;
                state.server.action_msg = "Starting server…".to_string();
                vec![UpdateAction::StartServer]
            } else {
                Vec::new()
            }
        }
        KeyCode::Char('x') => {
            if !state.server.loading {
                state.server.loading = true;
                state.server.action_msg = "Stopping server…".to_string();
                vec![UpdateAction::StopServer]
            } else {
                Vec::new()
            }
        }
        KeyCode::Char('r') => {
            if !state.server.loading {
                state.server.loading = true;
                state.server.action_msg = String::new();
                vec![UpdateAction::RefreshStatus]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Key handler for the Tokens screen.
///
/// Browsing mode:
/// - `j`/`Down`: move cursor down.
/// - `k`/`Up`: move cursor up.
/// - `c`: open the create form.
/// - `d`/`x`: revoke the selected token.
/// - `r`: reload list.
/// - `Enter`: open the QR overlay for the just-minted token (if one is held).
/// - `Esc`: clear the minted secret.
///
/// Creating mode (a text-editing field — see [`AppState::is_text_editing`]):
/// - Char input (including `c`, `q`, Space): type the token name literally.
/// - `Backspace`: delete last char.
/// - `Ctrl-Space`: toggle read-only.
/// - `Enter`: submit the create request.
/// - `Esc`: cancel and return to browsing.
fn handle_tokens_key(state: &mut AppState, key: KeyEvent) -> Vec<UpdateAction> {
    match state.tokens.form_phase {
        TokensFormPhase::Browsing => match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if !state.tokens.tokens.is_empty() {
                    state.tokens.cursor = (state.tokens.cursor + 1) % state.tokens.tokens.len();
                }
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !state.tokens.tokens.is_empty() {
                    let len = state.tokens.tokens.len();
                    state.tokens.cursor = (state.tokens.cursor + len - 1) % len;
                }
                Vec::new()
            }
            KeyCode::Char('c') => {
                state.tokens.form_phase = TokensFormPhase::Creating;
                state.tokens.form_name = String::new();
                state.tokens.form_read_only = false;
                state.tokens.form_expiry = TokenExpiryChoice::default();
                Vec::new()
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                if !state.tokens.loading
                    && let Some(name) = state.tokens.selected_name().map(str::to_string)
                {
                    state.tokens.loading = true;
                    state.tokens.status = format!("Revoking '{name}'…");
                    state.tokens.last_minted_secret = None;
                    return vec![UpdateAction::RevokeToken(name)];
                }
                Vec::new()
            }
            KeyCode::Char('r') => {
                if !state.tokens.loading {
                    state.tokens.loading = true;
                    state.tokens.status = "Refreshing…".to_string();
                    vec![UpdateAction::LoadTokens]
                } else {
                    Vec::new()
                }
            }
            KeyCode::Enter => {
                // Open the QR overlay for the freshly-minted token — the only one
                // whose plaintext we still hold. The shown token is a real user
                // token; it is NEVER revoked when the overlay closes. If nothing
                // was just minted, this is a no-op (with a status hint).
                if let Some((name, secret, read_only)) = state.tokens.last_minted_secret.clone() {
                    state.qr_seq = state.qr_seq.wrapping_add(1);
                    let seq = state.qr_seq;
                    // Snapshot the advertise_trust at the time of QR request so the
                    // async build task uses the value active when Enter was pressed.
                    let advertise_trust = state.cert.advertise_trust;
                    state.qr_overlay = Some(QrOverlay {
                        phase: QrOverlayPhase::Generating,
                        seq,
                        baseline_clients: 0,
                        token_name: name,
                        read_only,
                        tick_counter: 0,
                    });
                    vec![UpdateAction::ShowTokenQr {
                        token: secret,
                        read_only,
                        seq,
                        advertise_trust,
                    }]
                } else {
                    state.tokens.status =
                        "Create a token first, then press Enter to show its QR.".to_string();
                    Vec::new()
                }
            }
            KeyCode::Esc => {
                state.tokens.last_minted_secret = None;
                Vec::new()
            }
            _ => Vec::new(),
        },
        TokensFormPhase::Creating => match key.code {
            KeyCode::Esc => {
                state.tokens.form_phase = TokensFormPhase::Browsing;
                state.tokens.form_name = String::new();
                Vec::new()
            }
            // Enter submits. (Note: `c` is NOT a submit shortcut here — it is a
            // literal character so token names may contain it.)
            KeyCode::Enter => {
                if !state.tokens.loading {
                    let name = state.tokens.form_name.trim().to_string();
                    let name_opt = if name.is_empty() { None } else { Some(name) };
                    let read_only = state.tokens.form_read_only;
                    let expiry_secs = state.tokens.form_expiry.ttl_secs();
                    state.tokens.loading = true;
                    state.tokens.status = "Creating token…".to_string();
                    vec![UpdateAction::CreateToken {
                        name: name_opt,
                        read_only,
                        expiry_secs,
                    }]
                } else {
                    Vec::new()
                }
            }
            // Ctrl-Space toggles read-only while editing (a bare Space is a
            // literal character in the name field).
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.tokens.form_read_only = !state.tokens.form_read_only;
                Vec::new()
            }
            // Ctrl-E cycles the expiry choice (never → 30m → 1h → 24h → 7d → …).
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                state.tokens.form_expiry = state.tokens.form_expiry.next();
                Vec::new()
            }
            KeyCode::Backspace => {
                state.tokens.form_name.pop();
                Vec::new()
            }
            KeyCode::Char(c) => {
                state.tokens.form_name.push(c);
                Vec::new()
            }
            _ => Vec::new(),
        },
    }
}

/// Build a SAN list from the reachable IPs discovered for the Config screen.
///
/// Returns app-layer [`San`] mirrors (the `server/` facade converts them into
/// infra `SanEntry`).
///
/// ## SAN derivation rules
///
/// 1. Each IP in `state.config.reachable_ips` is included as `San::Ip`, **except**
///    unspecified addresses (`0.0.0.0`, `::`) — `reachable_ipv4` should never
///    return them, but we guard here as belt-and-suspenders.
/// 2. If the configured bind host is itself a concrete (non-empty, non-unspecified)
///    IP or DNS name, it is also included so that a user who has pinned a specific
///    LAN IP in Config gets a cert valid for that address.
/// 3. Advertise SANs from the `ZELLIMSERVER_SAN` env (loaded via `ConfigLoaded`)
///    are merged in — these cover externally-advertised addresses that are not
///    local interfaces (e.g. a tailnet IP behind a container's NAT publish).
/// 4. De-duplication preserves first-seen order.
///
/// When the bind host is `0.0.0.0` (wildcard) — the common tailnet scenario — it
/// is **omitted** as a SAN (a wildcard SAN is meaningless to TLS clients). Only the
/// real interface IPs from `reachable_ips` are added.
fn build_sans_from_config(state: &AppState) -> Vec<San> {
    let mut seen = std::collections::HashSet::new();
    let mut sans: Vec<San> = Vec::new();

    // 1. Add each reachable IP (already filtered for loopback/link-local by
    //    pairing::net::reachable_ipv4), guarding against unspecified here too.
    for ip in &state.config.reachable_ips {
        if ip.is_unspecified() {
            continue;
        }
        let key = ip.to_string();
        if seen.insert(key.clone()) {
            sans.push(San::Ip(key));
        }
    }

    // 2. Include the bind host if it is a concrete (non-empty, non-unspecified)
    //    IP or DNS name.  This covers the case where the user configured a
    //    specific LAN IP directly in the Config host field.
    let host = state.config.host.trim();
    if !host.is_empty() {
        let is_unspecified = host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_unspecified())
            .unwrap_or(false); // DNS names are never "unspecified"
        if !is_unspecified && seen.insert(host.to_string()) {
            sans.push(San::from_host(host));
        }
    }

    // 3. Merge advertise SANs from the `ZELLIMSERVER_SAN` env (loaded via
    //    `ConfigLoaded`). These cover externally-advertised addresses that are
    //    NOT discoverable as local interfaces — e.g. a tailnet IP that reaches
    //    the server through a host-side NAT publish inside a container. Without
    //    this, a TUI-generated cert would miss the address the phone dials, even
    //    though the daemon's `collect_sans` honours the same env var.
    for entry in &state.config.advertise_sans {
        let val = entry.trim();
        if val.is_empty() {
            continue;
        }
        let is_unspecified = val
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_unspecified())
            .unwrap_or(false);
        if !is_unspecified && seen.insert(val.to_string()) {
            sans.push(San::from_host(val));
        }
    }

    sans
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::Screen;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn q_sets_should_quit() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('q'))));
        assert!(state.should_quit);
        assert!(matches!(actions.as_slice(), [UpdateAction::Quit]));
    }

    #[test]
    fn ctrl_c_quits() {
        let mut state = AppState::new();
        let ev = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let actions = update(&mut state, Message::Key(ev));
        assert!(state.should_quit);
        assert!(matches!(actions.as_slice(), [UpdateAction::Quit]));
    }

    #[test]
    fn quit_message_sets_flag() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Quit);
        assert!(state.should_quit);
        assert!(matches!(actions.as_slice(), [UpdateAction::Quit]));
    }

    #[test]
    fn tab_advances_screen() {
        let mut state = AppState::new();
        assert_eq!(state.screen, Screen::Dashboard);
        // Tab from Dashboard → Config triggers LoadConfig.
        let actions = update(&mut state, Message::Key(key(KeyCode::Tab)));
        assert_eq!(state.screen, Screen::Config);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadConfig))
        );
    }

    #[test]
    fn backtab_goes_back() {
        let mut state = AppState::new();
        update(&mut state, Message::Key(key(KeyCode::BackTab)));
        assert_eq!(state.screen, Screen::Server);
    }

    #[test]
    fn nav_to_jumps_directly() {
        let mut state = AppState::new();
        update(&mut state, Message::NavTo(Screen::Tokens));
        assert_eq!(state.screen, Screen::Tokens);
    }

    #[test]
    fn tick_is_noop_on_non_server_screen() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::Tick);
        assert!(actions.is_empty());
        assert!(!state.should_quit);
    }

    #[test]
    fn tick_triggers_refresh_on_server_screen_after_20() {
        let mut state = AppState::new();
        state.screen = Screen::Server;
        // Drive 19 ticks — no refresh yet.
        for _ in 0..19 {
            let actions = update(&mut state, Message::Tick);
            assert!(actions.is_empty());
        }
        // 20th tick → RefreshStatus.
        let actions = update(&mut state, Message::Tick);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RefreshStatus))
        );
    }

    #[test]
    fn tick_triggers_refresh_on_dashboard_screen_after_20() {
        // Mirror of tick_triggers_refresh_on_server_screen_after_20 but for
        // Screen::Dashboard — confirms daemon-status live polling on the overview.
        let mut state = AppState::new();
        state.screen = Screen::Dashboard;
        state.server.loading = false;
        // Drive 19 ticks — no refresh yet.
        for _ in 0..19 {
            let actions = update(&mut state, Message::Tick);
            assert!(
                actions.is_empty(),
                "expected no action before tick 20; got: {actions:?}"
            );
        }
        // 20th tick → RefreshStatus.
        let actions = update(&mut state, Message::Tick);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RefreshStatus)),
            "expected RefreshStatus on 20th tick; got: {actions:?}"
        );
    }

    #[test]
    fn overlay_and_dashboard_poll_coalesce_to_single_refresh() {
        // Regression: when the QR overlay is `Showing` over the Dashboard screen,
        // the screen poll and the overlay poll share the `server.loading`
        // single-flight guard, so at most ONE RefreshStatus is dispatched per
        // ~1 s window (no redundant concurrent status() reads).
        let mut state = AppState::new();
        state.screen = Screen::Dashboard;
        state.server.loading = false;
        show_overlay(&mut state, 0);
        // The overlay's tick_counter starts at 0; align both pollers' cadence.
        state.server.tick_counter = 0;
        state.qr_overlay.as_mut().unwrap().tick_counter = 0;

        let mut total_refreshes = 0;
        for _ in 0..20 {
            let actions = update(&mut state, Message::Tick);
            total_refreshes += actions
                .iter()
                .filter(|a| matches!(a, UpdateAction::RefreshStatus))
                .count();
            // Clear the in-flight flag the way StatusLoaded would, so the next
            // window can poll again — but within a single tick only one fires.
            state.server.loading = false;
        }
        assert_eq!(
            total_refreshes, 1,
            "exactly one RefreshStatus expected across the shared 20-tick window; got {total_refreshes}"
        );
    }

    #[test]
    fn unhandled_key_kind_still_navigates_on_press() {
        let mut ev = key(KeyCode::Tab);
        ev.kind = KeyEventKind::Press;
        let mut state = AppState::new();
        update(&mut state, Message::Key(ev));
        assert_eq!(state.screen, Screen::Config);
    }

    #[test]
    fn server_s_key_dispatches_start() {
        let mut state = AppState::new();
        state.screen = Screen::Server;
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('s'))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::StartServer))
        );
    }

    #[test]
    fn server_s_key_clears_stopped_flag() {
        // Regression: pressing `s` (StartServer) while the daemon is known-stopped
        // must clear `stopped` so the renderer doesn't hold a stale "Stopped" panel
        // for the one cycle before the first StatusLoaded(Some(_)) arrives.
        let mut state = AppState::new();
        state.screen = Screen::Server;
        state.server.stopped = true;
        state.server.status = None;
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('s'))));
        assert!(
            !state.server.stopped,
            "stopped should be cleared on StartServer"
        );
        assert!(state.server.loading);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::StartServer))
        );
    }

    #[test]
    fn server_x_key_dispatches_stop() {
        let mut state = AppState::new();
        state.screen = Screen::Server;
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('x'))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::StopServer))
        );
    }

    #[test]
    fn server_r_key_dispatches_refresh() {
        let mut state = AppState::new();
        state.screen = Screen::Server;
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('r'))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RefreshStatus))
        );
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn config_ctrl_s_dispatches_save_while_editing() {
        let mut state = AppState::new();
        state.screen = Screen::Config;
        state.config.host = "127.0.0.1".to_string();
        state.config.port = "50051".to_string();
        // Focused on Host (a text field) → Ctrl-S saves.
        let actions = update(&mut state, Message::Key(ctrl(KeyCode::Char('s'))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::SaveBind(_)))
        );
    }

    #[test]
    fn config_enter_dispatches_save_while_editing() {
        let mut state = AppState::new();
        state.screen = Screen::Config;
        state.config.host = "10.0.0.5".to_string();
        state.config.port = "50051".to_string();
        let actions = update(&mut state, Message::Key(key(KeyCode::Enter)));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::SaveBind(addr) if addr == "10.0.0.5:50051"))
        );
    }

    #[test]
    fn config_host_field_can_type_a_dns_name_and_save() {
        // Regression for review Major #3: 's'/'r'/'q' must be literal characters
        // while editing the host field, so a name like "server.local" can be typed.
        let mut state = AppState::new();
        state.screen = Screen::Config;
        state.config.host = String::new();
        state.config.port = "50051".to_string();
        for c in "server.local".chars() {
            let actions = update(&mut state, Message::Key(key(KeyCode::Char(c))));
            // None of these keystrokes should trigger a side effect.
            assert!(actions.is_empty(), "char {c:?} produced an action");
            assert!(!state.should_quit, "char {c:?} quit the app");
        }
        assert_eq!(state.config.host, "server.local");
        // Now save and confirm the full address is dispatched.
        let actions = update(&mut state, Message::Key(ctrl(KeyCode::Char('s'))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::SaveBind(addr) if addr == "server.local:50051"))
        );
    }

    #[test]
    fn config_save_rejects_empty_host() {
        let mut state = AppState::new();
        state.screen = Screen::Config;
        state.config.host = String::new();
        state.config.port = "50051".to_string();
        let actions = update(&mut state, Message::Key(ctrl(KeyCode::Char('s'))));
        assert!(actions.is_empty());
        assert!(state.config.status.contains("Error"));
    }

    #[test]
    fn config_save_rejects_bad_port() {
        let mut state = AppState::new();
        state.screen = Screen::Config;
        state.config.host = "127.0.0.1".to_string();
        state.config.port = "not-a-port".to_string();
        let actions = update(&mut state, Message::Key(key(KeyCode::Enter)));
        assert!(actions.is_empty());
        assert!(state.config.status.contains("Error"));
    }

    #[test]
    fn q_is_literal_in_config_host_field() {
        let mut state = AppState::new();
        state.screen = Screen::Config;
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('q'))));
        assert!(!state.should_quit);
        assert!(actions.is_empty());
        assert_eq!(state.config.host, "q");
    }

    #[test]
    fn q_is_literal_in_tokens_create_form() {
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        state.tokens.form_phase = TokensFormPhase::Creating;
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('q'))));
        assert!(!state.should_quit);
        assert!(actions.is_empty());
        assert_eq!(state.tokens.form_name, "q");
    }

    #[test]
    fn cert_t_key_cycles_advertise_trust() {
        use crate::app::state::AdvertiseTrust;
        let mut state = AppState::new();
        state.screen = Screen::Cert;
        // Default is Auto.
        assert_eq!(state.cert.advertise_trust, AdvertiseTrust::Auto);
        // First `t` → Ca.
        update(&mut state, Message::Key(key(KeyCode::Char('t'))));
        assert_eq!(state.cert.advertise_trust, AdvertiseTrust::Ca);
        // Second `t` → Pin.
        update(&mut state, Message::Key(key(KeyCode::Char('t'))));
        assert_eq!(state.cert.advertise_trust, AdvertiseTrust::Pin);
        // Third `t` → wraps back to Auto.
        update(&mut state, Message::Key(key(KeyCode::Char('t'))));
        assert_eq!(state.cert.advertise_trust, AdvertiseTrust::Auto);
    }

    #[test]
    fn cert_t_key_emits_save_advertise_trust_action() {
        // Finding 3: the `t` toggle must emit a SaveAdvertiseTrust action so the
        // runner can persist the new value.  This tests the action is present and
        // carries the newly cycled value (Auto → Ca on the first press).
        use crate::app::state::AdvertiseTrust;
        let mut state = AppState::new();
        state.screen = Screen::Cert;
        assert_eq!(state.cert.advertise_trust, AdvertiseTrust::Auto);
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('t'))));
        // State cycles to Ca.
        assert_eq!(state.cert.advertise_trust, AdvertiseTrust::Ca);
        // A SaveAdvertiseTrust(Ca) action is emitted.
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::SaveAdvertiseTrust(AdvertiseTrust::Ca))),
            "expected SaveAdvertiseTrust(Ca) in actions; got: {actions:?}"
        );
    }

    #[test]
    fn advertise_trust_persist_str_round_trips() {
        // Finding 3: every AdvertiseTrust variant must round-trip through its
        // persist string so the saved file is always parseable back to the same
        // variant.
        use crate::app::state::AdvertiseTrust;
        for variant in [
            AdvertiseTrust::Auto,
            AdvertiseTrust::Ca,
            AdvertiseTrust::Pin,
        ] {
            let s = variant.persist_str();
            let back = AdvertiseTrust::from_persist_str(s);
            assert_eq!(
                back, variant,
                "persist_str round-trip failed for {variant:?}: stored {s:?}, got {back:?}"
            );
        }
    }

    #[test]
    fn advertise_trust_from_persist_str_unknown_defaults_to_auto() {
        // Unknown/corrupt values must default to Auto, not panic.
        use crate::app::state::AdvertiseTrust;
        assert_eq!(
            AdvertiseTrust::from_persist_str("garbage"),
            AdvertiseTrust::Auto
        );
        assert_eq!(AdvertiseTrust::from_persist_str(""), AdvertiseTrust::Auto);
    }

    #[test]
    fn cert_g_key_dispatches_ensure_cert() {
        let mut state = AppState::new();
        state.screen = Screen::Cert;
        state.config.host = "192.168.1.5".to_string();
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('g'))));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::EnsureCert(_)))
        );
    }

    /// Build a test `ServerInfo` with the given client count.
    fn server_info(client_count: usize) -> crate::app::state::ServerInfo {
        crate::app::state::ServerInfo {
            version: "0.1.0".to_string(),
            bind_addr: "127.0.0.1:50051".to_string(),
            pid: 1234,
            uptime_secs: 42,
            client_count,
        }
    }

    #[test]
    fn status_loaded_running_updates_server_state() {
        let mut state = AppState::new();
        update(&mut state, Message::StatusLoaded(Some(server_info(2))));
        assert!(state.server.status.is_some());
        assert!(!state.server.stopped);
        assert_eq!(state.server.status.unwrap().client_count, 2);
    }

    #[test]
    fn status_loaded_none_marks_stopped() {
        let mut state = AppState::new();
        state.server.status = Some(server_info(1));
        update(&mut state, Message::StatusLoaded(None));
        assert!(state.server.status.is_none());
        assert!(state.server.stopped);
    }

    #[test]
    fn config_loaded_populates_fields() {
        use crate::app::message::ConfigSnapshot;
        let mut state = AppState::new();
        let snap = ConfigSnapshot {
            bind_addr: "0.0.0.0:9090".to_string(),
            cert_dir: "/tmp/certs".to_string(),
            reachable_ips: vec![],
            advertise_sans: vec![],
        };
        update(&mut state, Message::ConfigLoaded(snap));
        assert_eq!(state.config.host, "0.0.0.0");
        assert_eq!(state.config.port, "9090");
        assert_eq!(state.config.cert_dir, "/tmp/certs");
    }

    #[test]
    fn cert_ensured_updates_cert_state() {
        let mut state = AppState::new();
        update(
            &mut state,
            Message::CertEnsured {
                fingerprint: "ab:cd".to_string(),
                sans: vec!["127.0.0.1".to_string()],
            },
        );
        assert_eq!(state.cert.fingerprint, "ab:cd");
        assert_eq!(state.cert.sans, vec!["127.0.0.1".to_string()]);
    }

    #[test]
    fn action_failed_sets_status_on_config_screen() {
        let mut state = AppState::new();
        state.screen = Screen::Config;
        update(&mut state, Message::ActionFailed("disk full".to_string()));
        assert!(state.config.status.contains("disk full"));
    }

    // ── Tokens screen tests ───────────────────────────────────────────────────

    #[test]
    fn enter_tokens_screen_dispatches_load() {
        let mut state = AppState::new();
        let actions = update(&mut state, Message::NavTo(Screen::Tokens));
        assert_eq!(state.screen, Screen::Tokens);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }

    #[test]
    fn tokens_c_key_opens_create_form() {
        use crate::app::state::TokensFormPhase;
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        update(&mut state, Message::Key(key(KeyCode::Char('c'))));
        assert_eq!(state.tokens.form_phase, TokensFormPhase::Creating);
    }

    #[test]
    fn tokens_esc_cancels_create_form() {
        use crate::app::state::TokensFormPhase;
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        state.tokens.form_phase = TokensFormPhase::Creating;
        state.tokens.form_name = "foo".to_string();
        update(&mut state, Message::Key(key(KeyCode::Esc)));
        assert_eq!(state.tokens.form_phase, TokensFormPhase::Browsing);
        assert!(state.tokens.form_name.is_empty());
    }

    #[test]
    fn tokens_ctrl_space_toggles_read_only_in_creating() {
        use crate::app::state::TokensFormPhase;
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        state.tokens.form_phase = TokensFormPhase::Creating;
        assert!(!state.tokens.form_read_only);
        // Ctrl-Space toggles; a bare Space is now a literal char in the name.
        update(
            &mut state,
            Message::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)),
        );
        assert!(state.tokens.form_read_only);
    }

    #[test]
    fn tokens_ctrl_e_cycles_expiry_in_creating() {
        use crate::app::state::{TokenExpiryChoice, TokensFormPhase};
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        state.tokens.form_phase = TokensFormPhase::Creating;
        assert_eq!(state.tokens.form_expiry, TokenExpiryChoice::Never);
        let ctrl_e = || Message::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        update(&mut state, ctrl_e());
        assert_eq!(state.tokens.form_expiry, TokenExpiryChoice::ThirtyMinutes);
        // Cycle all the way around back to Never.
        for _ in 0..4 {
            update(&mut state, ctrl_e());
        }
        assert_eq!(state.tokens.form_expiry, TokenExpiryChoice::Never);
    }

    #[test]
    fn tokens_open_create_form_resets_expiry() {
        use crate::app::state::{TokenExpiryChoice, TokensFormPhase};
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        state.tokens.form_expiry = TokenExpiryChoice::SevenDays;
        // Pressing `c` from Browsing opens a fresh form with expiry reset.
        update(&mut state, Message::Key(key(KeyCode::Char('c'))));
        assert_eq!(state.tokens.form_phase, TokensFormPhase::Creating);
        assert_eq!(state.tokens.form_expiry, TokenExpiryChoice::Never);
    }

    #[test]
    fn tokens_bare_space_is_literal_in_creating() {
        use crate::app::state::TokensFormPhase;
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        state.tokens.form_phase = TokensFormPhase::Creating;
        update(&mut state, Message::Key(key(KeyCode::Char(' '))));
        assert_eq!(state.tokens.form_name, " ");
        assert!(!state.tokens.form_read_only);
    }

    #[test]
    fn tokens_loaded_updates_list() {
        use crate::server::tokens::TokenRecord;
        let mut state = AppState::new();
        let records = vec![TokenRecord {
            name: "my-token".to_string(),
            created_at: "2026-01-01".to_string(),
            read_only: false,
        }];
        update(&mut state, Message::TokensLoaded(records));
        assert_eq!(state.tokens.tokens.len(), 1);
        assert_eq!(state.tokens.tokens[0].name, "my-token");
        assert!(!state.tokens.loading);
    }

    #[test]
    fn token_created_stores_secret_and_triggers_reload() {
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        let actions = update(
            &mut state,
            Message::TokenCreated {
                token: "plaintext-secret".to_string(),
                name: "new-tok".to_string(),
                read_only: false,
            },
        );
        assert_eq!(
            state.tokens.last_minted_secret,
            Some(("new-tok".to_string(), "plaintext-secret".to_string(), false))
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens))
        );
    }

    // ── Token QR overlay tests ────────────────────────────────────────────────

    /// Seed `last_minted_secret` so the Tokens-screen `Enter` can open an overlay.
    fn with_minted_secret(state: &mut AppState) {
        state.screen = Screen::Tokens;
        state.tokens.last_minted_secret =
            Some(("my-tok".to_string(), "plaintext-secret".to_string(), false));
    }

    #[test]
    fn tokens_enter_with_minted_secret_opens_overlay_and_dispatches_show() {
        let mut state = AppState::new();
        with_minted_secret(&mut state);
        let before_seq = state.qr_seq;
        let actions = update(&mut state, Message::Key(key(KeyCode::Enter)));
        // The overlay is opened in the Generating phase.
        let overlay = state.qr_overlay.as_ref().expect("overlay should be set");
        assert!(matches!(overlay.phase, QrOverlayPhase::Generating));
        assert_eq!(overlay.token_name, "my-tok");
        assert_eq!(overlay.seq, before_seq + 1);
        assert_eq!(state.qr_seq, before_seq + 1);
        // A ShowTokenQr action with the matching seq + plaintext token is emitted.
        assert!(actions.iter().any(|a| matches!(
            a,
            UpdateAction::ShowTokenQr { token, seq, .. }
                if token == "plaintext-secret" && *seq == overlay.seq
        )));
    }

    #[test]
    fn tokens_enter_without_minted_secret_is_noop() {
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        state.tokens.last_minted_secret = None;
        let actions = update(&mut state, Message::Key(key(KeyCode::Enter)));
        assert!(state.qr_overlay.is_none());
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, UpdateAction::ShowTokenQr { .. }))
        );
    }

    /// Open an overlay in the `Generating` phase with the given seq.
    fn open_overlay(state: &mut AppState, seq: u64) {
        state.qr_seq = seq;
        state.qr_overlay = Some(QrOverlay {
            phase: QrOverlayPhase::Generating,
            seq,
            baseline_clients: 0,
            token_name: "my-tok".to_string(),
            read_only: false,
            tick_counter: 0,
        });
    }

    #[test]
    fn token_qr_ready_with_matching_seq_transitions_to_showing() {
        let mut state = AppState::new();
        open_overlay(&mut state, 7);
        update(
            &mut state,
            Message::TokenQrReady {
                uri: "muxr://pair?v=1".to_string(),
                host: "192.168.1.1".to_string(),
                port: 50051,
                fingerprint_short: "abcd1234…".to_string(),
                baseline_clients: 2,
                seq: 7,
            },
        );
        let overlay = state.qr_overlay.as_ref().expect("overlay should remain");
        assert!(matches!(overlay.phase, QrOverlayPhase::Showing { .. }));
        assert_eq!(overlay.baseline_clients, 2);
    }

    #[test]
    fn token_qr_ready_with_stale_seq_is_ignored() {
        let mut state = AppState::new();
        open_overlay(&mut state, 7); // current overlay seq is 7
        let actions = update(
            &mut state,
            Message::TokenQrReady {
                uri: "muxr://pair?v=1".to_string(),
                host: "192.168.1.1".to_string(),
                port: 50051,
                fingerprint_short: "abcd…".to_string(),
                baseline_clients: 9,
                seq: 5, // stale
            },
        );
        // Phase unchanged (still Generating); NO action emitted, NO revoke — the
        // shown token is a real user token and nothing was minted.
        let overlay = state.qr_overlay.as_ref().expect("overlay should remain");
        assert!(matches!(overlay.phase, QrOverlayPhase::Generating));
        assert_eq!(overlay.baseline_clients, 0);
        assert!(actions.is_empty());
    }

    #[test]
    fn token_qr_failed_with_matching_seq_sets_failed_phase() {
        let mut state = AppState::new();
        open_overlay(&mut state, 3);
        update(
            &mut state,
            Message::TokenQrFailed {
                err: "boom".to_string(),
                seq: 3,
            },
        );
        let overlay = state.qr_overlay.as_ref().expect("overlay should remain");
        assert!(matches!(&overlay.phase, QrOverlayPhase::Failed { err } if err == "boom"));
    }

    /// Put the overlay into the `Showing` phase with the given baseline.
    fn show_overlay(state: &mut AppState, baseline: usize) {
        open_overlay(state, 1);
        let overlay = state.qr_overlay.as_mut().unwrap();
        overlay.baseline_clients = baseline;
        overlay.phase = QrOverlayPhase::Showing {
            uri: "muxr://pair?v=1".to_string(),
            host: "192.168.1.1".to_string(),
            port: 50051,
            fingerprint_short: "abcd…".to_string(),
        };
    }

    #[test]
    fn overlay_connection_detected_when_client_count_rises() {
        let mut state = AppState::new();
        // Overlay can be up over any screen — connection detection is not gated by
        // the active screen, only by the overlay being `Showing`.
        state.screen = Screen::Tokens;
        show_overlay(&mut state, 2);
        update(&mut state, Message::StatusLoaded(Some(server_info(3))));
        let overlay = state.qr_overlay.as_ref().expect("overlay should remain");
        assert!(matches!(overlay.phase, QrOverlayPhase::Connected));
    }

    #[test]
    fn overlay_connection_not_detected_without_client_rise() {
        let mut state = AppState::new();
        show_overlay(&mut state, 3);
        // Same count as baseline — no rise, no promotion.
        update(&mut state, Message::StatusLoaded(Some(server_info(3))));
        let overlay = state.qr_overlay.as_ref().expect("overlay should remain");
        assert!(matches!(overlay.phase, QrOverlayPhase::Showing { .. }));
    }

    #[test]
    fn esc_closes_overlay_with_no_revoke_action() {
        let mut state = AppState::new();
        show_overlay(&mut state, 0);
        let actions = update(&mut state, Message::Key(key(KeyCode::Esc)));
        // Overlay is gone, and NO action (no revoke, no ShowTokenQr) is emitted —
        // the shown token is a real user token that must never be revoked on close.
        assert!(state.qr_overlay.is_none());
        assert!(actions.is_empty());
    }

    #[test]
    fn q_closes_overlay_without_quitting() {
        let mut state = AppState::new();
        show_overlay(&mut state, 0);
        let actions = update(&mut state, Message::Key(key(KeyCode::Char('q'))));
        // `q` is intercepted by the overlay → closes it rather than quitting.
        assert!(state.qr_overlay.is_none());
        assert!(!state.should_quit);
        assert!(actions.is_empty());
    }

    #[test]
    fn tab_is_swallowed_while_overlay_open() {
        let mut state = AppState::new();
        state.screen = Screen::Tokens;
        show_overlay(&mut state, 0);
        let actions = update(&mut state, Message::Key(key(KeyCode::Tab)));
        // No screen navigation behind the overlay; the overlay stays up.
        assert_eq!(state.screen, Screen::Tokens);
        assert!(state.qr_overlay.is_some());
        assert!(actions.is_empty());
    }

    #[test]
    fn pairing_payload_assembly_produces_valid_uri() {
        // Unit test for the pairing payload assembly path.
        // Exercises PairingParams::to_uri() with realistic values for both
        // trust modes (Pin and Ca) — updated for the v=2 payload format (C1).
        use crate::pairing::payload::{PairingParams, PairingTrust};
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        // ── Pin trust (self-signed / LAN) ─────────────────────────────────────
        let pin_params = PairingParams {
            host: "10.0.1.5".to_string(),
            port: 50051,
            trust: PairingTrust::Pin {
                fingerprint: "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
                    .to_string(),
            },
            token: "secret-token-value".to_string(),
            read_only: false,
            label: "My Server".to_string(),
        };
        let pin_uri = pin_params.to_uri();

        assert!(
            pin_uri.starts_with("muxr://pair?v=2"),
            "expected v=2 prefix: {pin_uri}"
        );
        assert!(pin_uri.contains("h=10.0.1.5"), "host: {pin_uri}");
        assert!(pin_uri.contains("p=50051"), "port: {pin_uri}");
        assert!(pin_uri.contains("ro=0"), "ro: {pin_uri}");
        assert!(pin_uri.contains("n=My%20Server"), "label: {pin_uri}");
        assert!(pin_uri.contains("tm=pin"), "tm=pin: {pin_uri}");
        assert!(pin_uri.contains("fp=a1b2c3d4e5f6"), "fp prefix: {pin_uri}");

        // Verify token round-trips correctly.
        let t_val = pin_uri
            .split('&')
            .find(|s| s.starts_with("t="))
            .and_then(|s| s.strip_prefix("t="))
            .expect("t= param missing");
        let decoded = URL_SAFE_NO_PAD.decode(t_val).unwrap();
        assert_eq!(std::str::from_utf8(&decoded).unwrap(), "secret-token-value");

        // ── Ca trust (CA-signed / h2c / proxy) ───────────────────────────────
        let ca_params = PairingParams {
            host: "server.example.com".to_string(),
            port: 443,
            trust: PairingTrust::Ca,
            token: "other-token".to_string(),
            read_only: true,
            label: "Cloud".to_string(),
        };
        let ca_uri = ca_params.to_uri();

        assert!(
            ca_uri.starts_with("muxr://pair?v=2"),
            "expected v=2 prefix: {ca_uri}"
        );
        assert!(ca_uri.contains("tm=ca"), "tm=ca: {ca_uri}");
        assert!(!ca_uri.contains("fp="), "no fp in CA URI: {ca_uri}");
        assert!(ca_uri.contains("ro=1"), "ro=1: {ca_uri}");
    }

    // ── build_sans_from_config tests ──────────────────────────────────────────

    /// Helper to call `build_sans_from_config` with a minimal state.
    fn sans_for(host: &str, reachable: &[std::net::Ipv4Addr]) -> Vec<San> {
        let mut state = AppState::new();
        state.config.host = host.to_string();
        state.config.reachable_ips = reachable.to_vec();
        build_sans_from_config(&state)
    }

    #[test]
    fn build_sans_filters_unspecified_bind_host() {
        // When bind host is 0.0.0.0 and reachable IPs are real addresses, the
        // result must NOT include 0.0.0.0 but MUST include the real IPs.
        use std::net::Ipv4Addr;
        let reachable = [Ipv4Addr::new(100, 64, 1, 2), Ipv4Addr::new(192, 168, 1, 10)];
        let sans = sans_for("0.0.0.0", &reachable);
        let values: Vec<&str> = sans.iter().map(|s| s.value()).collect();
        assert!(
            !values.contains(&"0.0.0.0"),
            "0.0.0.0 must not appear as a SAN; got: {values:?}"
        );
        assert!(
            values.contains(&"100.64.1.2"),
            "tailnet IP missing: {values:?}"
        );
        assert!(
            values.contains(&"192.168.1.10"),
            "LAN IP missing: {values:?}"
        );
    }

    #[test]
    fn build_sans_deduplicates_when_bind_host_matches_reachable() {
        // If the user sets the bind host to a specific LAN IP that also appears
        // in reachable_ips, it should only appear once in the SAN list.
        use std::net::Ipv4Addr;
        let ip = Ipv4Addr::new(192, 168, 1, 10);
        let sans = sans_for("192.168.1.10", &[ip]);
        let values: Vec<&str> = sans.iter().map(|s| s.value()).collect();
        let count = values.iter().filter(|&&v| v == "192.168.1.10").count();
        assert_eq!(count, 1, "IP should appear exactly once; got: {values:?}");
    }

    #[test]
    fn build_sans_merges_advertise_sans_and_dedupes() {
        // The tailnet/docker scenario: bind 0.0.0.0, the only reachable IP is the
        // container's internal address, and ZELLIMSERVER_SAN advertises the
        // externally-reachable tailnet IP. The cert must include BOTH the
        // reachable IP and the advertise SAN, and not duplicate one that overlaps.
        use std::net::Ipv4Addr;
        let mut state = AppState::new();
        state.config.host = "0.0.0.0".to_string();
        state.config.reachable_ips = vec![Ipv4Addr::new(172, 19, 0, 2)];
        state.config.advertise_sans = vec!["100.71.31.57".to_string(), "172.19.0.2".to_string()];
        let values: Vec<String> = build_sans_from_config(&state)
            .iter()
            .map(|s| s.value().to_string())
            .collect();
        assert!(
            values.iter().any(|v| v.as_str() == "172.19.0.2"),
            "reachable IP missing: {values:?}"
        );
        assert!(
            values.iter().any(|v| v.as_str() == "100.71.31.57"),
            "advertise SAN (tailnet IP) missing: {values:?}"
        );
        // 0.0.0.0 (wildcard bind host) must never become a SAN.
        assert!(
            !values.iter().any(|v| v.as_str() == "0.0.0.0"),
            "wildcard leaked as SAN: {values:?}"
        );
        // The overlapping 172.19.0.2 appears exactly once.
        assert_eq!(
            values.iter().filter(|v| v.as_str() == "172.19.0.2").count(),
            1,
            "duplicate SAN: {values:?}"
        );
    }

    #[test]
    fn build_sans_includes_concrete_bind_host_not_in_reachable() {
        // A user-configured specific LAN IP that reachable_ipv4 didn't pick up
        // (e.g. an alias) should still appear in the SANs.
        use std::net::Ipv4Addr;
        let reachable = [Ipv4Addr::new(10, 0, 0, 1)];
        let sans = sans_for("192.168.99.5", &reachable);
        let values: Vec<&str> = sans.iter().map(|s| s.value()).collect();
        assert!(
            values.contains(&"192.168.99.5"),
            "concrete bind host missing: {values:?}"
        );
    }

    #[test]
    fn build_sans_dns_bind_host_included() {
        // A DNS bind host (e.g. "tailscale-host.example.com") should be added as
        // a DNS SAN.
        use std::net::Ipv4Addr;
        let reachable = [Ipv4Addr::new(10, 0, 0, 1)];
        let sans = sans_for("myserver.local", &reachable);
        let values: Vec<&str> = sans.iter().map(|s| s.value()).collect();
        assert!(
            values.contains(&"myserver.local"),
            "DNS bind host missing: {values:?}"
        );
        // Confirm it was captured as a Dns SAN.
        assert!(
            sans.iter()
                .any(|s| matches!(s, San::Dns(d) if d == "myserver.local")),
            "DNS bind host should be San::Dns; got: {sans:?}"
        );
    }

    #[test]
    fn build_sans_empty_when_no_reachable_and_unspecified_bind() {
        // No reachable IPs + wildcard bind → empty SAN list (avoids meaningless
        // 0.0.0.0 SAN that would pass vacuously in sidecar_covers).
        let sans = sans_for("0.0.0.0", &[]);
        assert!(sans.is_empty(), "expected empty SANs; got: {sans:?}");
    }

    #[test]
    fn enter_cert_screen_dispatches_load_config() {
        // Navigating to the Cert screen must dispatch LoadConfig so reachable_ips
        // are populated even without visiting the Config screen first.
        let mut state = AppState::new();
        let actions = update(&mut state, Message::NavTo(Screen::Cert));
        assert_eq!(state.screen, Screen::Cert);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadConfig)),
            "entering Cert must dispatch LoadConfig to populate reachable_ips"
        );
    }

    // ── Dashboard screen tests ────────────────────────────────────────────────

    #[test]
    fn enter_dashboard_dispatches_all_four_read_actions() {
        // Entering the Dashboard must dispatch RefreshStatus + LoadConfig +
        // LoadTokens + LoadCertInfo so all sub-states populate.
        let mut state = AppState::new();
        // Start on a different screen so we can navigate *to* Dashboard.
        state.screen = Screen::Server;
        let actions = update(&mut state, Message::NavTo(Screen::Dashboard));
        assert_eq!(state.screen, Screen::Dashboard);

        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::RefreshStatus)),
            "Dashboard on_enter must dispatch RefreshStatus; got: {actions:?}"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadConfig)),
            "Dashboard on_enter must dispatch LoadConfig; got: {actions:?}"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadTokens)),
            "Dashboard on_enter must dispatch LoadTokens; got: {actions:?}"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadCertInfo)),
            "Dashboard on_enter must dispatch LoadCertInfo; got: {actions:?}"
        );
    }

    #[test]
    fn enter_dashboard_sets_loading_flags() {
        // All relevant loading flags must be raised so the overview can show
        // per-section loading indicators until results arrive.
        let mut state = AppState::new();
        state.screen = Screen::Config;
        update(&mut state, Message::NavTo(Screen::Dashboard));
        assert!(state.server.loading, "server.loading should be set");
        assert!(state.config.loading, "config.loading should be set");
        assert!(state.tokens.loading, "tokens.loading should be set");
        assert!(state.cert.loading, "cert.loading should be set");
    }

    #[test]
    fn cert_info_loaded_sets_fingerprint_and_sans_without_touching_status() {
        // CertInfoLoaded is a passive read: it populates fingerprint/sans but
        // must NOT overwrite the status line (that is reserved for CertEnsured,
        // the active regeneration path).
        let mut state = AppState::new();
        // Pre-populate a status line to prove it is not cleared.
        state.cert.status = "Certificate ready.".to_string();

        update(
            &mut state,
            Message::CertInfoLoaded {
                fingerprint: Some("aabbccddeeff0011".to_string()),
                sans: vec!["192.168.1.1".to_string(), "server.local".to_string()],
            },
        );

        assert_eq!(state.cert.fingerprint, "aabbccddeeff0011");
        assert_eq!(
            state.cert.sans,
            vec!["192.168.1.1".to_string(), "server.local".to_string()]
        );
        // Status must be unchanged — passive load does not set "Certificate ready.".
        assert_eq!(
            state.cert.status, "Certificate ready.",
            "CertInfoLoaded must not overwrite cert.status"
        );
        assert!(!state.cert.loading, "cert.loading should be cleared");
    }

    #[test]
    fn cert_info_loaded_none_fingerprint_stores_empty_string() {
        // When no cert exists on disk, fingerprint is None → stored as "".
        let mut state = AppState::new();
        update(
            &mut state,
            Message::CertInfoLoaded {
                fingerprint: None,
                sans: vec![],
            },
        );
        assert!(
            state.cert.fingerprint.is_empty(),
            "None fingerprint must map to empty string"
        );
        assert!(state.cert.sans.is_empty());
    }

    /// Structural assertion: `LoadCertInfo` action must never be confused with
    /// `EnsureCert`.  The action enum variants are distinct types — this test
    /// documents and protects that invariant by exhaustive matching.
    #[test]
    fn load_cert_info_action_is_distinct_from_ensure_cert() {
        let load_info = UpdateAction::LoadCertInfo;
        let ensure = UpdateAction::EnsureCert(vec![]);

        // These pattern-matches would be compile errors if the variants didn't exist.
        assert!(matches!(load_info, UpdateAction::LoadCertInfo));
        assert!(matches!(ensure, UpdateAction::EnsureCert(_)));
        // Cross-checks: each matches only its own variant.
        assert!(!matches!(load_info, UpdateAction::EnsureCert(_)));
        assert!(!matches!(ensure, UpdateAction::LoadCertInfo));
    }
}
