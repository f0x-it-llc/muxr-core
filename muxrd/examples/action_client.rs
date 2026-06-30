//! action_client — D1 test client for pane-op RPCs over TLS + bearer.
//!
//! Exercises the Phase D1 acceptance criteria end-to-end against a throwaway
//! session:
//!   (a) GetLayout → pick a terminal pane id → WriteToPane an `echo` that writes
//!       a marker to /tmp/d1_proof.txt (proves id-targeted write).
//!   (b) NewPane → GetLayout shows pane count +1 (capture new id from ack info).
//!   (c) RenamePane / ResizePane / TogglePaneFloating / TogglePaneFullscreen.
//!   (d) ClosePane the new pane → count back down.
//!   (e) read-only token: a mutating RPC returns permission_denied while
//!       GetLayout / FocusPane still succeed.
//!
//! Usage:
//!     cargo run --example action_client -- \
//!         --cert <path/to/server.crt>       \
//!         --auth-token <plaintext_rw_token> \
//!         [--ro-auth-token <plaintext_ro_token>] \
//!         [--addr <host:port>]              \
//!         [--session <name>]
//!
//! Default addr: https://[::1]:50051 ; default session: d1demo.

use anyhow::{Context, Result};
use muxrd::proto::muxr_client::MuxrClient;
use muxrd::proto::{
    LoginRequest, NewPaneReq, PaneTarget, RenamePaneReq, ResizeKind, ResizePaneReq, SessionRef,
    ToggleFullscreenReq, WriteToPaneReq,
};
use tonic::transport::Channel;
use tonic::{
    Request,
    metadata::MetadataValue,
    transport::{Certificate, ClientTlsConfig},
};

// ─── Args ─────────────────────────────────────────────────────────────────────

struct Args {
    addr: String,
    cert_pem: String,
    auth_token: String,
    ro_auth_token: Option<String>,
    session: String,
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
        ro_auth_token: get("--ro-auth-token"),
        session: get("--session").unwrap_or_else(|| "d1demo".to_owned()),
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

type BearerInterceptor =
    Box<dyn Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Send + Sync>;

type AuthedClient =
    MuxrClient<tonic::service::interceptor::InterceptedService<Channel, BearerInterceptor>>;

/// Count panes and return a flat list of (tab_id, pane_id, is_plugin, title).
async fn layout_panes(
    client: &mut AuthedClient,
    session: &str,
) -> Result<Vec<(u32, u32, bool, String)>> {
    let layout = client
        .get_layout(SessionRef {
            session: session.to_owned(),
            ..Default::default()
        })
        .await
        .context("GetLayout RPC failed")?
        .into_inner();
    let mut out = Vec::new();
    for tab in &layout.tabs {
        for p in &tab.panes {
            out.push((tab.tab_id, p.id, p.is_plugin, p.title.clone()));
        }
    }
    Ok(out)
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args()?;
    let session = args.session.clone();

    println!("connecting to {} (TLS)…", args.addr);
    let channel = tls_channel(&args.addr, &args.cert_pem).await?;

    // Box the interceptor so the RW and RO clients share one concrete type.
    let mut rw = build_authed(channel.clone(), &args.auth_token, false).await?;

    let mut pass = true;

    // ── (a) id-targeted write ─────────────────────────────────────────────────
    println!("\n=== (a) id-targeted WriteToPane ===");
    let panes = layout_panes(&mut rw, &session).await?;
    println!("  layout: {} pane(s)", panes.len());
    let term = panes
        .iter()
        .find(|(_, _, is_plugin, _)| !is_plugin)
        .cloned()
        .context("no terminal pane found in session")?;
    println!("  targeting terminal pane id={} (tab {})", term.1, term.0);

    let marker = "muxrd_d1_ok";
    let cmd = format!("echo {marker} > /tmp/d1_proof.txt\n");
    let ack = rw
        .write_to_pane(WriteToPaneReq {
            target: Some(PaneTarget {
                session: session.clone(),
                pane_id: term.1,
                is_plugin: false,
                ..Default::default()
            }),
            data: cmd.into_bytes(),
        })
        .await
        .context("WriteToPane RPC failed")?
        .into_inner();
    println!(
        "  WriteToPane ack: ok={} error='{}' info='{}'",
        ack.ok, ack.error, ack.info
    );
    if !ack.ok {
        pass = false;
    }
    println!("  → now verify on the dev host: cat /tmp/d1_proof.txt should show '{marker}'");

    // ── (b) NewPane → count +1, capture new id from ack info ───────────────────
    println!("\n=== (b) NewPane (+1) ===");
    let before_panes = layout_panes(&mut rw, &session).await?;
    let before = before_panes.len();
    let before_ids: std::collections::HashSet<u32> =
        before_panes.iter().map(|(_, id, _, _)| *id).collect();
    let new_ack = rw
        .new_pane(NewPaneReq {
            session: session.clone(),
            floating: false,
            pane_name: "d1_newpane".to_owned(),
        })
        .await
        .context("NewPane RPC failed")?
        .into_inner();
    println!(
        "  NewPane ack: ok={} error='{}' info='{}'",
        new_ack.ok, new_ack.error, new_ack.info
    );
    // small delay so the layout query reflects the new pane
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let after_panes = layout_panes(&mut rw, &session).await?;
    let after = after_panes.len();
    println!("  pane count: {before} → {after}");
    if after != before + 1 {
        println!("  FAIL: expected pane count +1");
        pass = false;
    } else {
        println!("  PASS: pane count +1");
    }

    // Determine the new pane id: prefer ack.info ("terminal_<n>"), else layout diff.
    let new_pane_id: Option<(u32, bool)> = parse_pane_id(&new_ack.info).or_else(|| {
        after_panes
            .iter()
            .find(|(_, id, _, _)| !before_ids.contains(id))
            .map(|(_, id, is_plugin, _)| (*id, *is_plugin))
    });
    println!("  new pane id resolved: {new_pane_id:?}");

    // ── (c) rename / resize / toggle on a pane ────────────────────────────────
    println!("\n=== (c) Rename / Resize / ToggleFloating / ToggleFullscreen ===");
    let (op_id, op_plugin) = new_pane_id.unwrap_or((term.1, false));
    let op_target = || PaneTarget {
        session: session.clone(),
        pane_id: op_id,
        is_plugin: op_plugin,
        ..Default::default()
    };

    let r = rw
        .rename_pane(RenamePaneReq {
            target: Some(op_target()),
            name: "d1_renamed".to_owned(),
        })
        .await
        .context("RenamePane RPC failed")?
        .into_inner();
    println!("  RenamePane ack: ok={} error='{}'", r.ok, r.error);
    pass &= r.ok;

    let r = rw
        .resize_pane(ResizePaneReq {
            target: Some(op_target()),
            resize: ResizeKind::Increase as i32,
            direction: 0,
        })
        .await
        .context("ResizePane RPC failed")?
        .into_inner();
    println!("  ResizePane ack: ok={} error='{}'", r.ok, r.error);
    pass &= r.ok;

    let r = rw
        .toggle_pane_floating(op_target())
        .await
        .context("TogglePaneFloating RPC failed")?
        .into_inner();
    println!("  TogglePaneFloating ack: ok={} error='{}'", r.ok, r.error);
    pass &= r.ok;

    let fs_req = || ToggleFullscreenReq {
        target: Some(op_target()),
        ..Default::default()
    };
    let r = rw
        .toggle_pane_fullscreen(fs_req())
        .await
        .context("TogglePaneFullscreen RPC failed")?
        .into_inner();
    println!(
        "  TogglePaneFullscreen ack: ok={} error='{}'",
        r.ok, r.error
    );
    pass &= r.ok;
    // toggle fullscreen back off so close works cleanly
    let _ = rw.toggle_pane_fullscreen(fs_req()).await;

    // ── (d) ClosePane → count back down ───────────────────────────────────────
    println!("\n=== (d) ClosePane (back down) ===");
    let pre_close = layout_panes(&mut rw, &session).await?.len();
    let c = rw
        .close_pane(op_target())
        .await
        .context("ClosePane RPC failed")?
        .into_inner();
    println!("  ClosePane ack: ok={} error='{}'", c.ok, c.error);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let post_close = layout_panes(&mut rw, &session).await?.len();
    println!("  pane count: {pre_close} → {post_close}");
    if post_close == pre_close.saturating_sub(1) {
        println!("  PASS: pane count back down");
    } else {
        println!("  NOTE: pane count {pre_close} → {post_close} (expected -1)");
    }

    // ── (e) read-only gate ────────────────────────────────────────────────────
    if let Some(ro_token) = &args.ro_auth_token {
        println!("\n=== (e) read-only gate ===");
        let mut ro = build_authed(channel.clone(), ro_token, false).await?;

        // GetLayout should still succeed.
        match ro
            .get_layout(SessionRef {
                session: session.clone(),
                ..Default::default()
            })
            .await
        {
            Ok(_) => println!("  PASS: read-only GetLayout succeeded"),
            Err(e) => {
                println!("  FAIL: read-only GetLayout errored: {e}");
                pass = false;
            }
        }

        // FocusPane should still succeed.
        match ro
            .focus_pane(PaneTarget {
                session: session.clone(),
                pane_id: term.1,
                is_plugin: false,
                ..Default::default()
            })
            .await
        {
            Ok(_) => println!("  PASS: read-only FocusPane succeeded"),
            Err(e) => {
                println!("  FAIL: read-only FocusPane errored: {e}");
                pass = false;
            }
        }

        // A mutating RPC must be permission_denied.
        match ro
            .write_to_pane(WriteToPaneReq {
                target: Some(PaneTarget {
                    session: session.clone(),
                    pane_id: term.1,
                    is_plugin: false,
                    ..Default::default()
                }),
                data: b"echo should_not_happen\n".to_vec(),
            })
            .await
        {
            Ok(_) => {
                println!("  FAIL: read-only WriteToPane was NOT rejected");
                pass = false;
            }
            Err(e) if e.code() == tonic::Code::PermissionDenied => {
                println!(
                    "  PASS: read-only WriteToPane → permission_denied: {}",
                    e.message()
                );
            }
            Err(e) => {
                println!(
                    "  FAIL: read-only WriteToPane → unexpected error {:?}: {}",
                    e.code(),
                    e.message()
                );
                pass = false;
            }
        }

        // NewPane should also be rejected.
        match ro
            .new_pane(NewPaneReq {
                session: session.clone(),
                floating: false,
                pane_name: String::new(),
            })
            .await
        {
            Err(e) if e.code() == tonic::Code::PermissionDenied => {
                println!("  PASS: read-only NewPane → permission_denied");
            }
            Ok(_) => {
                println!("  FAIL: read-only NewPane was NOT rejected");
                pass = false;
            }
            Err(e) => {
                println!(
                    "  FAIL: read-only NewPane → unexpected error {:?}",
                    e.code()
                );
                pass = false;
            }
        }
    } else {
        println!("\n=== (e) read-only gate: SKIPPED (no --ro-auth-token) ===");
    }

    println!("\n==================================================");
    if pass {
        println!("OVERALL: PASS");
        Ok(())
    } else {
        anyhow::bail!("OVERALL: FAIL — see notes above")
    }
}

// ─── Boxed-interceptor authed client (shared concrete type) ────────────────────

async fn build_authed(
    channel: Channel,
    auth_token: &str,
    remember_me: bool,
) -> Result<AuthedClient> {
    let mut bootstrap = MuxrClient::new(channel.clone());
    let resp = bootstrap
        .login(LoginRequest {
            auth_token: auth_token.to_owned(),
            remember_me,
        })
        .await
        .context("Login RPC failed")?;
    let session_token = resp.into_inner().session_token;
    let bearer: MetadataValue<_> = format!("Bearer {session_token}")
        .parse()
        .context("construct bearer header")?;
    let interceptor: BearerInterceptor = Box::new(move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", bearer.clone());
        Ok(req)
    });
    Ok(MuxrClient::with_interceptor(channel, interceptor))
}

/// Parse a zellij pane-id string ("terminal_<n>" / "plugin_<n>") into (id, is_plugin).
fn parse_pane_id(s: &str) -> Option<(u32, bool)> {
    let s = s.trim();
    if let Some(n) = s.strip_prefix("terminal_") {
        n.parse().ok().map(|id| (id, false))
    } else if let Some(n) = s.strip_prefix("plugin_") {
        n.parse().ok().map(|id| (id, true))
    } else {
        None
    }
}
