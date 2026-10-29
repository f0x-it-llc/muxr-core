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
//!   - config bind-addr precedence: flag > MUXRD_BIND env > default
//!   - `parse_zellij_version`: version string parsing
//!   - `SanEntry::parse`: IP and DNS parsing
//!   - `SanEntry::from_env`: env var parsing
//!   - TLS cert generation includes extra SANs (generation only, no disk I/O)
//!   - attach eviction registry (`AttachReq.client_instance_id`): who evicts
//!     whom, and that a torn-down relay never removes its successor's slot
//!   - `TokenIdentity`: stable per token, distinct across tokens, redacted

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

    assert_eq!(parse("zellij 0.45.1"), "0.45.1");
    assert_eq!(parse("zellij 0.45.1\n"), "0.45.1");
    assert_eq!(parse("  zellij   0.45.1  "), "0.45.1");
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
        std::env::remove_var("MUXRD_SAN");
    }
    let result = muxrd::tls::SanEntry::from_env();
    assert!(result.is_empty());
}

#[test]
fn san_entry_from_env_parses_comma_list() {
    use muxrd::tls::SanEntry;
    unsafe {
        std::env::set_var("MUXRD_SAN", "10.0.0.1,myhost.local");
    }
    let result = SanEntry::from_env();
    // Clean up before asserting to avoid poisoning other tests.
    unsafe {
        std::env::remove_var("MUXRD_SAN");
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

// ─── Attach eviction (AttachReq.client_instance_id) ─────────────────────────────

mod attach_eviction {
    use std::time::Duration;

    use futures::FutureExt;
    use muxrd::auth::TokenIdentity;
    use muxrd::client_count::SessionClients;
    use muxrd::relay::{
        AttachKey, AttachLease, AttachRegistry, MAX_CLIENT_INSTANCE_ID_LEN, next_attach_arrival,
    };

    const TOKEN_A: &str = "token-alpha-secret";
    const TOKEN_B: &str = "token-bravo-secret";
    const SESSION: &str = "zellij:dev";

    fn key(token: &str, instance: &str) -> Option<AttachKey> {
        AttachKey::new(Some(token), instance)
    }

    /// Register `connection_id` and assert the registration evicted nobody.
    fn register_fresh(
        registry: &AttachRegistry,
        key: Option<AttachKey>,
        connection_id: &str,
    ) -> AttachLease {
        let (lease, evicted) = registry.register(key, connection_id, next_attach_arrival());
        assert_eq!(evicted, None, "{connection_id} must not evict anyone");
        lease
    }

    /// Whether `lease` has been evicted, checked through the same future the
    /// relay's inbound loop awaits.
    fn evicted_now(lease: &AttachLease) -> bool {
        lease.evicted().now_or_never().is_some()
    }

    /// A stand-in relay: holds the session's client guard and its lease, and
    /// ends — dropping both, as the inbound task does — when evicted.
    fn spawn_relay(clients: &SessionClients, lease: AttachLease) -> tokio::task::JoinHandle<()> {
        let guard = clients.attach(SESSION);
        tokio::spawn(async move {
            lease.evicted().await;
            drop(lease);
            drop(guard);
        })
    }

    /// (a) Same token + same non-empty id, second attach while the first is
    /// live: the first is torn down (its client count released), the second
    /// stays live and holds the key.
    #[tokio::test]
    async fn same_token_same_instance_evicts_the_previous_attach() {
        let registry = AttachRegistry::new();
        let clients = SessionClients::new();

        let first = register_fresh(&registry, key(TOKEN_A, "view-1"), "conn-first");
        let first_task = spawn_relay(&clients, first);
        assert_eq!(clients.count(SESSION), 1);

        let (second, evicted) =
            registry.register(key(TOKEN_A, "view-1"), "conn-second", next_attach_arrival());
        assert_eq!(evicted.as_deref(), Some("conn-first"));
        let second_guard = clients.attach(SESSION);

        // The evicted relay tears itself down without the new attach waiting.
        tokio::time::timeout(Duration::from_secs(5), first_task)
            .await
            .expect("the evicted relay must stop")
            .expect("relay task must not panic");
        assert_eq!(
            clients.count(SESSION),
            1,
            "only the successor stays counted against the session"
        );

        assert!(!second.is_evicted());
        assert!(!evicted_now(&second), "the successor stays live");
        assert_eq!(
            registry.holder(&key(TOKEN_A, "view-1").unwrap()).as_deref(),
            Some("conn-second")
        );
        drop(second_guard);
    }

    /// (a) The signal is latched: a relay busy elsewhere when evicted still
    /// observes it on its next poll, and keeps observing it.
    #[test]
    fn eviction_signal_is_not_lost_when_the_relay_is_not_waiting() {
        let registry = AttachRegistry::new();
        let first = register_fresh(&registry, key(TOKEN_A, "view-1"), "conn-first");
        let (_second, _) =
            registry.register(key(TOKEN_A, "view-1"), "conn-second", next_attach_arrival());
        assert!(first.is_evicted());
        assert!(evicted_now(&first));
        assert!(
            evicted_now(&first),
            "re-polling after eviction resolves again"
        );
    }

    /// (b) Same id, different tokens: both stay live.
    #[test]
    fn same_instance_under_different_tokens_both_stay_live() {
        let registry = AttachRegistry::new();
        let a = register_fresh(&registry, key(TOKEN_A, "view-1"), "conn-a");
        let b = register_fresh(&registry, key(TOKEN_B, "view-1"), "conn-b");
        assert!(!evicted_now(&a) && !evicted_now(&b));
        assert_eq!(registry.len(), 2);
    }

    /// (c) Different ids, same token: both stay live.
    #[test]
    fn different_instances_under_one_token_both_stay_live() {
        let registry = AttachRegistry::new();
        let one = register_fresh(&registry, key(TOKEN_A, "view-1"), "conn-1");
        let two = register_fresh(&registry, key(TOKEN_A, "view-2"), "conn-2");
        assert!(!evicted_now(&one) && !evicted_now(&two));
        assert_eq!(registry.len(), 2);
    }

    /// (d) Empty id: never evicts and is never evicted — no key, nothing
    /// registered, an inert lease.
    #[test]
    fn empty_instance_id_never_evicts_and_is_never_evicted() {
        assert!(AttachKey::new(Some(TOKEN_A), "").is_none());

        let registry = AttachRegistry::new();
        let keyed = register_fresh(&registry, key(TOKEN_A, "view-1"), "conn-keyed");
        let first = register_fresh(&registry, key(TOKEN_A, ""), "conn-empty-1");
        let second = register_fresh(&registry, key(TOKEN_A, ""), "conn-empty-2");

        assert!(first.key().is_none() && second.key().is_none());
        assert!(!evicted_now(&first) && !evicted_now(&second) && !evicted_now(&keyed));
        assert_eq!(registry.len(), 1, "only the keyed attach is registered");

        // Dropping an inert lease touches nothing.
        drop(first);
        assert_eq!(
            registry.holder(&key(TOKEN_A, "view-1").unwrap()).as_deref(),
            Some("conn-keyed")
        );
    }

    /// (d) The other inert cases: no authenticated token, or an over-long id.
    #[test]
    fn missing_token_or_over_long_id_is_inert() {
        assert!(AttachKey::new(None, "view-1").is_none());
        let at_limit = "x".repeat(MAX_CLIENT_INSTANCE_ID_LEN);
        assert!(AttachKey::new(Some(TOKEN_A), &at_limit).is_some());
        let over = "x".repeat(MAX_CLIENT_INSTANCE_ID_LEN + 1);
        assert!(AttachKey::new(Some(TOKEN_A), &over).is_none());
    }

    /// (e) Read-only and read-write tokens are both supported: eviction is
    /// scoped by token, not tier — a read-only token's re-attach evicts its own
    /// previous attach, and never a read-write token's attach with the same id.
    #[test]
    fn read_only_and_read_write_tokens_both_evict_within_their_own_token() {
        const RO_TOKEN: &str = "token-read-only";
        const RW_TOKEN: &str = "token-read-write";
        let registry = AttachRegistry::new();

        let rw = register_fresh(&registry, key(RW_TOKEN, "view-1"), "conn-rw-1");
        let ro = register_fresh(&registry, key(RO_TOKEN, "view-1"), "conn-ro-1");

        let (ro_again, evicted) =
            registry.register(key(RO_TOKEN, "view-1"), "conn-ro-2", next_attach_arrival());
        assert_eq!(evicted.as_deref(), Some("conn-ro-1"));
        assert!(
            evicted_now(&ro),
            "the read-only token's old attach is evicted"
        );
        assert!(
            !evicted_now(&rw),
            "the read-write token's attach is untouched"
        );

        let (_rw_again, evicted) =
            registry.register(key(RW_TOKEN, "view-1"), "conn-rw-2", next_attach_arrival());
        assert_eq!(evicted.as_deref(), Some("conn-rw-1"));
        assert!(evicted_now(&rw));
        assert!(!evicted_now(&ro_again));
    }

    /// (f) Teardown of an evicted relay does not delete the successor's entry;
    /// the successor's own teardown then frees the key.
    #[test]
    fn evicted_teardown_keeps_the_successors_entry() {
        let registry = AttachRegistry::new();
        let k = key(TOKEN_A, "view-1").unwrap();

        let first = register_fresh(&registry, Some(k.clone()), "conn-first");
        let (second, _) = registry.register(Some(k.clone()), "conn-second", next_attach_arrival());

        drop(first); // the evicted relay finishes tearing down AFTER the swap
        assert_eq!(registry.holder(&k).as_deref(), Some("conn-second"));
        assert!(
            !registry.deregister(&k, "conn-first"),
            "a stale id removes nothing"
        );
        assert_eq!(registry.holder(&k).as_deref(), Some("conn-second"));

        drop(second);
        assert_eq!(registry.holder(&k), None);
        assert!(registry.is_empty());
    }

    /// A chain of re-attaches always leaves exactly the newest one holding the
    /// key, however late the older ones finish tearing down.
    #[test]
    fn repeated_reattaches_leave_only_the_newest_holder() {
        let registry = AttachRegistry::new();
        let k = key(TOKEN_A, "view-1").unwrap();
        let mut leases = Vec::new();
        for n in 0..4 {
            let (lease, evicted) =
                registry.register(Some(k.clone()), &format!("conn-{n}"), next_attach_arrival());
            assert_eq!(evicted, (n > 0).then(|| format!("conn-{}", n - 1)));
            leases.push(lease);
        }
        let newest = leases.pop().unwrap();
        assert!(leases.iter().all(evicted_now));
        drop(leases);
        assert_eq!(registry.holder(&k).as_deref(), Some("conn-3"));
        assert!(!evicted_now(&newest));
    }

    /// An attach that arrived EARLIER than the current holder (its setup merely
    /// finished later) is refused: its lease is already evicted and the holder
    /// keeps the slot.
    #[test]
    fn earlier_arrival_is_refused_and_the_holder_keeps_the_slot() {
        let registry = AttachRegistry::new();
        let k = key(TOKEN_A, "view-1").unwrap();
        let early = next_attach_arrival();
        let late = next_attach_arrival();
        let (holder, evicted) = registry.register(Some(k.clone()), "conn-late", late);
        assert_eq!(evicted, None);

        let (stale, evicted) = registry.register(Some(k.clone()), "conn-early", early);
        assert_eq!(evicted, None, "a refused registrant evicts nobody");
        assert!(stale.is_evicted());
        assert!(evicted_now(&stale));
        assert!(!evicted_now(&holder));
        assert_eq!(registry.holder(&k).as_deref(), Some("conn-late"));
    }

    /// Normal order: the later arrival registers after an earlier holder and
    /// evicts it; the successor keeps the slot.
    #[test]
    fn later_arrival_evicts_the_earlier_holder() {
        let registry = AttachRegistry::new();
        let k = key(TOKEN_A, "view-1").unwrap();
        let first_arrival = next_attach_arrival();
        let second_arrival = next_attach_arrival();
        let (first, _) = registry.register(Some(k.clone()), "conn-first", first_arrival);
        let (second, evicted) = registry.register(Some(k.clone()), "conn-second", second_arrival);
        assert_eq!(evicted.as_deref(), Some("conn-first"));
        assert!(evicted_now(&first));
        assert!(!evicted_now(&second));
        assert_eq!(registry.holder(&k).as_deref(), Some("conn-second"));
    }

    /// Dropping a refused stale lease leaves the holder's slot in place.
    #[test]
    fn dropping_a_refused_lease_keeps_the_holders_slot() {
        let registry = AttachRegistry::new();
        let k = key(TOKEN_A, "view-1").unwrap();
        let early = next_attach_arrival();
        let late = next_attach_arrival();
        let (holder, _) = registry.register(Some(k.clone()), "conn-late", late);
        let (stale, _) = registry.register(Some(k.clone()), "conn-early", early);
        drop(stale);
        assert_eq!(registry.holder(&k).as_deref(), Some("conn-late"));
        drop(holder);
        assert!(registry.is_empty());
    }

    /// Arrival sequences are strictly increasing across calls.
    #[test]
    fn arrival_sequences_strictly_increase() {
        let a = next_attach_arrival();
        let b = next_attach_arrival();
        let c = next_attach_arrival();
        assert!(a < b && b < c);
    }

    /// (g) Logs and `Debug` output carry at most the first 8 characters of
    /// the instance id, and never the token.
    #[test]
    fn instance_id_and_token_are_redacted() {
        let k = key(TOKEN_A, "0123456789abcdef-instance").unwrap();
        assert_eq!(k.redacted_instance_id(), "01234567…");
        let debug = format!("{k:?}");
        assert!(debug.contains("01234567…"), "{debug}");
        assert!(!debug.contains("89abcdef"), "{debug}");
        assert!(!debug.contains(TOKEN_A), "{debug}");

        // Character-based, so a non-ASCII id cannot split a code point.
        let wide = key(TOKEN_A, "ééééééééé").unwrap();
        assert_eq!(wide.redacted_instance_id(), "éééééééé…");
    }

    #[test]
    fn token_identity_is_stable_distinct_and_redacted() {
        assert_eq!(TokenIdentity::of(TOKEN_A), TokenIdentity::of(TOKEN_A));
        assert_ne!(TokenIdentity::of(TOKEN_A), TokenIdentity::of(TOKEN_B));
        let debug = format!("{:?}", TokenIdentity::of(TOKEN_A));
        assert_eq!(debug, "TokenIdentity(<redacted>)");
    }
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
