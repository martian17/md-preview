//! The Phase 2 persistent daemon: an async `warp` + `tokio` server that serves
//! a live preview of **any** Markdown file under a confinement root and pushes
//! re-rendered HTML to the browser over a WebSocket whenever the file changes on
//! disk.
//!
//! ## Why the pure renderer is reused, not duplicated
//! [`crate::render_page_with`] is the pure seam (no web deps) that assembles the
//! standalone document. The server injects only a tiny WebSocket client into
//! `<head>` and wraps the body in `<div id="doc">`; on each push the client
//! swaps `#doc.innerHTML`. KaTeX's `connectedCallback` and the delegated
//! copy-button click handler (both shipped by `render_page_with`) re-initialise
//! automatically against the new DOM, so no script is re-added per update.
//!
//! ## The document registry
//! The daemon serves any `.md` under the confinement `root`. Each viewed path
//! gets its own [`Entry`] — a live session + watch loop + broadcast channel +
//! latest-rendered-body cache — created **lazily** on the first `/view` or `/ws`
//! for that path and reused thereafter. Entries are stored in
//! `Arc<Mutex<HashMap<PathBuf, Arc<Entry>>>>`, keyed by the *canonical confined
//! path* (so two relative spellings of the same file share one entry).
//!
//! ## Threading model (the `YrsSession` is single-threaded)
//! [`crate::session::YrsSession`] is intentionally **not** `Send` (see
//! `file_peer.rs`). warp/tokio futures must be `Send`, so the session never
//! crosses an `.await` — and it is never shared across threads. Each entry's
//! [`FilePeer`]/session is constructed *inside* its own dedicated OS watch
//! thread and lives there for the thread's lifetime, the sole owner of that
//! document. It communicates with the async world only through `Send` channels
//! carrying plain `String`s:
//!   * a [`tokio::sync::broadcast`] of the freshly-rendered `#doc` body fragment,
//!     which every `/ws` subscriber for that path forwards to its browser, and
//!   * an `Arc<Mutex<String>>` holding the latest rendered body so a fresh
//!     `/view`/`/ws` can reflect the current state without touching the doc.
//!
//! ## Cross-file `.md` links and local assets
//! The rendered HTML emits ordinary relative URLs — both `<a href="other.md">`
//! links and `<img src="pic.png">` (and other local) asset references. Rather
//! than rely on the browser resolving those against `/view?path=…` (which a
//! query string breaks), the server rewrites them in [`rewrite_doc_urls`]:
//!   * relative `.md` links become `/view?path=<rel>` (a confined live page), and
//!   * other relative asset URLs (`src="…"`, plus non-`.md` `href`s) become
//!     `/asset?path=<rel>` (served read-only by the [`asset`](serve_asset) route).
//!
//! Both share one small attribute-rewrite pass: each candidate URL is resolved
//! relative to the current document's directory and re-confined through
//! [`FilePeer::within`]; a URL that escapes the root is left untouched (it simply
//! won't resolve to a confined target). The rewrite is the only post-processing
//! of the otherwise-pure render; `render_markdown` stays unchanged.
//!
//! ## Sandboxed static assets
//! [`GET /asset?path=<rel>`](serve_asset) serves local files referenced by the
//! document (images, stylesheets, …) so they render in the live preview without
//! a CDN. It mirrors `/view`'s confinement exactly: `<rel>` is resolved via
//! [`FilePeer::within`] (traversal/symlink escapes are rejected), the existing
//! [`DEFAULT_MAX_FILE_SIZE`] read cap is enforced, and the bytes are returned
//! read-only with a `Content-Type` inferred from the file extension. It never
//! follows a link outside the root and never writes.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use warp::ws::{Message, WebSocket};
use warp::Filter;

use crate::doc::DocSession;
use crate::file_peer::{FilePeer, DEFAULT_MAX_FILE_SIZE};
use crate::render_markdown;
use crate::render_page_with;
use crate::session::YrsSession;

/// How often each blocking watch loop polls its [`FilePeer`] for coalesced file
/// events. The underlying `notify` watcher is event-driven; this interval only
/// bounds how long after a save the daemon reacts when running its own poll
/// loop (we avoid blocking forever in `recv` so the task can observe shutdown).
const WATCH_POLL: Duration = Duration::from_millis(150);

/// Capacity of each entry's broadcast channel carrying rendered body fragments.
/// A slow WS client that lags past this many updates is dropped from the
/// *oldest* end (`broadcast`'s lagging semantics); it simply receives the next
/// live update — acceptable for a live preview where only the latest state
/// matters.
const BROADCAST_CAP: usize = 16;

/// One live document in the registry: everything a `/view`/`/ws` for a single
/// confined path needs, with **no** `YrsSession` in it (the session lives only
/// on this entry's watch thread). All fields here are `Send + Sync`.
struct Entry {
    /// Latest rendered body **fragment** (the inner HTML of `#doc`, with
    /// cross-file links already rewritten), kept current by the watch loop so a
    /// fresh `/view`/`/ws` reflects the document's current state immediately
    /// without driving the single-threaded session.
    latest_body: Arc<Mutex<String>>,
    /// Broadcast of the freshly-rendered body fragment on every change. Each
    /// `/ws` client for this path drops it straight into `#doc.innerHTML`.
    tx: broadcast::Sender<String>,
}

/// The lazily-populated `canonical path -> Entry` registry.
type Registry = Arc<Mutex<HashMap<PathBuf, Arc<Entry>>>>;

/// Shared application state handed to every warp route.
///
/// Everything here is `Send + Sync` and contains **no** `YrsSession` — each
/// session lives only on its entry's blocking watch task. `root` is the
/// confinement input; `registry` owns the per-path live entries.
#[derive(Clone)]
struct AppState {
    /// Canonical confinement root. ALL path access is resolved under this via
    /// [`FilePeer::within`].
    root: PathBuf,
    /// `path -> Entry`, lazily filled on first access and reused after.
    registry: Registry,
}

impl AppState {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            registry: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolve `rel` under the root and get (or lazily create) its registry
    /// entry, keyed by the canonical confined path so different spellings of the
    /// same file share one entry. Returns the canonical path alongside the entry
    /// (the canonical path is what cross-file links resolve against).
    ///
    /// Errors only on a confinement failure (traversal/symlink escape) or a
    /// watch-setup failure — both surface to the caller as a 400.
    fn entry_for(&self, rel: &str) -> std::io::Result<(PathBuf, Arc<Entry>)> {
        // Confine first so a bad path never reaches the registry. We resolve to
        // the canonical target path (final component included) via a throwaway
        // peer; this is the same `within` check the watch loop uses.
        let canon = FilePeer::within(&self.root, rel, YrsSession::from_text(""))?
            .path()
            .to_path_buf();

        // Fast path: already in the registry.
        if let Some(entry) = self.registry.lock().unwrap().get(&canon).cloned() {
            return Ok((canon, entry));
        }

        // Slow path: create the entry + its watch thread, then insert. We build
        // the channel/cache here (cheap, Send) and hand them to the thread; the
        // !Send session is born inside the thread.
        let (tx, _rx) = broadcast::channel::<String>(BROADCAST_CAP);
        let latest_body = Arc::new(Mutex::new(String::new()));
        spawn_watch(
            self.root.clone(),
            canon.clone(),
            tx.clone(),
            latest_body.clone(),
        )?;
        let entry = Arc::new(Entry { latest_body, tx });

        // Re-check under the lock in case a concurrent request created it first;
        // if so, reuse theirs (and let our just-spawned thread idle harmlessly —
        // its sender has no subscribers and is dropped when our `entry` is).
        let mut reg = self.registry.lock().unwrap();
        let entry = reg.entry(canon.clone()).or_insert(entry).clone();
        Ok((canon, entry))
    }
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

    // GET /asset?path=<rel> -> raw bytes of a confined in-root file, read-only,
    // with a Content-Type inferred from the extension (images, css, …).
    let asset_state = state.clone();
    let asset = warp::path("asset")
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::query::<PathQuery>())
        .map(move |q: PathQuery| serve_asset(&asset_state, &q.path));

    // GET /ws?path=<rel> -> WebSocket; forwards each new body fragment.
    let ws_state = state.clone();
    let ws = warp::path("ws")
        .and(warp::path::end())
        .and(warp::query::<PathQuery>())
        .and(warp::ws())
        .map(move |q: PathQuery, ws: warp::ws::Ws| {
            let st = ws_state.clone();
            ws.on_upgrade(move |socket| client_ws(socket, st, q.path))
        });

    health.or(view).or(asset).or(ws)
}

/// The `?path=<rel>` query both `/view` and `/ws` accept.
#[derive(serde::Deserialize)]
struct PathQuery {
    path: String,
}

/// Render the full live-preview page for `rel`, confined under the app root.
///
/// On a confinement failure (traversal/symlink escape, or any other resolution
/// error) we return a 400 rather than leak whether a path exists. On success the
/// entry is lazily created/reused; the page body comes from the entry's cached
/// latest render (kept current by its watch loop), so `/view` reflects live
/// state without touching the single-threaded session.
fn view_page(state: &AppState, rel: &str) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    let (canon, entry) = match state.entry_for(rel) {
        Ok(pair) => pair,
        Err(_) => {
            return warp::reply::with_status("invalid path", StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    // The cached body fragment already has cross-file links rewritten by the
    // watch loop, resolved relative to this document. Wrap it in #doc and inject
    // the WS client pointed at this path.
    let body = entry.latest_body.lock().unwrap().clone();
    let page = render_page_with(&wrap_doc(&body), &ws_client_head(&rel_for(&state.root, &canon, rel)), "");
    warp::reply::html(page).into_response()
}

/// Serve the raw bytes of a confined in-root file for `/asset?path=<rel>`.
///
/// Read-only, confined exactly like `/view`: `rel` is resolved through
/// [`FilePeer::within`] (traversal/symlink escapes → 400, with no leak of
/// whether a path exists), the existing [`DEFAULT_MAX_FILE_SIZE`] read cap is
/// enforced (oversize → 413), and a missing file is a 404. On success the bytes
/// are returned with a `Content-Type` inferred from the file extension
/// ([`content_type_for`]). This never follows a link outside the root and never
/// writes — it only lets local images/assets referenced by a document render in
/// the live preview without a CDN.
fn serve_asset(state: &AppState, rel: &str) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    let status = |code| warp::reply::with_status("", code).into_response();

    // Confine first: resolve the full path (final component included) under the
    // root. A traversal/symlink escape is a 400, mirroring `/view`.
    let canon = match FilePeer::within(&state.root, rel, YrsSession::from_text("")) {
        Ok(peer) => peer.path().to_path_buf(),
        Err(_) => return status(StatusCode::BAD_REQUEST),
    };

    // Size cap before reading (Security #4): stat so a huge file cannot OOM us.
    match std::fs::metadata(&canon) {
        Ok(meta) if meta.is_dir() => return status(StatusCode::NOT_FOUND),
        Ok(meta) if meta.len() > DEFAULT_MAX_FILE_SIZE => {
            return status(StatusCode::PAYLOAD_TOO_LARGE)
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return status(StatusCode::NOT_FOUND)
        }
        Err(_) => return status(StatusCode::INTERNAL_SERVER_ERROR),
    }

    let bytes = match std::fs::read(&canon) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return status(StatusCode::NOT_FOUND)
        }
        Err(_) => return status(StatusCode::INTERNAL_SERVER_ERROR),
    };

    warp::reply::with_header(bytes, "content-type", content_type_for(&canon)).into_response()
}

/// Infer a `Content-Type` from a file's extension (ASCII-case-insensitive).
///
/// Covers the common asset types a Markdown document references; anything
/// unrecognised falls back to `application/octet-stream` (a safe default the
/// browser will download rather than mis-render).
fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// The relative spelling to embed in the WS-client URL and link rewrites for a
/// given canonical path: prefer the path relative to `root`, falling back to the
/// caller's `rel` if (unexpectedly) the canonical path is not under root.
fn rel_for(root: &Path, canon: &Path, fallback: &str) -> String {
    canon
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Wrap a rendered body fragment in the `#doc` container the WS client swaps.
fn wrap_doc(body: &str) -> String {
    format!("<div id=\"doc\">{body}</div>")
}

/// Rewrite in-document relative URLs so cross-file links and local assets
/// resolve to confined daemon routes:
///   * a relative `href="…"` to a local `.md` file becomes
///     `href="/view?path=<rel>"` (a confined live-preview page), and
///   * a relative `src="…"`, or a relative non-`.md` `href="…"`, becomes
///     `src/href="/asset?path=<rel>"` (the read-only [`serve_asset`] route).
///
/// `body` is a rendered HTML fragment; `doc_rel` is the current document's path
/// relative to `root` (e.g. `sub/a.md`). Each candidate URL is resolved
/// **relative to the current document's directory** and re-confined via
/// [`FilePeer::within`]. On a confinement failure (escape), or for
/// absolute/external/`data:`/fragment-only URLs, the value is left exactly
/// as-is.
///
/// This is deliberately a small, targeted string rewrite over the pure render
/// rather than a change to `render_markdown`: the renderer stays web-agnostic
/// and byte-identical for the standalone use case. `href` and `src` share one
/// pass ([`rewrite_attr`]); they differ only in which daemon route a resolved
/// URL maps to ([`rewritten_doc_url`]).
fn rewrite_doc_urls(body: &str, root: &Path, doc_rel: &str) -> String {
    // Directory of the current document, relative to root, used to resolve a
    // URL the same way a browser would resolve a relative href/src.
    let doc_dir = Path::new(doc_rel).parent().map(Path::to_path_buf).unwrap_or_default();
    // `src` always targets an asset; `href` may target a `.md` view or an asset.
    let out = rewrite_attr(body, "href=\"", |u| rewritten_doc_url(u, root, &doc_dir, false));
    rewrite_attr(&out, "src=\"", |u| rewritten_doc_url(u, root, &doc_dir, true))
}

/// Find every `<attr>="<value>"` occurrence (`attr_open` is e.g. `href="`),
/// run `f` on the value, and substitute its `Some(_)` replacement (leaving the
/// value untouched on `None`). A malformed/unterminated value is emitted
/// verbatim. Shared by the `href`/`src` rewrites so the scanning logic lives in
/// one place.
fn rewrite_attr(body: &str, attr_open: &str, mut f: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;

    while let Some(i) = rest.find(attr_open) {
        // Emit everything up to and including the opening `<attr>="`.
        let start = i + attr_open.len();
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        // The value runs up to the next double quote.
        let Some(end) = after.find('"') else {
            // Malformed; emit the remainder verbatim and stop.
            out.push_str(after);
            return out;
        };
        let value = &after[..end];

        match f(value) {
            Some(new_value) => out.push_str(&new_value),
            None => out.push_str(value),
        }
        out.push('"');

        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// If `url` is a local relative URL that resolves to a confined in-root file,
/// return its rewritten form; otherwise `None` (leave it unchanged).
///
/// A local `.md` target maps to `/view?path=<rel>` (a confined live page).
/// Anything else — and any URL reached via a `src` attribute (`src_attr` true) —
/// maps to `/asset?path=<rel>` (the read-only asset route). Absolute,
/// external (`scheme:`/protocol-relative), `data:`, and fragment-only URLs are
/// rejected (returns `None`).
fn rewritten_doc_url(url: &str, root: &Path, doc_dir: &Path, src_attr: bool) -> Option<String> {
    // Skip empties, fragments, absolute-path, scheme/protocol-relative, and
    // already-rewritten URLs. Only plain relative URLs are candidates. The
    // `://`/`scheme:` checks also cover `data:`, `mailto:`, `http:`, etc.
    if url.is_empty()
        || url.starts_with('#')
        || url.starts_with('/')
        || url.starts_with("//")
        || has_uri_scheme(url)
    {
        return None;
    }

    // Split off any #fragment / ?query so we resolve just the path part, then
    // re-attach the suffix to the rewritten URL.
    let (path_part, suffix) = match url.find(['#', '?']) {
        Some(p) => (&url[..p], &url[p..]),
        None => (url, ""),
    };
    if path_part.is_empty() {
        return None;
    }

    // An `href` to a `.md` file is a cross-file *view* link; everything else
    // (all `src`, and non-`.md` `href`s) is a static *asset*.
    let is_md = path_part.to_ascii_lowercase().ends_with(".md");
    let route = if is_md && !src_attr { "/view" } else { "/asset" };

    // Resolve relative to the current document's directory, then confine.
    let candidate = doc_dir.join(path_part);
    let canon = FilePeer::within(root, &candidate, YrsSession::from_text(""))
        .ok()?
        .path()
        .to_path_buf();
    let rel = canon.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
    if rel.is_empty() {
        return None;
    }

    // Build <route>?path=<rel>, percent-encoding the path value, then re-attach
    // any fragment/query suffix (the fragment lets in-page anchors keep working).
    Some(format!("{}?path={}{}", route, encode_query_value(&rel), suffix))
}

/// Whether `url` begins with a URI scheme (`scheme:` per RFC 3986: an ASCII
/// letter followed by letters/digits/`+`/`-`/`.`, then `:`). Catches `http:`,
/// `https:`, `data:`, `mailto:`, `tel:`, etc. without matching a relative path
/// like `a:b/c.png` would *not* — but a leading drive-like `c:` is rare in web
/// URLs and treating it as a scheme (left untouched) is the safe choice.
fn has_uri_scheme(url: &str) -> bool {
    let mut chars = url.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for (_, c) in chars {
        match c {
            ':' => return true,
            c if c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.' => {}
            _ => return false,
        }
    }
    false
}

/// Minimal percent-encoding for a query-string *value*: keep the unreserved set
/// and `/`/`.`/`-`/`_` (common, safe in a path value); percent-encode the rest.
/// Enough for confined relative paths; avoids pulling in a URL-encoding crate.
fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

/// Per-connection WebSocket handler: resolve `rel` to its registry entry, then
/// subscribe to *that entry's* broadcast and forward each new body fragment.
async fn client_ws(ws: WebSocket, state: Arc<AppState>, rel: String) {
    // Look the path up in the registry (creating its entry if this is the first
    // touch). A confinement failure simply closes the socket.
    let entry = match state.entry_for(&rel) {
        Ok((_, e)) => e,
        Err(_) => return,
    };

    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut rx = entry.tx.subscribe();

    // Send the current body immediately so a just-loaded page is in sync even if
    // no change has happened since /view rendered it.
    let initial = entry.latest_body.lock().unwrap().clone();
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
/// for one confined `target` path and broadcasts a re-rendered body fragment on
/// every change.
///
/// [`YrsSession`] is deliberately **not** `Send` (see `file_peer.rs`), so it can
/// never be moved onto a thread. We therefore construct the peer *inside* a
/// fresh OS thread (the closure captures only `Send` values: the root, the
/// canonical target path, the channel, and the shared `latest_body`) and report
/// the setup outcome back over a one-shot [`std::sync::mpsc`] channel so the
/// caller still surfaces a bad path / watch failure synchronously. A plain
/// thread (not `spawn_blocking`) keeps the blocking `notify`/`sync_from_disk`
/// loop off tokio's worker pool entirely.
///
/// The rendered fragment has cross-file `.md` links and local asset URLs
/// rewritten relative to this document (via [`rewrite_doc_urls`]) before it is
/// cached/broadcast, so every consumer (initial `/view`, initial `/ws` frame,
/// live updates) is consistent.
fn spawn_watch(
    root: PathBuf,
    target: PathBuf,
    tx: broadcast::Sender<String>,
    latest_body: Arc<Mutex<String>>,
) -> std::io::Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();

    // The document's path relative to root — the basis for resolving its
    // outbound links. Computed here (Send) and moved into the thread.
    let doc_rel = rel_for(&root, &target, &target.to_string_lossy());

    std::thread::Builder::new()
        .name("md-preview-watch".into())
        .spawn(move || {
            // Construct + seed the confined peer here, where the !Send session
            // is born and lives for the thread's lifetime. `target` is already a
            // canonical in-root path, so `within` accepts it (and re-confines).
            let mut peer = match FilePeer::within(&root, &target, YrsSession::from_text("")) {
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

            let render = |text: &str| rewrite_doc_urls(&render_markdown(text), &root, &doc_rel);

            // Initial render after `watch`'s catch-up sync. We cache the
            // *fragment* (inner HTML of #doc): both the cached value and every
            // broadcast feed `#doc.innerHTML` on the client, so neither is
            // wrapped in the `#doc` container.
            *latest_body.lock().unwrap() = render(&peer.session().text());
            let _ = ready_tx.send(Ok(()));

            loop {
                // Coalesce any pending fs events into a single sync + render.
                match peer.try_drain() {
                    Ok(true) => {
                        let body = render(&peer.session().text());
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

/// Build and run the daemon on `addr`, serving live preview of any `.md` under
/// the confinement `root` (the canonical parent directory of `file`). The
/// initially-named `file` is not special — it is just the first path the thin
/// client points the browser at; its entry is created lazily on that first
/// `/view` like any other.
///
/// Returns once the server stops. The caller owns the tokio runtime.
pub async fn serve(file: &Path, addr: SocketAddr) -> std::io::Result<()> {
    // root = canonical parent dir of the served file (ADR-0003 confinement root).
    let abs = file.canonicalize()?;
    let root = abs
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no parent dir"))?
        .to_path_buf();

    let state = AppState::new(root);
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
        // Canonicalize so the registry key and `rel_for` comparisons match what
        // `FilePeer::within` produces (temp dirs can be symlinked, e.g. /tmp).
        p.canonicalize().unwrap()
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
    fn entry_for_confines_and_reuses() {
        let dir = temp_dir("entry");
        std::fs::write(dir.join("doc.md"), "# Hi").unwrap();
        let state = AppState::new(dir.clone());

        let (canon1, e1) = state.entry_for("doc.md").unwrap();
        // Same path -> same entry (reused, not recreated).
        let (canon2, e2) = state.entry_for("doc.md").unwrap();
        assert_eq!(canon1, canon2);
        assert!(Arc::ptr_eq(&e1, &e2), "second lookup must reuse the entry");
        assert_eq!(state.registry.lock().unwrap().len(), 1);

        // Traversal escape is rejected (would 400).
        assert!(state.entry_for("../escape.md").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_doc_urls_rewrites_relative_md_only_for_href() {
        let dir = temp_dir("rw");
        std::fs::write(dir.join("a.md"), "a").unwrap();
        std::fs::write(dir.join("b.md"), "b").unwrap();
        std::fs::write(dir.join("img.png"), "PNG").unwrap();

        let body = r##"<a href="b.md">B</a> <a href="https://x/y.md">ext</a> <a href="#frag">f</a> <a href="img.png">i</a>"##;
        let out = rewrite_doc_urls(body, &dir, "a.md");
        assert!(out.contains(r#"href="/view?path=b.md""#), "local .md href -> /view: {out}");
        assert!(out.contains(r#"href="https://x/y.md""#), "external left alone");
        assert!(out.contains(r##"href="#frag""##), "fragment left alone");
        // A non-.md href to an in-root file is an asset link, not a view link.
        assert!(out.contains(r#"href="/asset?path=img.png""#), "non-md href -> /asset: {out}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_doc_urls_rewrites_img_src_to_asset() {
        let dir = temp_dir("rwimg");
        std::fs::write(dir.join("pic.png"), "PNG").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inner.png"), "PNG").unwrap();

        let body = r#"<img src="pic.png" alt="x"> <img src="sub/inner.png"> <img src="https://cdn/x.png"> <img src="data:image/png;base64,AAAA">"#;
        let out = rewrite_doc_urls(body, &dir, "a.md");
        assert!(out.contains(r#"src="/asset?path=pic.png""#), "relative img -> /asset: {out}");
        assert!(out.contains(r#"src="/asset?path=sub/inner.png""#), "nested img -> /asset: {out}");
        assert!(out.contains(r#"src="https://cdn/x.png""#), "external img left alone: {out}");
        assert!(out.contains(r#"src="data:image/png;base64,AAAA""#), "data: URL left alone: {out}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_doc_urls_resolves_relative_to_doc_dir_and_confines() {
        let dir = temp_dir("rwsub");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("top.md"), "t").unwrap();
        std::fs::write(dir.join("sub/child.md"), "c").unwrap();

        // From sub/child.md, "../top.md" resolves to top.md (in-root) -> rewritten.
        let body = r#"<a href="../top.md">up</a> <a href="../../etc/passwd.md">escape</a>"#;
        let out = rewrite_doc_urls(body, &dir, "sub/child.md");
        assert!(out.contains(r#"href="/view?path=top.md""#), "in-root parent link rewritten: {out}");
        // The escape attempt is left untouched (within() rejected it).
        assert!(out.contains(r#"href="../../etc/passwd.md""#), "escape left as-is: {out}");

        // An asset src that escapes the root is likewise left untouched.
        let asset = rewrite_doc_urls(r#"<img src="../../etc/passwd">"#, &dir, "sub/child.md");
        assert!(asset.contains(r#"src="../../etc/passwd""#), "asset escape left as-is: {asset}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_doc_urls_preserves_fragment_suffix() {
        let dir = temp_dir("rwfrag");
        std::fs::write(dir.join("a.md"), "a").unwrap();
        std::fs::write(dir.join("b.md"), "b").unwrap();
        let out = rewrite_doc_urls(r#"<a href="b.md#section">x</a>"#, &dir, "a.md");
        assert!(out.contains(r#"href="/view?path=b.md#section""#), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_type_for_infers_by_extension() {
        assert_eq!(content_type_for(Path::new("pic.png")), "image/png");
        assert_eq!(content_type_for(Path::new("a.JPG")), "image/jpeg");
        assert_eq!(content_type_for(Path::new("a.jpeg")), "image/jpeg");
        assert_eq!(content_type_for(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(content_type_for(Path::new("anim.gif")), "image/gif");
        assert_eq!(content_type_for(Path::new("x.webp")), "image/webp");
        assert_eq!(content_type_for(Path::new("style.css")), "text/css; charset=utf-8");
        // Unknown / no extension -> safe default.
        assert_eq!(content_type_for(Path::new("blob.xyz")), "application/octet-stream");
        assert_eq!(content_type_for(Path::new("noext")), "application/octet-stream");
    }

    #[test]
    fn has_uri_scheme_detects_schemes_not_relative_paths() {
        assert!(has_uri_scheme("https://x/y"));
        assert!(has_uri_scheme("data:image/png;base64,AAAA"));
        assert!(has_uri_scheme("mailto:a@b"));
        assert!(!has_uri_scheme("pic.png"));
        assert!(!has_uri_scheme("sub/pic.png"));
        assert!(!has_uri_scheme("../up/pic.png"));
        assert!(!has_uri_scheme(""));
    }

    #[tokio::test]
    async fn serve_asset_serves_in_root_file_with_content_type() {
        let dir = temp_dir("asset-ok");
        // A tiny PNG-like payload; only the bytes + content-type matter here.
        let bytes: &[u8] = b"\x89PNG\r\n\x1a\nfake-image-bytes";
        std::fs::write(dir.join("pic.png"), bytes).unwrap();
        let state = AppState::new(dir.clone());

        let resp = serve_asset(&state, "pic.png");
        assert_eq!(resp.status(), warp::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "image/png",
            "png served with image/png"
        );
        let body = warp::hyper::body::to_bytes(resp.into_body()).await.unwrap();
        assert_eq!(&body[..], bytes, "served bytes match the file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serve_asset_rejects_traversal_and_missing() {
        let dir = temp_dir("asset-bad");
        let state = AppState::new(dir.clone());

        // `../` traversal escape -> 400 (confinement failure).
        let trav = serve_asset(&state, "../etc/passwd");
        assert_eq!(trav.status(), warp::http::StatusCode::BAD_REQUEST);

        // Absolute path elsewhere -> 400.
        let abs = serve_asset(&state, "/etc/passwd");
        assert_eq!(abs.status(), warp::http::StatusCode::BAD_REQUEST);

        // In-root but nonexistent -> 404.
        let missing = serve_asset(&state, "nope.png");
        assert_eq!(missing.status(), warp::http::StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serve_asset_refuses_over_size_cap() {
        let dir = temp_dir("asset-big");
        // One byte over the default read cap.
        let big = vec![b'x'; (DEFAULT_MAX_FILE_SIZE + 1) as usize];
        std::fs::write(dir.join("big.bin"), &big).unwrap();
        let state = AppState::new(dir.clone());

        let resp = serve_asset(&state, "big.bin");
        assert_eq!(
            resp.status(),
            warp::http::StatusCode::PAYLOAD_TOO_LARGE,
            "oversize asset refused before read"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn img_rewrite_through_render_produces_asset_url() {
        // The img-rewrite called out in the verification plan, exercised over a
        // real render: `![x](pic.png)` -> `<img src="/asset?path=pic.png">`.
        let dir = temp_dir("imgrender");
        std::fs::write(dir.join("pic.png"), b"PNG").unwrap();
        let body = rewrite_doc_urls(&render_markdown("![x](pic.png)"), &dir, "doc.md");
        assert!(body.contains(r#"src="/asset?path=pic.png""#), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Broadcast-path verification at the channel level (no browser, no socket):
    /// editing a file makes *its* entry's watch loop re-render and publish a body
    /// that a subscriber receives. This is the headless stand-in for "a WS client
    /// gets the new body" called out in the verification plan, now exercising the
    /// registry's per-path entry.
    #[tokio::test]
    async fn editing_file_broadcasts_rerendered_body_via_registry() {
        let dir = temp_dir("bcast");
        let file = dir.join("doc.md");
        std::fs::write(&file, "# One").unwrap();
        let state = AppState::new(dir.clone());

        // First touch lazily creates the entry + its watch loop.
        let (_canon, entry) = state.entry_for("doc.md").unwrap();
        let mut rx = entry.tx.subscribe();

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
        // And the cached latest_body reflects the latest render.
        assert!(entry.latest_body.lock().unwrap().contains("Two"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cross-file links work end to end through the registry: viewing a.md (which
    /// links to b.md) yields a body whose link points at /view?path=b.md, and
    /// b.md gets its own working entry.
    #[tokio::test]
    async fn cross_file_link_resolves_through_registry() {
        let dir = temp_dir("xfile");
        std::fs::write(dir.join("a.md"), "# A\n\n[go to B](b.md)\n").unwrap();
        std::fs::write(dir.join("b.md"), "# B\n").unwrap();
        let state = AppState::new(dir.clone());

        let (_ca, ea) = state.entry_for("a.md").unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let a_body = ea.latest_body.lock().unwrap().clone();
        assert!(a_body.contains("href=\"/view?path=b.md\""), "a.md links to b.md view: {a_body}");

        // b.md has its own independent entry.
        let (_cb, eb) = state.entry_for("b.md").unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(eb.latest_body.lock().unwrap().contains("B"));
        assert!(!Arc::ptr_eq(&ea, &eb), "a.md and b.md are distinct entries");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
