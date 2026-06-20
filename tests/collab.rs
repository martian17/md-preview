//! Integration tests for the Phase-3 `/collab` collaborative-editing WebSocket.
//!
//! These drive the **server-wired** behaviour end to end: a real daemon bound on
//! an ephemeral loopback port, a real binary WebSocket client (`tokio-tungstenite`)
//! speaking the y-websocket framing, and the per-path watch thread that owns the
//! `!Send` session. The per-frame decode/encode and the #5 guard are unit-tested
//! next to the code in `src/collab.rs`; here we verify the wiring those units
//! plug into: handshake, write-back without a feedback loop, and that one-way
//! `/ws` preview viewers are not regressed by a `/collab` editor.
//!
//! All of `/collab` is daemon-gated, so these tests require the default
//! `daemon` feature (which is on by default).

#![cfg(feature = "daemon")]

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use md_preview::doc::DocSession;
use md_preview::server::collab;
use md_preview::session::YrsSession;
use tokio_tungstenite::tungstenite::Message as TMessage;

/// A unique temp dir containing `doc.md` with `contents`. Caller cleans up.
fn temp_doc(tag: &str, contents: &str) -> PathBuf {
    static C: AtomicU64 = AtomicU64::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let mut dir = std::env::temp_dir();
    dir.push(format!("md-preview-collab-it-{}-{}-{}", std::process::id(), tag, n));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("doc.md");
    fs::write(&path, contents).unwrap();
    // Canonicalize so the served root matches what `within` produces (temp dirs
    // are often symlinked, e.g. /tmp -> /private/tmp).
    path.canonicalize().unwrap()
}

/// Pick a free loopback port by binding to :0 and reading the assigned port,
/// then dropping the listener. A small race window remains before the daemon
/// rebinds; acceptable for a test (the daemon also has its own retry-free bind).
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    l.local_addr().unwrap().port()
}

/// Spawn the daemon serving `file`'s directory on `addr` as a background task,
/// then poll `/healthz` until it answers (or time out). Returns once it is live.
async fn spawn_daemon(file: PathBuf, addr: SocketAddr) {
    tokio::spawn(async move {
        // `serve` runs until the process ends; the test task is detached.
        let _ = md_preview::server::serve(&file, addr, true).await;
    });

    // Wait for liveness via /healthz so the WS connect below doesn't race the bind.
    let client_url = format!("http://{addr}/healthz");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("daemon did not become healthy within 5s");
        }
        // A bare TCP connect is enough to know the listener is up.
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            // Give warp a beat to be fully ready to upgrade.
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = &client_url;
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Percent-encode an absolute path for use as a `path=` query value (mirrors the
/// server's encoder). A document is identified by its canonical absolute path.
fn encode_path(p: &std::path::Path) -> String {
    let mut out = String::new();
    for b in p.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Connect a binary WebSocket client to `/collab?path=<abs>` on `addr`. `file` is
/// the canonical absolute document path. tungstenite sets `Host: <addr>`
/// (loopback) automatically, satisfying the daemon's host guard.
async fn connect_collab(
    addr: SocketAddr,
    file: &std::path::Path,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let url = format!("ws://{addr}/collab?path={}", encode_path(file));
    let (ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .expect("collab websocket should connect");
    ws
}

/// Receive the next *binary* frame from the socket within a timeout, skipping
/// pings/pongs. Returns the frame bytes.
async fn next_binary(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Vec<u8> {
    let deadline = Duration::from_secs(3);
    loop {
        match tokio::time::timeout(deadline, ws.next()).await {
            Ok(Some(Ok(TMessage::Binary(b)))) => return b,
            Ok(Some(Ok(_))) => continue, // ping/pong/text — skip
            other => panic!("expected a binary frame, got: {other:?}"),
        }
    }
}

/// On connect, the server sends its own SyncStep1 (a type-0 sync frame). A fresh
/// client then sends *its* SyncStep1 and must receive a SyncStep2 carrying the
/// document, converging its local doc to the server's text.
#[tokio::test]
async fn collab_handshake_seeds_a_fresh_client() {
    let file = temp_doc("handshake", "# Title\n\nbody text\n");
    let root = file.parent().unwrap().to_path_buf();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    spawn_daemon(file.clone(), addr).await;

    let mut ws = connect_collab(addr, &file).await;

    // 1. Server's SyncStep1 arrives first (we don't need to act on it here).
    let _server_step1 = next_binary(&mut ws).await;

    // 2. We send our SyncStep1 = our (empty) state vector; expect a SyncStep2.
    let mine = YrsSession::from_text("");
    let step1 = collab::encode_sync_step1(mine.state_vector());
    ws.send(TMessage::Binary(step1)).await.unwrap();

    // The next binary frame should be a SyncStep2; merging it converges us to
    // the server's document text.
    let mut local = YrsSession::from_text("");
    // We may receive the server's own SyncStep1 echo or the SyncStep2; loop a
    // couple of frames feeding any sync update into our local doc.
    for _ in 0..3 {
        let frame = next_binary(&mut ws).await;
        // Decode via the same module the server uses; feed any update bytes in.
        if let Some(update) = collab::sync_update_payload(&frame) {
            local.merge(&update);
        }
        if local.text().contains("body text") {
            break;
        }
    }
    assert!(
        local.text().contains("body text"),
        "client should converge to the server's document, got: {:?}",
        local.text()
    );

    let _ = fs::remove_dir_all(&root);
}

/// A browser-origin edit (sent as a y-sync Update frame) must reach the canonical
/// document and be written back to the file by the watch thread's debounced
/// write-back — and the watcher must NOT re-ingest its own write (no feedback
/// loop / write storm). We assert the file on disk converges to the edited text.
#[tokio::test]
async fn collab_edit_writes_back_without_feedback_loop() {
    let file = temp_doc("writeback", "alpha\n");
    let root = file.parent().unwrap().to_path_buf();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    spawn_daemon(file.clone(), addr).await;

    // Build a local session converged with the server, then make an edit and
    // send it as an Update frame.
    let mut ws = connect_collab(addr, &file).await;
    let _server_step1 = next_binary(&mut ws).await;

    // Sync up: send our SyncStep1, absorb the SyncStep2.
    let mut local = YrsSession::from_text("");
    let step1 = collab::encode_sync_step1(local.state_vector());
    ws.send(TMessage::Binary(step1)).await.unwrap();
    for _ in 0..3 {
        let frame = next_binary(&mut ws).await;
        if let Some(update) = collab::sync_update_payload(&frame) {
            local.merge(&update);
        }
        if local.text() == "alpha\n" {
            break;
        }
    }
    assert_eq!(local.text(), "alpha\n", "client converged to file contents");

    // Edit locally: "alpha\n" -> "alpha beta\n", send the minimal update.
    let before_sv = local.state_vector();
    let edits = md_preview::diff::diff(&local.text(), "alpha beta\n");
    local.apply(&edits);
    let update = local.update_since(&before_sv);
    ws.send(TMessage::Binary(collab::encode_sync_update(update)))
        .await
        .unwrap();

    // The watch thread merges, then debounce-writes the file. Poll the file.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if fs::read_to_string(&file).unwrap() == "alpha beta\n" {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "file did not converge to the browser edit; got: {:?}",
                fs::read_to_string(&file).unwrap()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // No feedback loop / write storm: after the write settles, the file stays
    // stable (the content-compare guard swallows the watch thread's own write).
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "alpha beta\n",
        "file must remain stable — no write storm from re-ingesting our own write"
    );

    let _ = fs::remove_dir_all(&root);
}

/// Viewer-not-regressed: a one-way `/ws` preview viewer still receives a fresh
/// rendered HTML body when a `/collab` editor changes the document. The two
/// channels are independent; the watch thread re-renders + broadcasts HTML on the
/// existing `String` channel for every applied collab update.
#[tokio::test]
async fn collab_edit_still_pushes_html_to_ws_viewer() {
    let file = temp_doc("viewer", "start\n");
    let root = file.parent().unwrap().to_path_buf();
    let port = free_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    spawn_daemon(file.clone(), addr).await;

    // A one-way preview viewer on /ws (text frames of rendered HTML).
    let ws_url = format!("ws://{addr}/ws?path={}", encode_path(&file));
    let (mut viewer, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .expect("/ws viewer connects");
    // First frame is the current body (sent immediately on connect).
    let _initial = tokio::time::timeout(Duration::from_secs(3), viewer.next())
        .await
        .expect("initial /ws frame")
        .expect("frame")
        .expect("ok");

    // A collab editor converges, then inserts unique marker text.
    let mut editor = connect_collab(addr, &file).await;
    let _server_step1 = next_binary(&mut editor).await;
    let mut local = YrsSession::from_text("");
    let step1 = collab::encode_sync_step1(local.state_vector());
    editor.send(TMessage::Binary(step1)).await.unwrap();
    for _ in 0..3 {
        let frame = next_binary(&mut editor).await;
        if let Some(update) = collab::sync_update_payload(&frame) {
            local.merge(&update);
        }
        if local.text() == "start\n" {
            break;
        }
    }
    let before_sv = local.state_vector();
    let edits = md_preview::diff::diff(&local.text(), "start UNIQUEMARKER\n");
    local.apply(&edits);
    let update = local.update_since(&before_sv);
    editor
        .send(TMessage::Binary(collab::encode_sync_update(update)))
        .await
        .unwrap();

    // The /ws viewer must receive a re-rendered HTML body containing the marker.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_marker = false;
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), viewer.next()).await {
            Ok(Some(Ok(TMessage::Text(html)))) => {
                if html.contains("UNIQUEMARKER") {
                    saw_marker = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => {} // timeout tick; loop until the outer deadline
        }
    }
    assert!(
        saw_marker,
        "the /ws preview viewer must still receive HTML when a /collab editor edits"
    );

    let _ = fs::remove_dir_all(&root);
}
