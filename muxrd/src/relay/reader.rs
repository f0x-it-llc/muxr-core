//! Blocking render-loop thread and associated query-fulfillment helpers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use tonic::Status;

use crate::multiplexer::{MuxEvent, MuxReceiver, MuxSender, MuxServerMsg};
use crate::proto::ServerFrame;
use crate::proto::{ControlEvent, server_frame};

use super::types::InFlightQuery;

// ─── render_loop ──────────────────────────────────────────────────────────────

/// Blocking render loop — runs on a dedicated std thread.
///
/// Returns the number of render frames forwarded (for logging on join).
///
/// ## FX-QUERY: Log capture (render-thread-owned reply-fulfillment)
///
/// This thread is the SOLE owner of `recv()`, so it is the only place a `Log`
/// (the reply to a `ListTabs`/`ListPanes` query) can be observed. It therefore
/// also owns the in-flight layout query end-to-end:
///
/// - It drains `query_rx` (non-blocking `try_recv`) once per loop iteration. A
///   newly arrived query REPLACES any held one (replace-on-new): the old
///   `reply` is dropped, cancelling its receiver so `get_layout` falls back.
/// - It retires a held query whose `reply.is_closed()` (i.e. `get_layout`'s
///   `RELAY_QUERY_TIMEOUT` fired and dropped the receiver) — so the timed-out
///   query's stray Logs are DROPPED, never misattributed to it.
/// - On a `Log` WITH a held query: the first fills `tabs`, the second fills
///   `panes`; once both are present it parses them into a `LayoutSnapshot`
///   (P2.00 A-2) and `reply.send(Ok(snapshot))`, then clears the slot. On a
///   `Log` with NO held query: it is dropped (it is a stale reply from an
///   already-retired query).
/// - Every `Render` is forwarded to `tx` regardless of query state.
///
/// **Invariants** (see also the module-level FX-QUERY notes):
/// 1. The inbound `select!` arm never blocks on the query — it only hands the
///    `InFlightQuery` over this channel and returns; ALL awaiting/fulfillment
///    happens here.
/// 2. A timed-out / failed / replaced query can NEVER cause a *later* query's
///    Logs to be misattributed: the slot is retired on close/replace before any
///    Log is matched, and a Log with no held query is dropped.
/// 3. Render frames are never dropped (`Log` is a distinct variant from
///    `Render`).
pub(super) fn render_loop(
    mut receiver: Box<dyn MuxReceiver>,
    tx: mpsc::Sender<Result<ServerFrame, Status>>,
    stop: Arc<AtomicBool>,
    session: &str,
    query_rx: std::sync::mpsc::Receiver<InFlightQuery>,
) -> u64 {
    let mut renders: u64 = 0;
    // The single in-flight layout query, owned exclusively by this thread.
    // At most one is outstanding; a newer one replaces it (replace-on-new).
    let mut in_flight: Option<InFlightQuery> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            log::debug!("relay reader [{session}]: stop-flag set, exiting");
            break;
        }

        // Pre-recv: drain newly handed-off queries (replace-on-new) and retire
        // any held query whose receiver has gone away (grpc timed out).
        drain_queries(&query_rx, &mut in_flight, session);

        let Some(msg) = receiver.recv() else {
            log::info!("relay reader [{session}]: IPC stream closed (recv None)");
            drop(in_flight.take()); // dropping the reply cancels grpc's receiver
            break;
        };

        // Re-check after the (blocking) recv returned — shutdown may have been
        // requested while we were parked.
        if stop.load(Ordering::Relaxed) {
            log::debug!("relay reader [{session}]: stop-flag set after recv, exiting");
            drop(in_flight.take());
            break;
        }

        // Post-recv: a query may have been handed off while we were parked on
        // recv(); pick it up before matching this message so a Log that arrives
        // in the same wakeup is attributed to it.
        drain_queries(&query_rx, &mut in_flight, session);

        match msg {
            MuxServerMsg::Render(content) => {
                let frame = ServerFrame {
                    kind: Some(server_frame::Kind::Render(content)),
                };
                // blocking_send applies backpressure; Err == receiver dropped
                // (gRPC client disconnected) → tear down.
                if tx.blocking_send(Ok(frame)).is_err() {
                    log::info!("relay reader [{session}]: outbound dropped (client gone), exiting");
                    drop(in_flight.take());
                    break;
                }
                renders += 1;
            }

            // FX-QUERY: Log is one reply line to a ListTabs/ListPanes query (the
            // neutral receiver already joined the server `Log` lines into one
            // string). First Log → tabs, second Log → panes; on the second,
            // fulfill the reply and clear the slot. A Log with no in-flight query
            // is a stale reply from an already-retired query — drop it.
            MuxServerMsg::Log(json) => {
                capture_query_log(&mut in_flight, json, session);
            }

            // ── Phase C: control events ──────────────────────────────────────
            //
            // Map neutral MuxEvent variants → ServerFrame::control so the gRPC
            // client can react to session lifecycle changes without polling.
            // Render bytes and control events interleave naturally on the same
            // bounded mpsc channel.
            MuxServerMsg::Event(event) => match event {
                MuxEvent::Exit { reason } => {
                    log::info!("relay reader [{session}]: session Exit: {reason:?}");
                    // Drop any in-flight query (its receiver cancels → grpc falls
                    // back / errors out rather than hanging to the outer timeout).
                    drop(in_flight.take());
                    // Surface the exit reason to the client before closing.
                    let frame = ServerFrame {
                        kind: Some(server_frame::Kind::Control(ControlEvent {
                            kind: "exit".to_owned(),
                            payload: reason,
                        })),
                    };
                    // Best-effort send; if the channel is already gone we just exit.
                    let _ = tx.blocking_send(Ok(frame));
                    break;
                }

                MuxEvent::RenamedSession { name } => {
                    log::info!("relay reader [{session}]: RenamedSession → '{name}'");
                    let frame = ServerFrame {
                        kind: Some(server_frame::Kind::Control(ControlEvent {
                            kind: "renamed_session".to_owned(),
                            payload: name,
                        })),
                    };
                    if tx.blocking_send(Ok(frame)).is_err() {
                        log::info!(
                            "relay reader [{session}]: outbound dropped (client gone), exiting"
                        );
                        break;
                    }
                }

                MuxEvent::ConfigUpdated => {
                    log::info!("relay reader [{session}]: ConfigFileUpdated");
                    let frame = ServerFrame {
                        kind: Some(server_frame::Kind::Control(ControlEvent {
                            kind: "config_updated".to_owned(),
                            payload: String::new(),
                        })),
                    };
                    if tx.blocking_send(Ok(frame)).is_err() {
                        log::info!(
                            "relay reader [{session}]: outbound dropped (client gone), exiting"
                        );
                        break;
                    }
                }

                MuxEvent::SwitchSession { name } => {
                    log::info!("relay reader [{session}]: SwitchSession → '{name}'");
                    let frame = ServerFrame {
                        kind: Some(server_frame::Kind::Control(ControlEvent {
                            kind: "switch_session".to_owned(),
                            payload: name,
                        })),
                    };
                    if tx.blocking_send(Ok(frame)).is_err() {
                        log::info!(
                            "relay reader [{session}]: outbound dropped (client gone), exiting"
                        );
                        break;
                    }
                }
            },

            // Messages with no remote-client semantics (UnblockInputThread,
            // Connected, QueryTerminalSize, PaneRenderUpdate, …) are mapped to
            // `Other` by the backend's receiver and drained here — preserving the
            // per-message loop cadence (stop-flag checks, query draining).
            //
            // Known 0.44.3 limitation: no per-change push for tab/pane structure
            // changes or bell notifications. Clients that need up-to-date
            // layout/bell state should poll GetLayout.
            MuxServerMsg::Other => {
                log::trace!("relay reader [{session}]: draining Other message");
            }
        }
    }
    log::info!("relay reader [{session}]: exited after {renders} renders");
    renders
}

// ─── render_loop query helpers (FX-QUERY) ──────────────────────────────────────

/// Drain newly handed-off layout queries and retire dead ones.
///
/// Called once before and once after each blocking `recv()` in [`render_loop`].
/// Two anti-cross-talk responsibilities, in order:
///
/// 1. **Retire-on-close.** If the held query's `reply.is_closed()` (grpc's
///    `RELAY_QUERY_TIMEOUT` fired and dropped the receiver), drop the slot so a
///    later Log can't be misattributed to the timed-out query. This is the
///    crucial idle-session fix: with no Renders flowing, the only way a stale
///    query is cleared is here (and on replace, below).
/// 2. **Replace-on-new.** Drain `query_rx` non-blocking; the LAST entry wins.
///    Replacing drops the previous `reply`, cancelling its receiver so grpc
///    falls back. (In practice get_layout polls serially, so at most one is
///    queued; the loop is defensive.)
fn drain_queries(
    query_rx: &std::sync::mpsc::Receiver<InFlightQuery>,
    in_flight: &mut Option<InFlightQuery>,
    session: &str,
) {
    // 1. Retire a held query whose consumer has gone away.
    if in_flight.as_ref().is_some_and(|q| q.reply.is_closed()) {
        let seq = in_flight.as_ref().map(|q| q.seq).unwrap_or(0);
        log::debug!(
            "relay reader [{session}]: in-flight query seq={seq} receiver closed \
             (grpc timed out) — retiring slot"
        );
        *in_flight = None;
    }

    // 2. Pick up any newly handed-off queries; a newer one replaces the older.
    while let Ok(q) = query_rx.try_recv() {
        if let Some(prev) = in_flight.take() {
            log::debug!(
                "relay reader [{session}]: query seq={} replaces in-flight seq={} \
                 (dropping old reply → grpc falls back)",
                q.seq,
                prev.seq
            );
            // `prev.reply` drops here → its receiver cancels.
        } else {
            log::trace!("relay reader [{session}]: query seq={} armed", q.seq);
        }
        *in_flight = Some(q);
    }
}

/// Apply a captured `Log` to the in-flight query (FX-QUERY).
///
/// First Log → tabs, second Log → panes. When both are present, parse the two
/// zellij-JSON payloads into a neutral [`LayoutSnapshot`] via the single
/// `crate::multiplexer::parse_zellij_layout` (P2.00 A-2 — moved here from
/// `grpc/layout.rs` so the gRPC layer is backend-agnostic), fulfill the `reply`
/// with the snapshot (or the parse `Err`), and clear the slot. A Log with no
/// in-flight query is a stale reply from an already-retired query — drop it.
///
/// RESIDUAL RISK (Log-ordering assumption): Logs are NOT tagged with the query
/// they answer. In the common timeout case the stalled session emits NO further
/// Logs, so the timed-out query is retired (reply closed) and the next query
/// fills cleanly from its OWN Logs. The one narrow window that remains: a query
/// that timed out AFTER its tabs Log but whose panes Log arrives LATE — if a new
/// query is already armed by then, that straggler panes Log lands in the new
/// query's tabs slot. This is inherent to untagged Logs (see the module NOTE);
/// the alternative is issuing the two actions one-at-a-time, which would
/// reintroduce an await on the inbound loop and is therefore rejected here.
fn capture_query_log(in_flight: &mut Option<InFlightQuery>, json: String, session: &str) {
    let Some(q) = in_flight.as_mut() else {
        log::debug!(
            "relay reader [{session}]: Log ({}B) with no in-flight query — \
             dropping (stale reply)",
            json.len()
        );
        return;
    };

    // If grpc already dropped the receiver, don't bother filling — retire it so
    // the *next* Log (e.g. the panes reply chasing a timed-out tabs query) is
    // also dropped rather than landing in a fresh query's slot.
    if q.reply.is_closed() {
        log::debug!(
            "relay reader [{session}]: in-flight query seq={} receiver closed mid-capture \
             — discarding Log + retiring slot",
            q.seq
        );
        *in_flight = None;
        return;
    }

    if q.tabs.is_none() {
        log::trace!(
            "relay reader [{session}]: captured tabs Log ({}B) for query seq={}",
            json.len(),
            q.seq
        );
        q.tabs = Some(json);
        return;
    }

    // Second Log → panes; both halves present, fulfill and clear.
    let seq = q.seq;
    log::trace!(
        "relay reader [{session}]: captured panes Log ({}B) for query seq={seq} — fulfilling",
        json.len()
    );
    let panes_json = json;
    let q = in_flight.take().expect("checked Some above");
    let tabs_json = q.tabs.unwrap_or_default();

    let result = if tabs_json.is_empty() {
        Err(anyhow::anyhow!(
            "empty tabs JSON from relay query (seq={seq})"
        ))
    } else if panes_json.is_empty() {
        Err(anyhow::anyhow!(
            "empty panes JSON from relay query (seq={seq})"
        ))
    } else {
        // P2.00 A-2: parse here (render thread) into a neutral LayoutSnapshot so
        // the gRPC layer never touches the zellij JSON wire format. This is THE
        // single zellij-JSON → snapshot parse, shared with the ephemeral path.
        crate::multiplexer::parse_zellij_layout(&tabs_json, &panes_json)
    };
    // Receiver may have been dropped between the is_closed() check and now; the
    // send just no-ops in that case.
    let _ = q.reply.send(result);
}

// ─── ShutdownGuard ────────────────────────────────────────────────────────────

/// Tears down the std reader thread on drop.
pub(super) struct ShutdownGuard {
    pub(super) stop: Arc<AtomicBool>,
    pub(super) nudge: Box<dyn MuxSender>,
    pub(super) reader: Option<JoinHandle<u64>>,
    pub(super) rows: u16,
    pub(super) cols: u16,
    /// When set, this attach is read-only and must NOT emit the teardown resize
    /// nudge (review round-2 Major A): the nudge is a `TerminalResize`, which
    /// re-runs zellij's `min_client_terminal_size` and could disturb a writer's
    /// geometry. For RO we rely solely on the IPC-close path to wake the reader.
    pub(super) read_only: bool,
    pub(super) session: String,
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        log::info!("relay [{}]: shutting down", self.session);
        // 1. Ask the reader to stop.
        self.stop.store(true, Ordering::Relaxed);
        // 2. Wake the reader so its parked blocking recv() returns and observes
        //    the stop-flag. Two paths, depending on read-only:
        //
        //    • RW (unchanged from before): send a no-op TerminalResize to the
        //      same dims — enough to provoke a redraw. RW already drives the
        //      session's geometry, so re-asserting its own size is a no-op.
        //
        //    • RO (round-2 Major A): a TerminalResize would make zellij
        //      recompute the shared min terminal size and could shrink a
        //      writer's geometry, so we must NOT resize here. Instead send
        //      `ClientExited`: the server removes this client and closes our
        //      connection, which wakes the parked recv() (returns None) — and
        //      *removing* a client can only raise/keep the min, never lower it,
        //      so a read-only detach can never perturb a writer's geometry.
        //
        //    Errors are ignored — the socket may already be gone.
        if self.read_only {
            let _ = self.nudge.send_client_exited();
        } else {
            let _ = self.nudge.send_resize(self.rows, self.cols);
        }
        // 3. Join — the relay future does not complete until the thread is gone.
        if let Some(handle) = self.reader.take() {
            match handle.join() {
                Ok(renders) => log::info!(
                    "relay [{}]: reader thread joined ({renders} renders total)",
                    self.session
                ),
                Err(_) => log::error!("relay [{}]: reader thread panicked", self.session),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Rx = tokio::sync::oneshot::Receiver<anyhow::Result<crate::multiplexer::LayoutSnapshot>>;

    fn mk_query(seq: u64) -> (InFlightQuery, Rx) {
        let (reply, rx) = tokio::sync::oneshot::channel();
        (
            InFlightQuery {
                seq,
                reply,
                tabs: None,
            },
            rx,
        )
    }

    #[test]
    fn capture_fills_tabs_then_panes_then_fulfills() {
        let (q, mut rx) = mk_query(1);
        let mut in_flight = Some(q);

        // P2.00 A-2: the render thread now PARSES the two captured Logs into a
        // neutral LayoutSnapshot before fulfilling the reply, so the Logs must be
        // valid ListTabs / ListPanes JSON (empty arrays → an empty snapshot). The
        // tabs-then-panes capture/pairing state machine is unchanged.
        capture_query_log(&mut in_flight, "[]".into(), "s");
        assert!(in_flight.is_some(), "after first Log the slot stays armed");

        capture_query_log(&mut in_flight, "[]".into(), "s");
        assert!(in_flight.is_none(), "after second Log the slot is cleared");

        let got = rx.try_recv().expect("reply sent").expect("ok result");
        assert!(
            got.tabs.is_empty(),
            "two empty-array Logs parse into an empty LayoutSnapshot"
        );
    }

    #[test]
    fn capture_with_invalid_json_fulfills_with_parse_error() {
        // P2.00 A-2: a non-empty but unparseable panes Log yields an Err on the
        // reply (the render thread's parse failed); grpc then falls back to the
        // ephemeral path. (Empty payloads are still short-circuited to Err above.)
        let (q, mut rx) = mk_query(7);
        let mut in_flight = Some(q);

        capture_query_log(&mut in_flight, "[]".into(), "s"); // valid tabs
        capture_query_log(&mut in_flight, "not json".into(), "s"); // invalid panes

        let got = rx.try_recv().expect("reply sent");
        assert!(got.is_err(), "unparseable JSON yields an error result");
    }

    #[test]
    fn capture_with_no_in_flight_query_is_a_noop() {
        let mut in_flight: Option<InFlightQuery> = None;
        capture_query_log(&mut in_flight, "stray".into(), "s");
        assert!(in_flight.is_none());
    }

    #[test]
    fn capture_empty_tabs_fulfills_with_error() {
        let (q, mut rx) = mk_query(2);
        let mut in_flight = Some(q);

        capture_query_log(&mut in_flight, String::new(), "s"); // tabs Log → ""
        capture_query_log(&mut in_flight, "PANES".into(), "s");

        let got = rx.try_recv().expect("reply sent");
        assert!(got.is_err(), "empty tabs JSON yields an error result");
    }

    #[test]
    fn capture_retires_slot_when_receiver_closed_mid_capture() {
        let (q, rx) = mk_query(3);
        drop(rx); // grpc timed out and dropped the receiver
        let mut in_flight = Some(q);

        capture_query_log(&mut in_flight, "TABS".into(), "s");
        assert!(
            in_flight.is_none(),
            "closed receiver → slot retired, Log discarded"
        );
    }

    #[test]
    fn drain_retires_query_with_closed_receiver() {
        let (q, rx) = mk_query(4);
        drop(rx);
        let mut in_flight = Some(q);
        let (_tx, query_rx) = std::sync::mpsc::channel::<InFlightQuery>();

        drain_queries(&query_rx, &mut in_flight, "s");
        assert!(
            in_flight.is_none(),
            "in-flight query with a dead receiver is retired"
        );
    }

    #[test]
    fn drain_replaces_in_flight_with_newer_query() {
        let (q1, mut rx1) = mk_query(1);
        let mut in_flight = Some(q1);
        let (tx, query_rx) = std::sync::mpsc::channel::<InFlightQuery>();
        let (q2, _rx2) = mk_query(2);
        tx.send(q2).unwrap();

        drain_queries(&query_rx, &mut in_flight, "s");

        assert_eq!(in_flight.as_ref().unwrap().seq, 2, "newer query armed");
        // The replaced query's reply was dropped → its receiver is cancelled.
        assert!(
            matches!(
                rx1.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed)
            ),
            "replaced query's receiver is cancelled so grpc falls back"
        );
    }

    // ─── P1.03 characterization: render_loop over neutral MuxReceiver ─────────
    //
    // The neutral `MuxReceiver`/`MuxSender` traits let us drive `render_loop` and
    // `ShutdownGuard` with in-process fakes — no live zellij socket. These pin the
    // behavior the relay had over the concrete `AttachReceiver`/`AttachSender`
    // before the dual-handle refactor: render passthrough, control-event
    // forwarding, query-Log pairing, Other-draining, and the read-only-vs-write
    // shutdown nudge.

    use crate::multiplexer::{FullscreenHint, MuxEvent, MuxReceiver, MuxServerMsg, PaneRef};
    use crate::proto::{ServerFrame, server_frame};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A `MuxReceiver` that yields a fixed script of messages, then `None` (EOF).
    struct ScriptedReceiver {
        msgs: VecDeque<MuxServerMsg>,
    }

    impl MuxReceiver for ScriptedReceiver {
        fn recv(&mut self) -> Option<MuxServerMsg> {
            self.msgs.pop_front()
        }
    }

    fn scripted(msgs: Vec<MuxServerMsg>) -> Box<dyn MuxReceiver> {
        Box::new(ScriptedReceiver {
            msgs: VecDeque::from(msgs),
        })
    }

    #[test]
    fn render_loop_forwards_render_control_and_drains_other() {
        let (tx, mut rx) = mpsc::channel::<Result<ServerFrame, Status>>(64);
        let receiver = scripted(vec![
            MuxServerMsg::Render(b"hello".to_vec()),
            MuxServerMsg::Other, // produces no frame; just drained
            MuxServerMsg::Event(MuxEvent::RenamedSession {
                name: "newname".into(),
            }),
            MuxServerMsg::Event(MuxEvent::Exit {
                reason: "bye".into(),
            }),
            // Exit breaks the loop, so this trailing Render is never observed.
            MuxServerMsg::Render(b"after".to_vec()),
        ]);
        let (_qtx, qrx) = std::sync::mpsc::channel::<InFlightQuery>();

        let renders = render_loop(receiver, tx, Arc::new(AtomicBool::new(false)), "s", qrx);
        assert_eq!(
            renders, 1,
            "exactly one Render forwarded (Other/control/Exit not counted)"
        );

        // Frame 1: the render bytes, verbatim.
        match rx.try_recv() {
            Ok(Ok(ServerFrame {
                kind: Some(server_frame::Kind::Render(bytes)),
            })) => assert_eq!(bytes, b"hello"),
            other => panic!("expected render frame, got {other:?}"),
        }
        // Frame 2: renamed_session control (the Other in between emitted nothing).
        match rx.try_recv() {
            Ok(Ok(ServerFrame {
                kind: Some(server_frame::Kind::Control(ev)),
            })) => {
                assert_eq!(ev.kind, "renamed_session");
                assert_eq!(ev.payload, "newname");
            }
            other => panic!("expected renamed_session control, got {other:?}"),
        }
        // Frame 3: exit control with the reason as payload.
        match rx.try_recv() {
            Ok(Ok(ServerFrame {
                kind: Some(server_frame::Kind::Control(ev)),
            })) => {
                assert_eq!(ev.kind, "exit");
                assert_eq!(ev.payload, "bye");
            }
            other => panic!("expected exit control, got {other:?}"),
        }
        // Nothing after Exit (the loop broke before the trailing Render).
        assert!(rx.try_recv().is_err(), "loop must stop emitting after Exit");
    }

    #[test]
    fn render_loop_pairs_query_logs_tabs_then_panes() {
        let (tx, _rx) = mpsc::channel::<Result<ServerFrame, Status>>(64);
        let (qtx, qrx) = std::sync::mpsc::channel::<InFlightQuery>();
        let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
        qtx.send(InFlightQuery {
            seq: 1,
            reply,
            tabs: None,
        })
        .unwrap();

        // Two consecutive Logs: first = tabs JSON, second = panes JSON. P2.00 A-2:
        // the render thread parses them into a LayoutSnapshot, so they must be
        // valid ListTabs / ListPanes JSON (empty arrays → empty snapshot). The
        // tabs-then-panes pairing over the neutral receiver is what this pins.
        let receiver = scripted(vec![
            MuxServerMsg::Log("[]".into()),
            MuxServerMsg::Log("[]".into()),
        ]);

        let _ = render_loop(receiver, tx, Arc::new(AtomicBool::new(false)), "s", qrx);

        let got = reply_rx
            .try_recv()
            .expect("reply fulfilled")
            .expect("ok result");
        assert!(
            got.tabs.is_empty(),
            "paired empty-array Logs parse into an empty LayoutSnapshot"
        );
    }

    /// A `MuxSender` that records the name of each method invoked.
    #[derive(Clone)]
    struct RecordingSender {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingSender {
        fn note(&self, name: &str) {
            self.calls.lock().unwrap().push(name.to_owned());
        }
    }

    impl MuxSender for RecordingSender {
        fn go_to_tab(&mut self, _tab_id: u64) -> anyhow::Result<()> {
            self.note("go_to_tab");
            Ok(())
        }
        fn focus_pane(&mut self, _pane: PaneRef) -> anyhow::Result<()> {
            self.note("focus_pane");
            Ok(())
        }
        fn toggle_fullscreen(
            &mut self,
            _pane: PaneRef,
            _hint: FullscreenHint,
        ) -> anyhow::Result<()> {
            self.note("toggle_fullscreen");
            Ok(())
        }
        fn query_layout(&mut self) -> anyhow::Result<()> {
            self.note("query_layout");
            Ok(())
        }
        fn send_mouse(
            &mut self,
            _kind: crate::multiplexer::MuxMouseKind,
            _col: u16,
            _row: u16,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn send_input_chars(&mut self, _text: &str) -> anyhow::Result<()> {
            self.note("send_input_chars");
            Ok(())
        }
        fn send_input_bytes(&mut self, _bytes: Vec<u8>) -> anyhow::Result<()> {
            self.note("send_input_bytes");
            Ok(())
        }
        fn send_resize(&mut self, _rows: u16, _cols: u16) -> anyhow::Result<()> {
            self.note("send_resize");
            Ok(())
        }
        fn send_client_exited(&mut self) -> anyhow::Result<()> {
            self.note("send_client_exited");
            Ok(())
        }
        fn box_clone(&self) -> Box<dyn MuxSender> {
            Box::new(self.clone())
        }
    }

    fn shutdown_guard_with(read_only: bool, calls: Arc<Mutex<Vec<String>>>) -> ShutdownGuard {
        ShutdownGuard {
            stop: Arc::new(AtomicBool::new(false)),
            nudge: Box::new(RecordingSender { calls }),
            reader: None, // no thread to join in the unit test
            rows: 24,
            cols: 80,
            read_only,
            session: "s".into(),
        }
    }

    #[test]
    fn shutdown_guard_read_only_nudges_via_client_exited() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        drop(shutdown_guard_with(true, calls.clone()));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["send_client_exited"],
            "read-only teardown must nudge via ClientExited (never a resize)"
        );
    }

    #[test]
    fn shutdown_guard_read_write_nudges_via_resize() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        drop(shutdown_guard_with(false, calls.clone()));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["send_resize"],
            "read-write teardown must nudge via a no-op resize"
        );
    }
}
