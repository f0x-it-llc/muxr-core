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

use crate::multiplexer::BackendSet;
use crate::proto::{ClientFrame, ServerFrame, client_frame};

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
    let handle = tokio::task::spawn_blocking(move || {
        open_backend.open_attach(&attach_session, rows, cols, read_only)
    })
    .await
    .map_err(|e| Status::internal(format!("attach task panicked: {e}")))?
    .map_err(|e| Status::not_found(format!("attach failed: {e:#}")))?;

    let session_name = handle.session_name.clone();
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
    {
        let init_session = session_name.clone();
        let view_state_init = view_state.clone();
        let conn_id = connection_id.clone();
        // Option C: the registry stores the opaque *id* the client echoes (not the
        // bare name) so `entry.session == req.session` stays an id-vs-id match.
        let session_for_entry = id.clone();
        let init_backend = backend.clone();
        let init_result = tokio::task::spawn_blocking(move || {
            helpers::init_relay_view_state(&init_backend, &init_session)
        })
        .await;
        let relay_vs = match init_result {
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

// ─── Utilities ────────────────────────────────────────────────────────────────

/// Clamp a proto `uint32` dimension into a sane `u16`, falling back to
/// `default` when zero/unset.
pub(crate) fn clamp_dim(v: u32, default: u16) -> u16 {
    if v == 0 {
        default
    } else {
        v.min(u16::MAX as u32) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
