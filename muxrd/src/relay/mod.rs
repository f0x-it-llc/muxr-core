//! relay — the blocking-multiplexer ↔ async-gRPC bridge for `AttachTerminal`.
//!
//! This is the Phase-B hot path, extended in Phase C to surface control events
//! and generalized in P1.03 to drive a neutral [`crate::multiplexer::MuxBackend`]
//! dual handle ([`MuxSender`]/[`MuxReceiver`]/[`MuxServerMsg`]) instead of zellij
//! IPC types directly — so a Phase-2 herdr backend reuses this machinery
//! verbatim. One [`attach_relay`] call drives a single `AttachTerminal`
//! bidirectional gRPC stream:
//!
//! ```text
//!                 ┌──────────────── std reader thread ───────────────┐
//!   backend       │  loop { MuxReceiver::recv()  (BLOCKING)           │
//!   open_attach ──┼──►  Render → bounded mpsc::blocking_send ─────────┼──► ReceiverStream
//!   (DualHandle)  │  Event (Exit/RenamedSession/…) ───────────────────┼──►  (outbound ServerFrame)
//!                 │  Log (query reply) → fills in-flight query slot ───┼──► reply oneshot
//!                 │       break on stop-flag, Exit, OR send error     │
//!                 └───────────────────────────────────────────────────┘
//!
//!                 ┌──────────────── tokio inbound task ──────────────┐
//!   gRPC client   │  Streaming<ClientFrame>::next()                   │
//!   ──────────────┼──►  input → MuxSender::send_input_chars/bytes     │
//!                 │     resize → MuxSender::send_resize               │
//!                 │     QueryLayout → MuxSender::query_layout + HAND   │
//!                 │                   query to render thread (no await)│
//!                 └───────────────────────────────────────────────────┘
//! ```
//!
//! [`MuxSender`]: crate::multiplexer::MuxSender
//! [`MuxReceiver`]: crate::multiplexer::MuxReceiver
//! [`MuxServerMsg`]: crate::multiplexer::MuxServerMsg
//!
//! ## Lifecycle / clean shutdown
//!
//! The std reader thread is the part that can leak (a blocking `recv()` can
//! park forever on an idle session). It is wound down by **two** cooperating
//! mechanisms, so no path leaks a thread:
//!
//! 1. **Channel backpressure / drop.** When the gRPC client disconnects, the
//!    outbound [`ReceiverStream`] is dropped, which drops the channel
//!    `Receiver`. The reader thread's next `blocking_send` then returns `Err`
//!    and the loop exits. This is the common case (zellij streams renders
//!    frequently while attached).
//!
//! 2. **Stop-flag + resize nudge.** A shared [`AtomicBool`] is checked each
//!    loop iteration. On shutdown the [`ShutdownGuard`] sets it and sends a
//!    `TerminalResize` over a cloned sender to *provoke a render* from an
//!    otherwise-idle session, guaranteeing the parked `recv()` wakes and
//!    observes the flag. The guard then `join()`s the thread, so the relay
//!    future does not return until the thread is gone.
//!
//! ## B-QUERY: relay-routed layout query (BE-LAYOUT)
//!
//! `GetLayout` normally opens an ephemeral `AttachClient` per poll, which
//! (a) registers a transient extra client — polluting per-client `is_focused`
//! / `active` unions — and (b) causes pane-frame flicker on every
//! attach/detach cycle.
//!
//! When a relay is attached the `QueryLayout` [`RelayControl`] variant lets
//! `get_layout` instead route the `ListTabs`/`ListPanes` query actions through
//! the relay's **existing** persistent client.  The crucial design constraint:
//! `recv()` is **exclusively owned** by the std reader thread (`render_loop`).
//! Moving it or sharing it with the inbound tokio task would require either a
//! mutex (head-of-line blocking) or unsafe unsync access.
//!
//! ### FX-QUERY: reply-fulfillment owned by the render thread
//!
//! BE-LAYOUT's first cut had the inbound task deposit a bare capture token then
//! `await` both Log replies *inline* in the `select!` arm. That had two defects:
//!
//! - **(A) orphan cross-talk.** A token deposited *before* the action was never
//!   retired on send-failure or timeout, so the next query's `Log` was captured
//!   by the dead sender — ListTabs JSON landing in the panes slot, cascading
//!   timeouts, worst in an idle session (no Renders to flush the stale token).
//! - **(B) select-loop block.** Awaiting the two Logs inline blocked input
//!   forwarding and the 30 s bearer recheck for up to ~16 s.
//!
//! The fix moves *all* reply-fulfillment into the render thread (which already
//! owns `recv()` and so is the only place a `Log` can be observed):
//!
//! ```text
//!   inbound task (QueryLayout arm — NEVER awaits):
//!     1. bump a monotonic `seq`
//!     2. hand InFlightQuery { seq, reply, tabs:None } → query_tx
//!     3. MuxSender::query_layout() (fires ListTabs THEN ListPanes)
//!     4. return immediately (no await, no per-arm timeout)
//!
//!   reader thread (render_loop) owns Option<InFlightQuery>:
//!     - drain query_tx (replace-on-new: a newer query drops the old → its
//!       receiver cancels and grpc falls back)
//!     - if the held query's `reply.is_closed()` (grpc timed out + dropped the
//!       receiver) → discard the slot so its stray Logs are dropped, not
//!       misattributed
//!     - on Log WITH an in-flight query: fill tabs (1st) then panes (2nd); when
//!       both present, parse into a `LayoutSnapshot` and `reply.send(Ok(snapshot))`,
//!       then clear the slot (P2.00 A-2 — the parse moved here from `grpc/layout.rs`)
//!     - on Log with NO in-flight query: discard it (drains a stale Log from a
//!       previous, already-retired query)
//!     - every Render is still forwarded unconditionally
//! ```
//!
//! The single outer bound is `RELAY_QUERY_TIMEOUT` in `grpc.rs get_layout`; when
//! it fires it drops `reply_rx`, which the render thread observes as
//! `reply.is_closed()` and uses to retire the slot.
//!
//! **Invariants** (also stated at the render-thread fulfillment site):
//! 1. the inbound `select!` arm never blocks on the query;
//! 2. a timed-out / failed / replaced query can NEVER cause a *later* query's
//!    Logs to be misattributed (the slot is retired on close/replace, and a Log
//!    with no in-flight query is dropped);
//! 3. Render frames are never dropped (`Log` is a distinct variant from
//!    `Render`; the reader forwards every `Render` regardless of query state).
//!
//! Log ordering: the relay only emits `ListTabs`/`ListPanes` Logs for its OWN
//! mobile-control queries, and always sends them tabs-then-panes, so within a
//! single in-flight query the first Log is tabs and the second is panes.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Status, Streaming};

use crate::multiplexer::{BackendSet, ResumeTarget, ResumedView};
use crate::proto::{AttachReq, ClientFrame, ServerFrame, client_frame};

mod helpers;
mod inbound;
mod reader;
mod types;

// Re-export public surface (used by grpc.rs as `crate::relay::<Name>`).
pub use types::{
    ControlEntry, ControlRegistry, FloatingHint, RelayControl, RelayViewState, ServerFrameStream,
    ViewStateEntry, ViewStateRegistry,
};

// ─── Unguessable per-connection id ────────────────────────────────────────────

/// Mint an **unguessable, non-enumerable** `connection_id` for an `AttachTerminal`
/// relay (S-M4 security fix).
///
/// connection_id is the SOLE per-connection isolation discriminator on a collapsed
/// backend session (herdr's single `herdr:herdr` session — see
/// [`crate::multiplexer::is_collapsed_backend_session`]): every co-attached relay
/// shares the same session id, so the routing layer relies entirely on a
/// connection_id match. The previous monotonic `AtomicU64` counter (starting at
/// one) was trivially guessable — an authed RW client could enumerate ids and
/// re-point a victim connection's stream. We therefore mint a 128-bit value from a
/// CSPRNG (`rand::thread_rng`, ChaCha-based, OS-seeded) and format it as a 32-char
/// lowercase hex string for the proto wire. It stays stable for the connection's
/// lifetime and a reattach mints a fresh one (as before).
fn mint_connection_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

use reader::{ShutdownGuard, render_loop};
use types::{InFlightQuery, RENDER_CHANNEL_BOUND, RO_FALLBACK_COLS, RO_FALLBACK_ROWS};

// ─── attach_relay ─────────────────────────────────────────────────────────────

/// Drive one `AttachTerminal` stream end to end.
///
/// Reads the first inbound frame (must be `AttachReq`), opens the IPC attach,
/// spawns the relay tasks/thread, and returns the outbound render stream.
#[allow(clippy::too_many_arguments)]
pub async fn attach_relay(
    mut inbound: Streaming<ClientFrame>,
    read_only: bool,
    token: Option<String>,
    clients: crate::client_count::SessionClients,
    control: ControlRegistry,
    view_state: ViewStateRegistry,
    backends: BackendSet,
) -> Result<ServerFrameStream, Status> {
    // ── 1. First frame must be AttachReq ────────────────────────────────────
    let first = inbound
        .next()
        .await
        .ok_or_else(|| Status::invalid_argument("stream closed before AttachReq"))?
        .map_err(|e| Status::internal(format!("error reading first frame: {e}")))?;

    let attach = match first.kind {
        Some(client_frame::Kind::Attach(req)) => req,
        Some(_) => {
            return Err(Status::invalid_argument(
                "first ClientFrame must be AttachReq (got input/resize)",
            ));
        }
        None => return Err(Status::invalid_argument("first ClientFrame had no kind")),
    };

    let client_rows = clamp_dim(attach.rows, 24);
    let client_cols = clamp_dim(attach.cols, 80);

    // Best-effort resume hint (additive AttachReq fields), tier-gated on write
    // access. Empty for every client that does not send them — and an empty target
    // is today's behavior exactly. See [`resume_target_for`] for why a read-only
    // attach never carries one.
    let resume = resume_target_for(&attach, read_only);

    // ── 1a. Option C: resolve the opaque session id → owning backend + bare name.
    // The id is `<backend>:<bare>` (e.g. `zellij:dev`); the client echoes the SAME
    // id in later unary RPCs, so the *id* is what we store in the control / view-
    // state registries (registry match stays id-vs-id), while the *bare* name is
    // what we hand the backend for the size query / attach. `resolve_session` runs
    // the path-traversal guard on the bare name (the attach path was previously
    // unvalidated — this tightens it).
    let id = attach.session.clone();
    let (backend_kind, backend, session) =
        crate::multiplexer::resolve_session_kind(&backends, &id)?;

    // ── Carried T04 fix: backend-qualified client-count key ──────────────────
    // The relay's connected-client count must bucket by the SAME opaque id that
    // `ListSessions` reads (`make_id(kind, bare)`), NOT by the bare session name.
    // Keying by the bare name would make two same-name sessions on different
    // backends (`zellij:dev` + `herdr:dev`) share one bucket and double-count.
    // Canonicalize here (rather than reusing the raw client-sent `id`) so a legacy
    // bare-name attach on a single-backend server still lands in the same bucket
    // the canonical `make_id`-keyed `ListSessions` reads.
    let count_key = crate::multiplexer::make_id(backend_kind, &session);

    // ── 1b. Major A (round-2): read-only attaches must NOT drive geometry ────
    //
    // zellij resizes the shared session to the MINIMUM terminal size across all
    // attached clients on every AttachClient handshake (zellij-server
    // lib.rs::min_client_terminal_size). A small read-only observer would
    // otherwise shrink the writer's session. So for a read-only attach we
    // attach with the session's CURRENT size (queried up-front), never the
    // client's. Writers (RW) keep driving their own size exactly as before.
    let (rows, cols) = if read_only {
        let query_session = session.clone();
        let size_backend = backend.clone();
        match tokio::task::spawn_blocking(move || size_backend.query_session_size(&query_session))
            .await
        {
            Ok(Ok((r, c))) => {
                log::info!(
                    "AttachTerminal: read-only attach to '{session}' — using current \
                     session size {r}x{c} (ignoring client {client_rows}x{client_cols})"
                );
                (r, c)
            }
            Ok(Err(e)) => {
                // Couldn't read the session size — fall back to a sane neutral
                // size that won't shrink a typical writer (and won't allocate a
                // giant grid). NEVER the client's small dims.
                log::warn!(
                    "AttachTerminal: read-only attach to '{session}' — could not query \
                     session size ({e:#}); falling back to neutral {RO_FALLBACK_ROWS}x\
                     {RO_FALLBACK_COLS}"
                );
                (RO_FALLBACK_ROWS, RO_FALLBACK_COLS)
            }
            Err(e) => {
                log::warn!(
                    "AttachTerminal: read-only attach to '{session}' — session-size query \
                     task panicked ({e}); falling back to neutral {RO_FALLBACK_ROWS}x\
                     {RO_FALLBACK_COLS}"
                );
                (RO_FALLBACK_ROWS, RO_FALLBACK_COLS)
            }
        }
    } else {
        (client_rows, client_cols)
    };

    log::info!(
        "AttachTerminal: opening IPC attach to session '{}' ({rows}x{cols}, read_only={read_only})",
        attach.session
    );

    // ── 2. Open the attach via the backend (blocking but cheap: connect +
    //       handshake), yielding a neutral DualHandle of boxed sender/receiver. ─
    let attach_session = session.clone();
    let open_backend = backend.clone();
    let open_resume = resume.clone();
    let handle = tokio::task::spawn_blocking(move || {
        // Resume-aware entry point: the trait default ignores the hint and
        // delegates to `open_attach`, so zellij (and any backend without a
        // resume concept) is untouched; only herdr resolves it.
        open_backend.open_attach_with_resume(&attach_session, rows, cols, read_only, &open_resume)
    })
    .await
    .map_err(|e| Status::internal(format!("attach task panicked: {e}")))?
    .map_err(|e| Status::not_found(format!("attach failed: {e:#}")))?;

    let session_name = handle.session_name.clone();
    // The view this attach actually landed on (herdr resume) — used to seed the
    // per-connection B-FOCUS state below. `None` for every hint-less attach.
    let resumed_view = handle.resumed_view.clone();
    let (sender, receiver) = handle.split();

    // ── Phase F: count this client against the session ──────────────────────
    // Increment now that the attach succeeded; the guard is moved into the
    // inbound task below and decrements on every stream-end path when that task
    // (and the guard with it) drops. Attach-failure paths above returned early
    // and were never counted.
    let client_guard = clients.attach(&count_key);

    // ── 3b. Mint an unguessable, non-enumerable connection_id for this relay. ─
    // S-M4: connection_id is the sole per-connection isolation discriminator on a
    // collapsed (herdr) session, so it must NOT be guessable/enumerable. A 128-bit
    // CSPRNG hex string (see `mint_connection_id`).
    let connection_id = mint_connection_id();
    // FS3: full connection_id must not appear in info/warn logs — it is the sole
    // per-connection isolation discriminator and effectively a per-connection secret.
    // Log an 8-hex-char prefix at info (enough for operational correlation) and
    // keep the full value at debug only.
    log::info!(
        "relay [{session_name}]: minted connection_id={}… (read_only={read_only})",
        &connection_id[..8]
    );
    log::debug!(
        "relay [{session_name}]: minted connection_id={connection_id} \
         (read_only={read_only})"
    );

    // ── 3. Outbound: bounded channel + std reader thread ────────────────────
    let (tx, rx) = mpsc::channel::<Result<ServerFrame, Status>>(RENDER_CHANNEL_BOUND);
    let stop = Arc::new(AtomicBool::new(false));

    // ── 3d. Advertise connection_id to the client via a ControlEvent frame. ─
    // Emit it as the FIRST frame, before any render bytes, so the client can
    // echo it in subsequent unary RPCs (GoToTab / FocusPane / GetLayout) to
    // ensure those are routed to THIS relay rather than another relay on the
    // same session. We do this before handing `tx` to the reader thread so we
    // don't need a clone.
    {
        use crate::proto::{ControlEvent, server_frame};
        let conn_event = ServerFrame {
            kind: Some(server_frame::Kind::Control(ControlEvent {
                kind: "connection_id".to_owned(),
                payload: connection_id.clone(),
            })),
        };
        // The channel is empty and the receiver hasn't been given to the stream
        // yet, so this cannot block. If somehow it fails (channel full from a
        // very small RENDER_CHANNEL_BOUND — not the case with 64) the client
        // never receives its connection_id and falls back to session-scoped
        // routing for all subsequent RPCs.
        if let Err(e) = tx.try_send(Ok(conn_event)) {
            // FS3: redact full connection_id at warn; full value logged at debug only.
            log::warn!(
                "relay [{session_name}]: failed to send connection_id frame to client \
                 (id={}…): {e} — client will fall back to session-scoped routing",
                &connection_id[..8]
            );
            log::debug!(
                "relay [{session_name}]: failed to send connection_id frame to client \
                 (connection_id={connection_id}): {e} — client will fall back to \
                 session-scoped routing"
            );
        }
    }

    // FX-QUERY: channel from inbound task → render thread carrying in-flight
    // layout queries. The std mpsc is non-blocking for the producer: the inbound
    // arm hands the query off with `send` and returns; the render thread drains
    // it with `try_recv`. (Capacity is effectively unbounded, but in practice at
    // most one query is outstanding — a newer one replaces the older.)
    let (query_tx, query_rx) = std::sync::mpsc::channel::<InFlightQuery>();

    let reader_stop = stop.clone();
    let reader_session = session_name.clone();
    let reader: JoinHandle<u64> = std::thread::Builder::new()
        .name(format!("relay-reader-{session_name}"))
        .spawn(move || render_loop(receiver, tx, reader_stop, &reader_session, query_rx))
        .expect("failed to spawn relay reader thread");

    // ShutdownGuard owns the stop-flag, an independent cloned sender for the
    // teardown nudge, and the join handle. Dropping it (when the inbound task
    // ends) tears the reader thread down deterministically.
    let guard = ShutdownGuard {
        stop,
        nudge: sender.box_clone(),
        reader: Some(reader),
        rows,
        cols,
        read_only,
        session: session_name.clone(),
    };

    // ── 3c. W2.0a control channel (created now; REGISTERED below). ───────────
    // Create the channel here, but DEFER `control.insert` until AFTER the
    // view-state is initialized and the query plumbing is ready (FX-QUERY part
    // C). Registering the control sender is what makes `get_layout` route a
    // `QueryLayout` to this relay; if we registered before view-state init, a
    // GetLayout landing in that window would route a query to a relay whose
    // view-state/query path isn't ready yet (and the old "relay hasn't
    // registered yet" comment was wrong — it *had* already registered).
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<RelayControl>();

    // ── B-FOCUS: initialize relay view state from live zellij state ──────────
    // Query once at attach so get_layout can immediately override active/is_focused.
    // This is best-effort: a failure just leaves the state as None (falls back to
    // the queried values from the relay's own Log, which for a single client ARE
    // correct). We use the ephemeral query path here because no QueryLayout RPC
    // can arrive yet: the control sender is registered only AFTER this block, so
    // there is no race against the render thread's query draining.
    //
    // A **resumed** attach skips that query: the backend already told us the exact
    // view it landed on, and the ephemeral query would report the daemon-GLOBAL
    // focus in the daemon's active-or-first workspace — precisely what the resume
    // exists to override.
    {
        let view_state_init = view_state.clone();
        let conn_id = connection_id.clone();
        // Option C: the registry stores the opaque *id* the client echoes (not the
        // bare name) so `entry.session == req.session` stays an id-vs-id match.
        let session_for_entry = id.clone();
        let relay_vs = match resumed_view {
            Some(view) => {
                log::info!(
                    "relay [{session_name}]: seeded view state from resumed attach: \
                     space='{}' active_tab={} focused_pane={}",
                    view.space_id,
                    view.tab_id,
                    view.pane.id
                );
                view_state_from_resumed(&view)
            }
            None => {
                let init_session = session_name.clone();
                let init_backend = backend.clone();
                let init_result = tokio::task::spawn_blocking(move || {
                    helpers::init_relay_view_state(&init_backend, &init_session)
                })
                .await;
                match init_result {
                    Ok(Ok(state)) => {
                        log::info!(
                            "relay [{session_name}]: initialized view state: \
                             active_tab={:?} focused_pane={:?}",
                            state.active_tab,
                            state.focused_pane
                        );
                        state
                    }
                    Ok(Err(e)) => {
                        log::warn!(
                            "relay [{session_name}]: view-state init failed (will use queried \
                             values until first action): {e:#}"
                        );
                        RelayViewState::default()
                    }
                    Err(e) => {
                        log::warn!("relay [{session_name}]: view-state init task panicked: {e}");
                        RelayViewState::default()
                    }
                }
            }
        };
        view_state_init.insert(
            conn_id,
            crate::relay::ViewStateEntry {
                session: session_for_entry,
                state: relay_vs,
            },
        );
    }

    // ── 3c (cont.): NOW register the control sender (FX-QUERY part C). ────────
    // View-state is initialized and the render thread + query plumbing are live,
    // so a GetLayout that finds this entry can safely route a QueryLayout.
    // Register by connection_id — not session name — so multiple concurrent
    // relays on the same session each get their own slot (fixes the multi-client
    // misroute bug where the old session-keyed insert overwrote prior entries).
    control.insert(
        connection_id.clone(),
        ControlEntry {
            // Option C: store the opaque id the client echoes (registry match stays
            // id-vs-id); the bare `session_name` is only for backend calls / counts.
            session: id.clone(),
            sender: ctrl_tx.clone(),
            read_only,
        },
    );

    // ── 4. Inbound: tokio task pumping ClientFrames → IPC sender ────────────
    tokio::spawn(inbound::inbound_loop(
        inbound,
        sender,
        backend,
        guard,
        session_name,
        connection_id,
        read_only,
        (rows, cols),
        token,
        client_guard,
        ctrl_rx,
        control,
        clients,
        query_tx,
        view_state,
    ));

    // ── 5. Outbound stream from the channel receiver ────────────────────────
    let stream = ReceiverStream::new(rx);
    Ok(Box::pin(stream) as ServerFrameStream)
}

// ─── Resume hint (additive AttachReq fields) ──────────────────────────────────

/// Upper bound on an accepted `resume_space_id`, mirroring the `MAX_SPACE_ID_LEN`
/// bound `grpc::space_ops` puts on the `SwitchSpace` verb — the hint addresses the
/// same opaque backend space id, so it gets the same hygiene. Unlike SwitchSpace,
/// a violation is **dropped, never rejected** (see [`resume_target`]).
const MAX_RESUME_SPACE_ID_LEN: usize = 128;

/// The resume hint this attach is allowed to carry: [`resume_target`] for a
/// read-write attach, **nothing at all** for a read-only one.
///
/// A resume hint is navigation: it names the space/tab/pane this connection will
/// render. Read-only relays are not permitted to navigate — the inbound loop drops
/// exactly these moves for a read-only token (`relay/inbound.rs`: `FocusPane`
/// dropped, `SwitchTab` skipped, `SwitchSpace` refused with an error reply; the
/// `GoToTab`/`SwitchSpace` RPC gates reject earlier still). Honouring the hint at
/// attach time would be the same navigation through a different door, and a wider
/// one: it would let a read-only token steer its attach to *any* pane it can name
/// (content disclosure beyond the daemon-focused pane) and take the herdr
/// owner/resize locks on that pane.
///
/// **The two paths must not drift.** If read-only relays ever gain a sanctioned
/// navigation story, this gate and the inbound-loop guards change together —
/// neither is meaningful on its own.
///
/// The hint is *dropped*, never rejected: a read-only client that sends one still
/// attaches, exactly as it does today, on the backend's own focused pane. An empty
/// [`ResumeTarget`] also means the backend reports no `resumed_view`, so nothing
/// downstream (view-state seeding included) can observe the difference between
/// this and a hint-less attach.
fn resume_target_for(attach: &AttachReq, read_only: bool) -> ResumeTarget {
    if read_only {
        // Log only when a hint was actually present, and never the values
        // themselves (client-supplied strings stay out of the logs).
        if !attach.resume_space_id.is_empty()
            || attach.resume_tab_id != 0
            || attach.resume_pane_id != 0
        {
            log::debug!(
                "AttachTerminal: dropping resume hint on a read-only attach \
                 (read-only relays do not navigate — see relay/inbound.rs)"
            );
        }
        return ResumeTarget::default();
    }
    resume_target(attach)
}

/// Translate the additive `AttachReq.resume_*` fields into the neutral
/// [`ResumeTarget`] the backend seam takes.
///
/// proto3 defaults ARE the "unset" encoding (`""` / `0`), so a client that does
/// not know these fields — or an attach with no view history — yields an empty
/// target, i.e. today's behavior byte for byte.
///
/// A malformed `resume_space_id` (over-long, or outside the `[A-Za-z0-9_-.:]`
/// charset `SwitchSpace` enforces) is **dropped rather than rejected**: the whole
/// contract of a resume hint is that a stale or garbage value degrades to the
/// current behavior instead of failing the attach. Dropping it here also keeps an
/// attacker-supplied string out of the backend call and the logs.
fn resume_target(attach: &AttachReq) -> ResumeTarget {
    let space_id = Some(attach.resume_space_id.as_str())
        .filter(|s| !s.is_empty())
        .filter(|s| {
            let sane = s.len() <= MAX_RESUME_SPACE_ID_LEN
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'));
            if !sane {
                log::debug!(
                    "AttachTerminal: ignoring malformed resume_space_id ({} bytes)",
                    s.len()
                );
            }
            sane
        })
        .map(str::to_owned);

    ResumeTarget {
        space_id,
        tab_id: Some(attach.resume_tab_id).filter(|id| *id != 0),
        pane_id: Some(attach.resume_pane_id).filter(|id| *id != 0),
    }
}

/// Seed a [`RelayViewState`] from the view an attach actually resolved
/// ([`crate::multiplexer::DualHandle::resumed_view`]).
///
/// Same state shape the switch verbs maintain (`inbound.rs`): `active_tab` +
/// `focused_pane` are what `get_layout`'s B-FOCUS pass overrides with, and
/// `current_space` is what `GetSpaces` marks connection-active. Setting all three
/// is what makes the client's FIRST `GetLayout`/`GetSpaces` after a resumed attach
/// report the resumed view instead of the backend's daemon-global flags.
fn view_state_from_resumed(view: &ResumedView) -> RelayViewState {
    RelayViewState {
        active_tab: Some(view.tab_id),
        focused_pane: Some(view.pane),
        // Unlike a hint-less attach (which lands on the daemon's focused
        // workspace, so `None` is correct), a resumed attach may render a
        // workspace the daemon does NOT consider active — record it so the
        // per-connection override is truthful from the first poll.
        current_space: Some(view.space_id.clone()),
    }
}

// ─── Utilities ────────────────────────────────────────────────────────────────

/// Upper bound on a single terminal dimension (rows or cols).
///
/// The backend allocates a viewport grid proportional to `rows × cols`, so an
/// unbounded dimension from a client is a memory-amplification DoS: an attach or
/// resize of `65535 × 65535` would ask the backend to allocate ~4.3 billion
/// cells and OOM the host, taking down every session on a self-hosted server.
/// `1024` comfortably exceeds any real device viewport while capping the worst
/// case at ~1M cells.
pub(crate) const MAX_TERMINAL_DIM: u16 = 1024;

/// Clamp a proto `uint32` dimension into a sane `u16`, falling back to
/// `default` when zero/unset and capping at [`MAX_TERMINAL_DIM`] so a
/// client-controlled dimension cannot drive an unbounded backend allocation.
pub(crate) fn clamp_dim(v: u32, default: u16) -> u16 {
    if v == 0 {
        default
    } else {
        v.min(MAX_TERMINAL_DIM as u32) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multiplexer::PaneRef;

    // ── Resume hint: wire → neutral target ───────────────────────────────────

    /// An `AttachReq` carrying only the pre-resume fields.
    fn bare_attach() -> AttachReq {
        AttachReq {
            session: "herdr:herdr".into(),
            rows: 24,
            cols: 80,
            ..Default::default()
        }
    }

    /// Backward compatibility (acceptance criterion 1): a client that does not
    /// send the additive fields — i.e. proto3 defaults — yields an EMPTY target,
    /// which is what makes the attach behave exactly as it did before.
    #[test]
    fn unset_resume_fields_yield_an_empty_target() {
        let target = resume_target(&bare_attach());
        assert!(target.is_empty(), "unset fields must mean no resume hint");
        assert_eq!(target, crate::multiplexer::ResumeTarget::default());
    }

    #[test]
    fn resume_fields_map_onto_the_neutral_target() {
        let target = resume_target(&AttachReq {
            resume_space_id: "ws-2".into(),
            resume_tab_id: 7,
            resume_pane_id: 21,
            ..bare_attach()
        });
        assert!(!target.is_empty());
        assert_eq!(target.space_id.as_deref(), Some("ws-2"));
        assert_eq!(target.tab_id, Some(7));
        assert_eq!(target.pane_id, Some(21));
    }

    /// Zero is the proto3 "unset" encoding for the numeric hints (registry ids
    /// start at 1), so a zero must never be forwarded as a real hint.
    #[test]
    fn zero_ids_are_treated_as_unset() {
        let target = resume_target(&AttachReq {
            resume_space_id: "ws-2".into(),
            resume_tab_id: 0,
            resume_pane_id: 0,
            ..bare_attach()
        });
        assert_eq!(target.space_id.as_deref(), Some("ws-2"));
        assert!(target.tab_id.is_none() && target.pane_id.is_none());
    }

    /// A malformed space hint is DROPPED, never an error — the attach proceeds
    /// with the remaining hints (a hint must not be able to fail an attach).
    #[test]
    fn malformed_space_hint_is_dropped_not_rejected() {
        for bad in [
            "ws 2".to_string(),                      // space
            "ws/../../etc".to_string(),              // path traversal chars
            "\u{1f4a5}".to_string(),                 // non-ascii
            "w".repeat(MAX_RESUME_SPACE_ID_LEN + 1), // over-long
        ] {
            let target = resume_target(&AttachReq {
                resume_space_id: bad.clone(),
                resume_tab_id: 7,
                resume_pane_id: 21,
                ..bare_attach()
            });
            assert!(
                target.space_id.is_none(),
                "malformed space hint {bad:?} must be dropped"
            );
            assert_eq!(target.tab_id, Some(7), "other hints must survive");
            assert_eq!(target.pane_id, Some(21));
        }
    }

    #[test]
    fn space_hint_at_the_length_limit_is_accepted() {
        let ok = "w".repeat(MAX_RESUME_SPACE_ID_LEN);
        let target = resume_target(&AttachReq {
            resume_space_id: ok.clone(),
            ..bare_attach()
        });
        assert_eq!(target.space_id.as_deref(), Some(ok.as_str()));
    }

    // ── Resume hint: read-only gate (Defect A) ───────────────────────────────

    /// An `AttachReq` with every resume field set to a well-formed value.
    fn fully_hinted_attach() -> AttachReq {
        AttachReq {
            resume_space_id: "ws-2".into(),
            resume_tab_id: 7,
            resume_pane_id: 21,
            ..bare_attach()
        }
    }

    /// A read-only attach ignores EVERY hint: the target it yields is byte-for-byte
    /// the one a hint-less attach yields, so the whole downstream chain (no
    /// `resumed_view` from the backend → no seeded view state → the usual live
    /// view-state init) is identical. Read-only relays do not navigate — the
    /// inbound loop drops the equivalent moves (`FocusPane`/`SwitchTab`/
    /// `SwitchSpace`) for the same reason.
    #[test]
    fn read_only_attach_drops_every_resume_hint() {
        let gated = resume_target_for(&fully_hinted_attach(), true);
        assert!(
            gated.is_empty(),
            "a read-only attach must carry no resume hint at all"
        );
        assert_eq!(
            gated,
            resume_target_for(&bare_attach(), false),
            "a hinted read-only attach must be indistinguishable from a hint-less one"
        );
        // An empty target is what makes the backend report no resumed view, which
        // in turn is what keeps `attach_relay` on its normal view-state init path
        // (the `Some(view)` seed arm is unreachable without one).
        assert_eq!(gated, ResumeTarget::default());
    }

    /// Even a hint whose only usable axis is the space id is dropped for a
    /// read-only attach — the gate is on the tier, not on which fields are set.
    #[test]
    fn read_only_attach_drops_a_space_only_hint() {
        let space_only = AttachReq {
            resume_space_id: "ws-2".into(),
            ..bare_attach()
        };
        assert!(resume_target_for(&space_only, true).is_empty());
    }

    /// The read-write path is untouched: a corroborating hint survives the gate
    /// exactly as `resume_target` produced it.
    #[test]
    fn read_write_attach_keeps_the_resume_hint() {
        let attach = fully_hinted_attach();
        assert_eq!(
            resume_target_for(&attach, false),
            resume_target(&attach),
            "a read-write attach must forward the hint unchanged"
        );
    }

    // ── Resume hint: resolved view → seeded B-FOCUS state ────────────────────

    /// The seeded state is what `get_layout`'s B-FOCUS pass reads, so the first
    /// poll after a resumed attach reports the resumed tab/pane rather than the
    /// backend's daemon-global flags.
    #[test]
    fn resumed_view_seeds_active_tab_focused_pane_and_space() {
        let state = view_state_from_resumed(&ResumedView {
            space_id: "ws-2".into(),
            tab_id: 7,
            pane: PaneRef::terminal(21),
        });
        assert_eq!(state.active_tab, Some(7));
        assert_eq!(state.focused_pane, Some(PaneRef::terminal(21)));
        assert_eq!(
            state.current_space.as_deref(),
            Some("ws-2"),
            "GetSpaces must mark the resumed space connection-active"
        );
    }

    /// The hint-less path is unchanged: no resumed view means the relay keeps its
    /// live-state init, whose default carries no override at all.
    #[test]
    fn default_view_state_carries_no_override() {
        let state = RelayViewState::default();
        assert!(state.active_tab.is_none());
        assert!(state.focused_pane.is_none());
        assert!(state.current_space.is_none());
    }

    #[test]
    fn clamp_dim_caps_at_max_terminal_dim() {
        // Zero → default.
        assert_eq!(clamp_dim(0, 24), 24);
        // Normal value passes through.
        assert_eq!(clamp_dim(80, 24), 80);
        // Oversized client value is capped, not forwarded (memory-DoS guard).
        assert_eq!(clamp_dim(65535, 80), MAX_TERMINAL_DIM);
        assert_eq!(clamp_dim(u32::MAX, 80), MAX_TERMINAL_DIM);
        // Exactly at the cap is preserved.
        assert_eq!(clamp_dim(MAX_TERMINAL_DIM as u32, 24), MAX_TERMINAL_DIM);
    }

    #[test]
    fn connection_id_is_32_char_lowercase_hex() {
        let id = mint_connection_id();
        assert_eq!(id.len(), 32, "connection_id must be 32 hex chars (128-bit)");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "connection_id must be lowercase hex: {id}"
        );
    }

    #[test]
    fn connection_ids_are_random_not_sequential() {
        // S-M4: the old mint was a monotonic AtomicU64 (adjacent ids differed by 1
        // and were trivially enumerable). The CSPRNG mint must produce values that
        // are neither equal nor adjacent across a batch of mints.
        let mut ids = Vec::new();
        for _ in 0..64 {
            ids.push(u128::from_str_radix(&mint_connection_id(), 16).expect("valid hex"));
        }
        // All distinct (collision probability for 64 draws from 2^128 is ~0).
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ids.len(),
            "minted connection_ids must be unique"
        );

        // No two consecutive mints differ by exactly 1 (the monotonic-counter
        // signature). With random 128-bit values this is astronomically unlikely.
        for pair in ids.windows(2) {
            let delta = pair[0].abs_diff(pair[1]);
            assert_ne!(
                delta, 1,
                "consecutive mints must not be adjacent (not a counter)"
            );
        }
    }
}
