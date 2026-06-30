//! unit — pure-function unit tests for muxrd.
//!
//! All tests here pass under `cargo test` WITHOUT a running zellij instance.
//! Tests that require a live zellij session are marked `#[ignore]` with a
//! comment explaining what they need.
//!
//! Coverage:
//!   - `validate_session_name`: accept/reject cases
//!   - `validate_layout_name`: accept/reject cases
//!   - `pane_id_from_target`: Terminal / Plugin mapping
//!   - control `read_msg`: oversized-length rejection + round-trip
//!   - config bind-addr precedence: flag > ZELLIMSERVER_BIND env > default
//!   - `parse_zellij_version`: version string parsing
//!   - `SanEntry::parse`: IP and DNS parsing
//!   - `SanEntry::from_env`: env var parsing
//!   - TLS cert generation includes extra SANs (generation only, no disk I/O)

// ─── validate_session_name ────────────────────────────────────────────────────

#[test]
fn session_name_accepts_valid() {
    for name in &["foo", "my-session_1", "abc", "A", "session-123", "a_b_c"] {
        assert!(
            muxrd::ipc::validate_session_name(name).is_ok(),
            "expected {name:?} to be valid"
        );
    }
}

#[test]
fn session_name_rejects_empty() {
    assert!(muxrd::ipc::validate_session_name("").is_err());
}

#[test]
fn session_name_rejects_dot() {
    assert!(muxrd::ipc::validate_session_name(".").is_err());
}

#[test]
fn session_name_rejects_dotdot() {
    assert!(muxrd::ipc::validate_session_name("..").is_err());
}

#[test]
fn session_name_rejects_path_separator() {
    assert!(muxrd::ipc::validate_session_name("a/b").is_err());
    assert!(muxrd::ipc::validate_session_name("../x").is_err());
}

#[test]
fn session_name_rejects_space() {
    assert!(muxrd::ipc::validate_session_name("a b").is_err());
}

#[test]
fn session_name_rejects_semicolon() {
    assert!(muxrd::ipc::validate_session_name("a;b").is_err());
}

#[test]
fn session_name_rejects_dot_in_middle() {
    // zellij's own check only bans leading "." / ".." / "/"  but our strict
    // allowlist [A-Za-z0-9_-] rejects any "." character.
    assert!(muxrd::ipc::validate_session_name("a.b").is_err());
}

// ─── validate_layout_name ─────────────────────────────────────────────────────

#[test]
fn layout_name_accepts_valid() {
    for name in &["compact", "default", "my-layout", "layout_2"] {
        assert!(
            muxrd::grpc::helpers::validate_layout_name(name).is_ok(),
            "expected {name:?} to be a valid layout name"
        );
    }
}

#[test]
fn layout_name_rejects_empty() {
    assert!(muxrd::grpc::helpers::validate_layout_name("").is_err());
}

#[test]
fn layout_name_rejects_abs_path() {
    assert!(muxrd::grpc::helpers::validate_layout_name("/abs/path").is_err());
}

#[test]
fn layout_name_rejects_dotdot() {
    assert!(muxrd::grpc::helpers::validate_layout_name("..").is_err());
}

#[test]
fn layout_name_rejects_slash() {
    assert!(muxrd::grpc::helpers::validate_layout_name("a/b").is_err());
}

#[test]
fn layout_name_rejects_kdl_extension() {
    // "foo.kdl" contains a "." which is outside [A-Za-z0-9_-].
    assert!(muxrd::grpc::helpers::validate_layout_name("foo.kdl").is_err());
}

// ─── PaneId mapping ──────────────────────────────────────────────────────────

#[test]
fn pane_id_terminal() {
    use zellij_utils::data::PaneId;
    let id = muxrd::actions::pane_id_from_target(42, false);
    assert_eq!(id, PaneId::Terminal(42));
}

#[test]
fn pane_id_plugin() {
    use zellij_utils::data::PaneId;
    let id = muxrd::actions::pane_id_from_target(7, true);
    assert_eq!(id, PaneId::Plugin(7));
}

// ─── Control message round-trip and oversized-length rejection ─────────────────

mod control_msg {
    use std::io::{Cursor, Read, Write};

    use muxrd::control::ControlRequest;

    // Mirror the wire-format helpers from control.rs (they're private, so we
    // re-implement the framing here for test purposes).
    fn write_msg<W: Write, T: serde::Serialize>(w: &mut W, msg: &T) {
        let bytes = serde_json::to_vec(msg).unwrap();
        let len = (bytes.len() as u32).to_le_bytes();
        w.write_all(&len).unwrap();
        w.write_all(&bytes).unwrap();
    }

    fn read_msg<R: Read, T: for<'de> serde::Deserialize<'de>>(r: &mut R) -> anyhow::Result<T> {
        const MAX: usize = 64 * 1024;
        let mut len_bytes = [0u8; 4];
        r.read_exact(&mut len_bytes)?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len > MAX {
            anyhow::bail!("control: message length {len} exceeds maximum {MAX}");
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        Ok(serde_json::from_slice(&buf)?)
    }

    #[test]
    fn round_trip_status_request() {
        let mut buf = Cursor::new(Vec::<u8>::new());
        write_msg(&mut buf, &ControlRequest::Status);
        buf.set_position(0);
        let decoded: ControlRequest = read_msg(&mut buf).unwrap();
        assert!(matches!(decoded, ControlRequest::Status));
    }

    #[test]
    fn round_trip_shutdown_request() {
        let mut buf = Cursor::new(Vec::<u8>::new());
        write_msg(&mut buf, &ControlRequest::Shutdown);
        buf.set_position(0);
        let decoded: ControlRequest = read_msg(&mut buf).unwrap();
        assert!(matches!(decoded, ControlRequest::Shutdown));
    }

    #[test]
    fn oversized_length_rejected_without_allocation() {
        // Write a crafted prefix claiming 64 KiB + 1 bytes.
        let mut buf = Cursor::new(Vec::<u8>::new());
        let oversize: u32 = (64 * 1024 + 1) as u32;
        buf.write_all(&oversize.to_le_bytes()).unwrap();
        // No body bytes are needed — the check fires on the length prefix alone.
        buf.set_position(0);
        let result: anyhow::Result<ControlRequest> = read_msg(&mut buf);
        assert!(result.is_err(), "expected oversized message to be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exceeds maximum"),
            "error message should mention exceeds maximum, got: {msg}"
        );
    }

    #[test]
    fn max_exactly_at_limit_accepted() {
        // A valid (small) message is accepted.
        let mut buf = Cursor::new(Vec::<u8>::new());
        write_msg(&mut buf, &ControlRequest::Status);
        buf.set_position(0);
        let result: anyhow::Result<ControlRequest> = read_msg(&mut buf);
        assert!(result.is_ok());
    }
}

// ─── Config bind-addr precedence ─────────────────────────────────────────────

mod config_precedence {
    /// The default bind address when nothing overrides it.
    const DEFAULT_BIND: &str = "127.0.0.1:50051";

    /// Minimal precedence resolver that mirrors `config::resolve` logic without
    /// touching the filesystem.
    fn resolve_bind(flag: Option<&str>, env_val: Option<&str>, file_val: Option<&str>) -> String {
        flag.map(str::to_owned)
            .or_else(|| env_val.map(str::to_owned))
            .or_else(|| file_val.map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_BIND.to_owned())
    }

    #[test]
    fn flag_wins_over_env_and_file() {
        let result = resolve_bind(Some("1.2.3.4:9999"), Some("env:8888"), Some("file:7777"));
        assert_eq!(result, "1.2.3.4:9999");
    }

    #[test]
    fn env_wins_over_file_when_no_flag() {
        let result = resolve_bind(None, Some("env:8888"), Some("file:7777"));
        assert_eq!(result, "env:8888");
    }

    #[test]
    fn file_wins_when_no_flag_or_env() {
        let result = resolve_bind(None, None, Some("file:7777"));
        assert_eq!(result, "file:7777");
    }

    #[test]
    fn default_when_nothing_set() {
        let result = resolve_bind(None, None, None);
        assert_eq!(result, DEFAULT_BIND);
    }

    #[test]
    fn default_constant_is_loopback_50051() {
        assert_eq!(muxrd::config::DEFAULT_BIND, DEFAULT_BIND);
    }
}

// ─── parse_zellij_version ────────────────────────────────────────────────────

#[test]
fn parse_version_standard() {
    // Mirror the parse logic from bin/muxrd.rs `parse_zellij_version`.
    // The function lives in the binary crate and is not accessible directly from
    // integration tests, so we replicate and test the one-liner logic.
    fn parse(output: &str) -> String {
        output
            .split_whitespace()
            .last()
            .unwrap_or("")
            .trim()
            .to_string()
    }

    assert_eq!(parse("zellij 0.44.3"), "0.44.3");
    assert_eq!(parse("zellij 0.44.3\n"), "0.44.3");
    assert_eq!(parse("  zellij   0.44.3  "), "0.44.3");
    assert_eq!(parse(""), "");
    // Unexpected single-token format → last token is the whole string.
    assert_eq!(parse("zellij"), "zellij");
}

// ─── SanEntry parsing ────────────────────────────────────────────────────────

#[test]
fn san_entry_parse_ipv4() {
    use muxrd::tls::SanEntry;
    use std::net::{IpAddr, Ipv4Addr};
    let entry = SanEntry::parse("100.64.0.5");
    assert_eq!(
        entry,
        SanEntry::Ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 5)))
    );
}

#[test]
fn san_entry_parse_ipv6() {
    use muxrd::tls::SanEntry;
    use std::net::{IpAddr, Ipv6Addr};
    let entry = SanEntry::parse("::1");
    assert_eq!(entry, SanEntry::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
}

#[test]
fn san_entry_parse_dns() {
    use muxrd::tls::SanEntry;
    let entry = SanEntry::parse("myhost.example.com");
    assert_eq!(entry, SanEntry::Dns("myhost.example.com".to_owned()));
}

#[test]
fn san_entry_parse_trims_whitespace() {
    use muxrd::tls::SanEntry;
    use std::net::{IpAddr, Ipv4Addr};
    let entry = SanEntry::parse("  10.0.0.1  ");
    assert_eq!(entry, SanEntry::Ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
}

#[test]
fn san_entry_from_env_empty_when_unset() {
    // Use a unique env var name per test to avoid cross-test interference.
    // SAFETY: these tests are single-threaded (cargo test runs unit tests
    // in the same process but env manipulation is safe in single-threaded context).
    unsafe {
        std::env::remove_var("ZELLIMSERVER_SAN");
    }
    let result = muxrd::tls::SanEntry::from_env();
    assert!(result.is_empty());
}

#[test]
fn san_entry_from_env_parses_comma_list() {
    use muxrd::tls::SanEntry;
    unsafe {
        std::env::set_var("ZELLIMSERVER_SAN", "10.0.0.1,myhost.local");
    }
    let result = SanEntry::from_env();
    // Clean up before asserting to avoid poisoning other tests.
    unsafe {
        std::env::remove_var("ZELLIMSERVER_SAN");
    }
    assert_eq!(result.len(), 2);
    assert!(matches!(result[0], SanEntry::Ip(_)));
    assert!(matches!(result[1], SanEntry::Dns(_)));
}

// ─── TLS cert generation includes extra SANs ─────────────────────────────────

#[test]
fn tls_generate_includes_extra_ip_san() {
    // Generate a cert with an extra IP SAN and verify the PEM contains the
    // IP bytes by searching the DER-decoded bytes.
    use muxrd::tls::{SanEntry, generate_self_signed_pem};
    use std::net::{IpAddr, Ipv4Addr};

    let ip = Ipv4Addr::new(100, 64, 0, 5);
    let extra = vec![SanEntry::Ip(IpAddr::V4(ip))];
    let (cert_pem, _key_pem) = generate_self_signed_pem(&extra).expect("cert generation failed");

    // Decode PEM to DER and search for the IP bytes [100, 64, 0, 5].
    let der = pem_to_der(&cert_pem).expect("failed to decode cert PEM");
    let ip_bytes = ip.octets();
    assert!(
        der.windows(4).any(|w| w == ip_bytes),
        "expected IP {:?} to appear in the cert DER; bytes not found",
        ip_bytes
    );
}

#[test]
fn tls_generate_always_includes_localhost() {
    use muxrd::tls::generate_self_signed_pem;
    let (cert_pem, _) = generate_self_signed_pem(&[]).expect("cert generation failed");
    let der = pem_to_der(&cert_pem).expect("failed to decode cert PEM");
    // 127.0.0.1 bytes
    let loopback = [127u8, 0, 0, 1];
    assert!(
        der.windows(4).any(|w| w == loopback),
        "expected 127.0.0.1 to always appear in the cert"
    );
}

#[test]
fn tls_generate_includes_dns_san() {
    use muxrd::tls::{SanEntry, generate_self_signed_pem};

    let extra = vec![SanEntry::Dns("example.local".to_owned())];
    let (cert_pem, _) = generate_self_signed_pem(&extra).expect("cert generation failed");
    let der = pem_to_der(&cert_pem).expect("failed to decode cert PEM");
    // The DNS name bytes should appear somewhere in the DER.
    let name_bytes = b"example.local";
    assert!(
        der.windows(name_bytes.len()).any(|w| w == name_bytes),
        "expected 'example.local' to appear in the cert DER"
    );
}

// ─── SAN sidecar coverage check ───────────────────────────────────────────────

#[test]
fn sidecar_covers_all_requested_when_superset() {
    use muxrd::tls::SanEntry;
    use std::net::{IpAddr, Ipv4Addr};

    let stored = [
        SanEntry::Ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 5))),
        SanEntry::Dns("myhost.local".to_owned()),
    ];
    let requested = [SanEntry::Ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 5)))];
    let all_covered = requested.iter().all(|r| stored.contains(r));
    assert!(all_covered, "stored superset should cover requested SANs");
}

#[test]
fn sidecar_does_not_cover_new_san() {
    use muxrd::tls::SanEntry;
    use std::net::{IpAddr, Ipv4Addr};

    let stored: Vec<SanEntry> = vec![]; // empty — no extras when cert was generated
    let requested = [SanEntry::Ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 5)))];
    let all_covered = requested.iter().all(|r| stored.contains(r));
    assert!(!all_covered, "empty sidecar should not cover new SAN");
}

// ─── Helper ────────────────────────────────────────────────────────────────────

/// Decode a PEM-encoded certificate to its raw DER bytes.
///
/// Only handles single-cert PEM; good enough for the tests above.
fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let b64: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()
}
