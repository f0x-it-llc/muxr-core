//! attach_client — B3 end-to-end test client for `AttachTerminal` over TLS + bearer auth.
//!
//! Flow:
//! 1. Connect to the server over TLS, trusting the self-signed cert via its PEM
//!    (loaded from a file or read from stdin; no system CA bundle needed).
//! 2. Call `Login(auth_token, remember_me=false)` → receive a session token.
//! 3. Open `AttachTerminal` with `authorization: Bearer <session_token>` header.
//! 4. Send `AttachReq`, dismiss the startup overlay, type the marker command,
//!    receive renders.
//!
//! Also demonstrates the **negative path** (if `--negative-test` is passed):
//! attempts `AttachTerminal` with a bogus / missing bearer and expects
//! `Status::unauthenticated`.
//!
//! Usage:
//!     cargo run --example attach_client -- \
//!         --cert <path/to/server.crt>      \
//!         --auth-token <plaintext_auth_tok> \
//!         [--addr <host:port>]             \
//!         [--session <name>]               \
//!         [--type <TEXT>]                  \
//!         [--rows N] [--cols N]            \
//!         [--negative-test]
//!
//! Default addr: https://[::1]:50051 ; default session: b3demo.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use muxrd::proto::client_frame::Kind as ClientKind;
use muxrd::proto::muxr_client::MuxrClient;
use muxrd::proto::server_frame::Kind as ServerKind;
use muxrd::proto::{AttachReq, ClientFrame, LoginRequest};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    metadata::MetadataValue,
    transport::{Certificate, Channel, ClientTlsConfig},
};

// ─── Args ─────────────────────────────────────────────────────────────────────

struct Args {
    addr: String,
    cert_pem: String,
    auth_token: String,
    session: String,
    type_text: String,
    rows: u32,
    cols: u32,
    negative_test: bool,
    /// How long to stay attached receiving renders/control events (seconds).
    window_secs: u64,
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

    // Load the self-signed server cert PEM from a file (for client trust anchoring).
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
        session: get("--session").unwrap_or_else(|| "b3demo".to_owned()),
        type_text: get("--type")
            .unwrap_or_else(|| "echo muxrd_b3_ok > /tmp/b3_proof.txt\n".to_owned())
            .replace("\\n", "\n"),
        rows: get("--rows").and_then(|v| v.parse().ok()).unwrap_or(24),
        cols: get("--cols").and_then(|v| v.parse().ok()).unwrap_or(80),
        negative_test: argv.iter().any(|a| a == "--negative-test"),
        window_secs: get("--window").and_then(|v| v.parse().ok()).unwrap_or(7),
    })
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn client_frame(kind: ClientKind) -> ClientFrame {
    ClientFrame { kind: Some(kind) }
}

/// Build a TLS channel that trusts the given self-signed cert PEM.
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

    // ── Negative test ────────────────────────────────────────────────────────
    if args.negative_test {
        println!("--- NEGATIVE TEST: AttachTerminal with no bearer ---");
        // Send AttachReq without any authorization header.
        let (tx, rx) = mpsc::channel::<ClientFrame>(1);
        tx.send(client_frame(ClientKind::Attach(AttachReq {
            session: args.session.clone(),
            rows: args.rows,
            cols: args.cols,
            // No resume hint: this harness always attaches to the session's
            // currently-focused pane (the additive resume_* fields default to unset).
            ..Default::default()
        })))
        .await?;
        drop(tx);

        let result = client.attach_terminal(ReceiverStream::new(rx)).await;

        match result {
            Err(status) if status.code() == tonic::Code::Unauthenticated => {
                println!(
                    "PASS (negative): AttachTerminal rejected with Unauthenticated — {}",
                    status.message()
                );
            }
            Err(e) => {
                bail!("FAIL (negative): expected Unauthenticated, got: {e}");
            }
            Ok(_) => {
                bail!("FAIL (negative): AttachTerminal succeeded without a bearer token!");
            }
        }
        println!();
    }

    // ── Positive path: Login → bearer → AttachTerminal ───────────────────────
    println!("--- Login ---");
    let login_resp = client
        .login(LoginRequest {
            auth_token: args.auth_token.clone(),
            remember_me: false,
        })
        .await
        .context("Login RPC failed")?;

    let session_token = login_resp.into_inner().session_token;
    println!("Login: received session token ({}…)", &session_token[..8]);

    // ── AttachTerminal with bearer ────────────────────────────────────────────
    println!("--- AttachTerminal (bearer auth) ---");

    // Build a client with the bearer token injected into every request.
    let bearer: MetadataValue<_> = format!("Bearer {session_token}")
        .parse()
        .context("construct bearer header")?;

    let mut authed_client =
        MuxrClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
            req.metadata_mut().insert("authorization", bearer.clone());
            Ok(req)
        });

    let (tx, rx) = mpsc::channel::<ClientFrame>(32);
    let outbound = ReceiverStream::new(rx);

    // 1. First frame: AttachReq.
    tx.send(client_frame(ClientKind::Attach(AttachReq {
        session: args.session.clone(),
        rows: args.rows,
        cols: args.cols,
        // No resume hint (see the negative-test attach above).
        ..Default::default()
    })))
    .await
    .context("send AttachReq")?;
    println!(
        "sent AttachReq{{session={}, {}x{}}}",
        args.session, args.rows, args.cols
    );

    let response = authed_client
        .attach_terminal(outbound)
        .await
        .context("AttachTerminal RPC failed")?;
    let mut inbound = response.into_inner();

    // 2. Input task: dismiss overlay, type the marker command.
    let tx_input = tx.clone();
    let type_text = args.type_text.clone();
    let input_task = tokio::spawn(async move {
        // Give the first render time to flush.
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Dismiss startup overlay: ESC × 3 then Enter (A2 recipe).
        for _ in 0..3 {
            let _ = tx_input
                .send(client_frame(ClientKind::Input(vec![0x1b])))
                .await;
            tokio::time::sleep(Duration::from_millis(180)).await;
        }
        let _ = tx_input
            .send(client_frame(ClientKind::Input(vec![0x0d])))
            .await;
        tokio::time::sleep(Duration::from_millis(400)).await;

        println!("typing marker command: {type_text:?}");
        let _ = tx_input
            .send(client_frame(ClientKind::Input(type_text.into_bytes())))
            .await;
    });

    // 3. Receive renders + control events.
    //    Default window is 7 s; pass --window N to extend (useful for C2
    //    testing where you need time to run rename-session / delete-session
    //    from another shell while the client is attached).
    let window_secs = args.window_secs;

    let mut renders: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut control_events: Vec<(String, String)> = Vec::new();
    let recv_window = tokio::time::sleep(Duration::from_secs(window_secs));
    tokio::pin!(recv_window);

    loop {
        tokio::select! {
            msg = inbound.message() => {
                match msg {
                    Ok(Some(sf)) => match sf.kind {
                        Some(ServerKind::Render(bytes)) => {
                            renders += 1;
                            total_bytes += bytes.len() as u64;
                            if renders == 1 {
                                println!("first render: {} bytes", bytes.len());
                            }
                        }
                        Some(ServerKind::Control(c)) => {
                            println!("[CONTROL EVENT] kind={} payload={:?}", c.kind, c.payload);
                            let is_exit = c.kind == "exit";
                            control_events.push((c.kind, c.payload));
                            // Exit means the stream will close; break cleanly.
                            if is_exit {
                                println!("received exit control event — stream ending");
                                break;
                            }
                        }
                        None => {}
                    },
                    Ok(None) => {
                        println!("server closed the render stream");
                        break;
                    }
                    Err(e) => {
                        eprintln!("render stream error: {e}");
                        break;
                    }
                }
            }
            _ = &mut recv_window => {
                println!("recv window elapsed ({window_secs}s) — disconnecting");
                break;
            }
        }
    }

    input_task.abort();
    drop(tx);

    println!("--------------------------------------------------");
    println!("renders received  : {renders}");
    println!("render bytes      : {total_bytes}");
    println!("control events    : {}", control_events.len());
    for (k, v) in &control_events {
        println!("  [{k}] {v}");
    }
    println!("--------------------------------------------------");

    if renders == 0 {
        bail!("FAIL: no render frames received");
    }
    println!("PASS: received {renders} render frame(s) over TLS+bearer gRPC");
    Ok(())
}
