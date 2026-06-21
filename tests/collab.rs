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
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

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
/// then poll until the listener accepts (or time out). Returns a
/// [`TestNonceMinter`] sharing the live daemon's auth state so the auth flow can
/// mint a real nonce in-process — there is no HTTP nonce endpoint by design (a
/// `GET /nonce` would let any cookieless loopback client claim a session).
async fn spawn_daemon(
    file: PathBuf,
    addr: SocketAddr,
) -> md_preview::server::TestNonceMinter {
    // `serve_for_test` binds the listener synchronously and spawns the server
    // future onto this runtime, returning the nonce minter immediately.
    let minter = md_preview::server::serve_for_test(&file, addr, true)
        .expect("daemon should bind for the test");

    // Wait for liveness so the WS connect below doesn't race the bind.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("daemon did not become healthy within 5s");
        }
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            // Give warp a beat to be fully ready to upgrade.
            tokio::time::sleep(Duration::from_millis(50)).await;
            return minter;
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

/// Perform the nonce → /claim → Set-Cookie auth flow against the daemon at
/// `addr`. The nonce is minted in-process via the [`TestNonceMinter`] (the
/// production trust path is the uid-authenticated control socket; there is no HTTP
/// nonce endpoint). Returns the raw `Cookie: <value>` header string.
///
/// Uses raw tokio TCP so we don't need an extra HTTP client dep — the payload
/// is trivially small.
async fn claim_session(
    addr: SocketAddr,
    minter: &md_preview::server::TestNonceMinter,
) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // 1. Mint a one-time nonce in-process (same store the daemon validates).
    let nonce = minter.mint_nonce().expect("mint a test nonce");

    // 2. POST /claim with nonce= and next= form body.
    let form = format!("nonce={}&next=%2Fview%3Fpath%3D%2Ffake", nonce);
    let req = format!(
        "POST /claim HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        form.len(),
        form
    );
    let mut conn2 = tokio::net::TcpStream::connect(addr).await.unwrap();
    conn2.write_all(req.as_bytes()).await.unwrap();
    let mut buf2 = Vec::new();
    conn2.read_to_end(&mut buf2).await.unwrap();
    let resp2 = String::from_utf8_lossy(&buf2);

    // Extract Set-Cookie: <token>; Path=…  → keep only the first segment.
    for line in resp2.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("set-cookie:") {
            let val = line["set-cookie:".len()..].trim();
            // Return just the cookie name=value (before the first ";").
            let cookie = val.split(';').next().unwrap_or(val).trim().to_string();
            return cookie;
        }
    }
    panic!("no Set-Cookie header in /claim response:\n{resp2}");
}

/// Connect a binary WebSocket client to `/collab?path=<abs>` on `addr`. `file` is
/// the canonical absolute document path. Obtains a session cookie first via the
/// nonce→/claim flow so the auth gate is satisfied.
async fn connect_collab(
    addr: SocketAddr,
    file: &std::path::Path,
    minter: &md_preview::server::TestNonceMinter,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let cookie = claim_session(addr, minter).await;
    let url = format!("ws://{addr}/collab?path={}", encode_path(file));
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut().insert(
        "cookie",
        cookie.parse().unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("collab websocket should connect");
    ws
}

/// Connect a text WebSocket client to `/ws?path=<abs>` on `addr` (viewer channel).
async fn connect_viewer(
    addr: SocketAddr,
    file: &std::path::Path,
    minter: &md_preview::server::TestNonceMinter,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let cookie = claim_session(addr, minter).await;
    let url = format!("ws://{addr}/ws?path={}", encode_path(file));
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut().insert(
        "cookie",
        cookie.parse().unwrap(),
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("/ws viewer connects");
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
    let minter = spawn_daemon(file.clone(), addr).await;

    let mut ws = connect_collab(addr, &file, &minter).await;

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
    let minter = spawn_daemon(file.clone(), addr).await;

    // Build a local session converged with the server, then make an edit and
    // send it as an Update frame.
    let mut ws = connect_collab(addr, &file, &minter).await;
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
    let minter = spawn_daemon(file.clone(), addr).await;

    // A one-way preview viewer on /ws (text frames of rendered HTML).
    let mut viewer = connect_viewer(addr, &file, &minter).await;
    // First frame is the current body (sent immediately on connect).
    let _initial = tokio::time::timeout(Duration::from_secs(3), viewer.next())
        .await
        .expect("initial /ws frame")
        .expect("frame")
        .expect("ok");

    // A collab editor converges, then inserts unique marker text.
    let mut editor = connect_collab(addr, &file, &minter).await;
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
