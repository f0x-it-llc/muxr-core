//! The certificate reducer — a stub for the cert card, plus the SAN derivation
//! the cert card owns.

use crate::app::action::UpdateAction;
use crate::app::state::cert::{CertMsg, TlsMode};
use crate::app::state::{AppState, San};

/// Apply a [`CertMsg`]. The cert card extends this.
pub fn update(state: &mut AppState, msg: CertMsg) -> Vec<UpdateAction> {
    match msg {
        CertMsg::Close => super::close_top(state),
    }
}

/// A cert was (re)generated: adopt its fingerprint and SANs.
pub fn on_cert_ensured(
    state: &mut AppState,
    fingerprint: String,
    sans: Vec<String>,
) -> Vec<UpdateAction> {
    state.cert.fingerprint = Some(fingerprint);
    state.cert.sans = sans;
    state.cert.loading = false;
    Vec::new()
}

/// A passive read of the on-disk cert (never regenerates).
pub fn on_cert_info_loaded(
    state: &mut AppState,
    fingerprint: Option<String>,
    sans: Vec<String>,
) -> Vec<UpdateAction> {
    state.cert.fingerprint = fingerprint;
    state.cert.sans = sans;
    state.cert.loading = false;
    Vec::new()
}

/// The daemon's reported transport identity; `None` while it is stopped.
pub fn on_cert_mode_loaded(state: &mut AppState, mode: Option<TlsMode>) -> Vec<UpdateAction> {
    state.cert.tls_mode = mode;
    Vec::new()
}

/// Build a SAN list from the reachable IPs discovered for the Config dialog.
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
/// 3. Advertise SANs from the `MUXRD_SAN` env (loaded via `ConfigLoaded`)
///    are merged in — these cover externally-advertised addresses that are not
///    local interfaces (e.g. a tailnet IP behind a container's NAT publish).
/// 4. De-duplication preserves first-seen order.
///
/// When the bind host is `0.0.0.0` (wildcard) — the common tailnet scenario — it
/// is **omitted** as a SAN (a wildcard SAN is meaningless to TLS clients). Only the
/// real interface IPs from `reachable_ips` are added.
#[allow(dead_code)] // called by the cert card's regenerate action.
pub fn build_sans_from_config(state: &AppState) -> Vec<San> {
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

    // 3. Merge advertise SANs from the `MUXRD_SAN` env (loaded via
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::message::{Message, UiMsg};
    use crate::app::state::DialogId;
    use crate::app::update::update;

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
        // container's internal address, and MUXRD_SAN advertises the
        // externally-reachable tailnet IP. The cert must include BOTH the
        // reachable IP and the advertise SAN, and not duplicate one that overlaps.
        use std::net::Ipv4Addr;
        let mut state = AppState::new();
        state.config.host = "0.0.0.0".to_string();
        state.config.reachable_ips = vec![Ipv4Addr::new(172, 19, 0, 2)];
        state.config.advertise_sans = vec!["100.64.0.7".to_string(), "172.19.0.2".to_string()];
        let values: Vec<String> = build_sans_from_config(&state)
            .iter()
            .map(|s| s.value().to_string())
            .collect();
        assert!(
            values.iter().any(|v| v.as_str() == "172.19.0.2"),
            "reachable IP missing: {values:?}"
        );
        assert!(
            values.iter().any(|v| v.as_str() == "100.64.0.7"),
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
    fn cert_ensured_updates_cert_state() {
        let mut state = AppState::new();
        state.cert.loading = true;
        update(
            &mut state,
            Message::CertEnsured {
                fingerprint: "abc123".to_string(),
                sans: vec!["10.0.0.1".to_string()],
            },
        );
        assert_eq!(state.cert.fingerprint.as_deref(), Some("abc123"));
        assert_eq!(state.cert.sans, vec!["10.0.0.1".to_string()]);
        assert!(!state.cert.loading);
    }

    #[test]
    fn cert_info_loaded_none_leaves_no_fingerprint() {
        let mut state = AppState::new();
        update(
            &mut state,
            Message::CertInfoLoaded {
                fingerprint: None,
                sans: Vec::new(),
            },
        );
        assert_eq!(state.cert.fingerprint, None);
    }

    #[test]
    fn cert_mode_loaded_stores_the_reported_mode() {
        let mut state = AppState::new();
        update(&mut state, Message::CertModeLoaded(Some(TlsMode::H2c)));
        assert_eq!(state.cert.tls_mode, Some(TlsMode::H2c));
        update(&mut state, Message::CertModeLoaded(None));
        assert_eq!(state.cert.tls_mode, None);
    }

    #[test]
    fn close_closes_the_cert_dialog_and_refreshes_it() {
        let mut state = AppState::new();
        update(&mut state, Message::Ui(UiMsg::Open(DialogId::Cert)));
        let actions = update(&mut state, Message::Ui(UiMsg::Cert(CertMsg::Close)));
        assert_eq!(state.top_dialog(), None);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, UpdateAction::LoadCertInfo))
        );
    }
}
