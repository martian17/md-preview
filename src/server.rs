//! The Phase 2 persistent daemon: an async `warp` + `tokio` server that serves
//! a live preview of a Markdown file and pushes re-rendered HTML to the browser
//! over a WebSocket whenever the file changes on disk.
//!
//! ## Why the pure renderer is reused, not duplicated
//! [`crate::render_page_with`] is the pure seam (no web deps) that assembles the
//! standalone document. The server injects only a tiny WebSocket client into
//! `<head>` and wraps the body in `<div id="doc">`; on each push the client
//! swaps `#doc.innerHTML`. KaTeX's `connectedCallback` and the delegated
//! copy-button click handler (both shipped by `render_page_with`) re-initialise
//! automatically against the new DOM, so no script is re-added per update.
//!
//! ## Threading model (the `YrsSession` is single-threaded)
//! [`crate::session::YrsSession`] is intentionally **not** `Send` (see
//! `file_peer.rs`). warp/tokio futures must be `Send`, so the session never
//! crosses an `.await`. Instead the [`FilePeer`] and its session live entirely
//! on one **blocking** task ([`tokio::task::spawn_blocking`]) — the watch loop.
//! That loop is the sole owner of the document; it communicates with the async
//! world only through `Send` channels carrying plain `String`s:
//!   * a [`tokio::sync::broadcast`] of the freshly-rendered `<body>` HTML, which
//!     every `/ws` subscriber forwards to its browser, and
//!   * an `Arc<Mutex<String>>` holding the latest rendered body so a fresh
//!     `/view` request can render the current state without touching the doc.
//!
//! ## Scope of this slice
//! This first slice serves a **single** file. A `path -> DocSession` registry
//! and cross-file `.md` links are deliberately deferred — see the
//! `TODO(phase2)` markers below and the product BACKLOG. Every path access still
//! goes through [`FilePeer::within`] so the confinement contract holds from day
//! one.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use warp::ws::{Message, WebSocket};
use warp::Filter;

use crate::doc::DocSession;
use crate::file_peer::FilePeer;
use crate::render_markdown;
use crate::render_page_with;
use crate::session::YrsSession;

/// How often the blocking watch loop polls the [`FilePeer`] for coalesced file
/// events. The underlying `notify` watcher is event-driven; this interval only
/// bounds how long after a save the daemon reacts when running its own poll
/// loop (we avoid blocking forever in `recv` so the task can observe shutdown).
const WATCH_POLL: Duration = Duration::from_millis(150);

/// Capacity of the broadcast channel carrying rendered `<body>` HTML. A slow WS
/// client that lags past this many updates is dropped from the *oldest* end
/// (`broadcast`'s lagging semantics); it simply receives the next live update —
/// acceptable for a live preview where only the latest state matters.
const BROADCAST_CAP: usize = 16;

/// Shared application state handed to every warp route.
///
/// Everything here is `Send + Sync` and contains **no** `YrsSession` — the
/// session lives only on the blocking watch task. `root` and `file` are the
/// confinement inputs; `latest_body` is the most recently rendered body so a
/// fresh `/view` reflects current state without driving the (non-`Send`) doc.
#[derive(Clone)]
struct AppState {
    /// Canonical confinement root (the served file's parent directory). ALL
    /// path access is resolved under this via [`FilePeer::within`].
    ///
    /// TODO(phase2): this slice serves a single watched file; the watch loop in
    /// [`spawn_watch`] owns it. Generalise to a `path -> DocSession` registry so
    /// any `.md` under `root` (and cross-file links) gets its own lazily-spawned
    /// session + watch loop + broadcast channel. Tracked in the product BACKLOG.
    root: PathBuf,
    /// Latest rendered body **fragment** (the inner HTML of `#doc`, *not*
    /// wrapped), kept in sync by the watch loop so a fresh `/ws` connection can
    /// send the current state immediately without touching the single-threaded
    /// session. `/view` wraps a freshly-read render in `#doc` itself.
    latest_body: Arc<Mutex<String>>,
    /// Broadcast of the freshly-rendered body **fragment** on every document
    /// change. Each `/ws` client drops it straight into `#doc.innerHTML`.
    tx: broadcast::Sender<String>,
}

/// Build the warp route filter for the given [`AppState`]. Split out from
/// [`serve`] so it is unit/integration-testable without binding a socket.
fn routes(
    state: AppState,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let state = Arc::new(state);

    // GET /healthz -> "ok"  (liveness; lets the thin client detect the daemon)
    let health = warp::path("healthz")
        .and(warp::path::end())
        .and(warp::get())
        .map(|| warp::reply::with_header("ok", "content-type", "text/plain; charset=utf-8"));

    // GET /view?path=<rel> -> full live-preview HTML page (confined).
    let view_state = state.clone();
    let view = warp::path("view")
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::query::<PathQuery>())
        .map(move |q: PathQuery| view_page(&view_state, &q.path));

    // GET /ws?path=<rel> -> WebSocket; forwards each new <body> HTML frame.
    let ws_state = state.clone();
    let ws = warp::path("ws")
        .and(warp::path::end())
        .and(warp::query::<PathQuery>())
        .and(warp::ws())
        .map(move |q: PathQuery, ws: warp::ws::Ws| {
            let st = ws_state.clone();
            ws.on_upgrade(move |socket| client_ws(socket, st, q.path))
        });

    health.or(view).or(ws)
}

/// The `?path=<rel>` query both `/view` and `/ws` accept.
#[derive(serde::Deserialize)]
struct PathQuery {
    path: String,
}

/// Render the full live-preview page for `rel`, confined under the app root.
///
/// On a confinement failure (traversal/symlink escape, or any other resolution
/// error) we return a 400 rather than leak whether a path exists.
fn view_page(state: &AppState, rel: &str) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    // Confine + read the requested file through FilePeer::within. For this slice
    // only the originally-served file is wired to a live watch loop; other paths
    // still resolve + render (confined) but won't push updates yet.
    // TODO(phase2): lazily spawn a session + watch loop per requested path.
    let text = match read_confined(&state.root, rel) {
        Ok(t) => t,
        Err(_) => {
            return warp::reply::with_status("invalid path", StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    let body = render_markdown(&text);
    let page = render_page_with(
        &wrap_doc(&body),
        &ws_client_head(rel),
        "",
    );
    warp::reply::html(page).into_response()
}

/// Wrap a rendered body fragment in the `#doc` container the WS client swaps.
fn wrap_doc(body: &str) -> String {
    format!("<div id=\"doc\">{body}</div>")
}

/// The injected `<head>` snippet: a WebSocket client that connects to `/ws` for
/// this path and replaces `#doc`'s innerHTML on every pushed body. KaTeX's
/// custom-element `connectedCallback` and the delegated copy-button handler
/// (both already in the page) re-init automatically against the new nodes, so
/// nothing is re-registered here.
fn ws_client_head(rel: &str) -> String {
    // Encode the path so it survives as a query value. Done in JS via the
    // browser's encodeURIComponent over a JSON-escaped literal to avoid any
    // server-side string-injection into the script.
    let path_json = json_string(rel);
    format!(
        r#"
        <script>
        (function () {{
            const path = {path_json};
            function connect() {{
                const url = "ws://" + location.host + "/ws?path=" + encodeURIComponent(path);
                const ws = new WebSocket(url);
                ws.onmessage = function (e) {{
                    const doc = document.getElementById('doc');
                    if (doc) doc.innerHTML = e.data;
                }};
                // Auto-reconnect if the daemon restarts or the socket drops.
                ws.onclose = function () {{ setTimeout(connect, 1000); }};
            }}
            connect();
        }})();
        </script>"#
    )
}

/// Serialize `s` as a JSON string literal (quotes included), safe to embed in a
/// `<script>`. Escapes the characters that would break out of the literal or
/// the script context.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"), // never let a literal </script> close the tag
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Resolve `rel` under `root` (confined) and read it as text. Reuses
/// [`FilePeer::within`] so HTTP path handling shares the exact confinement +
/// size-cap logic the watch loop uses — there is no second, weaker path check.
fn read_confined(root: &Path, rel: &str) -> std::io::Result<String> {
    // A throwaway session is fine: we only want `within`'s confinement + the
    // size-capped read via sync_from_disk, then the resulting text.
    let mut peer = FilePeer::within(root, rel, YrsSession::from_text(""))?;
    peer.sync_from_disk()?;
    Ok(peer.session().text())
}

/// Per-connection WebSocket handler: subscribe to the broadcast and forward each
/// new `<body>` HTML as a text frame. For this slice every connection subscribes
/// to the single served file's channel regardless of `rel`.
/// TODO(phase2): look `rel` up in the registry and subscribe to *its* channel.
async fn client_ws(ws: WebSocket, state: Arc<AppState>, _rel: String) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut rx = state.tx.subscribe();

    // Send the current body immediately so a just-loaded page is in sync even if
    // no change has happened since /view rendered it.
    let initial = state.latest_body.lock().unwrap().clone();
    if ws_tx.send(Message::text(initial)).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            // New rendered body from the watch loop -> forward to the browser.
            msg = rx.recv() => match msg {
                Ok(body) => {
                    if ws_tx.send(Message::text(body)).await.is_err() {
                        break; // client gone
                    }
                }
                // Lagged: skip ahead; the next recv delivers the latest state.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Drain inbound frames so pings/closes are handled; ignore content
            // (this slice is preview-only — no inbound edits yet).
            inbound = ws_rx.next() => match inbound {
                Some(Ok(m)) if m.is_close() => break,
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
}

/// Spawn the dedicated watch thread that owns the [`FilePeer`]/[`YrsSession`]
/// for the served file and broadcasts a re-rendered body on every change.
///
/// [`YrsSession`] is deliberately **not** `Send` (see `file_peer.rs`), so it can
/// never be moved onto a thread. We therefore construct the peer *inside* a
/// fresh OS thread (the closure captures only `Send` values: paths, the channel,
/// and the shared `latest_body`) and report the setup outcome back over a
/// one-shot [`std::sync::mpsc`] channel so [`serve`] still surfaces a bad path /
/// watch failure synchronously. A plain thread (not `spawn_blocking`) keeps the
/// blocking `notify`/`sync_from_disk` loop off tokio's worker pool entirely.
fn spawn_watch(
    root: PathBuf,
    file: String,
    tx: broadcast::Sender<String>,
    latest_body: Arc<Mutex<String>>,
) -> std::io::Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();

    std::thread::Builder::new()
        .name("md-preview-watch".into())
        .spawn(move || {
            // Construct + seed the confined peer here, where the !Send session
            // is born and lives for the thread's lifetime.
            let mut peer = match FilePeer::within(&root, &file, YrsSession::from_text("")) {
                Ok(p) => p,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            if let Err(e) = peer.watch().map_err(|e| std::io::Error::other(e.to_string())) {
                let _ = ready_tx.send(Err(e));
                return;
            }

            // Initial render after `watch`'s catch-up sync. We cache the
            // *fragment* (inner HTML of #doc): both the cached value and every
            // broadcast feed `#doc.innerHTML` on the client, so neither is
            // wrapped in the `#doc` container.
            *latest_body.lock().unwrap() = render_markdown(&peer.session().text());
            let _ = ready_tx.send(Ok(()));

            loop {
                // Coalesce any pending fs events into a single sync + render.
                match peer.try_drain() {
                    Ok(true) => {
                        let body = render_markdown(&peer.session().text());
                        *latest_body.lock().unwrap() = body.clone();
                        // A send error only means there are no live subscribers;
                        // the latest body is still cached for the next /ws.
                        let _ = tx.send(body);
                    }
                    Ok(false) => {}
                    Err(_) => { /* transient I/O; next tick re-reads from disk */ }
                }
                std::thread::sleep(WATCH_POLL);
            }
        })?;

    // Block until the watch thread reports its setup result. If the thread
    // panicked before sending, the channel closes and we surface that too.
    ready_rx
        .recv()
        .unwrap_or_else(|_| Err(std::io::Error::other("watch thread exited during setup")))
}

/// Build and run the daemon on `addr`, serving live preview of `file` (resolved
/// under its canonical parent directory as the confinement `root`).
///
/// Returns once the server stops. The caller owns the tokio runtime.
pub async fn serve(file: &Path, addr: SocketAddr) -> std::io::Result<()> {
    // root = canonical parent dir of the served file (ADR-0003 confinement root).
    let abs = file.canonicalize()?;
    let root = abs
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no parent dir"))?
        .to_path_buf();
    let rel = abs
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no name"))?
        .to_string_lossy()
        .into_owned();

    let (tx, _rx) = broadcast::channel::<String>(BROADCAST_CAP);
    let latest_body = Arc::new(Mutex::new(String::new()));

    spawn_watch(root.clone(), rel.clone(), tx.clone(), latest_body.clone())?;

    let state = AppState {
        root,
        latest_body,
        tx,
    };

    warp::serve(routes(state)).run(addr).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("md-preview-srv-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn json_string_escapes_script_breakers() {
        // `</script>` must not be able to close the injected tag.
        let out = json_string("a</script>b");
        assert!(!out.contains("</script>"));
        assert!(out.contains("\\u003c/script\\u003e"));
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn wrap_doc_wraps_in_doc_div() {
        assert_eq!(wrap_doc("<p>hi</p>"), "<div id=\"doc\"><p>hi</p></div>");
    }

    #[test]
    fn ws_client_head_references_ws_and_doc() {
        let head = ws_client_head("a.md");
        assert!(head.contains("new WebSocket"));
        assert!(head.contains("/ws?path="));
        assert!(head.contains("getElementById('doc')"));
        assert!(head.contains(".innerHTML"));
    }

    #[test]
    fn read_confined_reads_in_root_and_rejects_escape() {
        let dir = temp_dir("read");
        std::fs::write(dir.join("doc.md"), "# Hi").unwrap();
        assert_eq!(read_confined(&dir, "doc.md").unwrap(), "# Hi");
        // Traversal escape is rejected by FilePeer::within.
        assert!(read_confined(&dir, "../escape.md").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Broadcast-path verification at the channel level (no browser, no socket):
    /// editing the file makes the watch loop re-render and publish a body that a
    /// subscriber receives. This is the headless stand-in for "a WS client gets
    /// the new body" called out in the verification plan.
    #[tokio::test]
    async fn editing_file_broadcasts_rerendered_body() {
        let dir = temp_dir("bcast");
        let file = dir.join("doc.md");
        std::fs::write(&file, "# One").unwrap();

        let (tx, _rx0) = broadcast::channel::<String>(BROADCAST_CAP);
        let latest = Arc::new(Mutex::new(String::new()));
        spawn_watch(dir.clone(), "doc.md".into(), tx.clone(), latest.clone()).unwrap();

        // Subscribe, then edit the file.
        let mut rx = tx.subscribe();

        // Give the watch loop a moment to establish, then save a change.
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&file, "# Two").unwrap();

        // The next broadcast should carry the re-rendered body for "# Two".
        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("a broadcast should arrive within 3s")
            .expect("channel open");
        assert!(
            got.contains("Two"),
            "broadcast body should reflect the edit, got: {got}"
        );
        // And the cached latest_body reflects the latest render (the fragment
        // the next /ws connection will send as its initial frame).
        assert!(latest.lock().unwrap().contains("Two"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
