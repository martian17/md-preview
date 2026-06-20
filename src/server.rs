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
//! ## Cross-file `.md` links and local assets (render-isolation; design §3)
//! The rendered HTML emits ordinary relative URLs — both `<a href="other.md">`
//! links and `<img src="pic.png">` (and other local) asset references. The
//! content path rewrites them in [`rewrite_doc_caps`], the capability-aware
//! rewrite applied AFTER the floor passes (the mint-time authorization context):
//!   * relative `.md` links become `/view?path=<abs>` — the trusted shell SPA,
//!     which (with the parent navigation gate) owns navigation, and
//!   * relative media (`src="…"`, non-`.md` assets) become a per-doc, short-TTL
//!     **capability URL** `http://<static>/cap/<token>` on the secondary static
//!     origin (minted via [`asset_origin::cap_url`]), so the sandboxed
//!     null-origin renderer can display but never read/exfiltrate the bytes.
//!
//! A URL that escapes every active root becomes the `/outside` 403 sentinel.
//! Each candidate is resolved relative to the document's directory and confined
//! via [`confine::confine_link`]; absolute/external/`data:`/fragment URLs are
//! left as-is. Both the same-origin `/content` API and the `/ws` watch-loop
//! broadcast (relayed into the iframe) use this one rewrite; `render_markdown`
//! stays unchanged. The old shell-origin `/asset?path=` rewrite is retired
//! (the renderer CSP would block a shell-origin asset URL anyway).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc};
use warp::ws::{Message, WebSocket};
use warp::Filter;

use crate::asset_origin;
use crate::auth::{self, Clock, NonceStore, SessionStore};
use crate::bundle;
use crate::confine::{self, ConfineError, ConfinedFile};
use crate::{Confine, ConfineSnapshot};
use crate::doc::DocSession;
use crate::file_peer::{FilePeer, DEFAULT_MAX_FILE_SIZE};
use crate::render_markdown;
use crate::roots::Roots;
use crate::session::YrsSession;

// The Phase-3 collaborative-editing WebSocket pump lives in its own module,
// declared FROM here (not `lib.rs`): `server.rs` owns the registry/`Entry`/watch
// thread it plugs into, so the warp/tokio wiring stays local to the daemon. The
// module is daemon-only (this whole file is `pub mod server` behind `daemon`). It
// re-exports the transport-agnostic codec from `doc_core::collab` (ADR-0008: the
// pure half moved to the kernel; only this socket pump stays in the daemon).
#[path = "collab_pump.rs"]
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
    /// Short-TTL, per-doc capability tokens for the **secondary static origin**'s
    /// asset server (design §3 / ADR-0007). The trusted shell mints a token here
    /// for each confined asset path; the static origin's `/cap/<token>` route
    /// resolves + re-confines it. Behind a `Mutex` like the nonce/session stores
    /// (mutates on `mint`/`resolve`, swept lazily); never held across an `.await`.
    /// The `<img>`/`<video>` rewrite that calls [`asset_origin::cap_url`] is wired
    /// in W6; this step stands up the store + route.
    pub caps: Arc<Mutex<asset_origin::CapStore>>,
    /// The secondary static origin's base URL (`http://127.0.0.1:<static_port>`),
    /// learned once that port is bound (see [`serve_with_control`]). `None`
    /// before the static origin is up (and in the test constructor). Stored for
    /// the future shell / content API / asset-URL rewrite (W6).
    pub static_base: Arc<Mutex<Option<String>>>,
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
            // The cap store shares the daemon's injected clock (one fresh
            // `Clock` from the factory), so its TTL tracks the same time source
            // as the auth stores and is deterministic under the test factory.
            caps: Arc::new(Mutex::new(asset_origin::CapStore::with_clock(clock_factory()))),
            static_base: Arc::new(Mutex::new(None)),
            registry: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Snapshot the current root union (cloned) for link rewriting (which needs
    /// `&[&Root]` + a `Roots` for the denylist). Mutex dropped before return.
    fn roots_snapshot(&self) -> Roots {
        self.roots.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Build an [`asset_origin::Confiner`] sharing this state's authoritative
    /// roots registry (plus the primary-root fallback), so the static origin's
    /// `/cap/<token>` route re-confines through the **same** funnel direct
    /// `?path=` requests use — a capability is never a weaker path than a typed
    /// one. See [`AppState::confine_read`].
    fn confiner(&self) -> asset_origin::Confiner {
        // `root` is the single-file/primary fallback; for a multi-root daemon this
        // may be None (the Confiner falls back to the registry union).
        let primary_root = None;
        asset_origin::Confiner::new(self.roots.clone(), primary_root)
    }

    /// Record the secondary static origin's base URL once that port is bound, for
    /// the future shell / asset-URL rewrite (W6). Best-effort: a poisoned lock
    /// is ignored (the daemon keeps serving; only minting full URLs is affected).
    fn set_static_base(&self, base: String) {
        if let Ok(mut slot) = self.static_base.lock() {
            *slot = Some(base);
        }
    }

    /// Snapshot the secondary static origin's base URL, if the static origin has
    /// been bound yet. The render-isolation shell ([`view_page`]) and the
    /// per-document srcdoc ([`srcdoc_page`]) need it to point the iframe at the
    /// cross-origin bundle + capability-asset origin; the asset rewrite
    /// ([`rewrite_doc_caps`]) needs it to mint `/cap/<token>` URLs. A poisoned
    /// lock or an unbound static origin yields `None` — callers fail closed (the
    /// shell still renders but cross-origin assets/bundle are unavailable until
    /// the static origin is up). Mutex dropped before return; never held across
    /// an `.await`.
    fn static_base_snapshot(&self) -> Option<String> {
        self.static_base
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
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
            WatchRenderDeps {
                roots: self.roots_snapshot(),
                caps: self.caps.clone(),
                static_base: self.static_base_snapshot(),
            },
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

/// The daemon's confinement site, riding the single [`Confine`] fan-out
/// (ADR-0008 Phase 2). `AppState::confine_read`/`confine_save` are now the
/// trait's provided methods over this snapshot; the *decision* is unchanged from
/// the prior inherent methods — same already-registered roots, same TTL renew,
/// same denylist, same primitives.
///
/// This is the SOLE entry to the confinement funnel for direct reads/saves in
/// the daemon. The auth permission floor is applied by [`serve_confined`] on the
/// returned metadata, NOT here.
impl Confine for AppState {
    /// Snapshot the ALREADY-registered roots for one access, renewing the owning
    /// root's sliding TTL first. We do NOT auto-register a requested path as its
    /// own root — that would defeat confinement (any absolute path would become
    /// serveable). Roots are registered at startup / via the control plane
    /// (`md <file>`); here we only RENEW. The lock is taken synchronously and
    /// dropped before the funnel runs — never held across an `.await`.
    fn confinement_snapshot(&self, requested: &Path) -> ConfineSnapshot {
        let now = SystemTime::now();
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        // Renew the TTL of whichever existing root owns this path (if any).
        let _ = roots.owning_root(requested, now);
        let union = roots.union().into_iter().cloned().collect();
        // Clone the registry for the denylist consult; the lock drops here.
        let registry = roots.clone();
        ConfineSnapshot { union, registry }
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
    /// The `Sec-Fetch-Mode` header value. With [`Self::sec_fetch_dest`] this lets
    /// the guard exempt top-level document navigations (the bootstrap PRG landing
    /// on `GET /view` arrives `mode=navigate, dest=document`, cross-site).
    sec_fetch_mode: Option<String>,
    /// The `Sec-Fetch-Dest` header value (top-level-navigation carve-out input).
    sec_fetch_dest: Option<String>,
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
        .and(warp::header::optional::<String>("sec-fetch-mode"))
        .and(warp::header::optional::<String>("sec-fetch-dest"))
        .and(warp::header::optional::<String>("cookie"))
        .map(
            move |host: Option<String>,
                  origin: Option<String>,
                  sec_fetch_site: Option<String>,
                  sec_fetch_mode: Option<String>,
                  sec_fetch_dest: Option<String>,
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
                    sec_fetch_mode,
                    sec_fetch_dest,
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
    // origin_guard: cross-origin / cross-site is rejected, EXCEPT a top-level
    // document navigation (the bootstrap PRG landing on `GET /view`).
    if auth::origin_allowed(
        ctx.origin.as_deref(),
        ctx.sec_fetch_site.as_deref(),
        ctx.sec_fetch_mode.as_deref(),
        ctx.sec_fetch_dest.as_deref(),
    ) == auth::OriginCheck::Deny
    {
        return Some(
            warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response(),
        );
    }
    None
}

/// Build the warp route filter for the given [`AppState`]. Split out from
/// [`serve`] so it is unit/integration-testable without binding a socket.
/// Accepts either an owned `AppState` (tests) or a shared `Arc<AppState>` (the
/// production call sites, which also hand the SAME Arc to the secondary static
/// origin so both origins share one capability store).
fn routes(
    state: impl Into<Arc<AppState>>,
    edit_mode: bool,
) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let state = state.into();

    // GET /healthz -> "ok"  (liveness; lets the thin client detect the daemon).
    // Unguarded by design: it serves no file bytes and leaks no path info.
    let health = warp::path("healthz")
        .and(warp::path::end())
        .and(warp::get())
        .map(|| warp::reply::with_header("ok", "content-type", "text/plain; charset=utf-8"));

    // GET /outside -> the escape sentinel. When a navigation target escapes every
    // active root (or /navigate refuses it), the shell sends the top frame here
    // instead of leaking a path. A plain 403 page with no path echo (no existence
    // leak); design §1 "the /outside path". Previously there was no route, so an
    // escaped cross-link 404'd; this gives the human a clear, contained sentinel.
    let outside = warp::path("outside")
        .and(warp::path::end())
        .and(warp::get())
        .map(|| {
            use warp::reply::Reply;
            warp::reply::with_status(
                warp::reply::html(
                    "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
                     <title>Outside the preview roots</title></head><body>\
                     <h1>Outside the preview roots</h1><p>That link points outside the \
                     folders md-preview is allowed to read, so it was not opened.</p>\
                     </body></html>",
                ),
                warp::http::StatusCode::FORBIDDEN,
            )
            .into_response()
        });

    // POST /claim (urlencoded form: nonce + next) -> consume the nonce, issue a
    // session, Set-Cookie, and 302 → `next` (PRG). A FORM POST is a navigation,
    // not a fetch, so the `file://` bootstrap page (Origin: null) reaches it
    // without CORS. Origin-EXEMPT per design §2 (nonce-gated), but still
    // host-guarded. See `claim`.
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

    // GET /view -> the trusted render-isolation SHELL (the privileged SPA that
    // holds the cookie, hosts the sandboxed renderer iframe, and does all the
    // authenticated fetching). COOP/COEP + frame-ancestors 'none' shell CSP. The
    // `?path=` is consumed client-side by the shell bootstrap (which fetches
    // `/content?path=` and mounts a fresh `/srcdoc` iframe per document), so the
    // shell itself serves NO file bytes and is only network-guarded. (W6.1, 3b-iii)
    let view_state = state.clone();
    let view = warp::path("view")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .map(move |ctx: ReqContext| view_page(&view_state, &ctx));

    // GET /srcdoc -> the sandboxed null-origin renderer document (a FRESH nonce
    // per request: a fresh iframe per navigation). renderer CSP (default-src
    // 'none', connect-src 'none', img/media from the static origin only) travels
    // in the srcdoc's <meta http-equiv>. Network-guarded; serves no file bytes.
    let srcdoc_state = state.clone();
    let srcdoc = warp::path("srcdoc")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .and(warp::query::<NonceQuery>())
        .map(move |ctx: ReqContext, q: NonceQuery| {
            srcdoc_page(&srcdoc_state, &ctx, q.n.as_deref())
        });

    // GET /content?path=<abs> -> the rendered markdown BODY fragment for one doc,
    // SAME-ORIGIN + authenticated. Goes through `serve_confined` (the floor): a
    // non-world-readable doc requires the same-origin session cookie. The shell
    // fetches this and relays {type:"render",html} into the sandboxed iframe.
    // (W6.1, 3b-iii)
    let content_state = state.clone();
    let content = warp::path("content")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .map(move |ctx: ReqContext, q: PathQuery| content_fragment(&content_state, &ctx, &q.path));

    // GET /navigate?path=<abs> -> the PARENT's path-authority gate: re-confine a
    // navigation target the sandboxed iframe reported. 204 (in-root) lets the
    // parent mount a fresh /srcdoc for it; 403/4xx (escape) is the /outside
    // sentinel. The iframe NEVER resolves a path itself. (W6.2, 3b-iii)
    let navigate_state = state.clone();
    let navigate = warp::path("navigate")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .map(move |ctx: ReqContext, q: PathQuery| navigate_gate(&navigate_state, &ctx, &q.path));

    // GET /edit?path=<abs> -> full multi-user collaborative editor page. Confined
    // + FLOORED (audit MED-3): a non-world-readable file requires auth even just
    // to load the editor shell. Gated by edit_mode: returns 403 in read-only mode.
    let edit_state = state.clone();
    let edit = warp::path("edit")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .map(move |ctx: ReqContext, q: PathQuery| {
            use warp::http::StatusCode;
            use warp::reply::Reply;
            if !edit_mode {
                return warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response();
            }
            edit_page(&edit_state, &ctx, &q.path)
        });

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
    // Gated by edit_mode: returns 403 in read-only mode.
    let collab_state = state.clone();
    let collab_ws_route = warp::path("collab")
        .and(warp::path::end())
        .and(with_context(state.clone()))
        .and(warp::query::<PathQuery>())
        .and(warp::ws())
        .map(move |ctx: ReqContext, q: PathQuery, ws: warp::ws::Ws| {
            use warp::http::StatusCode;
            use warp::reply::Reply;
            if !edit_mode {
                return warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response();
            }
            collab_route(&collab_state, &ctx, q.path, ws)
        });

    // GET /editor-bundle/<filename> -> first-party offline mycelium-editor dist
    // files, embedded in the binary. Same-origin, no CORS, strict allowlist.
    // Served on the MAIN origin (not the secondary bundle/static origin) because
    // the editor module scripts import them as same-origin `/editor-bundle/…` URLs.
    let editor_bundle_route = crate::editor_bundle::editor_bundle_routes();

    health
        .or(outside)
        .or(claim)
        .or(view)
        .or(srcdoc)
        .or(content)
        .or(navigate)
        .or(edit)
        .or(raw)
        .or(save)
        .or(ws)
        .or(collab_ws_route)
        .or(editor_bundle_route)
}

/// The `?path=<abs>` query the content routes accept. The value is an absolute
/// filesystem path (URL-encoded by the client); the confinement funnel rejects
/// any path that is not absolute or escapes the active roots.
#[derive(serde::Deserialize)]
struct PathQuery {
    path: String,
}

/// Query for `GET /srcdoc?n=<nonce>`: the **shell page's CSP nonce**, which the
/// srcdoc's inline bootstrap must carry so it passes the *inherited* shell CSP (a
/// srcdoc iframe inherits + enforces the embedder's CSP on top of its own). The
/// shell threads its nonce here on every per-navigation mount. Optional + bounded:
/// a missing/oversized/ill-formed value falls back to a fresh-minted nonce (a
/// direct `/srcdoc` hit outside the shell won't render anyway), and the builders
/// (`render_srcdoc` → `escape_attr`/`escape_csp_token`) neutralize any injection.
#[derive(serde::Deserialize)]
struct NonceQuery {
    n: Option<String>,
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

/// The shell origin's COOP header (cross-origin isolation; design §3 backstops).
const SHELL_COOP_HEADER: (&str, &str) = ("cross-origin-opener-policy", crate::shell::SHELL_COOP);
/// The shell origin's COEP header (pairs with COOP for Site Isolation).
const SHELL_COEP_HEADER: (&str, &str) = ("cross-origin-embedder-policy", crate::shell::SHELL_COEP);

/// Serve the **trusted render-isolation shell** (design §3 "Two contexts";
/// ADR-0007). This is the privileged SPA that holds the session cookie, hosts
/// the sandboxed renderer iframe, and does ALL authenticated fetching; it loads
/// no untrusted content (the bundle comes cross-origin from the static origin).
///
/// Headers (the security isolation boundary):
///   * `Cross-Origin-Opener-Policy: same-origin` + `Cross-Origin-Embedder-Policy:
///     require-corp` → cross-origin isolation, so the null-origin iframe runs in
///     its own process (Spectre-class backstop),
///   * `Content-Security-Policy: <shell_csp>` — which includes
///     `frame-ancestors 'none'` (the shell may never itself be framed),
///     `connect-src 'self'` (same-origin content API + WS only), and no CDN.
///
/// Only the network guard runs here: the shell serves NO file bytes (the floor
/// is enforced by `/content`/`/ws`, which the shell fetches same-origin carrying
/// the cookie — which is what makes `SameSite=Strict` work post-bootstrap). The
/// `?path=` is consumed entirely client-side by the shell bootstrap.
fn view_page(state: &AppState, ctx: &ReqContext) -> warp::reply::Response {
    use warp::reply::Reply;

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    // The shell needs the static origin for its own bundle (script/style/font);
    // if the static origin is not up yet, fall back to a self placeholder so the
    // CSP is still well-formed and the shell renders (bundle simply 404s).
    let static_origin = state
        .static_base_snapshot()
        .unwrap_or_else(|| "http://127.0.0.1".to_string());
    let nonce = crate::shell::gen_nonce();
    let csp = crate::shell::shell_csp(&static_origin, &nonce);
    let page = crate::shell::render_shell_page(&static_origin, &nonce);

    let mut resp = warp::reply::html(page).into_response();
    let h = resp.headers_mut();
    insert_header(h, SHELL_COOP_HEADER.0, SHELL_COOP_HEADER.1);
    insert_header(h, SHELL_COEP_HEADER.0, SHELL_COEP_HEADER.1);
    insert_header(h, "content-security-policy", &csp);
    resp
}

/// Serve a **fresh sandboxed renderer document** (the iframe `srcdoc`; design §3
/// "Renderer CSP"). A FRESH nonce is minted per request — a fresh iframe per
/// navigation — and the renderer CSP (`default-src 'none'`, `connect-src 'none'`,
/// `img/media` from the static capability origin only) is embedded as the
/// srcdoc's `<meta http-equiv>` (a srcdoc has no response headers of its own).
/// The frame is mounted null-origin sandboxed (`sandbox="allow-scripts"`) by the
/// shell. Network-guarded; serves no file bytes (content arrives over the bus).
fn srcdoc_page(
    state: &AppState,
    ctx: &ReqContext,
    nonce: Option<&str>,
) -> warp::reply::Response {
    use warp::reply::Reply;

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    let static_origin = state
        .static_base_snapshot()
        .unwrap_or_else(|| "http://127.0.0.1".to_string());
    // The srcdoc's inline bootstrap must carry the SHELL page's nonce so it passes
    // the inherited shell CSP (a srcdoc iframe inherits + enforces the embedder's
    // CSP on top of its own <meta>). Accept the shell-supplied nonce ONLY if it is
    // well-formed (the URL-safe nonce alphabet, bounded length) — otherwise mint a
    // fresh one (a direct /srcdoc hit outside the shell renders nothing anyway).
    // The builders also escape it, so this is defense-in-depth, not the only gate.
    let nonce = nonce
        .filter(|n| {
            !n.is_empty()
                && n.len() <= 64
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .map(str::to_owned)
        .unwrap_or_else(crate::shell::gen_nonce);
    // One secondary origin serves both the bundle and the capability assets
    // (ADR-0007 interpretation), so the asset origin == the static origin here.
    let doc = crate::shell::render_srcdoc(&static_origin, &static_origin, &nonce);
    warp::reply::html(doc).into_response()
}

/// Serve the rendered markdown **BODY fragment** for one document, SAME-ORIGIN +
/// authenticated (design §3 "postMessage bus"). The shell fetches this (carrying
/// the cookie same-origin) and relays `{type:"render", html}` to the sandboxed
/// iframe.
///
/// This funnels through the SAME floor chokepoint as every other read
/// ([`serve_confined`]): a non-world-readable doc requires `ctx.authenticated`.
/// Because the floor passes HERE, this is the **mint-time authorization context**
/// the /cap design requires — so the in-document `<img>`/`<video>` rewrite mints
/// capability URLs ([`rewrite_doc_caps`]) only after this gate. The body is
/// rendered via the lib.rs render core ([`render_markdown`]).
fn content_fragment(state: &AppState, ctx: &ReqContext, requested: &str) -> warp::reply::Response {
    use std::io::Read;
    use warp::http::StatusCode;
    use warp::reply::Reply;

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    let path = Path::new(requested);
    // FLOOR chokepoint: confine (held fd) + floor on the fstat'd metadata. This
    // is the post-floor, mint-time-authorized context for cap_url (W6.2).
    let mut confined = match serve_confined(state, ctx, path) {
        Ok(c) => c,
        Err(deny) => return deny.into_response(),
    };
    // Read the markdown SOURCE from the HELD fd (no re-open).
    let mut src = Vec::with_capacity(confined.metadata.len() as usize);
    if confined.file.read_to_end(&mut src).is_err() {
        return warp::reply::with_status("", StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    let Ok(src) = std::str::from_utf8(&src) else {
        return warp::reply::with_status("", StatusCode::BAD_REQUEST).into_response();
    };
    let canon = confined.canonical.clone();

    // Render the body via the lib.rs render core, then rewrite document media to
    // capability URLs on the static origin (minted ONLY here, post-floor) and
    // links to /view (shell) / the /outside sentinel for escapes.
    let body = render_markdown(src);
    let roots = state.roots_snapshot();
    let static_base = state.static_base_snapshot();
    let body = rewrite_doc_caps(&body, &roots, &canon, &state.caps, static_base.as_deref());

    warp::reply::with_header(body, "content-type", "text/html; charset=utf-8").into_response()
}

/// The PARENT's sole-path-authority gate (design §3 "Navigation"). The sandboxed
/// iframe reports a clicked link's `{type:"navigate", target}`; the shell asks
/// the server to re-confine it here. An in-root target → **204** (the parent then
/// destroys + recreates the iframe with fresh `/srcdoc` for it); an escape /
/// sensitive / missing target → **403** (the `/outside` semantics). The iframe
/// NEVER resolves the path itself. Network-guarded.
fn navigate_gate(state: &AppState, ctx: &ReqContext, requested: &str) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    if let Some(resp) = network_guard(ctx) {
        return resp;
    }
    // Classify against a plain (non-mutating) snapshot via the single `Confine`
    // fan-out — link probes never renew a root's sliding TTL, so this rides the
    // `impl Confine for Roots` (read-only) gate, not `AppState`'s renewing one.
    let roots = state.roots_snapshot();
    match roots.confine_link(Path::new(requested)) {
        // In-root: the parent may mount a fresh srcdoc for the canonical path.
        confine::LinkResolution::InRoot(_) => {
            warp::reply::with_status("", StatusCode::NO_CONTENT).into_response()
        }
        // Escape / sensitive / missing → the /outside 403 marker.
        confine::LinkResolution::Outside => {
            warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response()
        }
    }
}

/// Insert a response header, ignoring an (impossible for these static values)
/// parse error rather than panicking on a request path.
fn insert_header(headers: &mut warp::http::HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = warp::http::HeaderValue::from_str(value) {
        headers.insert(name, v);
    }
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
    // Use the new offline editor page (mycelium-editor bundle, no CDN/importmap).
    // The old `crate::render_editor_page` from md-render is no longer called for
    // the daemon route; it remains re-exported for other callers/tests.
    warp::reply::html(crate::editor_page::render(&doc_id)).into_response()
}

// ---------------------------------------------------------------------------
// /claim — consume a bootstrap nonce → issue a session (design §2, step 4)
// ---------------------------------------------------------------------------

/// `POST /claim` (urlencoded form `nonce=…&next=…`): consume the single-use
/// nonce via the [`NonceStore`], issue a fresh session, `Set-Cookie` the session
/// token (`HttpOnly; SameSite=Strict; Path=/`, `secure=false` for loopback), and
/// reply `302 Location: <next>` — Post/Redirect/Get, so the browser lands on a
/// clean, reload-safe `GET /view`.
///
/// The bootstrap page submits this as a **form POST** (a navigation, not a
/// `fetch`) precisely so a `file://` page (`Origin: null`) can reach the loopback
/// daemon without CORS — a cross-origin `fetch` from `file://` is blocked.
///
/// **Origin-EXEMPT by design:** the request arrives with `Origin: null` (or
/// none), so we do NOT run the origin guard here (it is the one route that
/// legitimately crosses origins). We DO still enforce the `Host` allowlist
/// (DNS-rebinding defense). The nonce is short-TTL + single-use + high-entropy,
/// so an attacker who cannot read the `0600` bootstrap file cannot forge it.
///
/// **Open-redirect guard:** `next` is only honored if it is a local same-origin
/// path (starts with a single `/`, not `//…`, no scheme/host/backslash). An
/// unsafe or absent `next` falls back to `/` — `next` is never reflected as a
/// `Location` without passing [`safe_next_path`].
fn claim(state: &AppState, host: Option<&str>, body: &[u8]) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    // Host guard still applies (the rebinding stopgap). Origin is exempt.
    if !host.map(auth::host_allowed).unwrap_or(false) {
        return warp::reply::with_status("forbidden", StatusCode::FORBIDDEN).into_response();
    }

    // Parse the urlencoded form body. We accept a single `nonce` field plus an
    // optional `next`. (A bare-nonce body with no `=` is also tolerated for
    // robustness, but the bootstrap always sends the form.)
    let (nonce, next_raw) = parse_claim_form(body);
    let nonce = nonce.unwrap_or_default();
    let nonce = nonce.trim();

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

    // Open-redirect guard: only a local same-origin path is allowed; anything
    // else falls back to a safe default.
    let location = next_raw
        .as_deref()
        .and_then(safe_next_path)
        .unwrap_or_else(|| "/".to_string());

    let max_age = auth::SESSION_TTL.as_secs();
    let cookie = auth::build_set_cookie(&token, state.auth.secure_cookies, Some(max_age));
    let resp = warp::reply::with_header(
        warp::reply::with_status("", StatusCode::FOUND),
        "set-cookie",
        cookie,
    );
    warp::reply::with_header(resp, "location", location).into_response()
}

/// Parse an `application/x-www-form-urlencoded` `/claim` body into
/// `(nonce, next)`. Percent-decodes values and converts `+` to space. A body
/// with no `=` is treated as a bare nonce (legacy/robustness). Unknown fields
/// are ignored.
fn parse_claim_form(body: &[u8]) -> (Option<String>, Option<String>) {
    let s = match std::str::from_utf8(body) {
        Ok(s) => s,
        Err(_) => return (None, None),
    };
    // No `=` and no `&` → the whole body is a bare nonce.
    if !s.contains('=') {
        let t = s.trim();
        return (
            if t.is_empty() { None } else { Some(t.to_string()) },
            None,
        );
    }
    let mut nonce = None;
    let mut next = None;
    for pair in s.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        match k {
            "nonce" => nonce = Some(form_urldecode(v)),
            "next" => next = Some(form_urldecode(v)),
            _ => {}
        }
    }
    (nonce, next)
}

/// Decode one `application/x-www-form-urlencoded` component: `+` → space, and
/// `%XX` → byte. Invalid UTF-8 in the result is replaced lossily; malformed `%`
/// escapes are passed through literally (best-effort).
fn form_urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Open-redirect guard for the `/claim` `next` parameter.
///
/// Returns `Some(path)` only when `next` is a **local same-origin path** safe to
/// use verbatim as a `Location`: it must start with exactly one `/`, NOT be a
/// scheme-relative `//host` URL, contain no scheme (`http:`, `javascript:`, …),
/// no backslash (`/\evil` is a known browser-normalization bypass), and no
/// control characters (CR/LF header-injection). Anything else → `None` (the
/// caller falls back to a safe default), so an attacker-supplied `next` can never
/// redirect off-origin.
fn safe_next_path(next: &str) -> Option<String> {
    // Must be a path-absolute reference: single leading slash, not `//`.
    if !next.starts_with('/') || next.starts_with("//") {
        return None;
    }
    // Reject backslashes (browsers treat `\` like `/`, so `/\evil.com` → host),
    // control chars (header injection), and any `:` before the first `/` (a
    // scheme like `/foo` can't have one, but be defensive about `/a:b`-style
    // confusion is unnecessary — a leading `/` already forbids a scheme; we only
    // need to bar control chars and backslashes).
    if next.bytes().any(|b| b == b'\\' || b.is_ascii_control()) {
        return None;
    }
    Some(next.to_string())
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

/// Serve the confined file's raw Markdown source for `GET /raw?path=<rel>`.
///
/// This is the source the in-browser editor loads into its textarea. Confined
/// and size-limited exactly like `view`: `rel` is resolved
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

/// The `/outside` 403 sentinel a document link is rewritten to when its target
/// escapes every active root (audit: don't emit a live link to an out-of-root
/// file). The route table has no `/outside` handler, so a click yields a 403/404
/// — the deliberate "this link leaves the workspace" marker.
const OUTSIDE_SENTINEL: &str = "/outside";

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

/// Rewrite a rendered body fragment for the **render-isolation content path**
/// (design §3 / ADR-0007): the capability-aware rewrite used by
/// [`content_fragment`] and the `/ws` watch loop AFTER the floor has passed (the
/// mint-time authorization context the /cap design requires).
///
///   * relative media (`src="…"`, e.g. `<img>`/`<video>`) resolving in-root →
///     a **capability URL** `http://<static>/cap/<token>` minted via
///     [`asset_origin::cap_url`] on the static (capability) origin — so the
///     sandboxed null-origin renderer (with `img-src/media-src` = the static
///     origin only, `connect-src 'none'`) can *display* but never *read* the
///     bytes (canvas-taint + zero egress). The OLD `/asset?path=` rewrite (shell
///     origin) is deliberately NOT used here: the renderer CSP would block it.
///   * relative `.md` `href="…"` resolving in-root → `/view?path=<abs>` (the
///     shell handles the actual navigation via the parent path-authority gate),
///   * any escape → the [`OUTSIDE_SENTINEL`] (`/outside`) marker.
///   * absolute/external/`data:`/fragment URLs → left as-is.
///
/// `cap_url` is reached ONLY from here, and only after [`serve_confined`] passed,
/// so it is unreachable from any unauthenticated / floor-failed path. If the
/// static origin is not up yet (`static_base` is `None`) or a mint fails, the
/// media `src` is left unchanged (it simply won't load under the renderer CSP) —
/// fail closed, never leak a shell-origin path into the sandbox.
fn rewrite_doc_caps(
    body: &str,
    roots: &Roots,
    doc_canon: &Path,
    caps: &Mutex<asset_origin::CapStore>,
    static_base: Option<&str>,
) -> String {
    let doc_dir = doc_canon.parent().map(Path::to_path_buf).unwrap_or_default();
    let union_owned: Vec<crate::roots::Root> = roots.union().into_iter().cloned().collect();
    let union: Vec<&crate::roots::Root> = union_owned.iter().collect();

    // href: a local `.md` -> /view (shell); other in-root href -> a capability
    // URL; escapes -> /outside. (Non-media href to a non-.md asset is rare; route
    // it to a capability URL too so it stays inside the isolation model.)
    let out = rewrite_attr(body, "href=\"", |u| {
        rewritten_cap_url(u, &doc_dir, &union, roots, caps, static_base, false)
    });
    // src: always media -> a capability URL on the static origin (or unchanged
    // if no static origin / mint failure -> fail closed under the renderer CSP).
    rewrite_attr(&out, "src=\"", |u| {
        rewritten_cap_url(u, &doc_dir, &union, roots, caps, static_base, true)
    })
}

/// Classify + rewrite one document URL for the capability content path (see
/// [`rewrite_doc_caps`]). `src_attr` distinguishes media (`src`) from links
/// (`href`): media in-root mints a capability URL; a `.md` `href` becomes a
/// `/view?path=` shell link. Escapes → `/outside`; non-candidates → `None`.
fn rewritten_cap_url(
    url: &str,
    doc_dir: &Path,
    union: &[&crate::roots::Root],
    roots: &Roots,
    caps: &Mutex<asset_origin::CapStore>,
    static_base: Option<&str>,
    src_attr: bool,
) -> Option<String> {
    if url.is_empty()
        || url.starts_with('#')
        || url.starts_with('/')
        || url.starts_with("//")
        || has_uri_scheme(url)
    {
        return None;
    }
    let (path_part, suffix) = match url.find(['#', '?']) {
        Some(p) => (&url[..p], &url[p..]),
        None => (url, ""),
    };
    if path_part.is_empty() {
        return None;
    }

    let candidate = doc_dir.join(path_part);
    match confine::confine_link(&candidate, union, roots) {
        confine::LinkResolution::InRoot(canon) => {
            let is_md = path_part.to_ascii_lowercase().ends_with(".md");
            if is_md && !src_attr {
                // A markdown link: route to the shell `/view` (the parent owns
                // navigation; the iframe also reports the click up the bus).
                let abs = canon.to_string_lossy().replace('\\', "/");
                Some(format!("/view?path={}{}", encode_query_value(&abs), suffix))
            } else {
                // Media (or a non-.md asset): mint a per-doc capability URL on the
                // static origin. cap_url is minted ONLY here, post-floor. With no
                // static origin yet OR a mint failure, leave the ref unchanged —
                // fail closed (the renderer CSP blocks a shell-origin path anyway).
                let base = static_base?;
                asset_origin::cap_url(caps, base, canon)
            }
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

/// Write a confined [`FilePeer`]'s session text to disk through the **symlink-safe
/// save funnel**, re-confining the target at flush time (audit B-security F1).
///
/// This is the confinement-aware collab write-back. It lives in the daemon (not in
/// `doc-core`) because it depends on the confinement funnel (`confine`/`roots`),
/// and the kernel crate must stay free of inter-kernel edges (ADR-0008 acyclic
/// DAG). It drives the peer through doc-core's public API
/// ([`FilePeer::root`]/[`FilePeer::path`]/[`FilePeer::session`]).
///
/// Unlike [`FilePeer::write_to_disk`]'s plain `fs::write` (which follows symlinks
/// and is not atomic), this routes through [`confine::confine_save`]: it holds the
/// confined parent as a dirfd opened with `O_DIRECTORY | O_NOFOLLOW` and commits
/// via a temp + `renameat`, so a final-component OR intermediate-parent symlink
/// swapped in **between spawn and this flush** cannot redirect the write outside
/// the confined root.
///
/// The write is confined against **the peer's own confinement root**
/// ([`FilePeer::within`]'s `root`) — the same directory boundary the peer was
/// pinned to at spawn. `registry` supplies the sensitive denylist, exactly as the
/// `/save` route does. A peer built via the non-confining [`FilePeer::new`] has no
/// root and gets a [`ConfineError::Escapes`]; such a peer should use
/// [`FilePeer::write_to_disk`] (it is trusted-local by construction). The
/// network-facing daemon always builds peers via `within`.
///
/// The resulting filesystem event still diffs to nothing in
/// [`FilePeer::sync_from_disk`] (the stateless content-compare guard), so this
/// does not feed back as an external edit.
fn write_within(peer: &FilePeer<YrsSession>, registry: &Roots) -> Result<(), ConfineError> {
    let Some(root) = peer.root() else {
        // A non-confined (trusted-local) peer has no boundary to re-confine
        // against; refuse rather than fall back to an unconfined write.
        return Err(ConfineError::Escapes(peer.path().to_path_buf()));
    };
    // Confine the write against the peer's OWN confinement root (a directory
    // boundary), so the save funnel re-resolves the parent dirfd-relative and
    // refuses any symlink swap that would escape that root.
    let confine_root = crate::roots::Root {
        kind: crate::roots::RootKind::Directory,
        path: root.to_path_buf(),
        last_used: std::time::SystemTime::now(),
    };
    let union_refs: Vec<&crate::roots::Root> = vec![&confine_root];
    confine::confine_save(
        peer.path(),
        &union_refs,
        registry,
        peer.session().text().as_bytes(),
    )
}

/// The render-time dependencies the watch loop needs to turn markdown into the
/// broadcast body fragment: the active root union snapshot (link confinement),
/// the shared capability store, and the static origin base. Grouped so
/// [`spawn_watch`] stays under the argument-count lint. Every broadcast body is
/// relayed into the sandboxed renderer iframe, so its media is rewritten to
/// capability URLs ([`rewrite_doc_caps`]) exactly like [`content_fragment`].
struct WatchRenderDeps {
    /// Snapshot of the active root union at spawn (link confinement).
    roots: Roots,
    /// The shared per-doc capability store (the SAME one the `/cap` route
    /// resolves against); media refs mint short-TTL tokens here.
    caps: Arc<Mutex<asset_origin::CapStore>>,
    /// The static (capability) origin base, if bound yet; `None` leaves media
    /// refs unchanged (fail closed under the renderer CSP).
    static_base: Option<String>,
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
/// The rendered fragment has cross-file `.md` links and local media URLs
/// rewritten relative to this document (via [`rewrite_doc_caps`] — capability
/// URLs for media, the shell `/view` for `.md` links) before it is
/// cached/broadcast, so every consumer (the initial `/ws` frame + live updates,
/// relayed by the shell into the sandboxed iframe) is consistent.
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
    render_deps: WatchRenderDeps,
) -> std::io::Result<()> {
    let WatchRenderDeps {
        roots,
        caps,
        static_base,
    } = render_deps;
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
            // Preserve the underlying `notify::Error` (it is `std::error::Error`)
            // by boxing it into the io error rather than flattening to a string,
            // so the error chain/source survives (audit C §5).
            if let Err(e) = peer.watch().map_err(std::io::Error::other) {
                let _ = ready_tx.send(Err(e));
                return;
            }

            // Render isolation: every broadcast body is relayed by the trusted
            // shell into the SANDBOXED renderer iframe, so its media MUST be
            // capability URLs on the static origin (the renderer CSP allows
            // img/media only from there and `connect-src 'none'`; a shell-origin
            // `/asset?path=` would be blocked). Links resolve against the doc's
            // CANONICAL path + the active root union (a snapshot at spawn); a `.md`
            // link → the shell `/view`, an escape → the /outside sentinel. Caps
            // are minted here only for an already-floor-gated entry (entries are
            // created only via floor-gated routes) and re-confined at /cap serve.
            let render = |text: &str| {
                rewrite_doc_caps(
                    &render_markdown(text),
                    &roots,
                    &target,
                    &caps,
                    static_base.as_deref(),
                )
            };

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
                            // in isolation in `doc_core::collab`); it is inlined here only
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
                    // Best-effort write through the SYMLINK-SAFE save funnel,
                    // re-confining the target at flush time (audit B-security F1):
                    // a final-component / intermediate-parent symlink swapped in
                    // between spawn and now cannot redirect the write outside the
                    // root. The content-compare guard means our own resulting fs
                    // event is a no-op (no feedback loop).
                    let _ = write_within(&peer, &roots);
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

// ===========================================================================
// Static origin — secondary loopback port (design §3 / ADR-0007)
// ===========================================================================

/// A warp custom rejection used by [`static_host_guard`] when the `Host` header
/// is absent or does not match the static-origin loopback authority.
#[derive(Debug)]
struct NetworkDenied;
impl warp::reject::Reject for NetworkDenied {}

/// Recovery handler for the static origin: map any unhandled rejection (including
/// `NetworkDenied`) to a 403. Unlike the shell origin the static origin has no
/// body routes beyond bundle/cap, so a 403 is the right fallback.
async fn handle_static_rejection(
    err: warp::Rejection,
) -> Result<impl warp::Reply, std::convert::Infallible> {
    use warp::http::StatusCode;
    let code = if err.find::<NetworkDenied>().is_some() {
        StatusCode::FORBIDDEN
    } else if err.is_not_found() {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::FORBIDDEN
    };
    Ok(warp::reply::with_status("", code))
}

/// A `Host` allowlist guard for the **secondary static origin**, identical in
/// spirit to the shell origin's host guard but bound to the *static* port (the
/// two origins live on different ports). DNS-rebinding defense: admit only
/// `127.0.0.1:<static>` / `localhost:<static>`.
fn static_host_guard(
    static_port: u16,
) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("host")
        .and_then(move |host: Option<String>| async move {
            let ok = match host.as_deref() {
                Some(h) => {
                    auth::host_allowed(h) && h.contains(&format!(":{static_port}"))
                }
                None => false,
            };
            if ok {
                Ok(())
            } else {
                Err(warp::reject::custom(NetworkDenied))
            }
        })
        .untuple_one()
}

/// Build the warp filter for the **secondary static origin** (design §3 /
/// ADR-0007): the SRI-pinned JS/CSS [`bundle::bundle_routes`] **plus** the
/// capability-gated [`asset_origin::asset_origin_routes`], so the sandboxed
/// null-origin renderer loads mermaid/KaTeX/CSS and document assets *cross-origin*
/// (with `Cross-Origin-Resource-Policy: cross-origin` on every response, which
/// `bundle.rs`/`asset_origin.rs` already set) from an origin that is **not** the
/// shell origin.
///
/// ## Why one secondary origin for both (design interpretation, ADR-0007)
/// The design calls for "separate local origins" for the bundle and the assets.
/// To avoid port sprawl we stand up **one** secondary "static" origin (a single
/// extra loopback TCP port) that serves both. The security model only needs this
/// origin to be *separate from the shell origin* — cross-origin to the
/// null-origin renderer (so canvas-taint + `connect-src 'none'` make asset bytes
/// visible-but-opaque-and-unexfiltratable) and CORP-tagged (so the COEP
/// shell/iframe can load them). A single secondary origin satisfies all of that;
/// the shell origin stays the primary port.
///
/// Unlike the shell origin this filter carries **no `Origin` guard and no session
/// cookie**: the bundle is public, and the assets are gated by the unguessable,
/// short-TTL capability token (re-confined at serve time), not the cookie. The
/// no-cors subresource loads from the null-origin iframe carry no `Origin`, so an
/// `Origin` guard would buy nothing here. Only the loopback `Host` allowlist is
/// applied (DNS-rebinding defense), via [`static_host_guard`].
fn static_origin_routes<F>(
    state: Arc<AppState>,
    static_port: u16,
    cache: bundle::BundleCache,
    fetcher: F,
) -> impl Filter<Extract = impl warp::Reply, Error = std::convert::Infallible> + Clone
where
    F: bundle::Fetcher + Clone + 'static,
{
    let assets = asset_origin::asset_origin_routes(state.caps.clone(), state.confiner());
    let bndl = bundle::bundle_routes(cache, fetcher);
    static_host_guard(static_port)
        .and(bndl.or(assets))
        .recover(handle_static_rejection)
}

/// Bind and spawn the **secondary static origin** (design §3 / ADR-0007) on the
/// same tokio runtime as the shell server, sharing `state` (`Arc<AppState>`).
///
/// The static port defaults to `shell_addr.port() + 1`; if that is busy (or 0)
/// `try_bind_ephemeral` falls back to an OS-chosen port, and we learn the ACTUAL
/// bound address. The bundle cache dir is [`bundle::default_cache_dir`] (a temp
/// fallback if `$HOME`/XDG are unset); the production fetcher is
/// [`bundle::UreqFetcher`]. The learned base URL (`http://127.0.0.1:<port>`) is
/// recorded in `state` for the future shell / asset-URL rewrite (W6).
///
/// Best-effort and non-fatal: a static-origin bind failure is logged and the
/// daemon continues serving the shell origin (assets/bundle simply unavailable
/// cross-origin until restart) rather than aborting startup.
fn spawn_static_origin(shell_addr: SocketAddr, state: Arc<AppState>) {
    // Default to shell_port + 1; saturating so port 65535 doesn't wrap. A 0 (or
    // busy) port is handled by the ephemeral fallback below.
    let static_port = shell_addr.port().saturating_add(1);
    let bind_addr = SocketAddr::new(shell_addr.ip(), static_port);

    // Bundle cache dir + production fetcher. A missing cache dir is non-fatal: the
    // bundle route fetches+verifies on demand and stores lazily.
    let cache_dir = bundle::default_cache_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("md-preview").join("bundle"));
    let cache = bundle::BundleCache::new(cache_dir);
    let fetcher = bundle::UreqFetcher;

    let routes = static_origin_routes(state.clone(), static_port, cache, fetcher);
    match warp::serve(routes).try_bind_ephemeral(bind_addr) {
        Ok((bound, fut)) => {
            // Learn the ACTUAL bound port (ephemeral fallback may differ) and
            // record the base URL for the shell / asset-URL rewrite (W6).
            state.set_static_base(format!("http://127.0.0.1:{}", bound.port()));
            tokio::spawn(fut);
        }
        Err(e) => {
            eprintln!(
                "md-preview: could not bind the secondary static origin on {bind_addr}: {}; \
                 cross-origin bundle/assets unavailable this run",
                e
            );
        }
    }
}

// ===========================================================================
// Public serve entry-points
// ===========================================================================

/// Build and run the daemon on `addr`. The first `file` is not special — it is
/// just the first path the thin client points the browser at; its owning root is
/// registered in the multi-root [`Roots`] registry and any other file under an
/// active root is confinable too (a document is identified by its canonical
/// absolute path, the `path=` query). Entries are created lazily on first touch.
///
/// Returns once the server stops. The caller owns the tokio runtime.
pub async fn serve(file: &Path, addr: SocketAddr, edit_mode: bool) -> std::io::Result<()> {
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

    // One shared Arc: the shell routes AND the secondary static origin must use
    // the SAME AppState (so they share one capability store + roots registry).
    let state = Arc::new(AppState::new(roots));
    // Use try_bind_ephemeral so a port-in-use (or any other bind) error is
    // returned as an io::Result rather than causing a panic.  The bound address
    // is fixed (addr was already chosen by the caller), so we ignore the
    // returned SocketAddr.
    let (_bound_addr, fut) = warp::serve(routes(state.clone(), edit_mode))
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
    // Stand up the secondary static origin (bundle + capability assets) on the
    // same runtime, sharing the AppState (best-effort; see `spawn_static_origin`).
    spawn_static_origin(addr, state);
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
    edit_mode: bool,
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

    // One shared Arc across the shell routes, the control accept loop, AND the
    // secondary static origin (they share one capability store + roots registry).
    let state = Arc::new(AppState::new(roots));

    // Try the preferred port first; fall back to OS-assigned on AddrInUse.
    let preferred: SocketAddr = ([127, 0, 0, 1], 7878).into();
    let ephemeral: SocketAddr = ([127, 0, 0, 1], 0).into();

    let (bound_addr, fut) = warp::serve(routes(state.clone(), edit_mode))
        .try_bind_ephemeral(preferred)
        .or_else(|_| warp::serve(routes(state.clone(), edit_mode)).try_bind_ephemeral(ephemeral))
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

    // Stand up the secondary static origin (bundle + capability assets) on the
    // same runtime, sharing the AppState. Keyed off the ACTUAL bound shell port
    // so its default neighbor port (+1) tracks the real shell port.
    spawn_static_origin(bound_addr, state.clone());

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

/// Start the server **without an initial document**: the daemon binds the
/// control socket and waits for `Open` requests from thin-client invocations
/// (`md <file>`). Used by `--daemon` / the systemd user unit, where the service
/// must be always-up even before any file is first opened.
///
/// Behaviour is identical to [`serve_with_control`] except no initial root is
/// registered — the roots registry starts from the persisted state on disk (if
/// any) so previously open tabs continue to work across restarts.
pub async fn serve_daemon_only(
    ctrl: std::os::unix::net::UnixListener,
    edit_mode: bool,
) -> std::io::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut roots = Roots::new(home);
    if let Ok(loaded) = roots.load() {
        roots = loaded;
    }

    let state = Arc::new(AppState::new(roots));

    let preferred: SocketAddr = ([127, 0, 0, 1], 7878).into();
    let ephemeral: SocketAddr = ([127, 0, 0, 1], 0).into();

    let (bound_addr, fut) = warp::serve(routes(state.clone(), edit_mode))
        .try_bind_ephemeral(preferred)
        .or_else(|_| warp::serve(routes(state.clone(), edit_mode)).try_bind_ephemeral(ephemeral))
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

    spawn_static_origin(bound_addr, state.clone());

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
            sec_fetch_mode: None,
            sec_fetch_dest: None,
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
    fn has_uri_scheme_detects_schemes_not_relative_paths() {
        assert!(has_uri_scheme("https://x/y"));
        assert!(has_uri_scheme("data:image/png;base64,AAAA"));
        assert!(!has_uri_scheme("pic.png"));
        assert!(!has_uri_scheme("../up/pic.png"));
        assert!(!has_uri_scheme(""));
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
    fn rewrite_doc_caps_media_to_cap_url_md_to_view_escape_to_outside() {
        let dir = temp_dir("rwcaps");
        let img = write_mode(&dir, "img.png", "PNG", 0o644);
        let b = write_mode(&dir, "b.md", "b", 0o644);
        let state = state_for(&dir);
        let roots = state.roots_snapshot();
        let doc = dir.join("a.md");
        let base = "http://127.0.0.1:8081";

        let body = r##"<a href="b.md">B</a> <a href="https://x/y.md">ext</a> <a href="#frag">f</a> <img src="img.png"> <img src="../../../../../etc/hosts">"##;
        let out = rewrite_doc_caps(body, &roots, &doc, &state.caps, Some(base));
        // local .md href -> the shell /view (the parent owns navigation).
        assert!(
            out.contains(&format!("href=\"/view?path={}\"", encode_query_value(&b))),
            "local .md href -> /view: {out}"
        );
        assert!(out.contains(r#"href="https://x/y.md""#), "external left alone");
        assert!(out.contains(r##"href="#frag""##), "fragment left alone");
        // in-root media -> a capability URL on the STATIC origin (not /asset).
        assert!(out.contains(&format!("src=\"{base}/cap/")), "in-root img -> cap URL: {out}");
        assert!(!out.contains("/asset?path="), "the old /asset rewrite is retired");
        // escaping media -> the /outside sentinel (no capability minted for it).
        assert!(out.contains(&format!("src=\"{OUTSIDE_SENTINEL}\"")), "escape -> /outside: {out}");
        // The minted token resolves the in-root image in the shared store.
        let token = out.split("/cap/").nth(1).and_then(|s| s.split('"').next()).unwrap();
        assert_eq!(
            state.caps.lock().unwrap().resolve(token),
            Some(img.into())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrite_doc_caps_without_static_origin_leaves_media_unchanged() {
        // Fail closed: with no static origin yet, an in-root media ref is left
        // unchanged (no shell-origin path leaks into the sandbox; the renderer
        // CSP blocks it) and NO capability is minted.
        let dir = temp_dir("rwcaps-nostatic");
        write_mode(&dir, "img.png", "PNG", 0o644);
        let state = state_for(&dir);
        let roots = state.roots_snapshot();
        let doc = dir.join("a.md");

        let out = rewrite_doc_caps(r#"<img src="img.png">"#, &roots, &doc, &state.caps, None);
        assert!(out.contains(r#"src="img.png""#), "media left unchanged: {out}");
        assert_eq!(state.caps.lock().unwrap().live_len(), 0, "no cap minted without a static origin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- read routes through the chokepoint -------------------------------

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

        // The byte-serving read routes all 403 on a private doc unauthenticated.
        // (`/view` is now the floorless shell SPA — it serves no bytes; the floor
        // moved to the SAME-ORIGIN `/content` API the shell fetches.)
        assert_eq!(
            content_fragment(&state, &unauth, &priv_abs).status(),
            warp::http::StatusCode::FORBIDDEN,
            "/content must deny a private file unauthenticated (the floor)"
        );
        assert_eq!(
            serve_raw(&state, &unauth, &priv_abs).status(),
            warp::http::StatusCode::FORBIDDEN,
            "/raw must deny a private file unauthenticated"
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
    fn network_guard_allows_top_level_navigation_landing() {
        // The real bootstrap PRG landing on GET /view: cross-site + Origin: null
        // but a top-level document navigation. The guard must NOT 403 it (this
        // was the bug — the chair's Chrome 403'd here).
        let mut nav = ctx(true);
        nav.origin = Some("null".to_string());
        nav.sec_fetch_site = Some("cross-site".to_string());
        nav.sec_fetch_mode = Some("navigate".to_string());
        nav.sec_fetch_dest = Some("document".to_string());
        assert!(
            network_guard(&nav).is_none(),
            "top-level document navigation (PRG landing) must be allowed"
        );

        // But a cross-site CORS subresource fetch with the same null origin is
        // still rejected — the carve-out is navigation-only.
        let mut cors = ctx(true);
        cors.origin = Some("null".to_string());
        cors.sec_fetch_site = Some("cross-site".to_string());
        cors.sec_fetch_mode = Some("cors".to_string());
        cors.sec_fetch_dest = Some("empty".to_string());
        assert!(
            network_guard(&cors).is_some(),
            "cross-site cors fetch must stay rejected"
        );
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
            content_fragment(&state, &cross, &pub_abs).status(),
            warp::http::StatusCode::FORBIDDEN
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- /claim: nonce -> session -> Set-Cookie ----------------------------

    /// Build a urlencoded `/claim` form body.
    fn claim_form(nonce: &str, next: &str) -> Vec<u8> {
        // The values in our tests need no escaping beyond `/` which is form-safe.
        format!("nonce={nonce}&next={next}").into_bytes()
    }

    #[test]
    fn claim_consumes_nonce_and_redirects_with_cookie() {
        let dir = temp_dir("claim");
        let state = state_for(&dir);
        // Arm a nonce (the control-plane side).
        let nonce = mint_claim_nonce(&state).expect("mint nonce");
        let next = "/view?path=/tmp/x.md";

        // A wrong nonce is rejected (403, no cookie).
        let bad = claim(&state, Some("127.0.0.1:7878"), &claim_form("not-the-nonce", next));
        assert_eq!(bad.status(), warp::http::StatusCode::FORBIDDEN);
        assert!(bad.headers().get("set-cookie").is_none());

        // The right nonce yields a 302 → next + Set-Cookie carrying a session.
        let ok = claim(&state, Some("127.0.0.1:7878"), &claim_form(&nonce, next));
        assert_eq!(ok.status(), warp::http::StatusCode::FOUND);
        assert_eq!(
            ok.headers().get("location").unwrap().to_str().unwrap(),
            next,
            "302 Location must echo the safe `next`"
        );
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

        // Single-use: replaying the burned nonce fails (403, no cookie).
        let replay = claim(&state, Some("127.0.0.1:7878"), &claim_form(&nonce, next));
        assert_eq!(replay.status(), warp::http::StatusCode::FORBIDDEN);
        assert!(replay.headers().get("set-cookie").is_none());

        // /claim is host-guarded even though origin-exempt.
        let bad_host = claim(&state, Some("evil.com"), &claim_form(&nonce, next));
        assert_eq!(bad_host.status(), warp::http::StatusCode::FORBIDDEN);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claim_open_redirect_guard_rejects_offorigin_next() {
        let dir = temp_dir("claim-redirect");
        let state = state_for(&dir);

        // Each unsafe `next` must NOT be reflected; Location falls back to "/".
        for bad in [
            "//evil.com",
            "https://evil",
            "http://evil.com/x",
            "/\\evil",            // backslash bypass
            "javascript:alert(1)",
            "evil",               // no leading slash
            "/x\r\nSet-Cookie: y=z", // CRLF header injection
        ] {
            let nonce = mint_claim_nonce(&state).expect("mint");
            let resp = claim(&state, Some("127.0.0.1:7878"), &claim_form(&nonce, bad));
            assert_eq!(resp.status(), warp::http::StatusCode::FOUND);
            let loc = resp.headers().get("location").unwrap().to_str().unwrap();
            assert_eq!(loc, "/", "unsafe next `{bad}` must fall back to `/`, got `{loc}`");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_next_path_unit() {
        assert_eq!(safe_next_path("/view?path=/a"), Some("/view?path=/a".to_string()));
        assert_eq!(safe_next_path("/"), Some("/".to_string()));
        assert_eq!(safe_next_path("//evil.com"), None);
        assert_eq!(safe_next_path("https://evil"), None);
        assert_eq!(safe_next_path("/\\evil"), None);
        assert_eq!(safe_next_path("evil"), None);
        assert_eq!(safe_next_path("/x\r\ny"), None);
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
        let resp = claim(
            &state,
            Some("127.0.0.1:7878"),
            &claim_form(&nonce, "/view?path=/x.md"),
        );
        assert_eq!(resp.status(), warp::http::StatusCode::FOUND);
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

    // --- render-isolation shell / srcdoc / content / navigate (W6, 3b-iii) -

    /// Read a response header as an owned String (panics in tests on absence).
    fn header(resp: &warp::reply::Response, name: &str) -> String {
        resp.headers()
            .get(name)
            .unwrap_or_else(|| panic!("header {name} present"))
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn view_serves_shell_with_coop_coep_and_frame_ancestors_none() {
        let dir = temp_dir("view-shell");
        let state = state_for(&dir);
        // The shell needs a static origin to point its bundle CSP at.
        state.set_static_base("http://127.0.0.1:8081".to_string());

        let resp = view_page(&state, &ctx(false));
        assert_eq!(resp.status(), warp::http::StatusCode::OK);
        // Cross-origin isolation pair (Site Isolation backstop).
        assert_eq!(header(&resp, "cross-origin-opener-policy"), "same-origin");
        assert_eq!(header(&resp, "cross-origin-embedder-policy"), "require-corp");
        // The shell CSP carries frame-ancestors 'none' and is the shell policy.
        let csp = header(&resp, "content-security-policy");
        assert!(csp.contains("frame-ancestors 'none'"), "csp: {csp}");
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("connect-src 'self'"));
        // The body is the trusted SPA shell mounting the sandboxed iframe.
        let html = String::from_utf8(
            warp::hyper::body::to_bytes(resp.into_body()).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains(&format!(r#"sandbox="{}""#, crate::shell::IFRAME_SANDBOX)));
        assert!(!html.contains("allow-same-origin"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outside_sentinel_route_serves_403_not_404() {
        // The shell sends the top frame to /outside when a navigation escapes the
        // roots. Previously there was no route, so the click 404'd; now it is a
        // contained 403 sentinel page with no path echo.
        let dir = temp_dir("outside");
        let state = Arc::new(state_for(&dir));
        let routes = routes(state, true);
        let resp = warp::test::request()
            .method("GET")
            .path("/outside")
            .header("host", "127.0.0.1:7878")
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 403, "/outside must be a 403 sentinel, not a 404");
        let body = String::from_utf8(resp.body().to_vec()).unwrap();
        assert!(body.contains("Outside the preview roots"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn srcdoc_has_fresh_nonce_and_zero_egress_renderer_csp() {
        let dir = temp_dir("srcdoc");
        let state = state_for(&dir);
        state.set_static_base("http://127.0.0.1:8081".to_string());

        let body_of = |resp: warp::reply::Response| async {
            String::from_utf8(
                warp::hyper::body::to_bytes(resp.into_body()).await.unwrap().to_vec(),
            )
            .unwrap()
        };

        // With the SHELL's nonce threaded in (?n=), the srcdoc echoes it verbatim
        // so its inline bootstrap passes the inherited shell CSP.
        let with_nonce = body_of(srcdoc_page(&state, &ctx(false), Some("sharedN0nce"))).await;
        assert!(with_nonce.contains(r#"<script nonce="sharedN0nce">"#));
        assert!(with_nonce.contains("&#x27;nonce-sharedN0nce&#x27;") || with_nonce.contains("'nonce-sharedN0nce'"));

        // Renderer CSP travels in the srcdoc meta and is zero-egress.
        let a = body_of(srcdoc_page(&state, &ctx(false), None)).await;
        let b = body_of(srcdoc_page(&state, &ctx(false), None)).await;
        assert!(a.contains(r#"<meta http-equiv="Content-Security-Policy""#));
        assert!(a.contains("connect-src &#x27;none&#x27;") || a.contains("connect-src 'none'"));
        // With NO shell nonce supplied (a direct /srcdoc hit), a fresh one is minted
        // per request, so the two differ — defense-in-depth for off-shell hits.
        assert_ne!(a, b, "a nonce-less /srcdoc hit mints a distinct nonce");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn content_api_enforces_the_floor() {
        let dir = temp_dir("content-floor");
        let priv_abs = write_mode(&dir, "secret.md", "# top secret", 0o600);
        let pub_abs = write_mode(&dir, "public.md", "# hello world", 0o644);
        let state = state_for(&dir);

        // Unauthenticated request for a non-world-readable doc -> 403 (floor).
        let denied = content_fragment(&state, &ctx(false), &priv_abs);
        assert_eq!(
            denied.status(),
            warp::http::StatusCode::FORBIDDEN,
            "content API must deny a private doc unauthenticated (the floor)"
        );
        // Authenticated unlocks the private doc.
        let ok_auth = content_fragment(&state, &ctx(true), &priv_abs);
        assert_eq!(ok_auth.status(), warp::http::StatusCode::OK);

        // World-readable served even unauthenticated, as the rendered BODY.
        let ok_pub = content_fragment(&state, &ctx(false), &pub_abs);
        assert_eq!(ok_pub.status(), warp::http::StatusCode::OK);
        assert_eq!(
            header(&ok_pub, "content-type"),
            "text/html; charset=utf-8"
        );
        let html = String::from_utf8(
            warp::hyper::body::to_bytes(ok_pub.into_body()).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains("hello world"), "rendered body fragment: {html}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn content_api_rewrites_media_to_static_origin_capability_urls() {
        let dir = temp_dir("content-cap");
        let img = write_mode(&dir, "img.png", "PNG", 0o644);
        let doc = dir.join("doc.md");
        std::fs::write(&doc, "![pic](img.png)\n\n[next](other.md)\n").unwrap();
        std::fs::set_permissions(&doc, std::fs::Permissions::from_mode(0o644)).unwrap();
        let other = write_mode(&dir, "other.md", "# other", 0o644);
        let state = state_for(&dir);
        state.set_static_base("http://127.0.0.1:8081".to_string());

        let resp = content_fragment(&state, &ctx(false), &doc.to_string_lossy());
        assert_eq!(resp.status(), warp::http::StatusCode::OK);
        let html = String::from_utf8(
            warp::hyper::body::to_bytes(resp.into_body()).await.unwrap().to_vec(),
        )
        .unwrap();
        // The in-root image is a capability URL on the STATIC origin (not /asset).
        assert!(
            html.contains("src=\"http://127.0.0.1:8081/cap/"),
            "in-root media -> static-origin capability URL: {html}"
        );
        assert!(!html.contains("/asset?path="), "old /asset rewrite is retired");
        // The markdown link routes to the shell /view (the parent navigates).
        assert!(
            html.contains(&format!("href=\"/view?path={}\"", encode_query_value(&other))),
            "md link -> shell /view: {html}"
        );
        // The minted token resolves in the SHARED cap store (post-floor mint).
        let token = html
            .split("/cap/")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap()
            .to_string();
        assert_eq!(
            state.caps.lock().unwrap().resolve(&token),
            Some(img.into()),
            "cap_url minted the in-root image path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn content_api_escaping_media_becomes_outside_sentinel() {
        let dir = temp_dir("content-esc");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let doc = dir.join("sub/child.md");
        std::fs::write(&doc, "![evil](../../../../../etc/hosts)\n").unwrap();
        std::fs::set_permissions(&doc, std::fs::Permissions::from_mode(0o644)).unwrap();
        let state = state_for(&dir);
        state.set_static_base("http://127.0.0.1:8081".to_string());

        let resp = content_fragment(&state, &ctx(false), &doc.to_string_lossy());
        let html = String::from_utf8(
            warp::hyper::body::to_bytes(resp.into_body()).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains(&format!("src=\"{OUTSIDE_SENTINEL}\"")), "escape -> /outside: {html}");
        assert!(!html.contains("/cap/"), "an escaping media ref must NOT mint a capability");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cap_url_is_unreachable_when_floor_fails() {
        // The content API denies a private doc unauthenticated BEFORE any render
        // or rewrite, so cap_url is never reached on a floor-failed path: the
        // shared cap store stays empty.
        let dir = temp_dir("cap-floor");
        let _img = write_mode(&dir, "img.png", "PNG", 0o644);
        let doc = dir.join("secret.md");
        std::fs::write(&doc, "![pic](img.png)\n").unwrap();
        std::fs::set_permissions(&doc, std::fs::Permissions::from_mode(0o600)).unwrap();
        let state = state_for(&dir);
        state.set_static_base("http://127.0.0.1:8081".to_string());

        let denied = content_fragment(&state, &ctx(false), &doc.to_string_lossy());
        assert_eq!(denied.status(), warp::http::StatusCode::FORBIDDEN);
        // No token was minted (cap_url unreachable from the floor-failed path).
        assert_eq!(state.caps.lock().unwrap().live_len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn navigate_gate_rejects_out_of_root_and_admits_in_root() {
        let dir = temp_dir("navigate");
        let pub_abs = write_mode(&dir, "doc.md", "# doc", 0o644);
        let state = state_for(&dir);

        // In-root target -> 204 (the parent may mount a fresh srcdoc for it).
        assert_eq!(
            navigate_gate(&state, &ctx(false), &pub_abs).status(),
            warp::http::StatusCode::NO_CONTENT
        );
        // An out-of-root target -> 403 (the /outside marker; parent refuses).
        assert_eq!(
            navigate_gate(&state, &ctx(false), "/etc/hosts").status(),
            warp::http::StatusCode::FORBIDDEN,
            "navigation to an out-of-root target is rejected by the parent"
        );
        // A relative (non-absolute) target is also refused.
        assert_eq!(
            navigate_gate(&state, &ctx(false), "../escape.md").status(),
            warp::http::StatusCode::FORBIDDEN
        );
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
        let (bound_addr, _fut1) = warp::serve(routes(state1, true))
            .try_bind_ephemeral(loopback)
            .expect("first bind must succeed");

        let state2 = state_for(&dir);
        let result = warp::serve(routes(state2, true))
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

    // -----------------------------------------------------------------------
    // Secondary static origin wiring (design §3 / ADR-0007).
    //
    // These exercise `static_origin_routes` — the bundle + capability-asset
    // filter mounted on the second loopback port — end-to-end via `warp::test`,
    // sharing one `Arc<AppState>` so a token minted into `state.caps` is the same
    // store the route resolves against (the real production wiring). Network-free:
    // a fake fetcher means the bundle path never touches the network.
    // -----------------------------------------------------------------------

    /// The static origin's test port.
    const TEST_STATIC_PORT: u16 = 8081;

    fn valid_static_host() -> String {
        format!("127.0.0.1:{TEST_STATIC_PORT}")
    }

    /// Network-free bundle fetcher: any fetch fails.
    #[derive(Clone, Copy)]
    struct OfflineFetcher;
    impl bundle::Fetcher for OfflineFetcher {
        fn get(&self, _url: &str) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::other("offline (test fetcher)"))
        }
    }

    fn static_routes_over(
        state: Arc<AppState>,
        cache_dir: PathBuf,
    ) -> impl warp::Filter<Extract = impl warp::Reply, Error = std::convert::Infallible> + Clone
    {
        let cache = bundle::BundleCache::new(cache_dir);
        static_origin_routes(state, TEST_STATIC_PORT, cache, OfflineFetcher)
    }

    #[tokio::test]
    async fn static_origin_serves_capability_asset_with_corp_and_no_cors() {
        let dir = temp_dir("static-cap");
        let bytes: &[u8] = b"\x89PNG\r\n\x1a\nfake";
        std::fs::write(dir.join("pic.png"), bytes).unwrap();
        std::fs::set_permissions(
            dir.join("pic.png"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let state = Arc::new(state_for(&dir));
        let token = state
            .caps
            .lock()
            .unwrap()
            .mint(
                dir.join("pic.png"),
                asset_origin::CAP_TTL_SECS,
            )
            .unwrap();
        let cache_dir = temp_dir("static-cap-cache");
        let routes = static_routes_over(state, cache_dir.clone());

        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .header("host", valid_static_host())
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
        assert_eq!(
            resp.headers().get("cross-origin-resource-policy").unwrap(),
            "cross-origin"
        );
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "static-origin assets must be CORS-less"
        );
        assert_eq!(resp.body().as_ref(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn static_origin_is_host_guarded_but_needs_no_cookie() {
        let dir = temp_dir("static-host");
        std::fs::write(dir.join("pic.png"), b"x").unwrap();
        std::fs::set_permissions(
            dir.join("pic.png"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let state = Arc::new(state_for(&dir));
        let token = state
            .caps
            .lock()
            .unwrap()
            .mint(
                dir.join("pic.png"),
                asset_origin::CAP_TTL_SECS,
            )
            .unwrap();
        let cache_dir = temp_dir("static-host-cache");
        let routes = static_routes_over(state, cache_dir.clone());

        let rebind = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .header("host", "evil.example.com")
            .reply(&routes)
            .await;
        assert_eq!(rebind.status(), 403, "static origin is Host-guarded");

        let no_host = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .reply(&routes)
            .await;
        assert_eq!(no_host.status(), 403, "missing Host rejected");

        let ok = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .header("host", valid_static_host())
            .reply(&routes)
            .await;
        assert_eq!(ok.status(), 200, "valid Host + valid cap serves, no cookie");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[tokio::test]
    async fn static_origin_mounts_the_bundle_route() {
        let dir = temp_dir("static-bundle");
        let state = Arc::new(state_for(&dir));
        let cache_dir = temp_dir("static-bundle-cache");
        let routes = static_routes_over(state, cache_dir.clone());

        let resp = warp::test::request()
            .method("GET")
            .path("/bundle/this-id-does-not-exist.js")
            .header("host", valid_static_host())
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 404, "unknown bundle id → 404 from bundle route");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn static_base_round_trips_through_app_state() {
        let dir = temp_dir("static-base");
        let state = state_for(&dir);
        assert_eq!(*state.static_base.lock().unwrap(), None);
        state.set_static_base("http://127.0.0.1:8081".to_string());
        assert_eq!(
            state.static_base.lock().unwrap().as_deref(),
            Some("http://127.0.0.1:8081")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_world_readable_asset_on_stale_token_is_denied() {
        // Track-A floor hardening: a token for a now-private (0o600) file → 403.
        let dir = temp_dir("static-floor");
        std::fs::write(dir.join("secret.png"), b"\x89PNG").unwrap();
        std::fs::set_permissions(
            dir.join("secret.png"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let state = Arc::new(state_for(&dir));
        let token = state
            .caps
            .lock()
            .unwrap()
            .mint(
                dir.join("secret.png"),
                asset_origin::CAP_TTL_SECS,
            )
            .unwrap();
        let cache_dir = temp_dir("static-floor-cache");
        let routes = static_routes_over(state, cache_dir.clone());

        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .header("host", valid_static_host())
            .reply(&routes)
            .await;
        assert_eq!(
            resp.status(),
            403,
            "non-world-readable asset on a stale token must be denied (Track-A floor)"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    // --- write_within: symlink-safe collab write-back (audit B-security F1) ---
    //
    // The confinement-aware write-back moved to the daemon (this file) when the
    // pure `FilePeer` was extracted into `doc-core` (ADR-0008): doc-core must not
    // depend on the `confine`/`roots` funnel. These tests follow it here.

    /// A registry whose `home` is an out-of-the-way path so the sensitive denylist
    /// never flags the temp targets. [`write_within`] derives the confinement
    /// union from the peer's own root, so the registry is consulted only for the
    /// denylist (mirrors `confine`'s test helpers).
    fn denylist_registry() -> Roots {
        Roots::new(PathBuf::from("/nonexistent-home"))
    }

    /// `write_within` lands the session text in-root through the save funnel, and
    /// a subsequent sync is a no-op (no feedback loop).
    #[test]
    fn write_within_lands_in_root_no_feedback() {
        let canon = temp_dir("ww-ok");
        std::fs::write(canon.join("doc.md"), "start").unwrap();

        let reg = denylist_registry();
        let mut peer = FilePeer::within(&canon, "doc.md", YrsSession::from_text("start")).unwrap();

        peer.session_mut()
            .apply(&[crate::doc::TextEdit::insert(5, "!")]);
        write_within(&peer, &reg).expect("write_within confines");
        assert_eq!(std::fs::read_to_string(canon.join("doc.md")).unwrap(), "start!");

        // Our own write must not feed back as an external change.
        assert!(
            !peer.sync_from_disk().unwrap(),
            "write_within must not feed back as an external edit"
        );

        let _ = std::fs::remove_dir_all(&canon);
    }

    /// F1 regression: if the target's final path component is swapped to a
    /// symlink-to-outside *after* the peer was confined at spawn, `write_within`
    /// must NOT follow it — the outside victim stays untouched and the bytes land
    /// on a real in-root file (the symlink-safe `renameat` funnel), unlike the old
    /// plain `fs::write` write-back.
    #[test]
    fn write_within_does_not_follow_swapped_symlink_at_flush() {
        let canon = temp_dir("ww-sym");
        // The file exists in-root at spawn so `within` confines cleanly.
        std::fs::write(canon.join("doc.md"), "original").unwrap();

        // An OUTSIDE victim a malicious symlink would point at.
        let outside = temp_dir("ww-sym-out");
        let victim = outside.join("victim.md");
        std::fs::write(&victim, "VICTIM ORIGINAL").unwrap();

        let reg = denylist_registry();
        let mut peer =
            FilePeer::within(&canon, "doc.md", YrsSession::from_text("original")).unwrap();
        peer.session_mut()
            .apply(&[crate::doc::TextEdit::insert(8, " edited")]);

        // ATTACK: between spawn and flush, swap the final component for a symlink
        // pointing OUTSIDE the root.
        std::fs::remove_file(canon.join("doc.md")).unwrap();
        std::os::unix::fs::symlink(&victim, canon.join("doc.md")).unwrap();

        // The funnel commits via renameat, replacing the symlink ENTRY rather than
        // writing through it.
        write_within(&peer, &reg).expect("write_within still confines");

        // The outside victim is UNTOUCHED — no out-of-root write occurred.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "VICTIM ORIGINAL",
            "write_within must not follow the swapped-in symlink (no out-of-root write)"
        );
        // The in-root entry is now a real file with the new bytes.
        let meta = std::fs::symlink_metadata(canon.join("doc.md")).unwrap();
        assert!(
            meta.file_type().is_file(),
            "target is now a real in-root file, not a symlink"
        );
        assert_eq!(
            std::fs::read_to_string(canon.join("doc.md")).unwrap(),
            "original edited"
        );

        let _ = std::fs::remove_dir_all(&canon);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
