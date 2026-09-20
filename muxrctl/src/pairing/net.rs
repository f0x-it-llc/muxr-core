//! net — reachable IPv4 address discovery.
//!
//! Returns non-loopback, non-link-local IPv4 addresses the mobile client could
//! plausibly reach (LAN candidates).  Used by the Configure dialog to
//! populate the address picker for the pairing QR.

use std::net::Ipv4Addr;

/// Collect non-loopback, non-link-local IPv4 addresses from all local interfaces.
///
/// Loopback (`127.x.x.x`) and link-local (`169.254.x.x`) addresses are excluded
/// because the mobile client on a different host cannot reach them.  The result
/// is deduplicated (preserving first-seen order).
#[allow(dead_code)]
pub fn reachable_ipv4() -> Vec<Ipv4Addr> {
    let addrs = match if_addrs::get_if_addrs() {
        Ok(a) => a,
        Err(e) => {
            log::warn!("pairing::net: failed to enumerate interfaces: {e}");
            return vec![];
        }
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for iface in addrs {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            let ip = v4.ip;
            if ip.is_loopback() {
                continue;
            }
            // Link-local: 169.254.0.0/16
            if is_link_local(ip) {
                continue;
            }
            // Guard against the unspecified address (0.0.0.0).  Real interfaces
            // should never be assigned this, but we exclude it explicitly so that
            // build_sans_from_config never receives a wildcard in reachable_ips.
            if ip.is_unspecified() {
                continue;
            }
            if seen.insert(ip) {
                out.push(ip);
            }
        }
    }

    out
}

/// Returns `true` if the address is in the IPv4 link-local range (169.254.0.0/16).
fn is_link_local(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 169 && octets[1] == 254
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_local_detection() {
        assert!(is_link_local(Ipv4Addr::new(169, 254, 1, 1)));
        assert!(is_link_local(Ipv4Addr::new(169, 254, 0, 0)));
        assert!(!is_link_local(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(!is_link_local(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!is_link_local(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn reachable_ipv4_excludes_loopback_and_link_local() {
        let addrs = reachable_ipv4();
        for ip in &addrs {
            assert!(
                !ip.is_loopback(),
                "loopback address {ip} should be excluded"
            );
            assert!(
                !is_link_local(*ip),
                "link-local address {ip} should be excluded"
            );
        }
    }

    #[test]
    fn reachable_ipv4_no_duplicates() {
        let addrs = reachable_ipv4();
        let unique: std::collections::HashSet<_> = addrs.iter().collect();
        assert_eq!(
            addrs.len(),
            unique.len(),
            "reachable_ipv4 should not return duplicates"
        );
    }

    #[test]
    fn reachable_ipv4_excludes_unspecified() {
        // The unspecified address (0.0.0.0) must never appear in the output.
        // Real interfaces are never assigned this address, but we guard explicitly
        // so that build_sans_from_config never receives a wildcard in reachable_ips.
        let addrs = reachable_ipv4();
        for ip in &addrs {
            assert!(
                !ip.is_unspecified(),
                "unspecified address {ip} should be excluded"
            );
        }
    }

    #[test]
    fn is_link_local_boundary_cases() {
        // Verify the link-local check covers the exact /16 boundary.
        assert!(is_link_local(Ipv4Addr::new(169, 254, 255, 255)));
        assert!(!is_link_local(Ipv4Addr::new(169, 255, 0, 0)));
        assert!(!is_link_local(Ipv4Addr::new(168, 254, 0, 0)));
    }
}
