//! muxrd binary — Phase-E CLI entrypoint.
//!
//! Subcommands:
//!   init           — ensure data dir + TLS cert exist (idempotent); accepts --san
//!   create-token   — create an API token and print it once
//!   list-tokens    — list all tokens (name / created_at / read_only)
//!   revoke-token   — revoke a token by name
//!   config --show  — print the effective config
//!   start          — start the TLS gRPC server (foreground or --daemonize); accepts --san
//!   status         — report whether a server is running (control socket)
//!   stop           — stop a running server (control socket / SIGTERM fallback)
//!
//! Precedence: CLI --bind > MUXRD_BIND env > config file > defaults.
//! SAN precedence: CLI --san values merged with MUXRD_SAN env (comma-separated).
//!
//! ## Daemonization (fork-before-runtime)
//!
//! `start --daemonize` calls [`daemonize::Daemonize::start`] **before** the
//! tokio runtime is constructed.  This matters: `fork()` in a multi-threaded
//! process is unsafe (only the calling thread survives in the child, leaving
//! mutexes/runtime state poisoned).  By forking while the process is still
//! single-threaded (`main` runs without `#[tokio::main]`), the runtime is built
//! fresh in the detached child via [`serve`] → `Runtime::new()`.

use std::path::PathBuf;
use std::time::Instant;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use muxrd::auth::BearerAuthLayer;
use muxrd::cli::{BackendKind, Cli, Command, InitArgs, StartArgs};
use muxrd::config::{self, CertSource, check_h2c_bind_safety};
use muxrd::control::{self, ControlRequest, ControlResponse};
use muxrd::grpc::MuxrService;
use muxrd::multiplexer::detect;
use muxrd::multiplexer::{BackendSet, HerdrBackend, MuxBackend, ZellijBackend};
use muxrd::proto::muxr_server::MuxrServer;
use muxrd::ratelimit::RateLimitLayer;
use muxrd::tls::SanEntry;
use tonic::transport::{Identity, Server, ServerTlsConfig};

/// Pidfile name inside the data dir.
const PIDFILE_NAME: &str = "muxrd.pid";

/// `main` is intentionally NOT `#[tokio::main]`: daemonization forks before the
/// runtime exists, so the runtime must be built inside the (post-fork) child.
fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let bind_override = cli.bind.clone();

    match cli.command {
        // start may daemonize → it owns runtime construction (post-fork).
        Command::Start(args) => cmd_start(bind_override.as_deref(), args),
        // status/stop are pure-sync IPC; no tokio needed.
        Command::Status => cmd_status(bind_override.as_deref()),
        Command::Stop => cmd_stop(),
        // The remaining subcommands are short-lived; run them on a small runtime.
        other => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build tokio runtime")?;
            rt.block_on(async move {
                install_crypto_provider();
                match other {
                    Command::Init(args) => cmd_init(bind_override.as_deref(), args).await,
                    Command::CreateToken(args) => cmd_create_token(args),
                    Command::ListTokens => cmd_list_tokens(),
                    Command::RevokeToken(args) => cmd_revoke_token(args),
                    Command::Config(args) => cmd_config(bind_override.as_deref(), args),
                    // start/status/stop handled above.
                    Command::Start(_) | Command::Status | Command::Stop => unreachable!(),
                }
            })
        }
    }
}

/// Install the ring crypto provider before any TLS operation.
///
/// Both ring and aws-lc-rs are pulled in transitively (zellij-utils brings
/// aws-lc-rs, tonic's tls-ring brings ring).  Without an explicit default
/// provider, rustls 0.23 panics when building the TLS config.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Path to the pidfile inside the data dir.
fn pidfile_path() -> Result<PathBuf> {
    Ok(config::data_dir()?.join(PIDFILE_NAME))
}

/// Env var that bypasses the zellij version-mismatch check (review Major F).
/// Referenced in error messages; the actual skip logic lives in
/// `muxrd::multiplexer::detect::probe_zellij`.
const SKIP_VERSION_CHECK_ENV: &str = "MUXRD_SKIP_VERSION_CHECK";

/// Refuse to start when the **installed** `zellij` binary's version differs from
/// the zellij crate this server was compiled against.
///
/// Delegates the actual probe (binary discovery, `zellij --version` run,
/// version comparison, and `MUXRD_SKIP_VERSION_CHECK` handling) to
/// [`detect::probe_zellij`] so the startup check and backend auto-detection
/// share a single implementation.
///
/// Returns the resolved zellij binary path so the caller can reuse it for
/// `create_session` (TOCTOU fix: resolve once, pass everywhere).
fn check_zellij_version() -> Result<PathBuf> {
    if std::env::var(SKIP_VERSION_CHECK_ENV).is_ok_and(|v| !v.is_empty() && v != "0") {
        log::warn!("{SKIP_VERSION_CHECK_ENV} set — skipping zellij version-mismatch check");
    }
    if !detect::probe_zellij() {
        let linked = zellij_utils::consts::VERSION;
        anyhow::bail!(
            "zellij version check failed: the zellij binary was not found on PATH or its \
             version does not match the linked IPC contract {linked}. \
             Install zellij {linked}, or set {SKIP_VERSION_CHECK_ENV}=1 to proceed anyway.",
        );
    }
    // probe_zellij passed → the binary is present and version-matched.
    // Resolve its path for the caller (cheap second lookup on the same PATH).
    muxrd::actions::which_zellij().context("locate zellij binary (post version-check)")
}

/// Collect extra SANs from CLI `--san` values and the `MUXRD_SAN` env var.
///
/// The env var supplements (does not replace) CLI flags.  Deduplication is not
/// performed; duplicates in `rcgen`'s SAN list are harmless.
///
/// Also auto-includes the bind IP when it is non-loopback (e.g. a LAN IP
/// or a LAN address) so the cert is valid for the address the server is actually
/// serving on.
fn collect_sans(cli_sans: &[String], bind_addr: &str) -> Vec<SanEntry> {
    let mut sans: Vec<SanEntry> = cli_sans.iter().map(|s| SanEntry::parse(s)).collect();

    // Merge from env var.
    sans.extend(SanEntry::from_env());

    // Auto-include the bind IP when it is non-loopback.
    if let Some(host) = bind_addr.rsplit_once(':').map(|(h, _)| h) {
        let host = host.trim_matches(|c| c == '[' || c == ']'); // strip IPv6 brackets
        if let Ok(ip) = host.parse::<std::net::IpAddr>()
            && !ip.is_loopback()
            && !ip.is_unspecified()
        {
            let entry = SanEntry::Ip(ip);
            if !sans.contains(&entry) {
                log::debug!("tls: auto-including non-loopback bind IP {ip} in cert SANs");
                sans.push(entry);
            }
        }
    }

    sans
}

// ── init ─────────────────────────────────────────────────────────────────────

async fn cmd_init(bind_override: Option<&str>, args: InitArgs) -> Result<()> {
    // Ensure the data dir exists (config::data_dir() creates it).
    let data_dir = config::data_dir().context("ensure data dir")?;
    println!("Data dir : {}", data_dir.display());

    // Ensure config file exists (writes a template if absent).
    let cfg_path = config::ensure_config_file().context("ensure config file")?;
    println!("Config   : {}", cfg_path.display());

    // Resolve the effective bind address to determine whether to auto-include
    // the bind IP in the cert SANs.
    let cfg = config::resolve(bind_override)?;

    // Collect SANs from CLI + env + bind IP.
    let extra_sans = collect_sans(&args.san, &cfg.bind_addr);

    // Use the same cert-source resolution as `cmd_start` (env fallbacks +
    // precedence + mutual-exclusion validation — no hand-rolled match here).
    let cert_source = config::resolve_cert_source(
        args.tls_cert.clone(),
        args.tls_key.clone(),
        args.insecure_h2c,
    )
    .context("invalid TLS cert configuration")?;

    match &cert_source {
        CertSource::External { cert, key } => {
            // Validate that the cert+key can actually be loaded now (gives the
            // operator early feedback at `init` time rather than at `start`).
            muxrd::tls::load_external_identity(cert, key).with_context(|| {
                format!(
                    "failed to validate external TLS cert '{}' and key '{}'",
                    cert.display(),
                    key.display()
                )
            })?;
            println!("Cert     : {} (external — valid)", cert.display());
            println!("Key      : {} (external — valid)", key.display());

            // Print the SANs covered by the cert (informational; ext cert SANs are
            // whatever the CA issued — we just validate the files are loadable).
            let builtin = ["127.0.0.1", "localhost"];
            let extra_desc: Vec<String> = extra_sans
                .iter()
                .map(|s| match s {
                    SanEntry::Ip(ip) => ip.to_string(),
                    SanEntry::Dns(d) => d.clone(),
                })
                .collect();
            let all_sans: Vec<&str> = builtin
                .iter()
                .copied()
                .chain(extra_desc.iter().map(String::as_str))
                .collect();
            println!(
                "Cert SANs: {} (note: external cert SANs are fixed by the issuer)",
                all_sans.join(", ")
            );
        }
        CertSource::H2c => {
            // No cert needed — plaintext h2c delegates TLS to the proxy.
            println!("Cert     : none (h2c — TLS is handled by the reverse proxy)");

            // Advisory: cmd_start hard-blocks non-loopback h2c without the ack
            // flag.  cmd_init doesn't bind, so there is no security gap here,
            // but surfacing the same note helps the operator anticipate what
            // `start` will require.
            if let Ok(addr) = cfg.bind_addr.parse::<std::net::SocketAddr>()
                && !addr.ip().is_loopback()
            {
                println!(
                    "Note     : h2c + non-loopback bind ({}) — \
                         `start` will require --i-know-this-is-behind-a-proxy",
                    cfg.bind_addr
                );
            }
        }
        CertSource::SelfSigned => {
            // Load or generate the self-signed cert+key (idempotent w.r.t. SANs).
            let (_identity, _cert_pem) = muxrd::tls::load_or_generate_identity(&extra_sans)
                .context("failed to load/generate TLS certificate")?;

            // Print the cert file path.
            let cert_path = data_dir.join("server.crt");
            println!("Cert     : {}", cert_path.display());
            println!("Key      : {}", data_dir.join("server.key").display());

            // Print the SANs covered by the cert.
            let builtin = ["127.0.0.1", "localhost"];
            let extra_desc: Vec<String> = extra_sans
                .iter()
                .map(|s| match s {
                    SanEntry::Ip(ip) => ip.to_string(),
                    SanEntry::Dns(d) => d.clone(),
                })
                .collect();
            let all_sans: Vec<&str> = builtin
                .iter()
                .copied()
                .chain(extra_desc.iter().map(String::as_str))
                .collect();
            println!("Cert SANs: {}", all_sans.join(", "));
        }
    }

    // Print the effective bind address.
    let is_default = bind_override.is_none()
        && std::env::var("MUXRD_BIND").is_err()
        && cfg.bind_addr == config::DEFAULT_BIND;
    if is_default {
        println!("Bind     : {} (default)", cfg.bind_addr);
    } else {
        println!("Bind     : {}", cfg.bind_addr);
    }

    println!("\nInit complete. Run `muxrd start` to serve.");
    Ok(())
}

// ── create-token ─────────────────────────────────────────────────────────────

fn cmd_create_token(args: muxrd::cli::CreateTokenArgs) -> Result<()> {
    use zellij_utils::web_authentication_tokens::create_token;

    // Resolve any expiry BEFORE minting so a bad --expires-in fails cleanly.
    let expires_at = match args.expires_in.as_deref() {
        Some(spec) => muxrd::token_expiry::parse_expires_in(spec)?,
        None => None,
    };

    // create_token returns (plaintext_token, name).
    let (token, actual_name) =
        create_token(args.name.clone(), args.read_only).context("failed to create token")?;

    // Record the opt-in muxr-side expiry keyed by the token's hash.
    if let Some(exp) = expires_at {
        muxrd::token_expiry::set_expiry(&token, exp).context("failed to record token expiry")?;
    }

    println!("Token created successfully.");
    println!();
    println!("  Name      : {actual_name}");
    println!("  Read-only : {}", args.read_only);
    println!(
        "  Expires   : {}",
        match expires_at {
            Some(exp) => format!(
                "in {} (epoch {exp})",
                args.expires_in.as_deref().unwrap_or("")
            ),
            None => "never".to_string(),
        }
    );
    println!();
    println!("  TOKEN: {token}");
    println!();
    println!("Store this token now — it will NOT be shown again.");
    Ok(())
}

// ── list-tokens ───────────────────────────────────────────────────────────────

fn cmd_list_tokens() -> Result<()> {
    use zellij_utils::web_authentication_tokens::list_tokens;

    let tokens = list_tokens().context("failed to list tokens")?;

    if tokens.is_empty() {
        println!("No tokens found.");
        return Ok(());
    }

    // Column widths: at least the header width, but expand for content.
    let name_w = tokens
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(0)
        .max(4);
    let date_w = tokens
        .iter()
        .map(|t| t.created_at.len())
        .max()
        .unwrap_or(0)
        .max(13); // len("Created (UTC)")

    println!(
        "{:<name_w$}  {:<date_w$}  Read-only",
        "Name",
        "Created (UTC)",
        name_w = name_w,
        date_w = date_w,
    );
    println!("{}", "-".repeat(name_w + 2 + date_w + 2 + 9));

    for t in &tokens {
        println!(
            "{:<name_w$}  {:<date_w$}  {}",
            t.name,
            t.created_at,
            t.read_only,
            name_w = name_w,
            date_w = date_w,
        );
    }

    Ok(())
}

// ── revoke-token ─────────────────────────────────────────────────────────────

fn cmd_revoke_token(args: muxrd::cli::RevokeTokenArgs) -> Result<()> {
    use zellij_utils::web_authentication_tokens::revoke_token;

    let removed = revoke_token(&args.name).context("failed to revoke token")?;
    if removed {
        println!("Token '{}' revoked successfully.", args.name);
    } else {
        println!(
            "Token '{}' not found (already revoked or never existed).",
            args.name
        );
    }
    Ok(())
}

// ── config ────────────────────────────────────────────────────────────────────

fn cmd_config(bind_override: Option<&str>, args: muxrd::cli::ConfigArgs) -> Result<()> {
    // Always ensure the config file exists.
    let cfg_path = config::ensure_config_file().context("ensure config file")?;

    if args.show {
        let cfg = config::resolve(bind_override)?;
        println!("Effective configuration:");
        println!("{}", cfg.display());
    } else {
        println!("Config file: {}", cfg_path.display());
        println!("Run `muxrd config --show` to print the effective values.");
    }
    Ok(())
}

// ── start ─────────────────────────────────────────────────────────────────────

/// `start [--daemonize] [--san <san>...] [--backend <zellij|herdr>]`.
///
/// In foreground mode this constructs a tokio runtime and serves directly.  In
/// daemon mode it forks FIRST (via `daemonize`, while still single-threaded),
/// and only the detached child builds the runtime and serves.
fn cmd_start(bind_override: Option<&str>, args: StartArgs) -> Result<()> {
    // ── Backend restriction: CLI wins over MUXRD_BACKEND env ────────────────
    // `--backend`/`MUXRD_BACKEND` is now an OPTIONAL restriction (Phase 3):
    //   - unset      → serve ALL detected backends (new default).
    //   - set        → serve ONLY that backend (Phase 2 selector behaviour).
    // The CLI flag wins over the env var (per DEVELOPMENT.md).
    let backend_override: Option<BackendKind> = args.backend.or_else(|| {
        std::env::var("MUXRD_BACKEND")
            .ok()
            .and_then(|v| match v.to_lowercase().as_str() {
                "zellij" => Some(BackendKind::Zellij),
                "herdr" => Some(BackendKind::Herdr),
                "" => None,
                other => {
                    log::warn!(
                        "MUXRD_BACKEND={other:?} is not a recognized backend; \
                         ignoring (serving all detected backends). \
                         Valid values: zellij, herdr."
                    );
                    None
                }
            })
    });

    // ── Auto-detect which backends are usable (fail fast if none) ────────────
    // `detect_backends(None)` probes every backend and returns the subset that
    // passes; `detect_backends(Some(kind))` probes only the requested one and
    // errors if it is unavailable. Either way an empty result is impossible —
    // the helper bails with a descriptive error, which we surface as a non-zero
    // exit.
    let resolved_kinds = detect::detect_backends(backend_override)
        .context("no usable multiplexer backend available")?;

    // ── Zellij version assert (only when zellij is among the served set) ─────
    // Major F: refuse to start when the installed `zellij` binary's version
    // disagrees with the zellij crate we linked against — a mismatch causes
    // silent IPC decode failures at runtime. `detect_backends` already probed
    // the version, but we keep the explicit assert for its detailed error
    // message. The herdr-only path skips it: herdr has no zellij dependency.
    if resolved_kinds.contains(&BackendKind::Zellij) {
        let _zellij_bin = check_zellij_version()?;
        // Note: _zellij_bin is currently unused here because create_session
        // resolves it internally. Future refactor can thread it through
        // actions::create_session for full TOCTOU elimination; the path is
        // resolved once and the version verified.
    } else {
        log::info!(
            "backend: zellij not in served set — skipping zellij version check \
             (herdr has no zellij dependency)"
        );
    }

    // ── Construct each detected backend into the ordered BackendSet ──────────
    // A concrete `Arc<HerdrBackend>` is retained (when present) so `serve()` can
    // start the herdr event kernel (task 01) sharing the backend's id registries;
    // the same `Arc` is also stored in the set as `Arc<dyn MuxBackend>`.
    let mut backend_entries: Vec<(BackendKind, Arc<dyn MuxBackend>)> =
        Vec::with_capacity(resolved_kinds.len());
    let mut herdr_backend: Option<Arc<HerdrBackend>> = None;
    for kind in &resolved_kinds {
        match kind {
            BackendKind::Zellij => {
                log::info!("backend: zellij (Unix-domain IPC)");
                backend_entries.push((BackendKind::Zellij, Arc::new(ZellijBackend)));
            }
            BackendKind::Herdr => {
                let b = Arc::new(HerdrBackend::from_env());
                log::info!("backend: herdr ({})", b.backend_version());
                herdr_backend = Some(Arc::clone(&b));
                backend_entries.push((BackendKind::Herdr, b as Arc<dyn MuxBackend>));
            }
        }
    }
    let backends = BackendSet::new(backend_entries);
    log::debug!(
        "serving backends: {:?}",
        backends.kinds().collect::<Vec<_>>()
    );

    let cfg = config::resolve(bind_override).context("resolve config")?;
    let addr: std::net::SocketAddr = cfg
        .bind_addr
        .parse()
        .with_context(|| format!("invalid bind address '{}'", cfg.bind_addr))?;

    // Resolve cert source from CLI args (applies precedence: h2c > external > self-signed).
    let cert_source = config::resolve_cert_source(
        args.tls_cert.clone(),
        args.tls_key.clone(),
        args.insecure_h2c,
    )
    .context("invalid TLS cert configuration")?;

    // Resolve the h2c non-loopback acknowledgement (CLI flag OR env var).
    let h2c_allow_public = args.h2c_allow_public || {
        std::env::var("MUXRD_H2C_ALLOW_PUBLIC")
            .ok()
            .is_some_and(|v| !v.is_empty() && v != "0")
    };

    // Security guard: refuse non-loopback h2c without explicit operator ack.
    check_h2c_bind_safety(&cert_source, addr, h2c_allow_public)
        .context("h2c bind-safety check failed")?;

    // Collect extra SANs for the TLS cert (only relevant for self-signed, but harmless to collect).
    let extra_sans = collect_sans(&args.san, &cfg.bind_addr);

    let pidfile = pidfile_path()?;

    // ── Guard against clobbering a LIVE daemon ──────────────────────────────
    //
    // Both foreground and daemon start run these checks BEFORE any destructive
    // file cleanup (incident 2026-07-12): a transient control-socket query
    // failure must NEVER be taken as proof of staleness and lead us to delete a
    // live daemon's pidfile/socket.
    //
    // Upgrade scenario (binary replaced in place + the old daemon still
    // running): the new `start` refuses here naming the old pid; `stop` still
    // works (control socket, or SIGTERM via the pidfile we did NOT delete); a
    // subsequent `start` then succeeds once the old daemon has exited.
    let control_responsive = control::query(&ControlRequest::Status).is_ok();
    let pidfile_pid = read_pidfile();
    let pid_alive = pidfile_pid.is_some_and(pid_is_alive);
    match start_staleness_decision(control_responsive, pidfile_pid, pid_alive) {
        StartDecision::RefuseSocketResponsive => anyhow::bail!(
            "a muxrd appears to already be running (control socket responsive). \
             Run `muxrd stop` first."
        ),
        StartDecision::RefuseLiveDaemon(pid) => anyhow::bail!(
            "a muxrd appears to already be running (pid {pid}; control socket \
             unresponsive). Run `muxrd stop` or `kill {pid}` first; if pid {pid} \
             is not actually muxrd (verify with `ps -p {pid}`), remove {} and retry.",
            pidfile.display()
        ),
        StartDecision::Stale => {}
    }

    // Pre-flight bind probe: if the port is already held (e.g. an old daemon
    // whose pidfile/socket were previously lost) bail BEFORE the destructive
    // cleanup + fork below — otherwise we would delete state we cannot replace
    // and the forked child would just die on `Address already in use`.  See
    // `probe_bind_available` for the (accepted) TOCTOU window.
    probe_bind_available(addr)?;

    if args.daemonize {
        // Genuinely stale pidfile/socket from a crashed run → clean up before
        // forking.  Safe now: we proved above no live daemon owns this host and
        // the bind address is free.
        let _ = std::fs::remove_file(&pidfile);
        control::cleanup();

        // Open the log file for the child's stdout/stderr.
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&cfg.log_path)
            .with_context(|| format!("open log file {}", cfg.log_path.display()))?;
        let log_err = log_file
            .try_clone()
            .context("clone log file handle for stderr")?;

        // CRITICAL: fork BEFORE building the tokio runtime.  daemonize does a
        // double-fork + setsid + pidfile + stdio redirect; in the parent it
        // exits immediately (so the CLI returns), and the detached child
        // returns Ok(()) here and proceeds to build the runtime in serve().
        let daemon = daemonize::Daemonize::new()
            .pid_file(&pidfile)
            .working_directory(config::data_dir()?)
            .stdout(log_file)
            .stderr(log_err);

        match daemon.start() {
            Ok(()) => {
                // We are the detached child.  stdout/stderr now go to the log
                // file; re-init the logger so log::info! lands there too.
                let _ = env_logger::Builder::from_env(
                    env_logger::Env::default().default_filter_or("info"),
                )
                .try_init();
                log::info!("muxrd daemonized (pid {})", std::process::id());
            }
            Err(e) => {
                anyhow::bail!("daemonize failed: {e}");
            }
        }
        // From here we are the child; pidfile is managed by daemonize and will
        // be removed when the process exits (daemonize holds a flock on it).
    } else {
        // Foreground: write our own pidfile (removed on clean exit below).
        std::fs::write(&pidfile, std::process::id().to_string())
            .with_context(|| format!("write pidfile {}", pidfile.display()))?;
    }

    // ── Build the tokio runtime in THIS process (post-fork in daemon mode) ───
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let result = rt.block_on(serve(
        addr,
        cfg.bind_addr.clone(),
        args.daemonize,
        extra_sans,
        cert_source,
        backends,
        herdr_backend,
    ));

    // Foreground cleanup (in daemon mode daemonize owns the pidfile).
    if !args.daemonize {
        let _ = std::fs::remove_file(&pidfile);
    }
    control::cleanup();

    result
}

/// Resolve a TLS identity from the given [`CertSource`].
///
/// Returns `Some((Identity, cert_pem_string))` for TLS modes, or `None` for
/// h2c (plaintext HTTP/2 — no identity needed).
///
/// This is a pure(-ish) helper extracted for unit testability.
fn cert_identity(
    cert_source: &CertSource,
    extra_sans: &[SanEntry],
) -> Result<Option<(Identity, String)>> {
    match cert_source {
        CertSource::SelfSigned => {
            let pair = muxrd::tls::load_or_generate_identity(extra_sans)
                .context("failed to load/generate self-signed TLS certificate")?;
            Ok(Some(pair))
        }
        CertSource::External { cert, key } => {
            let pair = muxrd::tls::load_external_identity(cert, key).with_context(|| {
                format!(
                    "failed to load external TLS cert '{}' and key '{}'",
                    cert.display(),
                    key.display()
                )
            })?;
            log::info!(
                "tls: using external certificate '{}' with key '{}'",
                cert.display(),
                key.display()
            );
            Ok(Some(pair))
        }
        CertSource::H2c => Ok(None),
    }
}

/// Build the gRPC server, wire up the control socket + graceful shutdown,
/// and serve until shutdown is requested.
///
/// TLS behaviour is driven by `cert_source`:
/// - `SelfSigned` → load or generate the self-signed cert; serve over TLS.
/// - `External`   → load the caller-supplied cert+key; serve over TLS.
/// - `H2c`        → serve plaintext HTTP/2; **MUST** sit behind a trusted TLS proxy.
///
/// `backends` is the resolved [`BackendSet`] (every detected backend: zellij
/// and/or herdr), constructed and logged by [`cmd_start`] before the runtime is
/// entered. `herdr_backend` is the concrete `Arc<HerdrBackend>` when herdr is in
/// the set (else `None`); it is used only to start the herdr event kernel (task
/// 01) so it shares the backend's id registries.
async fn serve(
    addr: std::net::SocketAddr,
    bind_addr: String,
    quiet: bool,
    extra_sans: Vec<SanEntry>,
    cert_source: CertSource,
    backends: BackendSet,
    herdr_backend: Option<Arc<HerdrBackend>>,
) -> Result<()> {
    install_crypto_provider();

    let cert_mode = cert_source.mode();

    // ── TLS / h2c transport setup ────────────────────────────────────────────
    let maybe_tls: Option<ServerTlsConfig> = match cert_identity(&cert_source, &extra_sans)? {
        Some((identity, cert_pem)) => {
            // Print the cert PEM so test clients can capture it (foreground
            // only — in daemon mode this lands in the logfile, which is fine).
            // Skip for h2c (no cert to print — handled by the None arm below).
            if !quiet {
                println!("=== SERVER CERT PEM ===");
                print!("{cert_pem}");
                println!("=== END SERVER CERT PEM ===");
            }
            Some(ServerTlsConfig::new().identity(identity))
        }
        None => {
            // H2c — no TLS.
            let h2c_warning = "\
╔══════════════════════════════════════════════════════════════════╗\n\
║  WARNING: muxrd is serving UNENCRYPTED h2c (plaintext)   ║\n\
║  All data — including API tokens and terminal output — travels   ║\n\
║  in the clear.  This mode MUST sit behind a TLS-terminating      ║\n\
║  reverse proxy (e.g. Traefik + Let's Encrypt, Cloudflare).       ║\n\
║  NEVER expose this port directly to the internet or an           ║\n\
║  untrusted network.                                               ║\n\
╚══════════════════════════════════════════════════════════════════╝";
            // Emit to stderr so the warning is visible in the foreground path
            // (log output may be redirected / suppressed in daemon mode).
            eprintln!("{h2c_warning}");
            log::warn!("{h2c_warning}");
            None
        }
    };

    let service = MuxrService::with_backends(backends);

    // ── Control socket + graceful shutdown signal ────────────────────────────
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // A clone-able shutdown signal (task 01): the control-socket oneshot drives
    // `serve_with_shutdown` exactly as before; when it fires we also flip this
    // `watch` so long-lived background tasks (the herdr event kernel) observe the
    // same shutdown and exit cleanly.
    let (kernel_shutdown_tx, kernel_shutdown_rx) = tokio::sync::watch::channel(false);
    // Pass the cert_mode so `status` can report the active transport mode.
    control::spawn_listener(
        bind_addr.clone(),
        Instant::now(),
        shutdown_tx,
        service.clients(),
        cert_mode,
    )
    .context("failed to start control socket")?;

    // ── herdr event kernel (task 01) ─────────────────────────────────────────
    // When a herdr backend is present, spawn the persistent `events.subscribe`
    // consumer, publishing agent-status transitions onto a process-local
    // broadcast bus. The bus handle is kept in scope so the notifications
    // notifier (task 05) can consume it later; today it has no consumer and the
    // kernel's sends are simply dropped. A zellij-only server spawns nothing and
    // creates no bus (zero behaviour change).
    let _event_bus: Option<muxrd::multiplexer::events::EventBus> = match herdr_backend {
        Some(backend) => {
            let (bus, _rx0) =
                tokio::sync::broadcast::channel(muxrd::multiplexer::events::EVENT_BUS_CAPACITY);
            muxrd::multiplexer::spawn_event_kernel(
                backend.as_ref(),
                bus.clone(),
                kernel_shutdown_rx,
            );
            Some(bus)
        }
        None => None,
    };

    let transport_desc = match cert_mode {
        muxrd::config::CertMode::SelfSigned => "TLS (self-signed)",
        muxrd::config::CertMode::External => "TLS (external cert)",
        muxrd::config::CertMode::H2c => "h2c (plaintext — proxy-terminated TLS expected)",
    };
    log::info!("muxrd starting on {addr} ({transport_desc} + bearer auth)");

    // ── Build the server builder, applying TLS only when not h2c ────────────
    // BearerAuthLayer wraps the router at the HTTP level so it sees the full
    // URI path — it can distinguish Login/GetVersion from AttachTerminal.
    let mut builder = Server::builder();
    if let Some(tls) = maybe_tls {
        builder = builder.tls_config(tls).context("failed to configure TLS")?;
    }

    // Per-IP rate limiting is added FIRST so it is the OUTERMOST middleware
    // (tonic applies the first-added layer outermost). It sheds request floods
    // before the BearerAuthLayer's token-DB work runs. The layer is a no-op when
    // disabled via MUXRD_RATE_LIMIT_DISABLE.
    builder
        .layer(RateLimitLayer::from_env())
        .layer(BearerAuthLayer)
        .add_service(MuxrServer::new(service))
        .serve_with_shutdown(addr, async move {
            let _ = shutdown_rx.await;
            log::info!("muxrd: graceful shutdown initiated");
            // Fan the shutdown out to background tasks (herdr event kernel). This
            // keeps `serve_with_shutdown`'s own behaviour identical — it still
            // resolves the instant the control-socket oneshot fires.
            let _ = kernel_shutdown_tx.send(true);
        })
        .await
        .context("gRPC server error")?;

    log::info!("muxrd stopped");
    Ok(())
}

// ── status ──────────────────────────────────────────────────────────────────

/// `status` — query the control socket and print running/stopped.
fn cmd_status(_bind_override: Option<&str>) -> Result<()> {
    match control::query(&ControlRequest::Status) {
        Ok(ControlResponse::Status(info)) => {
            println!("muxrd: running");
            println!("  version   : {}", info.version);
            println!("  bind      : {}", info.bind_addr);
            println!("  pid       : {}", info.pid);
            println!("  uptime    : {}s", info.uptime_secs);
            println!("  clients   : {}", info.client_count);
            Ok(())
        }
        Ok(other) => {
            // Unexpected reply shape — still report running.
            println!("muxrd: running (unexpected reply: {other:?})");
            Ok(())
        }
        Err(_) => {
            // The control socket is unresponsive.  This does NOT automatically
            // mean the daemon is gone: there is a window during `start
            // --daemonize` where the pidfile already exists (daemonize wrote it)
            // but the control socket isn't bound yet.  Cross-check the pid before
            // declaring the daemon dead — `status` must never delete a live
            // daemon's pidfile (review Major C).
            match read_pidfile() {
                Some(pid) if pid_is_alive(pid) => {
                    // Pidfile present + pid alive but socket not answering →
                    // the daemon is starting up (or its control thread is
                    // wedged).  Report "starting" and DO NOT clean up.
                    println!("muxrd: starting (running, control socket not ready)");
                    println!("  pid       : {pid}");
                    Ok(())
                }
                Some(pid) => {
                    // Pidfile present but pid confirmed dead → genuinely stale.
                    log::debug!("status: pid {pid} from pidfile is dead — cleaning up stale state");
                    println!("muxrd: stopped");
                    cleanup_stale();
                    Ok(())
                }
                None => {
                    // No pidfile and no socket → not running.  Best-effort
                    // socket cleanup (no live pid to protect).
                    println!("muxrd: stopped");
                    cleanup_stale();
                    Ok(())
                }
            }
        }
    }
}

// ── stop ──────────────────────────────────────────────────────────────────────

/// `stop` — request a graceful shutdown over the control socket; fall back to
/// the pidfile + SIGTERM if the socket is unresponsive.  Cleans up afterwards.
fn cmd_stop() -> Result<()> {
    match control::query(&ControlRequest::Shutdown) {
        Ok(_) => {
            println!("muxrd: shutdown requested via control socket.");
        }
        Err(e) => {
            log::debug!("control socket unresponsive ({e:#}); trying pidfile + SIGTERM");
            if let Some(pid) = read_pidfile() {
                // SAFETY: kill(2) with SIGTERM is async-signal-safe and the pid
                // is read from our own pidfile.
                let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
                if rc == 0 {
                    println!("muxrd: sent SIGTERM to pid {pid} (control socket was unresponsive).");
                } else {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ESRCH) {
                        println!("muxrd: no running server found (stale pidfile).");
                    } else {
                        anyhow::bail!("failed to SIGTERM pid {pid}: {err}");
                    }
                }
            } else {
                println!(
                    "muxrd: not running (no control socket, no pidfile). If a bind \
                     address still appears held, an orphaned daemon may still hold \
                     the port — check with `ss -ltnp | grep <port>` (or fuser)."
                );
            }
        }
    }

    // Give the server a moment to release the socket/pidfile, then clean up any
    // remnants (daemonize-held pidfiles are released on the child's exit).
    std::thread::sleep(std::time::Duration::from_millis(300));
    cleanup_stale();
    Ok(())
}

// ── stale-state cleanup helpers ────────────────────────────────────────────────

/// Read the pid from the pidfile, if present and parseable.
fn read_pidfile() -> Option<libc::pid_t> {
    let path = pidfile_path().ok()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    raw.trim().parse::<libc::pid_t>().ok()
}

/// Is the given pid alive?  Uses `kill(pid, 0)`, which performs the permission
/// check and existence test without delivering a signal.
///
/// Returns `true` if the process exists (signal would be deliverable, or exists
/// but we lack permission → `EPERM`); `false` only when the kernel reports the
/// pid does not exist (`ESRCH`).  This conservative reading is what protects a
/// live daemon's pidfile in `status` (review Major C).
fn pid_is_alive(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 is async-signal-safe and only tests for the
    // process's existence/permission — it delivers no signal.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM → the process exists but we can't signal it (still alive).
    // ESRCH → no such process (dead).
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Remove the control socket and pidfile (best-effort).
///
/// Only safe to call from `stop`, `start`'s explicit pre-fork cleanup, or after
/// a pid has been **confirmed dead** — never speculatively from `status` while a
/// daemon may be mid-startup (review Major C).
fn cleanup_stale() {
    control::cleanup();
    if let Ok(path) = pidfile_path() {
        let _ = std::fs::remove_file(&path);
    }
}

// ── start-path liveness decision ────────────────────────────────────────────────

/// Decision for whether `start` may treat existing pidfile/socket state as
/// stale (and therefore clean it up), or must refuse.
///
/// Kept as a pure function of three observations so the start-path branch logic
/// is unit-testable without forking a daemon (in-process fork tests are
/// infeasible — the start path compensates with precise doc comments instead).
#[derive(Debug, PartialEq, Eq)]
enum StartDecision {
    /// Control socket answered → a healthy daemon is already running.  Refuse.
    RefuseSocketResponsive,
    /// Control socket silent, but the pidfile names a live process → refuse and
    /// name the pid.  Crucially, do NOT delete the pidfile/socket in this case
    /// (the 2026-07-12 incident: a transient query failure here led to deleting
    /// a live daemon's control state, leaving it unstoppable).
    RefuseLiveDaemon(libc::pid_t),
    /// No evidence of a live daemon → any leftover files are genuinely stale.
    Stale,
}

/// Decide whether `start` may treat existing pidfile/socket state as stale.
///
/// A control-query failure ALONE is never treated as proof of staleness: only
/// when the control socket is silent AND the pidfile names no live pid do we
/// call the state stale.  This is what prevents a transient control-query
/// failure from destroying a live daemon's pidfile/socket.
fn start_staleness_decision(
    control_responsive: bool,
    pidfile_pid: Option<libc::pid_t>,
    pid_alive: bool,
) -> StartDecision {
    if control_responsive {
        return StartDecision::RefuseSocketResponsive;
    }
    match pidfile_pid {
        Some(pid) if pid_alive => StartDecision::RefuseLiveDaemon(pid),
        _ => StartDecision::Stale,
    }
}

/// Pre-flight probe: is `addr` bindable right now?
///
/// Returns `Ok(())` if a `TcpListener` can bind `addr` (the listener is dropped
/// immediately on success).  Returns an "already in use" error if the port is
/// held; other bind errors are surfaced too (they would also stop the real
/// bind).
///
/// TOCTOU: there is a small window between dropping this probe listener and the
/// daemon child's real bind in which another process could grab the port.  That
/// is acceptable — the child's own bind error remains the backstop; this probe
/// exists only to bail BEFORE the destructive cleanup when the port is clearly
/// taken.  All binds today are TCP `host:port` (see `config::resolve`), so a
/// `TcpListener` probe is always applicable.
fn probe_bind_available(addr: std::net::SocketAddr) -> Result<()> {
    match std::net::TcpListener::bind(addr) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!(
                "bind address {addr} is already in use — is another muxrd running? \
                 If no pidfile/control socket exists, find the holder with \
                 `ss -ltnp | grep {}` (or fuser) and kill it.",
                addr.port()
            )
        }
        Err(e) => Err(e).with_context(|| format!("probe bind {addr}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── start-path staleness decision ───────────────────────────────────────

    /// A responsive control socket is the fast-path refusal — pid state is
    /// irrelevant.
    #[test]
    fn staleness_socket_responsive_refuses_regardless_of_pid() {
        assert_eq!(
            start_staleness_decision(true, Some(1234), true),
            StartDecision::RefuseSocketResponsive
        );
        assert_eq!(
            start_staleness_decision(true, None, false),
            StartDecision::RefuseSocketResponsive
        );
    }

    /// The incident: control query failed (transient) but the pidfile names a
    /// live process → REFUSE (naming the pid), never clean up.
    #[test]
    fn staleness_live_pid_dead_socket_refuses_without_cleanup() {
        assert_eq!(
            start_staleness_decision(false, Some(4321), true),
            StartDecision::RefuseLiveDaemon(4321)
        );
    }

    /// Control silent + pidfile pid confirmed dead → genuinely stale.
    #[test]
    fn staleness_dead_pid_dead_socket_is_stale() {
        assert_eq!(
            start_staleness_decision(false, Some(4321), false),
            StartDecision::Stale
        );
    }

    /// Control silent + no pidfile → genuinely stale (crashed-run remnant).
    #[test]
    fn staleness_no_pidfile_dead_socket_is_stale() {
        assert_eq!(
            start_staleness_decision(false, None, false),
            StartDecision::Stale
        );
    }

    // ── pre-flight bind probe ────────────────────────────────────────────────

    /// Holding an ephemeral port must make the probe report it as in use.
    #[test]
    fn probe_bind_available_refuses_a_held_port() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let addr = held.local_addr().expect("local_addr");
        let err = probe_bind_available(addr).expect_err("held port must refuse");
        assert!(
            err.to_string().contains("already in use"),
            "unexpected error: {err}"
        );
    }

    /// A freshly released ephemeral port should be bindable again.
    #[test]
    fn probe_bind_available_accepts_a_free_port() {
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
            l.local_addr().expect("local_addr")
        };
        assert!(
            probe_bind_available(addr).is_ok(),
            "freshly released port should be bindable"
        );
    }

    /// `cert_identity` must return `None` for h2c — no TLS identity to load.
    #[test]
    fn cert_identity_h2c_returns_none() {
        // No crypto provider needed — h2c returns before touching TLS.
        let result = cert_identity(&CertSource::H2c, &[]);
        assert!(
            result.is_ok(),
            "cert_identity(H2c) should not error: {:?}",
            result.err()
        );
        assert!(
            result.unwrap().is_none(),
            "cert_identity(H2c) must return None"
        );
    }

    /// `cert_identity` for SelfSigned must return `Some` with a non-empty cert PEM.
    #[test]
    fn cert_identity_self_signed_returns_some() {
        // Install the ring crypto provider so TLS ops work in the test runtime.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let result = cert_identity(&CertSource::SelfSigned, &[]);
        assert!(
            result.is_ok(),
            "cert_identity(SelfSigned) should succeed: {:?}",
            result.err()
        );
        let pair = result.unwrap();
        assert!(pair.is_some(), "cert_identity(SelfSigned) must return Some");
        let (_identity, cert_pem) = pair.unwrap();
        assert!(
            cert_pem.contains("CERTIFICATE"),
            "cert PEM must contain CERTIFICATE block"
        );
    }
}
