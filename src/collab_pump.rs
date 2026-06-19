//! `/collab` — the **warp/tokio WebSocket pump** (the daemon half of collab).
//!
//! The transport-agnostic wire codec and the untrusted-update apply funnel were
//! extracted into [`doc_core::collab`] (ADR-0008 ruling (a)) so the LAN/WAN relay
//! (goal D2) can reuse them without the daemon's socket stack. What stays here is
//! exactly the part that touches warp/tokio: the per-connection byte pump
//! ([`collab_ws`]) and the `tokio::select!` frame loop. The codec is re-exported
//! below so existing `server::collab::{encode_sync_step1, sync_update_payload,
//! CollabMsg, …}` call sites (server.rs and `tests/collab.rs`) resolve unchanged.
//!
//! ## Threading invariant (the whole point)
//! The async WS task here never touches the session. It is a pure byte pump:
//!   * inbound binary frame → [`decode_inbound`] → a `Send` [`CollabMsg`] carrying
//!     only bytes / a `Send` reply channel → the per-path watch thread
//!     (`collab_in`); the watch thread is the **sole** owner/mutator of the
//!     `!Send` `YrsSession`;
//!   * outbound frames arrive on `collab_out` (a broadcast) and are forwarded to
//!     the socket as **binary** (y-protocols frames are not UTF-8).
//!
//! See [`doc_core::collab`] for the codec, the apply funnel ([`apply_client_update`]),
//! the `#5` size cap ([`MAX_UPDATE_BYTES`]), and the full protocol notes.

use std::path::PathBuf;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use warp::ws::{Message as WsMessage, WebSocket};

// Re-export the transport-agnostic codec so daemon call sites keep using
// `collab::<item>` exactly as before the split.
pub use doc_core::collab::{
    apply_client_update, decode_inbound, encode_sync_step1, encode_sync_step2, encode_sync_update,
    sync_update_payload, ApplyOutcome, Inbound, COLLAB_BROADCAST_CAP, MAX_UPDATE_BYTES,
};

/// The daemon's concrete [`doc_core::collab::CollabMsg`] reply channel: a tokio
/// one-shot carrying a fully-encoded y-protocols frame back to a single client.
/// The codec is runtime-agnostic (`CollabMsg<Reply>`); this binds the reply to
/// the daemon's tokio runtime so server.rs uses a plain `CollabMsg` as before.
pub type CollabMsg = doc_core::collab::CollabMsg<oneshot::Sender<Vec<u8>>>;

/// Per-connection `/collab` handler: confine `rel`, find/create the shared
/// per-path [`Entry`](crate::server::Entry), seed the client, then pump frames
/// both ways. The session is never touched here — all doc work goes to the watch
/// thread over `collab_in`, and outbound frames arrive on `collab_out`.
///
/// On a confinement failure the socket is simply closed (the 400-equivalent for
/// an already-upgraded WebSocket), mirroring `/ws`'s `client_ws`.
pub(crate) async fn collab_ws(
    ws: WebSocket,
    collab_in: mpsc::Sender<CollabMsg>,
    collab_out: broadcast::Sender<Vec<u8>>,
    _rel: PathBuf,
) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut out_rx = collab_out.subscribe();

    // --- Connect handshake -------------------------------------------------
    // 1. Send the server's own SyncStep1 (its state vector). The client answers
    //    with a SyncStep2 carrying what we are missing.
    // 2. The client also sends *its* SyncStep1; we answer with a SyncStep2 of
    //    what it is missing. Both directions are driven entirely by the watch
    //    thread (it owns the session); we only relay the encoded frames.
    let (snap_tx, snap_rx) = oneshot::channel();
    if collab_in
        .send(CollabMsg::SnapshotRequest(snap_tx))
        .await
        .is_err()
    {
        return; // watch thread gone
    }
    let server_step1 = match snap_rx.await {
        Ok(frame) => frame,
        Err(_) => return,
    };
    if ws_tx.send(WsMessage::binary(server_step1)).await.is_err() {
        return;
    }

    // --- Frame pump --------------------------------------------------------
    loop {
        tokio::select! {
            // Outbound: a frame produced by the watch thread (a peer's update,
            // an awareness relay, or this client's own SyncStep2 answer) → socket.
            out = out_rx.recv() => match out {
                Ok(frame) => {
                    if ws_tx.send(WsMessage::binary(frame)).await.is_err() {
                        break; // client gone
                    }
                }
                // Lagged: skip ahead. The client's provider re-syncs on the next
                // update; CRDT apply is idempotent so a skipped frame is safe.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Inbound: a binary frame from the browser → classify → watch thread.
            inbound = ws_rx.next() => match inbound {
                Some(Ok(m)) if m.is_close() => break,
                Some(Ok(m)) if m.is_binary() => {
                    let bytes = m.as_bytes();
                    // #5 step 1: cap the raw frame before any decode. A keystroke
                    // frame is tiny; anything over the cap is dropped (optionally
                    // we could disconnect — we drop and keep the socket).
                    if bytes.len() > MAX_UPDATE_BYTES {
                        continue;
                    }
                    match decode_inbound(bytes) {
                        Inbound::SyncStep1(sv) => {
                            // Answer this client's SyncStep1 with a SyncStep2 of
                            // exactly what it is missing — sent only to it.
                            let (reply_tx, reply_rx) = oneshot::channel();
                            if collab_in
                                .send(CollabMsg::SyncStep1(sv, reply_tx))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            match reply_rx.await {
                                Ok(frame) => {
                                    if ws_tx.send(WsMessage::binary(frame)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        Inbound::Update(update) => {
                            if collab_in.send(CollabMsg::ClientUpdate(update)).await.is_err() {
                                break;
                            }
                        }
                        Inbound::Awareness(frame) => {
                            if collab_in.send(CollabMsg::AwarenessFrame(frame)).await.is_err() {
                                break;
                            }
                        }
                        Inbound::Ignore => {}
                    }
                }
                // Non-binary (text/ping/pong) frames are not part of the protocol;
                // ignore their content but keep the socket.
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
}
