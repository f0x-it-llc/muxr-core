//! read_client — C1 test client for `ListSessions` + `GetLayout` over TLS + bearer.
//!
//! Flow:
//! 1. Connect over TLS (trust the self-signed cert from --cert).
//! 2. Login to get a session token.
//! 3. Call ListSessions — print all sessions.
//! 4. Call GetLayout(session) — print tab/pane tree with counts.
//!
//! Usage:
//!     cargo run --example read_client -- \
//!         --cert <path/to/server.crt>    \
//!         --auth-token <plaintext_token> \
//!         [--addr <host:port>]           \
//!         [--session <backend-qualified-id>]
//!
//! Default addr: https://[::1]:50051.
//!
//! `--session` takes the BACKEND-QUALIFIED id that `ListSessions` returns in
//! `SessionInfo.id` — e.g. `zellij:demo`, `herdr:herdr` — not a bare session
//! name. A server driving more than one backend rejects an unqualified name.
//! Omit the flag and this client picks the first id `ListSessions` reports,
//! which is the discovery pattern a real client should follow.

use anyhow::{Context, Result};
use muxrd::proto::muxr_client::MuxrClient;
use muxrd::proto::{Empty, LoginRequest, SessionRef};
use tonic::{
    metadata::MetadataValue,
    transport::{Certificate, Channel, ClientTlsConfig},
};

// ─── Args ─────────────────────────────────────────────────────────────────────

struct Args {
    addr: String,
    cert_pem: String,
    auth_token: String,
    /// Backend-qualified session id (`SessionInfo.id`). `None` = discover it
    /// from `ListSessions` rather than guessing a name.
    session: Option<String>,
}

fn parse_args() -> Result<Args> {
    let argv: Vec<String> = std::env::args().collect();
    let get = |key: &str| -> Option<String> {
        argv.iter()
            .position(|a| a == key)
            .and_then(|p| argv.get(p + 1).cloned())
    };

    let addr = get("--addr").unwrap_or_else(|| "https://[::1]:50051".to_owned());
    let addr = if addr.starts_with("https://") || addr.starts_with("http://") {
        addr
    } else {
        format!("https://{addr}")
    };

    let cert_path =
        get("--cert").ok_or_else(|| anyhow::anyhow!("--cert <path/to/server.crt> is required"))?;
    let cert_pem = std::fs::read_to_string(&cert_path)
        .with_context(|| format!("read cert from {cert_path}"))?;

    let auth_token =
        get("--auth-token").ok_or_else(|| anyhow::anyhow!("--auth-token <token> is required"))?;

    Ok(Args {
        addr,
        cert_pem,
        auth_token,
        session: get("--session"),
    })
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

async fn tls_channel(addr: &str, cert_pem: &str) -> Result<Channel> {
    let ca_cert = Certificate::from_pem(cert_pem.as_bytes());
    let tls = ClientTlsConfig::new()
        .ca_certificate(ca_cert)
        .domain_name("localhost");
    Channel::from_shared(addr.to_owned())
        .context("invalid address")?
        .tls_config(tls)
        .context("TLS config error")?
        .connect()
        .await
        .with_context(|| format!("connect to {addr}"))
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args()?;

    println!("connecting to {} (TLS)…", args.addr);
    let channel = tls_channel(&args.addr, &args.cert_pem).await?;
    let mut client = MuxrClient::new(channel.clone());

    // ── Login ────────────────────────────────────────────────────────────────
    println!("\n--- Login ---");
    let login_resp = client
        .login(LoginRequest {
            auth_token: args.auth_token.clone(),
            remember_me: false,
        })
        .await
        .context("Login RPC failed")?;
    let session_token = login_resp.into_inner().session_token;
    println!(
        "Login: received session token ({}…)",
        &session_token[..8.min(session_token.len())]
    );

    // Build authed client.
    let bearer: MetadataValue<_> = format!("Bearer {session_token}")
        .parse()
        .context("construct bearer header")?;
    let mut authed_client =
        MuxrClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        });

    // ── ListSessions ─────────────────────────────────────────────────────────
    println!("\n--- ListSessions ---");
    let sessions_resp = authed_client
        .list_sessions(Empty {})
        .await
        .context("ListSessions RPC failed")?;
    let sessions = sessions_resp.into_inner().sessions;

    if sessions.is_empty() {
        println!("  (no sessions found)");
    } else {
        println!("  {} session(s):", sessions.len());
        for s in &sessions {
            let age_m = s.age_secs / 60;
            let age_s = s.age_secs % 60;
            let marker = if s.resurrectable {
                " [resurrectable]"
            } else {
                ""
            };
            // Print the qualified `id` alongside the display name: `id` is what
            // every other RPC wants, and showing both makes the difference
            // obvious the first time someone reads this output.
            println!(
                "  - id='{}' name='{}' age={}m{}s{}",
                s.id, s.name, age_m, age_s, marker
            );
        }
    }

    // Resolve the session to address. RPCs take the BACKEND-QUALIFIED id from
    // `SessionInfo.id` ("zellij:demo"), not the bare `name`: a server driving
    // more than one backend rejects an unqualified name outright. An explicit
    // --session is passed through untouched; otherwise discover it here rather
    // than defaulting to a name that only ever worked on a single-backend
    // server.
    let target: String = match &args.session {
        Some(explicit) => {
            println!("  using --session '{explicit}' (passed through verbatim)");
            explicit.clone()
        }
        None => {
            let first = sessions.first().ok_or_else(|| {
                anyhow::anyhow!(
                    "no sessions reported by ListSessions, and no --session given — \
                     the server has no session to attach to (is one seeded?). \
                     Pass --session <backend-qualified-id> to address one explicitly."
                )
            })?;
            println!(
                "  no --session given; discovered '{}' from ListSessions",
                first.id
            );
            first.id.clone()
        }
    };
    let target = &target;

    // ── GetLayout ────────────────────────────────────────────────────────────
    println!("\n--- GetLayout(session='{target}') ---");
    let layout_resp = authed_client
        .get_layout(SessionRef {
            session: target.clone(),
            ..Default::default()
        })
        .await
        .context("GetLayout RPC failed")?;
    let layout = layout_resp.into_inner();

    println!("  {} tab(s):", layout.tabs.len());
    for tab in &layout.tabs {
        let bell = if tab.has_bell { " [BELL]" } else { "" };
        let active = if tab.active { " [active]" } else { "" };
        println!(
            "  Tab #{} '{}' id={}{}{} — {} pane(s)",
            tab.position,
            tab.name,
            tab.tab_id,
            active,
            bell,
            tab.panes.len()
        );
        for pane in &tab.panes {
            let focused = if pane.is_focused { " [focused]" } else { "" };
            let floating = if pane.is_floating { " [float]" } else { "" };
            let plugin = if pane.is_plugin { " [plugin]" } else { "" };
            let cmd_info = if !pane.command.is_empty() {
                format!(" cmd={}", pane.command)
            } else {
                String::new()
            };
            let cwd_info = if !pane.cwd.is_empty() {
                format!(" cwd={}", pane.cwd)
            } else {
                String::new()
            };
            println!(
                "    Pane {} '{}'{}{}{}{}{} {}x{} @({},{})",
                pane.id,
                pane.title,
                focused,
                floating,
                plugin,
                cmd_info,
                cwd_info,
                pane.cols,
                pane.rows,
                pane.x,
                pane.y
            );
        }
    }

    // Summary / pass check.
    println!("\n--------------------------------------------------");
    let tab_count = layout.tabs.len();
    let pane_count: usize = layout.tabs.iter().map(|t| t.panes.len()).sum();
    println!("tabs: {tab_count}  panes: {pane_count}");

    let has_cwd = layout
        .tabs
        .iter()
        .flat_map(|t| &t.panes)
        .any(|p| !p.cwd.is_empty());
    let has_cmd = layout
        .tabs
        .iter()
        .flat_map(|t| &t.panes)
        .any(|p| !p.command.is_empty());

    if tab_count == 0 {
        anyhow::bail!("FAIL: GetLayout returned 0 tabs for session '{target}'");
    }
    println!("PASS: GetLayout returned {tab_count} tab(s), {pane_count} pane(s)");
    if has_cwd {
        println!("PASS: at least one pane has cwd populated");
    } else {
        println!("NOTE: no pane cwd populated (may be expected for new sessions)");
    }
    if has_cmd {
        println!("PASS: at least one pane has command populated");
    } else {
        println!("NOTE: no pane command populated");
    }

    Ok(())
}
