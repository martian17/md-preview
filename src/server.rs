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
use std::time::{Duration, SystemTime};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use warp::ws::{Message, WebSocket};
use warp::Filter;

use crate::auth::{self, Clock, NonceStore, SessionStore};
use crate::confine::{self, ConfineError, ConfinedFile};
use crate::doc::DocSession;
use crate::file_peer::{FilePeer, DEFAULT_MAX_FILE_SIZE};
use crate::render_markdown;
use crate::render_page_with;
use crate::roots::Roots;
use crate::session::YrsSession;

// The Phase-3 collaborative-editing WebSocket lives in its own module, declared
// FROM here (not `lib.rs`): `server.rs` owns the registry/`Entry`/watch thread it
// plugs into, so the wiring stays local to the daemon. The module is itself
// daemon-only (this whole file is `pub mod server` behind the `daemon` feature).
#[path = "collab.rs"]
pub mod collab;
use collab::{CollabMsg, COLLAB_BROADCAST_CAP, MAX_UPDATE_BYTES};

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

/// How long the watch thread waits after the last browser-origin edit before
/// flushing the document to disk (Phase 3 write-back). Debouncing coalesces a
/// burst of keystrokes into one write; the file-peer's content-compare guard
/// then swallows the resulting fs event, so write-back never feeds back as an
/// external edit. Kept short so the on-disk file tracks the live doc closely.
const WRITE_BACK_DEBOUNCE: Duration = Duration::from_millis(500);

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
    /// `/collab` async tasks → this path's watch thread (Phase 3). Carries
    /// **Send bytes only** ([`CollabMsg`]); the `!Send` session stays on the
    /// watch thread. Cloned per connection so editors of the same path feed one
    /// serialization point. The thread holds the matching receiver.
    collab_in: mpsc::Sender<CollabMsg>,
    /// Watch thread → `/collab` async tasks (Phase 3). Carries encoded outbound
    /// y-protocols frames (peer updates + relayed awareness). Each editor
    /// subscribes; drop-oldest lag semantics like [`Entry::tx`].
    collab_out: broadcast::Sender<Vec<u8>>,
}

/// The lazily-populated `canonical path -> Entry` registry.
type Registry = Arc<Mutex<HashMap<PathBuf, Arc<Entry>>>>;

/// The authentication state shared by every route: the bootstrap-nonce store
/// (for `/claim`) and the sliding-expiry session store. Both are guarded by a
/// `std::sync::Mutex`; the lock is always taken *synchronously* and dropped
/// before any `.await` (the security invariant — never hold a `Mutex` across an
/// await point).
struct AuthState {
    /// Single-use, short-TTL bootstrap claim nonces (consumed by `/claim`).
    nonces: Mutex<NonceStore>,
    /// Sliding-expiry per-origin session tokens (issued by `/claim`, validated
    /// on every content route).
    sessions: Mutex<SessionStore>,
    /// Whether to set the `Secure` cookie attribute. `false` for loopback
    /// `http://` today; a future WAN/https exposure flips this.
    secure_cookies: bool,
}

impl AuthState {
    /// Build auth state from a single shared clock factory. Production wires the
    /// system clock; tests inject a deterministic one.
    fn new(clock_factory: impl Fn() -> Clock, secure_cookies: bool) -> Self {
        Self {
            nonces: Mutex::new(NonceStore::new(clock_factory())),
            sessions: Mutex::new(SessionStore::new(clock_factory())),
            secure_cookies,
        }
    }
}

/// Shared application state handed to every warp route.
///
/// Everything here is `Send + Sync` and contains **no** `YrsSession` — each
/// session lives only on its entry's blocking watch task.
///
/// A document is identified by its **canonical absolute path** (the URL-encoded
/// `path=` query value, which must be absolute). All file access is confined
/// against [`Roots::union`] through the [`confine`] funnel; the owning root is
/// registered/renewed in [`Roots`] on each access. The per-path live entries
/// live in `registry`, keyed by that canonical path so two spellings share one.
#[derive(Clone)]
struct AppState {
    /// The multi-root registry. Confinement fans out over `roots.union()`; the
    /// owning root is renewed (sliding TTL) on access. Guarded by a sync mutex,
    /// never held across an `.await`.
    roots: Arc<Mutex<Roots>>,
    /// Auth: bootstrap nonces + session tokens (shared across all routes).
    auth: Arc<AuthState>,
    /// Injected wall clock (millis). The auth stores own their own [`Clock`]s;
    /// this one is held for parity with the design's "injected clock" and for a
    /// future periodic nonce/session sweep. Not read on the hot path yet.
    #[allow(dead_code)]
    clock: Arc<Clock>,
    /// Placeholder for a future capability / static-asset origin (a LATER wave).
    /// Always `None` here — the static-origin split is not built in W3.
    #[allow(dead_code)]
    static_base: Option<PathBuf>,
    /// `canonical path -> Entry`, lazily filled on first access and reused after.
    registry: Registry,
}

impl AppState {
    /// Build state for the given multi-root registry, using the system clock and
    /// loopback (non-secure) cookies.
    fn new(roots: Roots) -> Self {
        Self::with_clocks(roots, Clock::system, false)
    }

    /// Build state with an injected clock factory (one fresh [`Clock`] per store
    /// that needs one) and an explicit `secure_cookies` flag. Tests use this to
    /// drive nonce/session/root TTLs deterministically.
    fn with_clocks(
        roots: Roots,
        clock_factory: impl Fn() -> Clock,
        secure_cookies: bool,
    ) -> Self {
        Self {
            roots: Arc::new(Mutex::new(roots)),
            auth: Arc::new(AuthState::new(&clock_factory, secure_cookies)),
            clock: Arc::new(clock_factory()),
            static_base: None,
            registry: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Confine `requested` (an absolute path from the `path=` query) through the
    /// read funnel, registering/renewing its owning root, and return the held-fd
    /// [`ConfinedFile`]. The Mutex over [`Roots`] is taken synchronously and
    /// dropped before returning — never across an `.await`.
    ///
    /// This is the SOLE entry to the confinement funnel for reads in the daemon.
    /// The auth permission floor is applied by [`serve_confined`] on the returned
    /// metadata, NOT here.
    fn confine_read(&self, requested: &Path) -> Result<ConfinedFile, ConfineError> {
        let now = SystemTime::now();
        // CONFINEMENT: fan out over the ALREADY-registered roots only. We do NOT
        // auto-register a requested path as its own root — that would defeat
        // confinement (any absolute path would become serveable). Roots are
        // registered at startup / via the control plane (`md <file>`); here we
        // only RENEW the owning root's sliding TTL. The lock is released at the
        // end of this synchronous block (never held across an await).
        let union_owned: Vec<crate::roots::Root>;
        {
            let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
            // Renew the TTL of whichever existing root owns this path (if any).
            let _ = roots.owning_root(requested, now);
            union_owned = roots.union().into_iter().cloned().collect();
        }
        let union_refs: Vec<&crate::roots::Root> = union_owned.iter().collect();
        // The funnel needs a Roots reference purely for the denylist; lock again
        // briefly (still synchronous, dropped before return).
        let roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        confine::confine_read(requested, &union_refs, &roots, DEFAULT_MAX_FILE_SIZE)
    }

    /// Confine + atomically write `bytes` to `requested` through the save funnel,
    /// against the already-registered roots only (no auto-registration — see
    /// [`Self::confine_read`]). Mutex never held across an `.await`.
    fn confine_save(&self, requested: &Path, bytes: &[u8]) -> Result<(), ConfineError> {
        let now = SystemTime::now();
        let union_owned: Vec<crate::roots::Root>;
        {
            let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
            let _ = roots.owning_root(requested, now);
            union_owned = roots.union().into_iter().cloned().collect();
        }
        let union_refs: Vec<&crate::roots::Root> = union_owned.iter().collect();
        let roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        confine::confine_save(requested, &union_refs, &roots, bytes)
    }

    /// Snapshot the current root union (cloned) for link rewriting (which needs
    /// `&[&Root]` + a `Roots` for the denylist). Mutex dropped before return.
    fn roots_snapshot(&self) -> Roots {
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Resolve `requested` (absolute) to its canonical path and get (or lazily
    /// create) its registry entry, keyed by the canonical path so different
    /// spellings of the same file share one entry. The owning root (the watch
    /// thread's confinement boundary) is resolved here too.
    ///
    /// Errors only on a confinement failure or a watch-setup failure. Does NOT
    /// apply the auth floor (that is [`serve_confined`]'s job). The held fd from
    /// the confine is dropped after canonicalization — the watch thread re-opens
    /// through its own confined `FilePeer`.
    fn entry_for(&self, requested: &Path) -> Result<(PathBuf, Arc<Entry>), ConfineError> {
        // Confine through the funnel (registers/renews the owning root). The held
        // fd proves the file is openable in-root; we only need its canonical path
        // for the registry key + the watch thread's confinement root.
        let confined = self.confine_read(requested)?;
        let canon = confined.canonical.clone();
        drop(confined);

        // Fast path: already in the registry.
        if let Some(entry) = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&canon)
            .cloned()
        {
            return Ok((canon, entry));
        }

        // The watch thread's confinement root is the owning root's path (a
        // directory root) or the file itself (single-file fallback). Resolve it
        // now so `FilePeer::within` inside the thread re-confines correctly.
        let watch_root = {
            let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
            match roots.owning_root(&canon, SystemTime::now()) {
                Some(root) => match root.kind {
                    crate::roots::RootKind::Directory => root.path,
                    // A single-file root's confinement root is the file's parent
                    // dir (FilePeer::within needs a directory to resolve under).
                    crate::roots::RootKind::SingleFile => canon
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| canon.clone()),
                },
                None => canon
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| canon.clone()),
            }
        };

        // Slow path: create the entry + its watch thread, then insert.
        let (tx, _rx) = broadcast::channel::<String>(BROADCAST_CAP);
        let latest_body = Arc::new(Mutex::new(String::new()));
        let (collab_in, collab_rx) = mpsc::channel::<CollabMsg>(COLLAB_BROADCAST_CAP);
        let (collab_out, _) = broadcast::channel::<Vec<u8>>(COLLAB_BROADCAST_CAP);
        spawn_watch(
            watch_root,
            canon.clone(),
            tx.clone(),
            latest_body.clone(),
            collab_rx,
            collab_out.clone(),
            self.roots_snapshot(),
        )
        .map_err(ConfineError::Io)?;
        let entry = Arc::new(Entry {
            latest_body,
            tx,
            collab_in,
            collab_out,
        });

        // Re-check under the lock in case a concurrent request created it first.
        let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let entry = reg.entry(canon.clone()).or_insert(entry).clone();
        Ok((canon, entry))
    }
}

/// The request context every content route needs: the security headers (for the
/// host + origin guards) and the parsed-and-validated `authenticated` capability
/// derived from the session cookie. Built once per request by [`with_context`].
#[derive(Clone)]
struct ReqContext {
    /// The `Host` header value (DNS-rebinding guard input).
    host: Option<String>,
    /// The `Origin` header value (cross-origin guard input).
    origin: Option<String>,
    /// The `Sec-Fetch-Site` header value (cross-site guard input).
    sec_fetch_site: Option<String>,
    /// Whether the request carried a valid (renewed) session cookie — the
    /// permission-floor capability. `true` unlocks the user's non-world-readable
    /// docs; `false` limits the request to world-readable files.
    authenticated: bool,
}

/// A warp filter that extracts the security headers and resolves the
/// `authenticated` capability from the session cookie (validating + renewing it
/// against the [`SessionStore`]). Used by every CONTENT route — the host/origin
/// guards run inside the handler so a single rejection path covers them all.
fn with_context(
    state: Arc<AppState>,
) -> impl Filter<Extract = (ReqContext,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("host")
        .and(warp::header::optional::<String>("origin"))
        .and(warp::header::optional::<String>("sec-fetch-site"))
        .and(warp::header::optional::<String>("cookie"))
        .map(
            move |host: Option<String>,
                  origin: Option<String>,
                  sec_fetch_site: Option<String>,
                  cookie: Option<String>| {
                // Session extraction: parse the cookie, validate + renew. The
                // Mutex is taken synchronously and dropped before this map
                // returns (never across an await).
                let authenticated = cookie
                    .as_deref()
                    .and_then(auth::parse_session_cookie)
                    .map(|tok| {
                        state
                            .auth
                            .sessions
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .validate_and_renew(&tok)
                    })
                    .unwrap_or(false);
                ReqContext {
                    host,
                    origin,
                    sec_fetch_site,
                    authenticated,
                }
            },
        )
}

/// THE network-defense chokepoint: run the `Host` allowlist (DNS-rebinding) and
/// `Origin`/`Sec-Fetch` (cross-origin) guards. Returns `Some(403 response)` when
/// the request must be rejected, or `None` when it may proceed. Every content
/// route calls this first.
fn network_guard(ctx: &ReqContext) -> Option<warp::reply::Response> {
    use warp::http::StatusCode;
    use warp::reply::Reply;
    // host_guard: a missing/invalid Host is rejected (DNS rebinding defense).
    let host_ok = ctx.host.as_deref().map(auth::host_allowed).unwrap_or(false);
    if !host_ok {
        return Some(
            warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response(),
        );
    }
    // origin_guard: cross-origin / cross-site is rejected.
    if auth::origin_allowed(ctx.origin.as_deref(), ctx.sec_fetch_site.as_deref())
        == auth::OriginCheck::Deny
    {
        return Some(
            warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response(),
        );
    }
    None
}

/// Build the warp route filter for the given [`AppState`]. Split out from
/// [`serve`] so it is unit/integration-testable without binding a socket.
fn routes(
    state: AppState,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let state = Arc::new(state);

    // GET /healthz -> "ok"  (liveness; lets the thin client detect the daemon).
    // Unguarded by design: it serves no file bytes and leaks no path info.
    let health = warp::path("healthz")
        .and(warp::path::end())
        .and(warp::get())
        .map(|| warp::reply::with_header("ok", "content-type", "text/plain; charset=utf-8"));

    // POST /claim (body = the bootstrap nonce) -> consume the nonce, issue a
    // session, Set-Cookie. Origin-EXEMPT per design §2 (the bootstrap file:// page
    // POSTs with `Origin: null`), but still host-guarded. See `claim`.
    let claim_state = state.clone();
    let claim = warp::path("claim")
        .and(warp::path::end())
        .and(warp::post())
        .and(warp::header::optional::<String>("host"))
        .and(warp::body::content_length_limit(4 * 1024))
        .and(warp::body::bytes())
        .map(move |host: Option<String>, body: warp::hyper::body::Bytes| {
            claim(&claim_state, host.as_deref(), &body)
        });

    // GET /view?path=<abs> -> full live-preview HTML page (confined + floored).
    let view_state = state.clone();
    let view = warp::path("view")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .and(warp::query::<ViewQuery>())
        .map(move |ctx: ReqContext, q: ViewQuery| {
            view_page(&view_state, &ctx, &q.path, q.view.as_deref())
        });

    // GET /edit?path=<abs> -> full multi-user collaborative editor page. Confined
    // + FLOORED (audit MED-3): a non-world-readable file requires auth even just
    // to load the editor shell.
    let edit_state = state.clone();
    let edit = warp::path("edit")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .map(move |ctx: ReqContext, q: PathQuery| edit_page(&edit_state, &ctx, &q.path));

    // GET /asset?path=<abs> -> raw bytes of a confined in-root file (floored).
    let asset_state = state.clone();
    let asset = warp::path("asset")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .map(move |ctx: ReqContext, q: PathQuery| serve_asset(&asset_state, &ctx, &q.path));

    // GET /raw?path=<abs> -> the confined file's raw Markdown source (floored).
    let raw_state = state.clone();
    let raw = warp::path("raw")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .map(move |ctx: ReqContext, q: PathQuery| serve_raw(&raw_state, &ctx, &q.path));

    // POST /save?path=<abs> (body = new Markdown) -> write via the confine_save
    // funnel. A WRITE: floored on the EXISTING file's mode when it exists, then
    // confined + size-limited strictly.
    let save_state = state.clone();
    let save = warp::path("save")
        .and(warp::path::end())
        .and(warp::post())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .and(warp::body::content_length_limit(DEFAULT_MAX_FILE_SIZE))
        .and(warp::body::bytes())
        .map(
            move |ctx: ReqContext, q: PathQuery, body: warp::hyper::body::Bytes| {
                save_doc(&save_state, &ctx, &q.path, &body)
            },
        );

    // GET /ws?path=<abs> -> WebSocket; forwards each new body fragment. FLOORED
    // (audit HIGH-1): the floor is checked BEFORE the upgrade so a non-world-
    // readable doc cannot be streamed to an unauthenticated client.
    let ws_state = state.clone();
    let ws = warp::path("ws")
        .and(warp::path::end())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .and(warp::ws())
        .map(move |ctx: ReqContext, q: PathQuery, ws: warp::ws::Ws| {
            ws_route(&ws_state, &ctx, q.path, ws)
        });

    // GET /collab?path=<abs> -> binary y-websocket. FLOORED (audit HIGH-1)
    // exactly like /ws: the floor gate runs before the upgrade.
    let collab_state = state.clone();
    let collab_ws_route = warp::path("collab")
        .and(warp::path::end())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .and(warp::ws())
        .map(move |ctx: ReqContext, q: PathQuery, ws: warp::ws::Ws| {
            collab_route(&collab_state, &ctx, q.path, ws)
        });

    health
        .or(claim)
        .or(view)
        .or(edit)
        .or(asset)
        .or(raw)
        .or(save)
        .or(ws)
        .or(collab_ws_route)
}

/// The `?path=<abs>` query the content routes accept. The value is an absolute
/// filesystem path (URL-encoded by the client); the confinement funnel rejects
/// any path that is not absolute or escapes the active roots.
#[derive(serde::Deserialize)]
struct PathQuery {
    path: String,
}

/// The `/view` query: `?path=<rel>` plus an optional `?view=preview|split|editor`
/// that picks the initial layout (the in-page toolbar can switch it afterwards,
/// persisting the choice in `localStorage`). An unknown/absent `view` falls back
/// to the persisted choice, then to `preview` — handled client-side.
#[derive(serde::Deserialize)]
struct ViewQuery {
    path: String,
    view: Option<String>,
}

// ---------------------------------------------------------------------------
// The single fail-closed permission-floor chokepoint (audit A0 / HIGH-1)
// ---------------------------------------------------------------------------

/// Map a [`ConfineError`] onto the HTTP status the route returns, with NO
/// path-existence leak beyond what the funnel already distinguishes.
fn confine_status(err: &ConfineError) -> warp::http::StatusCode {
    use warp::http::StatusCode;
    match err {
        // Escape / non-absolute / sensitive → 400 (do not confirm existence).
        ConfineError::NotAbsolute(_)
        | ConfineError::Escapes(_)
        | ConfineError::Sensitive(_) => StatusCode::BAD_REQUEST,
        ConfineError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        ConfineError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NOT_FOUND
        }
        ConfineError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// **THE chokepoint** every content-serving read funnels through (audit A0):
/// confine the path through the held-fd read funnel, then apply the auth
/// permission floor to the *fstat'd metadata of that same descriptor* before any
/// byte is handed back. Structured so a route CANNOT serve bytes without passing
/// the floor — the only way to obtain a [`ConfinedFile`] for serving is via this
/// function, which returns `Err(Forbidden)` when the floor denies.
///
/// World-readable files are served always (even unauthenticated); a
/// non-world-readable file requires `ctx.authenticated`. The floor is evaluated
/// on the held fd's metadata, so a symlink swapped in after confinement cannot
/// downgrade the mode (audit HIGH-2/HIGH-3).
fn serve_confined(state: &AppState, ctx: &ReqContext, requested: &Path) -> Result<ConfinedFile, FloorDeny> {
    let confined = state
        .confine_read(requested)
        .map_err(|e| FloorDeny::Confine(confine_status(&e)))?;
    // The floor, on the SAME fd's metadata. Fail closed.
    if auth::floor_deny(auth::mode_of(&confined.metadata), ctx.authenticated) {
        return Err(FloorDeny::Floor);
    }
    Ok(confined)
}

/// Why [`serve_confined`] refused: either a confinement failure (mapped to its
/// status) or a permission-floor denial (always 403). Kept distinct so a route
/// can render the right response.
enum FloorDeny {
    /// Confinement failed; carries the status to return.
    Confine(warp::http::StatusCode),
    /// The permission floor denied (non-world-readable + unauthenticated) → 403.
    Floor,
}

impl FloorDeny {
    fn into_response(self) -> warp::reply::Response {
        use warp::http::StatusCode;
        use warp::reply::Reply;
        match self {
            FloorDeny::Confine(status) => {
                warp::reply::with_status("", status).into_response()
            }
            FloorDeny::Floor => {
                warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response()
            }
        }
    }
}

/// Render the full live-preview page for `requested`, confined + FLOORED.
///
/// First the network guard (host/origin), then the floor chokepoint: a
/// non-world-readable file requires auth (audit A0). On success the entry is
/// lazily created/reused; the page body comes from the entry's cached latest
/// render. Cross-file links are already rewritten to `/view?path=<abs>` /
/// `/asset?path=<abs>` (and the `/outside` 403 sentinel for escapes) by the
/// watch loop.
fn view_page(
    state: &AppState,
    ctx: &ReqContext,
    requested: &str,
    view: Option<&str>,
) -> warp::reply::Response {
    use warp::reply::Reply;

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    let path = Path::new(requested);
    // Floor gate FIRST (one chokepoint). We drop the held fd: the watch loop
    // re-opens through its own confined FilePeer; the floor has already passed.
    if let Err(deny) = serve_confined(state, ctx, path) {
        return deny.into_response();
    }

    let (canon, entry) = match state.entry_for(path) {
        Ok(pair) => pair,
        Err(e) => return FloorDeny::Confine(confine_status(&e)).into_response(),
    };

    let doc_id = path_id(&canon);
    let body = entry.latest_body.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let initial_view = normalize_view(view);
    let page = render_page_with(
        &wrap_doc(&body),
        &format!("{}{}", ws_client_head(&doc_id), views_head()),
        &views_body(&doc_id, initial_view),
    );
    warp::reply::html(page).into_response()
}

/// Render the full collaborative editor page for `requested`, confined + FLOORED
/// (audit MED-3: `/edit` previously skipped the floor). The page's client
/// connects back to `/collab?path=<abs>`; the per-path session is created lazily
/// by that upgrade.
fn edit_page(state: &AppState, ctx: &ReqContext, requested: &str) -> warp::reply::Response {
    use warp::reply::Reply;

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    let path = Path::new(requested);
    let canon = match serve_confined(state, ctx, path) {
        Ok(confined) => confined.canonical.clone(),
        Err(deny) => return deny.into_response(),
    };
    let doc_id = path_id(&canon);
    warp::reply::html(crate::render_editor_page(&doc_id)).into_response()
}

// ---------------------------------------------------------------------------
// /claim — consume a bootstrap nonce → issue a session (design §2, step 4)
// ---------------------------------------------------------------------------

/// `POST /claim` (body = the bootstrap nonce): consume the single-use nonce via
/// the [`NonceStore`], issue a fresh session, and reply `204` with a
/// `Set-Cookie` carrying the session token (`HttpOnly; SameSite=Strict; Path=/`,
/// `secure=false` for loopback).
///
/// **Origin-EXEMPT by design:** the bootstrap `file://` page POSTs with
/// `Origin: null`, so we do NOT run the origin guard here (it is the one route
/// that legitimately crosses origins). We DO still enforce the `Host` allowlist
/// (DNS-rebinding defense). The nonce is short-TTL + single-use + high-entropy,
/// so an attacker who cannot read the `0600` bootstrap file cannot forge it.
fn claim(state: &AppState, host: Option<&str>, body: &[u8]) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    // Host guard still applies (the rebinding stopgap). Origin is exempt.
    if !host.map(auth::host_allowed).unwrap_or(false) {
        return warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response();
    }

    let nonce = match std::str::from_utf8(body) {
        Ok(n) => n.trim(),
        Err(_) => return warp::reply::with_status("", StatusCode::BAD_REQUEST).into_response(),
    };

    // Consume (burn) the nonce in constant time. The Mutex is taken + released
    // synchronously (no await held).
    let consumed = state
        .auth
        .nonces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .verify_and_consume(nonce);
    if !consumed {
        return warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response();
    }

    // Issue a session token.
    let token = match state
        .auth
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .issue()
    {
        Ok(t) => t,
        Err(_) => {
            return warp::reply::with_status("", StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    };

    let max_age = auth::SESSION_TTL.as_secs();
    let cookie = auth::build_set_cookie(&token, state.auth.secure_cookies, Some(max_age));
    warp::reply::with_header(
        warp::reply::with_status("", StatusCode::NO_CONTENT),
        "set-cookie",
        cookie,
    )
    .into_response()
}

/// Mint a fresh bootstrap nonce (the daemon's control-plane side of §2 step 1).
/// Exposed so the control plane / tests can arm a claim. Returns the nonce
/// string to embed in the `0600` bootstrap file.
#[allow(dead_code)]
fn mint_claim_nonce(state: &AppState) -> Result<String, getrandom::Error> {
    state
        .auth
        .nonces
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mint()
}

/// The canonical-absolute-path document identity used in `path=` query values
/// and the WS-client URL. A document is identified by its canonical absolute
/// path (URL-encoded by the caller); this is just that path as a UTF-8 string.
fn path_id(canon: &Path) -> String {
    canon.to_string_lossy().replace('\\', "/")
}

/// The three supported layouts. `preview` is the default editor-less live view;
/// `split` is editor + preview side by side; `editor` is a full-page textarea.
const VIEW_MODES: [&str; 3] = ["preview", "split", "editor"];

/// Normalise a `?view=` value to one of [`VIEW_MODES`], or `None` if unset/
/// unrecognised (the client then falls back to `localStorage`, then `preview`).
fn normalize_view(view: Option<&str>) -> Option<&'static str> {
    let v = view?;
    VIEW_MODES.iter().copied().find(|&m| m == v)
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
fn serve_asset(state: &AppState, ctx: &ReqContext, requested: &str) -> warp::reply::Response {
    use std::io::Read;
    use warp::http::StatusCode;
    use warp::reply::Reply;

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    // ONE chokepoint: confine (held fd) + floor on the fstat'd metadata.
    let mut confined = match serve_confined(state, ctx, Path::new(requested)) {
        Ok(c) => c,
        Err(deny) => return deny.into_response(),
    };
    // Read from the HELD fd — no re-open, no second stat (audit HIGH-2/3).
    let mut bytes = Vec::with_capacity(confined.metadata.len() as usize);
    if confined.file.read_to_end(&mut bytes).is_err() {
        return warp::reply::with_status("", StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    let ct = content_type_for(&confined.canonical);
    warp::reply::with_header(bytes, "content-type", ct).into_response()
}

/// Serve the confined file's raw Markdown source for `GET /raw?path=<rel>`.
///
/// This is the source the in-browser editor loads into its textarea. Confined
/// and size-limited exactly like [`serve_asset`]/`view`: `rel` is resolved
/// through [`FilePeer::within`] (traversal/symlink escape → **400**, with no
/// leak of whether a path exists), the existing [`DEFAULT_MAX_FILE_SIZE`] read
/// cap is enforced (oversize → **413**), a missing file is **404**, and a
/// directory is **404**. The bytes are returned verbatim as
/// `text/plain; charset=utf-8`. Read-only; never follows a link outside the
/// root and never writes.
fn serve_raw(state: &AppState, ctx: &ReqContext, requested: &str) -> warp::reply::Response {
    use std::io::Read;
    use warp::http::StatusCode;
    use warp::reply::Reply;

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    let mut confined = match serve_confined(state, ctx, Path::new(requested)) {
        Ok(c) => c,
        Err(deny) => return deny.into_response(),
    };
    let mut bytes = Vec::with_capacity(confined.metadata.len() as usize);
    if confined.file.read_to_end(&mut bytes).is_err() {
        return warp::reply::with_status("", StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    warp::reply::with_header(bytes, "content-type", "text/plain; charset=utf-8").into_response()
}

/// Write a new Markdown body to the confined file for `POST /save?path=<rel>`.
///
/// **This is the one WRITE endpoint, so confinement is strict.** `rel` is
/// resolved through [`FilePeer::within`] (traversal/symlink escape, absolute
/// path elsewhere → **400**, with no path-existence leak); the body is rejected
/// if it exceeds [`DEFAULT_MAX_FILE_SIZE`] (→ **413**; warp also rejects an
/// over-cap request earlier via `content_length_limit`). On success the bytes
/// are written through the same [`FilePeer`]/`write_to_disk` path used
/// elsewhere — the file is only ever written *inside* the confinement root.
///
/// We do **not** drive the registry's single-threaded session here: instead we
/// let the file be the source of truth. After the write the per-path watch loop
/// observes the change, ingests it (its content-compare feedback guard means our
/// write is treated as an ordinary external edit), re-renders, and broadcasts
/// the new body over `/ws` → every preview pane updates. Writing the bytes that
/// the editor sent (rather than round-tripping through this request's session)
/// keeps `/save` stateless and avoids touching the `!Send` session from the
/// async world.
///
/// **First cut is last-write-wins via the file.** Concurrent editors (or an
/// external edit) can overwrite each other; the editing client's textarea may go
/// stale until reloaded. Acceptable for v1 (see the CHANGELOG / editor JS note).
fn save_doc(
    state: &AppState,
    ctx: &ReqContext,
    requested: &str,
    body: &[u8],
) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    let status = |code| warp::reply::with_status("", code).into_response();

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }

    // Defence in depth: re-check the materialised body against the cap.
    if body.len() as u64 > DEFAULT_MAX_FILE_SIZE {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    }
    // The new contents must be valid UTF-8 (the document model is text).
    if std::str::from_utf8(body).is_err() {
        return status(StatusCode::BAD_REQUEST);
    }

    let path = Path::new(requested);

    // FLOOR on a WRITE: if the target already exists, you may only overwrite it
    // if you could already read it (world-readable) OR you are authenticated.
    // This routes the existing file through the same held-fd floor chokepoint;
    // a brand-new in-root file (NotFound) is allowed (nothing to protect yet).
    match serve_confined(state, ctx, path) {
        Ok(_existing) => { /* floor passed on the existing file */ }
        Err(FloorDeny::Floor) => return FloorDeny::Floor.into_response(),
        Err(FloorDeny::Confine(s)) if s == StatusCode::NOT_FOUND => { /* new file */ }
        Err(deny) => return deny.into_response(),
    }

    // Write through the SOLE save funnel (symlink-safe atomic rename).
    match state.confine_save(path, body) {
        Ok(()) => status(StatusCode::NO_CONTENT),
        Err(e) => status(confine_status(&e)),
    }
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

/// The `/outside` 403 sentinel a document link is rewritten to when its target
/// escapes every active root (audit: don't emit a live link to an out-of-root
/// file). The route table has no `/outside` handler, so a click yields a 403/404
/// — the deliberate "this link leaves the workspace" marker.
const OUTSIDE_SENTINEL: &str = "/outside";

/// Wrap a rendered body fragment in the `#doc` container the WS client swaps.
fn wrap_doc(body: &str) -> String {
    format!("<div id=\"doc\">{body}</div>")
}

/// Rewrite in-document relative URLs so cross-file links and local assets
/// resolve to confined daemon routes, using the multi-root confinement funnel:
///   * a relative `href="…"` to a local `.md` file becomes
///     `href="/view?path=<abs>"` (a confined live-preview page),
///   * a relative `src="…"`, or a relative non-`.md` `href="…"`, becomes
///     `src/href="/asset?path=<abs>"` (the read-only [`serve_asset`] route), and
///   * a relative URL that **escapes every active root** becomes the
///     [`OUTSIDE_SENTINEL`] (`/outside`) — the 403 "leaves the workspace" marker
///     instead of a live link to an out-of-root file.
///
/// `body` is a rendered HTML fragment; `doc_canon` is the current document's
/// CANONICAL ABSOLUTE path. Each candidate URL is resolved **relative to the
/// document's directory** (absolute) and classified via [`confine::confine_link`]
/// against `roots.union()`. Absolute/external/`data:`/fragment-only URLs are
/// left exactly as-is.
fn rewrite_doc_urls(body: &str, roots: &Roots, doc_canon: &Path) -> String {
    let doc_dir = doc_canon.parent().map(Path::to_path_buf).unwrap_or_default();
    let union_owned: Vec<crate::roots::Root> = roots.union().into_iter().cloned().collect();
    let union: Vec<&crate::roots::Root> = union_owned.iter().collect();
    // `src` always targets an asset; `href` may target a `.md` view or an asset.
    let out = rewrite_attr(body, "href=\"", |u| {
        rewritten_doc_url(u, &doc_dir, &union, roots, false)
    });
    rewrite_attr(&out, "src=\"", |u| {
        rewritten_doc_url(u, &doc_dir, &union, roots, true)
    })
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

/// Rewrite a local relative document URL through the multi-root confine funnel.
///
/// - A relative URL resolving in-root → `Some("/view?path=<abs>")` (a `.md`
///   `href`) or `Some("/asset?path=<abs>")` (a `src`, or a non-`.md` `href`),
///   the canonical absolute path percent-encoded as the `path=` value.
/// - A relative URL that **escapes every root** → `Some(OUTSIDE_SENTINEL)` (the
///   `/outside` 403 marker — `confine_link` returns `LinkResolution::Outside`).
/// - Absolute, external (`scheme:`/protocol-relative), `data:`, and
///   fragment-only URLs → `None` (left exactly as-is).
fn rewritten_doc_url(
    url: &str,
    doc_dir: &Path,
    union: &[&crate::roots::Root],
    roots: &Roots,
    src_attr: bool,
) -> Option<String> {
    // Only plain relative URLs are candidates. The `://`/`scheme:` checks also
    // cover `data:`, `mailto:`, `http:`, etc.
    if url.is_empty()
        || url.starts_with('#')
        || url.starts_with('/')
        || url.starts_with("//")
        || has_uri_scheme(url)
    {
        return None;
    }

    // Split off any #fragment / ?query so we resolve just the path part.
    let (path_part, suffix) = match url.find(['#', '?']) {
        Some(p) => (&url[..p], &url[p..]),
        None => (url, ""),
    };
    if path_part.is_empty() {
        return None;
    }

    let is_md = path_part.to_ascii_lowercase().ends_with(".md");
    let route = if is_md && !src_attr { "/view" } else { "/asset" };

    // Resolve relative to the current document's (absolute) directory, then
    // classify via the funnel. An escape → the `/outside` 403 sentinel.
    let candidate = doc_dir.join(path_part);
    match confine::confine_link(&candidate, union, roots) {
        confine::LinkResolution::InRoot(canon) => {
            let abs = canon.to_string_lossy().replace('\\', "/");
            Some(format!("{}?path={}{}", route, encode_query_value(&abs), suffix))
        }
        confine::LinkResolution::Outside => Some(OUTSIDE_SENTINEL.to_string()),
    }
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

/// The `<head>` CSS for the three-view layout. Pure CSS, no per-request data:
/// the active layout is selected by a `data-view` attribute on `<body>` (set by
/// [`views_body`]'s script), so switching views is a class/attribute flip with
/// no re-render. In `preview` mode the editor pane is hidden and the page looks
/// exactly like the original editor-less live preview. `split` shows the editor
/// (left) and the preview (right); `editor` is a full-page textarea.
fn views_head() -> &'static str {
    r#"
        <style>
        /* Three-view layout. The body is the preview surface (.markdown-body);
           we lay it out as a flex row holding the editor pane + the preview
           (#doc), with a fixed toolbar on top. `data-view` on <body> selects
           which panes are visible. */
        body.mdv { max-width: none; margin: 0; padding: 0; display: flex; flex-direction: column; min-height: 100vh; box-sizing: border-box; }
        .mdv-toolbar {
            position: sticky; top: 0; z-index: 10;
            display: flex; gap: 6px; align-items: center;
            padding: 6px 10px; border-bottom: 1px solid rgba(175,184,193,.3);
            background: #f6f8fa; font: 13px/1 system-ui, sans-serif;
        }
        .mdv-toolbar button {
            padding: 4px 10px; border: 1px solid rgba(175,184,193,.4);
            border-radius: 6px; background: #fff; color: #24292f; cursor: pointer;
        }
        .mdv-toolbar button.active { background: #0969da; color: #fff; border-color: #0969da; }
        .mdv-panes { flex: 1 1 auto; display: flex; min-height: 0; }
        .mdv-editor {
            box-sizing: border-box; width: 50%; border: none; outline: none; resize: none;
            padding: 16px; font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
            background: #fff; color: #24292f;
            border-right: 1px solid rgba(175,184,193,.3);
        }
        /* #doc holds the rendered preview; it scrolls independently and is
           centred like the standalone page via its own max-width. */
        .mdv-panes #doc { flex: 1 1 auto; overflow: auto; padding: 0 45px; box-sizing: border-box; }
        .mdv-panes #doc > * { max-width: 980px; margin-left: auto; margin-right: auto; }
        @media (max-width: 767px) { .mdv-panes #doc { padding: 0 15px; } }

        /* preview (default): editor hidden, preview full width. */
        body[data-view="preview"] .mdv-editor { display: none; }
        body[data-view="preview"] .mdv-panes #doc { width: 100%; }
        /* split: editor + preview side by side. */
        body[data-view="split"] .mdv-editor { display: block; }
        /* editor: full-page textarea, preview hidden. */
        body[data-view="editor"] .mdv-editor { display: block; width: 100%; border-right: none; }
        body[data-view="editor"] .mdv-panes #doc { display: none; }

        @media (prefers-color-scheme: dark) {
            .mdv-toolbar { background: #161b22; border-color: rgba(99,110,123,.4); }
            .mdv-toolbar button { background: #21262d; color: #c9d1d9; border-color: rgba(99,110,123,.4); }
            .mdv-toolbar button.active { background: #1f6feb; color: #fff; border-color: #1f6feb; }
            .mdv-editor { background: #0d1117; color: #c9d1d9; border-color: rgba(99,110,123,.4); }
        }
        </style>"#
}

/// The before-`</body>` markup + JS for the three-view toolbar and editor.
///
/// Vanilla JS, no deps. On load it: picks the initial view (`?view=` →
/// `localStorage` → `preview`), wires the three toolbar buttons (each persists
/// the choice in `localStorage`), fetches `/raw?path=` into the textarea, and on
/// input **debounces ~400ms** then `POST`s the textarea to `/save?path=`. The
/// preview pane (#doc) keeps updating via the existing `/ws` push — saving the
/// file makes the watch loop re-render and broadcast, so the editor never pushes
/// HTML itself.
///
/// **v1 caveat (last-write-wins):** the textarea is loaded once and is not
/// re-synced from `/ws`; an external edit (or another tab) can leave it stale
/// until reload. Acceptable for this non-collaborative first cut (ADR-0002
/// describes the future ephemeral-CRDT editing model — not built here).
fn views_body(rel: &str, initial_view: Option<&str>) -> String {
    let path_json = json_string(rel);
    // The server-side initial view (from ?view=), or JS `null` to let the client
    // fall back to localStorage/preview. Always one of VIEW_MODES, so a bare
    // identifier-safe string literal is injected via json_string.
    let initial_json = match initial_view {
        Some(v) => json_string(v),
        None => "null".to_string(),
    };
    format!(
        r#"
        <div class="mdv-panes">
            <textarea class="mdv-editor" spellcheck="false" aria-label="Markdown source"></textarea>
        </div>
        <script>
        (function () {{
            const path = {path_json};
            const initialView = {initial_json};
            const MODES = ["preview", "split", "editor"];
            const KEY = "md-preview:view";
            const body = document.body;

            // The page renders #doc inside the body; move it into the panes row
            // so the editor sits beside it.
            const panes = document.querySelector('.mdv-panes');
            const editor = panes.querySelector('.mdv-editor');
            const doc = document.getElementById('doc');
            if (doc) panes.appendChild(doc);

            // Build the toolbar.
            const bar = document.createElement('div');
            bar.className = 'mdv-toolbar';
            const buttons = {{}};
            for (const m of MODES) {{
                const b = document.createElement('button');
                b.type = 'button';
                b.textContent = m.charAt(0).toUpperCase() + m.slice(1);
                b.addEventListener('click', function () {{ setView(m, true); }});
                bar.appendChild(b);
                buttons[m] = b;
            }}
            body.classList.add('mdv');
            body.insertBefore(bar, body.firstChild);

            function setView(mode, persist) {{
                if (!MODES.includes(mode)) mode = 'preview';
                body.setAttribute('data-view', mode);
                for (const m of MODES) buttons[m].classList.toggle('active', m === mode);
                if (persist) {{ try {{ localStorage.setItem(KEY, mode); }} catch (e) {{}} }}
            }}

            // Initial view: ?view= (server-validated) -> localStorage -> preview.
            let stored = null;
            try {{ stored = localStorage.getItem(KEY); }} catch (e) {{}}
            setView(initialView || stored || 'preview', false);

            // Load the raw source into the editor.
            const rawUrl = "/raw?path=" + encodeURIComponent(path);
            fetch(rawUrl).then(function (r) {{
                if (r.ok) return r.text();
                throw new Error('raw load failed: ' + r.status);
            }}).then(function (text) {{
                editor.value = text;
            }}).catch(function (err) {{ console.error(err); }});

            // Debounced save: POST the textarea to /save; the preview updates via
            // the existing /ws push once the watch loop ingests the write.
            let timer = null;
            const saveUrl = "/save?path=" + encodeURIComponent(path);
            editor.addEventListener('input', function () {{
                if (timer) clearTimeout(timer);
                timer = setTimeout(function () {{
                    fetch(saveUrl, {{
                        method: 'POST',
                        headers: {{ 'Content-Type': 'text/plain; charset=utf-8' }},
                        body: editor.value,
                    }}).catch(function (err) {{ console.error('save failed:', err); }});
                }}, 400);
            }});
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

/// The `/ws` route entry (synchronous): run the network guard, then the FLOOR
/// gate (audit HIGH-1) BEFORE upgrading. A non-world-readable doc requested by
/// an unauthenticated client is rejected with a 403 and the socket is NEVER
/// upgraded — so no body fragment can ever stream to it. On a confinement /
/// floor failure we return the matching HTTP response (the upgrade is declined).
fn ws_route(
    state: &Arc<AppState>,
    ctx: &ReqContext,
    requested: String,
    ws: warp::ws::Ws,
) -> warp::reply::Response {
    use warp::reply::Reply;
    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    // Floor chokepoint before the upgrade. Holds the fd only briefly.
    if let Err(deny) = serve_confined(state, ctx, Path::new(&requested)) {
        return deny.into_response();
    }
    let st = state.clone();
    ws.on_upgrade(move |socket| client_ws(socket, st, requested))
        .into_response()
}

/// Per-connection WebSocket handler: resolve `requested` to its registry entry,
/// then subscribe to *that entry's* broadcast and forward each new body
/// fragment. The floor has ALREADY passed in [`ws_route`] before the upgrade.
async fn client_ws(ws: WebSocket, state: Arc<AppState>, requested: String) {
    // Look the path up in the registry (creating its entry if this is the first
    // touch). A confinement failure simply closes the socket.
    let entry = match state.entry_for(Path::new(&requested)) {
        Ok((_, e)) => e,
        Err(_) => return,
    };

    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut rx = entry.tx.subscribe();

    // Send the current body immediately so a just-loaded page is in sync even if
    // no change has happened since /view rendered it.
    let initial = entry
        .latest_body
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
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

/// The `/collab` route entry (synchronous): network guard + FLOOR gate (audit
/// HIGH-1) BEFORE upgrading, exactly like [`ws_route`]. A non-world-readable doc
/// requested unauthenticated is rejected 403 and the socket is never upgraded —
/// so the CRDT document can never be pumped to an unauthorized client.
fn collab_route(
    state: &Arc<AppState>,
    ctx: &ReqContext,
    requested: String,
    ws: warp::ws::Ws,
) -> warp::reply::Response {
    use warp::reply::Reply;
    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    if let Err(deny) = serve_confined(state, ctx, Path::new(&requested)) {
        return deny.into_response();
    }
    let st = state.clone();
    ws.on_upgrade(move |socket| collab_upgrade(socket, st, requested))
        .into_response()
}

/// Per-connection `/collab` upgrade wrapper: resolve `requested` to its shared
/// registry entry (creating it on first touch — editors and viewers of the same
/// file share ONE session), then hand the socket and the entry's collab channels
/// to [`collab::collab_ws`]. The floor has ALREADY passed in [`collab_route`].
///
/// The session is never reached from this async path: `collab_ws` only pumps
/// `Send` bytes over `collab_in`/`collab_out` to/from the path's watch thread.
async fn collab_upgrade(ws: WebSocket, state: Arc<AppState>, requested: String) {
    let (canon, entry) = match state.entry_for(Path::new(&requested)) {
        Ok(pair) => pair,
        Err(_) => return, // confinement failure → close the socket
    };
    collab::collab_ws(
        ws,
        entry.collab_in.clone(),
        entry.collab_out.clone(),
        canon,
    )
    .await;
}

/// Rebuild a confined, watching [`FilePeer`] seeded from `last_good` text, used
/// by the **#5 guard** when an integrate panic poisons the in-flight session.
///
/// `target` is an already-validated canonical in-root path, so `within` and
/// `watch` are expected to succeed; if either *does* fail (e.g. the file was
/// concurrently removed), we fall back to a non-watching peer seeded from the
/// same text so the watch thread keeps serving the last-known-good document
/// rather than crashing. Either way the returned peer's text equals `last_good`.
fn rebuild_peer(root: &Path, target: &Path, last_good: &str) -> FilePeer<YrsSession> {
    match FilePeer::within(root, target, YrsSession::from_text(last_good)) {
        Ok(mut peer) => {
            // Re-establish the directory watch; ignore a watch-setup error (the
            // peer is still usable via collab even without fs events).
            let _ = peer.watch();
            peer
        }
        // Confinement/IO failure on a previously-valid path: keep a usable,
        // non-watching peer at last-good text. `new` only needs the parent dir
        // to canonicalize (which it did at first spawn), so this is expected to
        // succeed; on the vanishingly rare failure we still keep last-good text.
        Err(_) => {
            let mut peer = FilePeer::new(target, YrsSession::from_text(last_good))
                .expect("rebuild fallback peer: parent dir was canonical at spawn");
            let _ = peer.watch();
            peer
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
///
/// ## Phase 3: the single serialization point for ALL doc mutations
/// This thread now also drains `collab_rx` (browser editors → here) alongside its
/// filesystem polling. Because the `!Send` session is mutated *only* here, in
/// arrival order, there is exactly one writer — the "two writers" problem is gone
/// and fs edits / browser edits commute by CRDT semantics (ADR-0001). For each
/// applied browser update the thread, in order: enforces the **#5 guard**,
/// re-renders + broadcasts HTML on `tx` (so one-way `/ws` viewers are *not*
/// regressed), broadcasts the update on `collab_out` (to the other editors), and
/// marks the doc dirty for the debounced write-back. A filesystem-origin edit is
/// likewise broadcast on `collab_out` (computed via `update_since` against the
/// pre-change state vector) so browser editors converge with on-disk edits.
fn spawn_watch(
    root: PathBuf,
    target: PathBuf,
    tx: broadcast::Sender<String>,
    latest_body: Arc<Mutex<String>>,
    mut collab_rx: mpsc::Receiver<CollabMsg>,
    collab_out: broadcast::Sender<Vec<u8>>,
    roots: Roots,
) -> std::io::Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();

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

            // Links resolve against the document's CANONICAL path + the active
            // root union (a snapshot taken at spawn). Escapes → the /outside
            // sentinel (see rewrite_doc_urls).
            let render = |text: &str| rewrite_doc_urls(&render_markdown(text), &roots, &target);

            // Initial render after `watch`'s catch-up sync. We cache the
            // *fragment* (inner HTML of #doc): both the cached value and every
            // broadcast feed `#doc.innerHTML` on the client, so neither is
            // wrapped in the `#doc` container.
            *latest_body.lock().unwrap_or_else(|e| e.into_inner()) = render(&peer.session().text());
            let _ = ready_tx.send(Ok(()));

            // Re-render the cache + broadcast the new body to one-way `/ws`
            // preview viewers. Used by BOTH the fs path and the collab path so a
            // browser-origin edit keeps the preview live (viewer-not-regressed).
            let publish_html = |peer: &FilePeer<YrsSession>| {
                let body = render(&peer.session().text());
                *latest_body.lock().unwrap_or_else(|e| e.into_inner()) = body.clone();
                // A send error only means there are no live subscribers; the
                // latest body is still cached for the next /ws.
                let _ = tx.send(body);
            };

            // Debounced write-back bookkeeping: when a browser edit lands we mark
            // the doc dirty; once `WRITE_BACK_DEBOUNCE` elapses with no further
            // edit we flush to disk. The file-peer's stateless content-compare
            // guard (`sync_from_disk`) swallows the fs event our own write emits,
            // so this never feeds back as an external edit (no write storm).
            let mut dirty_since: Option<std::time::Instant> = None;

            loop {
                // --- Filesystem-origin edits ----------------------------------
                // IMPORTANT ordering: while a browser edit is pending write-back
                // (`dirty_since.is_some()`), the on-disk file is known-stale —
                // the session is ahead of it. Ingesting the file then would diff
                // the stale contents against the newer session and REVERT the
                // un-written browser edit (a lost update). So we ingest fs events
                // only when the session and file are in sync (not dirty); once the
                // debounced write-back below flushes, file == session and fs
                // watching resumes normally. This keeps the single serialization
                // point coherent without racing our own pending write.
                if dirty_since.is_none() {
                    // Snapshot the pre-change SV so we can compute exactly the
                    // update an external edit introduced and forward it to editors.
                    let pre_sv = peer.session().state_vector();
                    match peer.try_drain() {
                        Ok(true) => {
                            publish_html(&peer);
                            // Forward the on-disk change to browser editors so they
                            // converge with the file (CRDT apply is idempotent).
                            let update = peer.session().update_since(&pre_sv);
                            if !update.is_empty() {
                                let _ = collab_out.send(collab::encode_sync_update(update));
                            }
                        }
                        Ok(false) => {}
                        Err(_) => { /* transient I/O; next tick re-reads from disk */ }
                    }
                }

                // --- Browser-origin collab messages ---------------------------
                // Drain everything currently queued (coalesce a burst) so the
                // session advances in arrival order at one serialization point.
                let mut applied_any = false;
                while let Ok(msg) = collab_rx.try_recv() {
                    match msg {
                        CollabMsg::SnapshotRequest(reply) => {
                            // Answer with the server's own SyncStep1 (its SV).
                            let frame =
                                collab::encode_sync_step1(peer.session().state_vector());
                            let _ = reply.send(frame);
                        }
                        CollabMsg::SyncStep1(sv, reply) => {
                            // #5 step 1 (bandwidth amplification): refuse an
                            // oversized state vector rather than echo the full
                            // doc. A tiny/empty SV is normal (a fresh client).
                            let frame = if sv.len() > MAX_UPDATE_BYTES {
                                Vec::new()
                            } else {
                                collab::encode_sync_step2(peer.session().update_since(&sv))
                            };
                            let _ = reply.send(frame);
                        }
                        CollabMsg::AwarenessFrame(frame) => {
                            // Ephemeral peer state: relay to the other editors,
                            // never touch the doc.
                            let _ = collab_out.send(frame);
                        }
                        CollabMsg::ClientUpdate(update) => {
                            // THE apply funnel (#5). This inlines the same guard
                            // as `collab::apply_client_update` (which is unit-tested
                            // in isolation in `collab.rs`); it is inlined here only
                            // because the merge and rebuild steps both need `&mut
                            // peer`, which two `FnMut` closures cannot co-borrow.
                            // Keep the two in lockstep. Snapshot the last-known-good
                            // text + pre-state-vector BEFORE applying, so a caught
                            // integrate panic can rebuild, and a success can be
                            // re-broadcast as a minimal update.
                            let last_good = peer.session().text();
                            let pre_sv = peer.session().state_vector();

                            // Step 1: pre-decode size cap (the network edge also
                            // caps, but the funnel is self-contained).
                            if update.len() > MAX_UPDATE_BYTES {
                                continue;
                            }
                            // Step 1b — THE yrs-UB GUARD (audit HIGH): walk the
                            // untrusted v1 update bytes with the bounds-checked,
                            // CHECKED-UTF-8 validator and DROP an unsafe update
                            // BEFORE any yrs decode. `Update::decode_v1` (reached
                            // by `merge`) reads embedded strings with
                            // `from_utf8_unchecked` — invalid UTF-8 there is UB /
                            // a non-unwinding abort that `catch_unwind` cannot
                            // contain. Rejecting here is the only safe defense.
                            if !crate::validate::is_update_bytes_safe(&update) {
                                continue;
                            }
                            // Steps 2+3: decode-guarded merge under catch_unwind.
                            let changed = match std::panic::catch_unwind(
                                std::panic::AssertUnwindSafe(|| peer.session_mut().merge(&update)),
                            ) {
                                Ok(changed) => changed,
                                Err(_) => {
                                    // #5 step 3: integrate panicked. Rebuild the
                                    // session from last-known-good text and carry
                                    // on; the !Send single-owner session means no
                                    // shared mutex was poisoned. Re-establish the
                                    // watch on the rebuilt peer.
                                    peer = rebuild_peer(&root, &target, &last_good);
                                    false
                                }
                            };

                            if changed {
                                applied_any = true;
                                // Keep one-way /ws preview viewers current.
                                publish_html(&peer);
                                // Fan the new ops out to the other editors as a
                                // minimal update (idempotent to echo to all).
                                let fanout = peer.session().update_since(&pre_sv);
                                if !fanout.is_empty() {
                                    let _ = collab_out.send(collab::encode_sync_update(fanout));
                                }
                            }
                        }
                    }
                }
                if applied_any {
                    dirty_since.get_or_insert_with(std::time::Instant::now);
                }

                // --- Debounced write-back -------------------------------------
                if let Some(since) = dirty_since
                    && since.elapsed() >= WRITE_BACK_DEBOUNCE
                {
                    // Best-effort write; the content-compare guard means our own
                    // resulting fs event is a no-op (no feedback loop).
                    let _ = peer.write_to_disk();
                    dirty_since = None;
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

/// Build and run the daemon on `addr`. The first `file` is not special — it is
/// just the first path the thin client points the browser at; its owning root is
/// registered in the multi-root [`Roots`] registry and any other file under an
/// active root is confinable too (a document is identified by its canonical
/// absolute path, the `path=` query). Entries are created lazily on first touch.
///
/// Returns once the server stops. The caller owns the tokio runtime.
pub async fn serve(file: &Path, addr: SocketAddr) -> std::io::Result<()> {
    let abs = file.canonicalize()?;

    // Build the multi-root registry rooted at the user's $HOME (the recursion
    // boundary + denylist anchor) and register the opened file's project root.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut roots = Roots::new(home);
    // Best-effort: load any persisted roots first, then register this file.
    if let Ok(loaded) = roots.load() {
        roots = loaded;
    }
    let _ = roots.register_for(&abs);
    let _ = roots.save();

    let state = AppState::new(roots);
    // Use try_bind_ephemeral so a port-in-use (or any other bind) error is
    // returned as an io::Result rather than causing a panic.  The bound address
    // is fixed (addr was already chosen by the caller), so we ignore the
    // returned SocketAddr.
    let (_bound_addr, fut) = warp::serve(routes(state))
        .try_bind_ephemeral(addr)
        .map_err(|e| {
            // Walk the error source chain to recover the io::Error kind
            // (e.g. AddrInUse) so callers can match on ErrorKind.
            // The chain is: warp::Error -> hyper::Error -> std::io::Error.
            let kind = {
                let mut src: Option<&dyn std::error::Error> =
                    std::error::Error::source(&e);
                let mut found = std::io::ErrorKind::Other;
                while let Some(s) = src {
                    if let Some(io) = s.downcast_ref::<std::io::Error>() {
                        found = io.kind();
                        break;
                    }
                    src = s.source();
                }
                found
            };
            std::io::Error::new(kind, e)
        })?;
    fut.await;
    Ok(())
}

/// Build the [`AppState`] from the first opened `file`, binding the HTTP server
/// with an ephemeral-fallback port strategy (try 7878, fall back to OS-assigned)
/// and running the control-plane accept loop alongside it.
///
/// The single-instance election is done by the caller via
/// [`crate::control::bind_or_detect`]; if the election returned `Daemon`, pass
/// the resulting [`std::os::unix::net::UnixListener`] here as `ctrl`. Returns
/// the bound HTTP address so the caller can embed it in the `Opened` response.
///
/// The control-plane loop lives on a dedicated blocking thread (since
/// [`crate::control::serve_connection`] is synchronous), so the tokio runtime
/// is free for the async HTTP stack.
pub async fn serve_with_control(
    file: &Path,
    ctrl: std::os::unix::net::UnixListener,
) -> std::io::Result<()> {
    let abs = file.canonicalize()?;

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut roots = Roots::new(home);
    if let Ok(loaded) = roots.load() {
        roots = loaded;
    }
    let _ = roots.register_for(&abs);
    let _ = roots.save();

    let state = AppState::new(roots);

    // Try the preferred port first; fall back to OS-assigned on AddrInUse.
    let preferred: SocketAddr = ([127, 0, 0, 1], 7878).into();
    let ephemeral: SocketAddr = ([127, 0, 0, 1], 0).into();

    let (bound_addr, fut) = warp::serve(routes(state.clone()))
        .try_bind_ephemeral(preferred)
        .or_else(|_| warp::serve(routes(state.clone())).try_bind_ephemeral(ephemeral))
        .map_err(|e| {
            let kind = {
                let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
                let mut found = std::io::ErrorKind::Other;
                while let Some(s) = src {
                    if let Some(io) = s.downcast_ref::<std::io::Error>() {
                        found = io.kind();
                        break;
                    }
                    src = s.source();
                }
                found
            };
            std::io::Error::new(kind, e)
        })?;

    let state_ctrl = state.clone();
    std::thread::Builder::new()
        .name("md-preview-control".into())
        .spawn(move || {
            for conn in ctrl.incoming() {
                match conn {
                    Ok(stream) => {
                        let state_conn = state_ctrl.clone();
                        let http_addr = bound_addr;
                        std::thread::Builder::new()
                            .name("md-preview-control-conn".into())
                            .spawn(move || {
                                let _ = crate::control::serve_connection(stream, |req| {
                                    handle_control_request(&state_conn, req, http_addr)
                                });
                            })
                            .ok();
                    }
                    Err(_) => break,
                }
            }
        })
        .map_err(std::io::Error::other)?;

    fut.await;
    Ok(())
}

/// Handle one control-plane request on the daemon side.
///
/// `Open{path,root}` → register the root, mint a bootstrap nonce, return
/// `Opened{url,nonce}`. `Ping` → `Pong`. Errors are returned as
/// `Response::Error` so the connection is never silently dropped.
fn handle_control_request(
    state: &AppState,
    req: crate::control::Request,
    http_addr: SocketAddr,
) -> crate::control::Response {
    use crate::control::{Request, Response};
    match req {
        Request::Ping => Response::Pong,
        Request::Open { path, root } => {
            // Register the root in the multi-root registry.
            let root_path = std::path::Path::new(&root);
            let file_path = std::path::Path::new(&path);
            {
                let now = std::time::SystemTime::now();
                let mut roots = state.roots.lock().unwrap_or_else(|e| e.into_inner());
                let _ = roots.register_root(root_path, now);
                let _ = roots.save();
            }
            // Mint a bootstrap nonce. The Mutex is held synchronously and
            // dropped before returning (never across an await).
            let nonce = {
                let mut nonces = state.auth.nonces.lock().unwrap_or_else(|e| e.into_inner());
                match nonces.mint() {
                    Ok(n) => n,
                    Err(e) => {
                        return Response::Error {
                            message: format!("failed to mint nonce: {e}"),
                        }
                    }
                }
            };
            // Build the view URL: the absolute path, encoded, on the daemon's
            // HTTP address.
            let abs_path = if file_path.is_absolute() {
                file_path.to_path_buf()
            } else {
                match file_path.canonicalize() {
                    Ok(p) => p,
                    Err(e) => {
                        return Response::Error {
                            message: format!("cannot resolve path: {e}"),
                        }
                    }
                }
            };
            let encoded = encode_query_value(&abs_path.to_string_lossy());
            let url = format!("http://{http_addr}/view?path={encoded}");
            Response::Opened { url, nonce }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("md-preview-srv-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        // Canonicalize so the registry key matches what the confine funnel
        // produces (temp dirs can be symlinked, e.g. /tmp -> /private/tmp).
        p.canonicalize().unwrap()
    }

    /// Build an [`AppState`] whose root union is the given directory (a recursive
    /// project root), with a fresh `$HOME` that is NOT an ancestor of `dir` so the
    /// denylist/home-boundary logic does not interfere with the temp tree.
    fn state_for(dir: &Path) -> AppState {
        let home = temp_dir("home");
        let mut roots = Roots::new(home);
        roots
            .register_root(dir, SystemTime::now())
            .expect("register temp dir as root");
        AppState::new(roots)
    }

    /// A localhost request context with the given auth bit. Host/origin set to a
    /// loopback same-origin request so the network guard passes.
    fn ctx(authenticated: bool) -> ReqContext {
        ReqContext {
            host: Some("127.0.0.1:7878".to_string()),
            origin: None,
            sec_fetch_site: Some("same-origin".to_string()),
            authenticated,
        }
    }

    /// Write `contents` to `dir/name` with mode `mode` (octal), returning the
    /// canonical absolute path string for use as a `path=` value.
    fn write_mode(dir: &Path, name: &str, contents: &str, mode: u32) -> String {
        let p = dir.join(name);
        std::fs::write(&p, contents).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p.canonicalize().unwrap().to_string_lossy().into_owned()
    }

    // --- pure helpers (unchanged behaviour) --------------------------------

    #[test]
    fn json_string_escapes_script_breakers() {
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
    fn content_type_for_infers_by_extension() {
        assert_eq!(content_type_for(Path::new("pic.png")), "image/png");
        assert_eq!(content_type_for(Path::new("a.JPG")), "image/jpeg");
        assert_eq!(content_type_for(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(content_type_for(Path::new("style.css")), "text/css; charset=utf-8");
        assert_eq!(content_type_for(Path::new("blob.xyz")), "application/octet-stream");
        assert_eq!(content_type_for(Path::new("noext")), "application/octet-stream");
    }

    #[test]
    fn has_uri_scheme_detects_schemes_not_relative_paths() {
        assert!(has_uri_scheme("https://x/y"));
        assert!(has_uri_scheme("data:image/png;base64,AAAA"));
        assert!(!has_uri_scheme("pic.png"));
        assert!(!has_uri_scheme("../up/pic.png"));
        assert!(!has_uri_scheme(""));
    }

    #[test]
    fn normalize_view_accepts_known_modes_only() {
        assert_eq!(normalize_view(Some("preview")), Some("preview"));
        assert_eq!(normalize_view(Some("bogus")), None);
        assert_eq!(normalize_view(None), None);
    }

    // --- multi-root AppState + confine routing (W3.1) ----------------------

    #[test]
    fn entry_for_confines_and_reuses() {
        let dir = temp_dir("entry");
        write_mode(&dir, "doc.md", "# Hi", 0o644);
        let state = state_for(&dir);
        let abs = dir.join("doc.md");

        let (canon1, e1) = state.entry_for(&abs).unwrap();
        let (canon2, e2) = state.entry_for(&abs).unwrap();
        assert_eq!(canon1, canon2);
        assert!(Arc::ptr_eq(&e1, &e2), "second lookup must reuse the entry");
        assert_eq!(state.registry.lock().unwrap().len(), 1);

        // Traversal escape (out of every root) is rejected.
        let escape = dir.join("..").join("escape.md");
        assert!(state.entry_for(&escape).is_err());
        // A relative path is rejected by the funnel (must be absolute).
        assert!(state.entry_for(Path::new("doc.md")).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_doc_urls_rewrites_relative_md_and_assets_to_abs_paths() {
        let dir = temp_dir("rw");
        write_mode(&dir, "a.md", "a", 0o644);
        let b = write_mode(&dir, "b.md", "b", 0o644);
        let img = write_mode(&dir, "img.png", "PNG", 0o644);
        let state = state_for(&dir);
        let roots = state.roots_snapshot();
        let doc = dir.join("a.md");

        let body = r##"<a href="b.md">B</a> <a href="https://x/y.md">ext</a> <a href="#frag">f</a> <img src="img.png">"##;
        let out = rewrite_doc_urls(body, &roots, &doc);
        assert!(
            out.contains(&format!("href=\"/view?path={}\"", encode_query_value(&b))),
            "local .md href -> /view with abs path: {out}"
        );
        assert!(out.contains(r#"href="https://x/y.md""#), "external left alone");
        assert!(out.contains(r##"href="#frag""##), "fragment left alone");
        assert!(
            out.contains(&format!("src=\"/asset?path={}\"", encode_query_value(&img))),
            "relative img -> /asset with abs path: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_doc_urls_escape_becomes_outside_sentinel() {
        let dir = temp_dir("rwesc");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        write_mode(&dir, "sub/child.md", "c", 0o644);
        let top = write_mode(&dir, "top.md", "t", 0o644);
        let state = state_for(&dir);
        let roots = state.roots_snapshot();
        let doc = dir.join("sub/child.md");

        // ../top.md is in-root -> rewritten; ../../etc/passwd.md escapes -> /outside.
        let body = r#"<a href="../top.md">up</a> <a href="../../../../../etc/passwd.md">escape</a>"#;
        let out = rewrite_doc_urls(body, &roots, &doc);
        assert!(
            out.contains(&format!("href=\"/view?path={}\"", encode_query_value(&top))),
            "in-root parent link rewritten: {out}"
        );
        assert!(
            out.contains(&format!("href=\"{OUTSIDE_SENTINEL}\"")),
            "escaping link rewritten to /outside sentinel: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- read routes through the chokepoint -------------------------------

    #[tokio::test]
    async fn serve_asset_serves_world_readable_file() {
        let dir = temp_dir("asset-ok");
        let bytes: &[u8] = b"\x89PNG\r\n\x1a\nfake";
        let p = dir.join("pic.png");
        std::fs::write(&p, bytes).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let abs = p.canonicalize().unwrap().to_string_lossy().into_owned();
        let state = state_for(&dir);

        let resp = serve_asset(&state, &ctx(false), &abs);
        assert_eq!(resp.status(), warp::http::StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
        let body = warp::hyper::body::to_bytes(resp.into_body()).await.unwrap();
        assert_eq!(&body[..], bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serve_asset_rejects_out_of_root_and_missing() {
        let dir = temp_dir("asset-bad");
        let outside = temp_dir("asset-outside");
        // An EXISTING file outside every active root -> escape -> 400 (no leak).
        let secret = outside.join("secret");
        std::fs::write(&secret, "top secret").unwrap();
        let state = state_for(&dir);
        assert_eq!(
            serve_asset(&state, &ctx(true), &secret.to_string_lossy()).status(),
            warp::http::StatusCode::BAD_REQUEST,
            "an out-of-root existing file is an escape (400)"
        );
        // A relative path is non-absolute -> 400.
        assert_eq!(
            serve_asset(&state, &ctx(true), "relative.png").status(),
            warp::http::StatusCode::BAD_REQUEST
        );
        // In-root but nonexistent -> 404.
        let missing = dir.join("nope.png");
        assert_eq!(
            serve_asset(&state, &ctx(true), &missing.to_string_lossy()).status(),
            warp::http::StatusCode::NOT_FOUND
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn serve_raw_returns_source_text() {
        let dir = temp_dir("raw-ok");
        let abs = write_mode(&dir, "doc.md", "# Hello\n\nsrc", 0o644);
        let state = state_for(&dir);
        let resp = serve_raw(&state, &ctx(false), &abs);
        assert_eq!(resp.status(), warp::http::StatusCode::OK);
        let body = warp::hyper::body::to_bytes(resp.into_body()).await.unwrap();
        assert_eq!(&body[..], b"# Hello\n\nsrc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- the fail-closed permission floor (A0 / HIGH-1, W3.2) --------------

    #[test]
    fn floor_denies_non_world_readable_to_unauthenticated_on_every_read_route() {
        let dir = temp_dir("floor-deny");
        let priv_abs = write_mode(&dir, "secret.md", "# top secret", 0o600);
        let state = state_for(&dir);
        let unauth = ctx(false);

        // HTTP read routes: /asset, /raw, /view, /edit all 403.
        assert_eq!(
            serve_asset(&state, &unauth, &priv_abs).status(),
            warp::http::StatusCode::FORBIDDEN,
            "/asset must deny a private file unauthenticated"
        );
        assert_eq!(
            serve_raw(&state, &unauth, &priv_abs).status(),
            warp::http::StatusCode::FORBIDDEN,
            "/raw must deny a private file unauthenticated"
        );
        assert_eq!(
            view_page(&state, &unauth, &priv_abs, None).status(),
            warp::http::StatusCode::FORBIDDEN,
            "/view must deny a private file unauthenticated"
        );
        assert_eq!(
            edit_page(&state, &unauth, &priv_abs).status(),
            warp::http::StatusCode::FORBIDDEN,
            "/edit must deny a private file unauthenticated (audit MED-3)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn floor_denies_non_world_readable_on_ws_and_collab() {
        let dir = temp_dir("floor-ws");
        let priv_abs = write_mode(&dir, "secret.md", "# secret", 0o600);
        let state = Arc::new(state_for(&dir));
        let unauth = ctx(false);

        // /ws and /collab (audit HIGH-1): the floor is checked BEFORE upgrade, so
        // these return a 403 response and the socket is never upgraded. We invoke
        // the route gate with a real Ws is hard to fabricate; instead assert via
        // serve_confined (the exact chokepoint the route calls before upgrade).
        assert!(
            matches!(
                serve_confined(&state, &unauth, Path::new(&priv_abs)),
                Err(FloorDeny::Floor)
            ),
            "/ws & /collab floor chokepoint must deny a private file unauthenticated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn floor_allows_authenticated_own_doc_and_world_readable() {
        let dir = temp_dir("floor-allow");
        let priv_abs = write_mode(&dir, "secret.md", "x", 0o600);
        let pub_abs = write_mode(&dir, "public.md", "y", 0o644);
        let state = state_for(&dir);

        // Authenticated unlocks the private (own) doc.
        assert!(
            serve_confined(&state, &ctx(true), Path::new(&priv_abs)).is_ok(),
            "authenticated must unlock a non-world-readable own doc"
        );
        // World-readable served even unauthenticated.
        assert!(
            serve_confined(&state, &ctx(false), Path::new(&pub_abs)).is_ok(),
            "world-readable file served unauthenticated"
        );
        // World-readable also served authenticated.
        assert!(serve_confined(&state, &ctx(true), Path::new(&pub_abs)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- network guards (host / origin) ------------------------------------

    #[test]
    fn network_guard_rejects_bad_host_and_cross_origin() {
        let mut bad_host = ctx(true);
        bad_host.host = Some("evil.com".to_string());
        assert_eq!(
            network_guard(&bad_host).expect("bad host rejected").status(),
            warp::http::StatusCode::FORBIDDEN
        );
        let mut missing_host = ctx(true);
        missing_host.host = None;
        assert!(network_guard(&missing_host).is_some(), "missing host rejected");

        let mut cross = ctx(true);
        cross.origin = Some("http://evil.com".to_string());
        cross.sec_fetch_site = Some("cross-site".to_string());
        assert_eq!(
            network_guard(&cross).expect("cross-origin rejected").status(),
            warp::http::StatusCode::FORBIDDEN
        );
        // A good loopback same-origin request passes.
        assert!(network_guard(&ctx(true)).is_none());
    }

    #[test]
    fn read_routes_reject_cross_origin_before_floor() {
        let dir = temp_dir("xorigin");
        let pub_abs = write_mode(&dir, "p.md", "x", 0o644);
        let state = state_for(&dir);
        let mut cross = ctx(false);
        cross.origin = Some("http://evil.example".to_string());
        // Even a world-readable file is refused on a cross-origin request.
        assert_eq!(
            serve_asset(&state, &cross, &pub_abs).status(),
            warp::http::StatusCode::FORBIDDEN
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- /claim: nonce -> session -> Set-Cookie ----------------------------

    #[test]
    fn claim_consumes_nonce_and_issues_session_cookie() {
        let dir = temp_dir("claim");
        let state = state_for(&dir);
        // Arm a nonce (the control-plane side).
        let nonce = mint_claim_nonce(&state).expect("mint nonce");

        // A wrong nonce is rejected.
        let bad = claim(&state, Some("127.0.0.1:7878"), b"not-the-nonce");
        assert_eq!(bad.status(), warp::http::StatusCode::FORBIDDEN);

        // The right nonce yields a 204 + Set-Cookie carrying a session.
        let ok = claim(&state, Some("127.0.0.1:7878"), nonce.as_bytes());
        assert_eq!(ok.status(), warp::http::StatusCode::NO_CONTENT);
        let set_cookie = ok
            .headers()
            .get("set-cookie")
            .expect("Set-Cookie present")
            .to_str()
            .unwrap()
            .to_string();
        assert!(set_cookie.contains("mdp_session="));
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
        assert!(set_cookie.contains("Path=/"));
        assert!(!set_cookie.contains("Secure"), "loopback cookie not Secure");

        // The issued token validates as a session (and unlocks the floor).
        let token = auth::parse_session_cookie(
            set_cookie.split(';').next().unwrap(),
        )
        .expect("token parses");
        assert!(state
            .auth
            .sessions
            .lock()
            .unwrap()
            .validate_and_renew(&token));

        // Single-use: replaying the burned nonce fails.
        let replay = claim(&state, Some("127.0.0.1:7878"), nonce.as_bytes());
        assert_eq!(replay.status(), warp::http::StatusCode::FORBIDDEN);

        // /claim is host-guarded even though origin-exempt.
        let bad_host = claim(&state, Some("evil.com"), nonce.as_bytes());
        assert_eq!(bad_host.status(), warp::http::StatusCode::FORBIDDEN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claim_then_read_unlocks_private_doc_end_to_end() {
        let dir = temp_dir("claim-e2e");
        let priv_abs = write_mode(&dir, "secret.md", "# private", 0o600);
        let state = state_for(&dir);

        // Before claim: unauthenticated read is denied.
        assert_eq!(
            serve_raw(&state, &ctx(false), &priv_abs).status(),
            warp::http::StatusCode::FORBIDDEN
        );

        // Claim a session.
        let nonce = mint_claim_nonce(&state).unwrap();
        let resp = claim(&state, Some("127.0.0.1:7878"), nonce.as_bytes());
        let sc = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
        let token =
            auth::parse_session_cookie(sc.split(';').next().unwrap()).unwrap();

        // A request carrying the cookie is authenticated → read allowed.
        let mut authed = ctx(false);
        authed.authenticated = state
            .auth
            .sessions
            .lock()
            .unwrap()
            .validate_and_renew(&token);
        assert!(authed.authenticated);
        assert_eq!(
            serve_raw(&state, &authed, &priv_abs).status(),
            warp::http::StatusCode::OK
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- save: floor + confine (write) -------------------------------------

    #[test]
    fn save_doc_writes_world_readable_and_rejects_traversal() {
        let dir = temp_dir("save-ok");
        let abs = write_mode(&dir, "doc.md", "old", 0o644);
        let state = state_for(&dir);

        let resp = save_doc(&state, &ctx(false), &abs, b"# New");
        assert_eq!(resp.status(), warp::http::StatusCode::NO_CONTENT);
        assert_eq!(std::fs::read_to_string(dir.join("doc.md")).unwrap(), "# New");

        // Traversal escape -> 400, nothing written outside root.
        let escape = dir.join("..").join("escape.md");
        assert_eq!(
            save_doc(&state, &ctx(true), &escape.to_string_lossy(), b"pwned").status(),
            warp::http::StatusCode::BAD_REQUEST
        );
        assert!(!dir.parent().unwrap().join("escape.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_doc_floor_denies_overwriting_private_file_unauthenticated() {
        let dir = temp_dir("save-floor");
        let priv_abs = write_mode(&dir, "secret.md", "keep", 0o600);
        let state = state_for(&dir);
        // Unauthenticated cannot overwrite a non-world-readable existing file.
        assert_eq!(
            save_doc(&state, &ctx(false), &priv_abs, b"pwned").status(),
            warp::http::StatusCode::FORBIDDEN
        );
        assert_eq!(std::fs::read_to_string(dir.join("secret.md")).unwrap(), "keep");
        // Authenticated may overwrite its own private file.
        assert_eq!(
            save_doc(&state, &ctx(true), &priv_abs, b"mine").status(),
            warp::http::StatusCode::NO_CONTENT
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_doc_creates_new_in_root_file() {
        let dir = temp_dir("save-new");
        let state = state_for(&dir);
        let fresh = dir.join("fresh.md").to_string_lossy().into_owned();
        let resp = save_doc(&state, &ctx(false), &fresh, b"hi");
        assert_eq!(resp.status(), warp::http::StatusCode::NO_CONTENT);
        assert_eq!(std::fs::read_to_string(dir.join("fresh.md")).unwrap(), "hi");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_doc_refuses_invalid_utf8() {
        let dir = temp_dir("save-utf8");
        let abs = write_mode(&dir, "doc.md", "keep", 0o644);
        let state = state_for(&dir);
        assert_eq!(
            save_doc(&state, &ctx(true), &abs, &[0xff, 0xfe, 0x00]).status(),
            warp::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(std::fs::read_to_string(dir.join("doc.md")).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- view page renders the toolbar/editor (adapted) --------------------

    #[tokio::test]
    async fn view_page_contains_three_modes_and_abs_path() {
        let dir = temp_dir("view-three");
        let abs = write_mode(&dir, "doc.md", "# Hi", 0o644);
        let state = state_for(&dir);

        let resp = view_page(&state, &ctx(false), &abs, None);
        assert_eq!(resp.status(), warp::http::StatusCode::OK);
        let html = String::from_utf8(
            warp::hyper::body::to_bytes(resp.into_body()).await.unwrap().to_vec(),
        )
        .unwrap();
        for mode in VIEW_MODES {
            assert!(html.contains(&format!("data-view=\"{mode}\"")));
        }
        assert!(html.contains("mdv-toolbar"));
        assert!(html.contains("/raw?path="));
        assert!(html.contains("/save?path="));
        assert!(html.contains("id=\"doc\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- broadcast through the registry (adapted) --------------------------

    #[tokio::test]
    async fn editing_file_broadcasts_rerendered_body_via_registry() {
        let dir = temp_dir("bcast");
        let file = dir.join("doc.md");
        std::fs::write(&file, "# One").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let state = state_for(&dir);

        let (_canon, entry) = state.entry_for(&file).unwrap();
        let mut rx = entry.tx.subscribe();
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&file, "# Two").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("a broadcast within 3s")
            .expect("channel open");
        assert!(got.contains("Two"), "broadcast reflects the edit: {got}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cross-file links resolve through the registry to absolute `path=` values.
    #[tokio::test]
    async fn cross_file_link_resolves_through_registry() {
        let dir = temp_dir("xfile");
        let a = dir.join("a.md");
        std::fs::write(&a, "# A\n\n[go to B](b.md)\n").unwrap();
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o644)).unwrap();
        let b = write_mode(&dir, "b.md", "# B\n", 0o644);
        let state = state_for(&dir);

        let (_ca, ea) = state.entry_for(&a).unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let a_body = ea.latest_body.lock().unwrap().clone();
        assert!(
            a_body.contains(&format!("href=\"/view?path={}\"", encode_query_value(&b))),
            "a.md links to b.md by abs path: {a_body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- yrs UB guard wiring (W3.3) ----------------------------------------

    #[test]
    fn unsafe_yrs_update_is_rejected_by_validator() {
        // A real, valid update is accepted; a bit-flipped / garbage update is
        // rejected by the pre-decode validator WITHOUT a panic or abort. This is
        // the guard the collab receive path calls before any yrs decode.
        use crate::doc::{DocSession as _, TextEdit};
        let mut s = YrsSession::from_text("");
        s.apply(&[TextEdit::insert(0, "hello collab")]);
        let valid = s.update_since(&YrsSession::from_text("").state_vector());
        assert!(crate::validate::is_update_bytes_safe(&valid), "valid update accepted");

        // Pure garbage is rejected.
        assert!(!crate::validate::is_update_bytes_safe(&[0xff, 0x00, 0x13, 0x37]));
        // Truncations never panic and are rejected (no abort).
        for cut in 1..valid.len() {
            let _ = crate::validate::is_update_bytes_safe(&valid[..cut]);
        }
    }

    // --- bind error handling (unchanged) -----------------------------------

    #[tokio::test]
    async fn double_bind_returns_addr_in_use_not_panic() {
        let dir = temp_dir("dbl");
        write_mode(&dir, "x.md", "# X", 0o644);

        let state1 = state_for(&dir);
        let loopback: std::net::SocketAddr = ([127, 0, 0, 1], 0).into();
        let (bound_addr, _fut1) = warp::serve(routes(state1))
            .try_bind_ephemeral(loopback)
            .expect("first bind must succeed");

        let state2 = state_for(&dir);
        let result = warp::serve(routes(state2))
            .try_bind_ephemeral(bound_addr)
            .map_err(|e| {
                let kind = {
                    let mut src: Option<&dyn std::error::Error> =
                        std::error::Error::source(&e);
                    let mut found = std::io::ErrorKind::Other;
                    while let Some(s) = src {
                        if let Some(io) = s.downcast_ref::<std::io::Error>() {
                            found = io.kind();
                            break;
                        }
                        src = s.source();
                    }
                    found
                };
                std::io::Error::new(kind, e)
            });
        let err = result.err().expect("second bind must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
