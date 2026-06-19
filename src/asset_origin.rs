//! Capability asset server for the **secondary static origin** (design §3
//! "Assets walled off" / ADR-0007).
//!
//! ## Design interpretation (one secondary origin, two duties)
//! The design calls for "separate local origins" — one for the SRI-pinned JS/CSS
//! bundle and one for document assets. To avoid port sprawl we stand up **one**
//! secondary "static" origin (a single extra loopback TCP port) that serves
//! **both**: [`crate::bundle::bundle_routes`] (public, SRI-pinned) *and* the
//! capability-gated document assets defined here. What the security model needs
//! is that this origin is **separate from the shell origin** — cross-origin to
//! the null-origin sandboxed renderer (so canvas-taint + `connect-src 'none'`
//! make the bytes visible-but-opaque-and-unexfiltratable) and CORP-tagged so the
//! COEP shell/iframe can load them. A single secondary origin satisfies all of
//! that; the shell origin stays the primary port. See [`crate::server`] wiring.
//!
//! ## How an asset is served (capability tokens, NOT `?path=`)
//! The trusted parent (shell), which alone holds the session capability, parses a
//! document, confines each asset reference through the §1 funnel, and **mints a
//! per-doc, short-TTL capability token** for the confined canonical path via
//! [`CapStore::mint`]. It rewrites the `<img>`/`<video>` `src` to this origin's
//! `GET /cap/<token>` URL ([`cap_url`]). The renderer therefore never sees a
//! filesystem path or the session cookie — only an opaque, expiring token. The
//! token alone is **never trusted**: at serve time the route RE-CONFINES the
//! resolved path against the registered roots and re-stats/size-caps the file, so
//! a stale token whose target has since left the roots (or grown huge) is
//! rejected.
//!
//! ## Headers
//! Every response carries `Cross-Origin-Resource-Policy: cross-origin` (so the
//! COEP shell/iframe can load it) and **no** CORS headers
//! (`Access-Control-Allow-Origin` is deliberately absent): the renderer can
//! *display* the bytes as an `<img>`/`<video>` but cross-origin canvas-taint
//! blocks `getImageData`/`toDataURL`, and the renderer's `connect-src 'none'`
//! blocks fetching them — visible to the human, opaque to script. Video / large
//! media supports HTTP **Range** (206 partial) so we never buffer a giant file.
//!
//! Daemon-only.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::confine;
use crate::roots;
use crate::{Confine, ConfineSnapshot};

/// Capability-token lifetime: ~5 minutes. Long enough to load a document's
/// assets (and seek around a video), short enough that a leaked token is inert
/// almost immediately. The token is also re-confined at serve time, so expiry is
/// only one of two independent gates.
pub const CAP_TTL_SECS: u64 = 5 * 60;

/// Bytes of CSPRNG entropy behind a capability token. 32 bytes = 256 bits, far
/// beyond brute-force for a loopback, short-lived secret. Mirrors the auth
/// nonce/session token sizing.
const TOKEN_BYTES: usize = 32;

// ===========================================================================
// Capability store
// ===========================================================================

/// One minted capability: the canonical absolute path it grants and the
/// unix-epoch second at which it expires.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cap {
    /// The confined canonical absolute path this token grants read access to.
    /// Single-doc scoped (the parent mints one token per asset reference).
    path: PathBuf,
    /// Unix-epoch second at which this token expires.
    expires_at: u64,
}

/// In-memory store of minted, not-yet-expired capability tokens.
///
/// `mint` returns an opaque token bound to one canonical path; `resolve` returns
/// that path iff the token exists and has not expired. Expired entries are swept
/// lazily on every `mint`/`resolve` (no background timer). The token is
/// high-entropy and single-doc scoped; it is **never** the only gate — the route
/// re-confines the resolved path before serving.
///
/// ## Injected clock (audit C §1g / §4)
/// Time comes from an injected [`crate::auth::Clock`], exactly like
/// [`crate::auth::NonceStore`] / `SessionStore`, so TTL expiry is
/// deterministically testable without touching the wall clock. Production builds
/// it with [`crate::auth::Clock::system`] ([`CapStore::new`]); tests pass a
/// closure over a shared counter ([`CapStore::with_clock`]).
pub struct CapStore {
    live: Vec<(String, Cap)>,
    /// The injected time source. Stored in millis (the [`Clock`] unit);
    /// capability TTLs are in seconds, so [`Self::now`] divides down.
    clock: crate::auth::Clock,
}

impl Default for CapStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CapStore {
    /// A fresh, empty store driven by the real wall clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(crate::auth::Clock::system())
    }

    /// A fresh, empty store driven by an injected `clock` (deterministic tests).
    #[must_use]
    pub fn with_clock(clock: crate::auth::Clock) -> Self {
        Self {
            live: Vec::new(),
            clock,
        }
    }

    /// Current unix-epoch time in **seconds**, from the injected clock (which is
    /// in millis). The TTL arithmetic is all in seconds, matching [`CAP_TTL_SECS`].
    fn now(&self) -> u64 {
        self.clock.now_secs()
    }

    /// Drop every capability whose TTL has elapsed at `now`.
    fn sweep(&mut self, now: u64) {
        self.live.retain(|(_, c)| c.expires_at > now);
    }

    /// Number of currently-stored (not-yet-swept) capabilities. Used by the
    /// server's render-isolation tests to assert that a floor-failed content
    /// request mints NOTHING (i.e. `cap_url` is unreachable off the floor).
    #[must_use]
    pub fn live_len(&self) -> usize {
        self.live.len()
    }

    /// Mint an opaque capability token for `canonical_path`, valid for `ttl`
    /// seconds from now (the injected clock). Returns the token string (embed it
    /// in a `/cap/<token>` URL). `canonical_path` MUST already be a confined
    /// canonical absolute path (the caller resolves+confines first); the route
    /// re-confines it anyway.
    pub fn mint(
        &mut self,
        canonical_path: PathBuf,
        ttl: u64,
    ) -> Result<String, getrandom::Error> {
        let now = self.now();
        self.sweep(now);
        let mut buf = [0u8; TOKEN_BYTES];
        getrandom::getrandom(&mut buf)?;
        // URL- and path-safe base64 (no padding) — same alphabet as the auth
        // tokens, so the token is safe in a `/cap/<token>` URL with no escaping
        // (never `+`, `/`, or `=`). Uses the already-present `base64` crate.
        let token = URL_SAFE_NO_PAD.encode(buf);
        self.live.push((
            token.clone(),
            Cap {
                path: canonical_path,
                expires_at: now.saturating_add(ttl),
            },
        ));
        Ok(token)
    }

    /// Resolve `token` to its granted canonical path iff it exists and is not
    /// expired now (the injected clock). Unknown or expired → `None`. Token
    /// comparison goes through [`crate::auth::ct_eq_str`] (the mandated
    /// constant-time chokepoint) so a timing side-channel cannot leak how much of
    /// a guessed token matched. Tokens are reusable within their TTL (a browser
    /// re-requests an `<img>` and a video range-streams over many requests), so
    /// `resolve` does **not** burn.
    #[must_use]
    pub fn resolve(&mut self, token: &str) -> Option<PathBuf> {
        let now = self.now();
        self.sweep(now);
        self.live
            .iter()
            .find(|(t, c)| crate::auth::ct_eq_str(t, token) && c.expires_at > now)
            .map(|(_, c)| c.path.clone())
    }

    /// Check whether `token` appears in the store at all, **without sweeping or
    /// expiry checking**. Used by [`resolve_target`] to distinguish a known-but-
    /// expired token (→ 410 Gone) from a never-issued / already-swept token
    /// (→ 404). Does NOT mutate the live set, so it cannot accidentally remove
    /// tokens in the presence of concurrent requests. Comparison goes through
    /// [`crate::auth::ct_eq_str`] (constant-time), matching [`Self::resolve`].
    fn contains_token(&self, token: &str) -> bool {
        self.live
            .iter()
            .any(|(t, _)| crate::auth::ct_eq_str(t, token))
    }
}

// ===========================================================================
// Re-confinement funnel (the route never trusts a token's path alone)
// ===========================================================================

/// A cloneable handle that re-runs the §1 confinement funnel for the capability
/// route, so a resolved-but-stale token can never serve a path that has since
/// left the registered roots. Wraps the same [`roots::Roots`] registry the shell
/// origin uses (shared `Mutex`) plus an optional primary-root fallback,
/// mirroring [`crate::server`]'s `confine_abs`.
#[derive(Clone)]
pub struct Confiner {
    roots: std::sync::Arc<Mutex<roots::Roots>>,
    /// Primary root fallback for the empty-registry / single-root case (matches
    /// `AppState::confine_abs`); `None` for the file-less always-on daemon.
    primary_root: Option<PathBuf>,
}

impl Confiner {
    /// Build a confiner sharing `roots` (the authoritative registry) with the
    /// optional `primary_root` fallback used when the registry is empty.
    #[must_use]
    pub fn new(
        roots: std::sync::Arc<Mutex<roots::Roots>>,
        primary_root: Option<PathBuf>,
    ) -> Self {
        Self {
            roots,
            primary_root,
        }
    }

}

/// Re-confine an (already canonical) absolute `candidate` through the unified §1
/// funnel and return the **held descriptor**, riding the single [`Confine`]
/// fan-out (ADR-0008 Phase 2). `Confiner::confine_read` is now the trait's
/// provided method over this snapshot; the *decision* is unchanged from the prior
/// inherent method — same registered-roots union, same empty-registry
/// `primary_root` fallback, same denylist, same primitive.
///
/// This is the TOCTOU-free path (audit HIGH-2): `confine_read` canonicalizes,
/// applies the sensitive denylist + containment, then **opens the canonical path
/// exactly once with `O_NOFOLLOW`** and **`fstat`s that same fd** for the size
/// cap and permission mode. The caller serves bytes from the returned
/// [`ConfinedFile::file`] and reads the floor from [`ConfinedFile::metadata`] —
/// never a second `stat` or re-`open` by path — so a symlink swapped in after
/// canonicalize cannot redirect the read, and a mode tightened after minting is
/// observed on the very fd the bytes come from.
impl Confine for Confiner {
    /// Snapshot the active root union (+ the empty-registry `primary_root`
    /// fallback) and the registry for the denylist. The registry `Mutex` is held
    /// only for the synchronous snapshot — never across the held fd's later reads
    /// or an `.await`.
    fn confinement_snapshot(&self, _candidate: &Path) -> ConfineSnapshot {
        // Snapshot the active root union (cloned), dropping the lock immediately.
        let mut union: Vec<roots::Root> = {
            let reg = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reg.union().into_iter().cloned().collect()
        };

        // Empty registry: synthesize a Directory root from the primary-root
        // fallback so the funnel has something to confine against (matches
        // `AppState`'s single-root behavior). With no primary root (file-less
        // daemon) the union stays empty and the funnel denies — the denylist is
        // still applied unconditionally by `confine_path`. A *sensitive* primary
        // root is still placed here and rejected by that denylist first (audit
        // MED-4), preserving the prior behavior exactly.
        if union.is_empty()
            && let Some(root) = self.primary_root.clone()
        {
            union.push(roots::Root {
                kind: roots::RootKind::Directory,
                path: root,
                last_used: SystemTime::now(),
            });
        }

        // Clone the registry for the denylist consult (its `is_sensitive` reads
        // only the injected `home`); the lock drops here.
        let registry = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        ConfineSnapshot { union, registry }
    }
}

// ===========================================================================
// Mint-URL helper (rewrite integration is W6; just expose minting now)
// ===========================================================================

/// Mint a capability and return the full `http://127.0.0.1:<port>/cap/<token>`
/// URL for a **confined canonical** asset path on the static origin.
///
/// `static_base` is the static origin base URL (e.g. `http://127.0.0.1:8081`);
/// `confined` MUST be a path already resolved+confined by the caller (the route
/// re-confines regardless). Returns `None` only on a CSPRNG failure. The
/// `<img>`/`<video>` rewrite that calls this is wired in W6; this step
/// only exposes the minting surface.
pub fn cap_url(
    store: &Mutex<CapStore>,
    static_base: &str,
    confined: PathBuf,
) -> Option<String> {
    let token = {
        let mut s = store.lock().ok()?;
        s.mint(confined, CAP_TTL_SECS).ok()?
    };
    Some(format!("{}/cap/{token}", static_base.trim_end_matches('/')))
}

// ===========================================================================
// warp route: GET /cap/<token>
// ===========================================================================

/// Build the capability-asset route: `GET /cap/<token>` → the confined file's
/// bytes, read-only, CORP-tagged, CORS-less, with HTTP Range support.
///
/// Pipeline (every gate independent of the token's mere existence):
///   1. resolve `token` against the cap-store → stored path (unknown → **404**,
///      known-but-expired → **410 Gone**),
///   2. **re-confine** that path through the unified §1 funnel
///      ([`confine::confine_read`]), which opens the canonical path ONCE with
///      `O_NOFOLLOW` and `fstat`s that held fd — escape / sensitive denylist /
///      non-regular / symlinked-final / missing → **404**, oversize → **413**.
///      This is TOCTOU-free: the floor and the served bytes come from the SAME
///      descriptor, never a re-`open` by path (audit HIGH-2),
///   3. Track-A floor on the held fd's fstat'd mode: must be world-readable
///      (stale token for a now-private file → **403**),
///   4. serve the bytes from the held descriptor with an extension-inferred
///      `Content-Type`, `Cross-Origin-Resource-Policy: cross-origin`, and **no**
///      CORS header; a `Range` request streams a **206** partial slice (bad
///      range → **416**).
///
/// SVG is served as `image/svg+xml` (the renderer embeds it as `<img>`, so its
/// external refs are inert under the renderer CSP). A resolve/confine failure is
/// a flat 404 (no existence leak); a genuinely expired-but-known token surfaces
/// as **410 Gone** so the parent can re-mint.
pub fn asset_origin_routes(
    store: std::sync::Arc<Mutex<CapStore>>,
    confiner: Confiner,
) -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    use warp::Filter;
    warp::path("cap")
        .and(warp::get())
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::header::optional::<String>("range"))
        .map(move |token: String, range: Option<String>| {
            serve_cap(&store, &confiner, &token, range.as_deref())
        })
}

/// The outcome of resolving a capability token against the store, separating a
/// never-existed/swept token (a flat 404) from a known-but-expired one (410).
/// Re-confinement + the held-fd open happen *after* this, in [`serve_cap`].
enum CapTarget {
    /// Resolved to this stored canonical path (still to be re-confined at serve).
    Ok(PathBuf),
    /// The token is known but its TTL has elapsed — `410 Gone` (re-mint).
    Expired,
    /// Unknown / already-swept token — flat `404` (no existence leak).
    NotFound,
}

/// Resolve `token` to its stored path against the cap-store ONLY. The actual
/// re-confinement (and the single `O_NOFOLLOW` open of the held fd) is deferred
/// to [`serve_cap`] so the floor and the served bytes come from the SAME fd
/// (audit HIGH-2). The store `Mutex` is dropped before any filesystem work.
fn resolve_target(store: &Mutex<CapStore>, token: &str) -> CapTarget {
    let Ok(mut s) = store.lock() else {
        return CapTarget::NotFound;
    };
    // Distinguish "known but expired" (410) from "unknown" (404).
    // Check token presence WITHOUT sweeping first (contains_token is a raw
    // iter, no mutation), then resolve with current time. If the token is
    // present but not live, it must be expired → 410. Using contains_token
    // (non-sweeping) avoids a race where a concurrent request's sweep has
    // already evicted the expired token before this check, which would
    // falsely return 404 for a known-but-expired token.
    let exists = s.contains_token(token);
    match s.resolve(token) {
        Some(p) => CapTarget::Ok(p),
        None if exists => CapTarget::Expired,
        None => CapTarget::NotFound,
    }
}

/// Serve one capability asset (shared by the route + reusable in tests).
fn serve_cap(
    store: &Mutex<CapStore>,
    confiner: &Confiner,
    token: &str,
    range: Option<&str>,
) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    let status = |code| warp::reply::with_status("", code).into_response();

    // 1. Resolve the token against the cap-store only (410 vs 404).
    let stored = match resolve_target(store, token) {
        CapTarget::Ok(p) => p,
        CapTarget::Expired => return status(StatusCode::GONE),
        CapTarget::NotFound => return status(StatusCode::NOT_FOUND),
    };

    // 2. Re-confine through the unified funnel, which opens the canonical path
    //    EXACTLY ONCE with O_NOFOLLOW and fstat's that held fd (audit HIGH-2:
    //    no separate `metadata` + `read`/`open` path syscalls). The denylist is
    //    applied unconditionally inside confine_path, which also closes the
    //    legacy denylist gap. Mapping mirrors `serve_asset`:
    //      escape / sensitive / not-a-regular-file / O_NOFOLLOW-refused → 404,
    //      oversize (fstat'd len) → 413.
    let confined = match confiner.confine_read(&stored) {
        Ok(c) => c,
        Err(confine::ConfineError::TooLarge { .. }) => {
            return status(StatusCode::PAYLOAD_TOO_LARGE)
        }
        // Escape, sensitive, non-absolute, missing, symlinked-final-component
        // (ELOOP), or non-regular file: a flat 404, no existence leak.
        Err(_) => return status(StatusCode::NOT_FOUND),
    };

    // 3. Track-A floor on the SAME fstat'd metadata the bytes come from. A token
    //    is minted only from a floor-passed context (W6 mint-time contract), but
    //    if the mode was tightened after minting we must not serve the stale
    //    token. Because this reads the held fd's fstat — not a fresh path stat —
    //    a swap between mint and serve cannot smuggle a private file past it.
    if !crate::auth::is_world_readable(crate::auth::mode_of(&confined.metadata)) {
        return status(StatusCode::FORBIDDEN);
    }

    let total = confined.metadata.len();
    let content_type = content_type_for(&confined.canonical);

    // 4. Serve bytes from the HELD fd (seek/read on the handle) — never re-open
    //    by path. Both the full 200 and the Range 206 read from `confined.file`.
    match range.and_then(|r| parse_range(r, total)) {
        // A Range header that is present but unsatisfiable → 416.
        _ if range.is_some() && parse_range(range.unwrap_or(""), total).is_none() => {
            range_not_satisfiable(total)
        }
        Some((start, end)) => serve_range(confined.file, start, end, total, content_type),
        None => serve_full(confined.file, content_type),
    }
}

/// Serve the whole file (200) with CORP + Accept-Ranges, no CORS. Reads from the
/// **held descriptor** opened once through the funnel (audit HIGH-2) — never a
/// re-`open` by path — so the bytes are exactly those the floor was checked on.
fn serve_full(mut file: std::fs::File, content_type: &'static str) -> warp::reply::Response {
    use std::io::Read as _;
    use warp::http::StatusCode;
    use warp::reply::Reply;
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return warp::reply::with_status("", StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    let mut resp = warp::reply::Response::new(bytes.into());
    *resp.status_mut() = StatusCode::OK;
    decorate(resp.headers_mut(), content_type);
    insert(resp.headers_mut(), "accept-ranges", "bytes");
    resp
}

/// Serve a byte range `[start, end]` (inclusive) as a 206 partial response. We
/// read only the requested slice so a giant video never buffers in full.
fn serve_range(
    mut file: std::fs::File,
    start: u64,
    end: u64,
    total: u64,
    content_type: &'static str,
) -> warp::reply::Response {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use warp::http::StatusCode;
    use warp::reply::Reply;

    // Seek/read on the HELD fd from `serve_cap` (audit HIGH-2): never re-open
    // `canon` by path, so a swap after the funnel's open cannot redirect bytes.
    if file.seek(SeekFrom::Start(start)).is_err() {
        return warp::reply::with_status("", StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }
    let len = end - start + 1;
    let mut buf = vec![0u8; len as usize];
    if file.read_exact(&mut buf).is_err() {
        return warp::reply::with_status("", StatusCode::INTERNAL_SERVER_ERROR).into_response();
    }

    let mut resp = warp::reply::Response::new(buf.into());
    *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
    let headers = resp.headers_mut();
    decorate(headers, content_type);
    insert(headers, "accept-ranges", "bytes");
    insert(headers, "content-range", &format!("bytes {start}-{end}/{total}"));
    resp
}

/// A `416 Range Not Satisfiable` carrying the authoritative `Content-Range` size.
fn range_not_satisfiable(total: u64) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;
    let mut resp = warp::reply::with_status("", StatusCode::RANGE_NOT_SATISFIABLE).into_response();
    insert(resp.headers_mut(), "content-range", &format!("bytes */{total}"));
    insert(resp.headers_mut(), "cross-origin-resource-policy", "cross-origin");
    resp
}

/// Apply the headers EVERY capability response carries: the inferred
/// `Content-Type` and `Cross-Origin-Resource-Policy: cross-origin`. Deliberately
/// **no** `Access-Control-Allow-Origin` (canvas-taint depends on its absence).
fn decorate(headers: &mut warp::http::HeaderMap, content_type: &str) {
    insert(headers, "content-type", content_type);
    insert(headers, "cross-origin-resource-policy", "cross-origin");
}

/// Insert a header, ignoring an (impossible for these values) parse error rather
/// than panicking.
fn insert(headers: &mut warp::http::HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = warp::http::HeaderValue::from_str(value) {
        headers.insert(name, v);
    }
}

/// Parse a single-range `Range: bytes=START-END` header against a `total` size,
/// returning the inclusive `[start, end]` to serve. Supports `bytes=a-b`,
/// `bytes=a-` (to EOF), and `bytes=-n` (last n bytes). Returns `None` for a
/// malformed, multi-range, or unsatisfiable spec (caller maps that to 416). Only
/// the first range of a comma list is honored (single-range is enough for media
/// playback; multipart byteranges are intentionally unsupported).
fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    // Honor only a single range; reject (None) a multi-range request.
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());

    if a.is_empty() {
        // Suffix range: last `b` bytes.
        let n: u64 = b.parse().ok()?;
        if n == 0 || total == 0 {
            return None;
        }
        let n = n.min(total);
        return Some((total - n, total - 1));
    }

    let start: u64 = a.parse().ok()?;
    if start >= total {
        return None; // unsatisfiable
    }
    let end = if b.is_empty() {
        total - 1
    } else {
        b.parse::<u64>().ok()?.min(total - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

/// Infer a `Content-Type` from the path extension. Mirrors the shell origin's
/// `serve_asset` mapping (kept in sync); SVG → `image/svg+xml` (embedded as
/// `<img>`), video/audio carry seekable media types.
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

// ===========================================================================
// Tests (network-free: an in-memory store, a real Roots over a tempdir)
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::SystemTime;

    /// A settable mock clock (millis) and the [`CapStore`] driven by it, so TTL
    /// expiry is exercised deterministically without the wall clock (audit C §4).
    /// The returned handle lets a test advance time between `mint` and `resolve`.
    fn store_with_mock_clock() -> (Arc<std::sync::Mutex<u64>>, CapStore) {
        let millis = Arc::new(std::sync::Mutex::new(0u64));
        let m = Arc::clone(&millis);
        let clock = crate::auth::Clock::new(move || *m.lock().unwrap_or_else(|e| e.into_inner()));
        (millis, CapStore::with_clock(clock))
    }

    /// Advance the mock clock to `secs` seconds (it is stored in millis).
    fn set_secs(millis: &std::sync::Mutex<u64>, secs: u64) {
        *millis.lock().unwrap_or_else(|e| e.into_inner()) = secs.saturating_mul(1_000);
    }

    /// A canonicalized tempdir registered as a single root, plus a `Confiner`
    /// over it (empty-registry fallback also covered by `primary_root`).
    fn confiner_over(dir: &Path) -> Confiner {
        let mut reg = roots::Roots::new("/nonexistent-home-for-tests");
        let _ = reg.register_root(dir, SystemTime::now());
        Confiner::new(Arc::new(Mutex::new(reg)), Some(dir.to_path_buf()))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("md-preview-cap-{}-{}-{}", tag, std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    // ---- capability store ----------------------------------------------------

    #[test]
    fn mint_then_resolve_round_trips() {
        let (millis, mut store) = store_with_mock_clock();
        set_secs(&millis, 1000);
        let p = PathBuf::from("/some/canonical/pic.png");
        let token = store.mint(p.clone(), CAP_TTL_SECS).expect("mint");
        set_secs(&millis, 1001);
        assert_eq!(store.resolve(&token), Some(p));
    }

    /// TTL expiry via the INJECTED clock (audit C §4): a token live just before
    /// its expiry second and gone just after, driven entirely by advancing the
    /// mock clock — no wall-clock dependency.
    #[test]
    fn expired_token_resolves_to_none() {
        let (millis, mut store) = store_with_mock_clock();
        set_secs(&millis, 1000);
        let token = store.mint(PathBuf::from("/x"), CAP_TTL_SECS).expect("mint");

        // Still live one second before expiry.
        set_secs(&millis, 1000 + CAP_TTL_SECS - 1);
        assert_eq!(store.resolve(&token), Some(PathBuf::from("/x")), "live pre-expiry");

        // Gone the second after the TTL elapses.
        set_secs(&millis, 1000 + CAP_TTL_SECS + 1);
        assert_eq!(store.resolve(&token), None, "expired post-TTL");
    }

    #[test]
    fn unknown_token_resolves_to_none() {
        let (millis, mut store) = store_with_mock_clock();
        set_secs(&millis, 1000);
        let _ = store.mint(PathBuf::from("/x"), CAP_TTL_SECS).expect("mint");
        set_secs(&millis, 1001);
        assert_eq!(store.resolve("not-a-real-token"), None);
    }

    #[test]
    fn tokens_are_unguessable_and_distinct() {
        let (millis, mut store) = store_with_mock_clock();
        set_secs(&millis, 1);
        let a = store.mint(PathBuf::from("/a"), CAP_TTL_SECS).expect("a");
        let b = store.mint(PathBuf::from("/b"), CAP_TTL_SECS).expect("b");
        assert_ne!(a, b, "CSPRNG tokens must differ");
        assert!(a.len() >= 40, "256-bit base64url token ≈ 43 chars");
    }

    // ---- re-confinement ------------------------------------------------------

    #[test]
    fn resolved_path_is_reconfined_outside_root_rejected() {
        let dir = temp_dir("reconf");
        std::fs::write(dir.join("ok.png"), b"PNG").unwrap();
        let confiner = confiner_over(&dir);
        let store = Mutex::new(CapStore::new());

        // A token whose path is OUTSIDE the root is rejected at serve (404),
        // even though the token itself resolves — the route never trusts a token.
        let outside = PathBuf::from("/etc/passwd");
        let token = store
            .lock()
            .unwrap()
            .mint(outside, CAP_TTL_SECS)
            .unwrap();
        let resp = serve_cap(&store, &confiner, &token, None);
        assert_eq!(resp.status(), warp::http::StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- route surface -------------------------------------------------------

    #[tokio::test]
    async fn cap_valid_serves_200_with_content_type_corp_and_no_cors() {
        let dir = temp_dir("serve-ok");
        let bytes: &[u8] = b"\x89PNG\r\n\x1a\nfake";
        std::fs::write(dir.join("pic.png"), bytes).unwrap();
        // Make world-readable so Track-A floor passes.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            dir.join("pic.png"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let confiner = confiner_over(&dir);
        let store = Arc::new(Mutex::new(CapStore::new()));
        let token = store
            .lock()
            .unwrap()
            .mint(dir.join("pic.png"), CAP_TTL_SECS)
            .unwrap();

        let routes = asset_origin_routes(store, confiner);
        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
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
            "capability assets must be CORS-less (canvas-taint depends on it)"
        );
        assert_eq!(resp.body().as_ref(), bytes);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cap_unknown_is_404() {
        let dir = temp_dir("unknown");
        std::fs::write(dir.join("f.png"), b"x").unwrap();
        let confiner = confiner_over(&dir);
        let store = Arc::new(Mutex::new(CapStore::new()));
        let routes = asset_origin_routes(store, confiner);

        let unknown = warp::test::request()
            .method("GET")
            .path("/cap/totally-unknown-token")
            .reply(&routes)
            .await;
        assert_eq!(unknown.status(), 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cap_expired_is_410() {
        // A token minted with ttl=0 in the past is "known but expired" → 410.
        // Use a separate store to avoid sweep interactions with the 404 test.
        let dir = temp_dir("expired");
        std::fs::write(dir.join("f.png"), b"x").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            dir.join("f.png"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let confiner = confiner_over(&dir);
        let store = Arc::new(Mutex::new(CapStore::new()));
        // Mint with ttl=0: expires_at == now, and resolve requires expires_at >
        // now, so the token is "known but expired" the instant it is minted.
        let expired = store
            .lock()
            .unwrap()
            .mint(dir.join("f.png"), 0)
            .unwrap();

        let routes = asset_origin_routes(store, confiner);
        let gone = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{expired}"))
            .reply(&routes)
            .await;
        assert_eq!(gone.status(), 410);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cap_range_returns_206_correct_slice() {
        let dir = temp_dir("range");
        let body: &[u8] = b"0123456789abcdef"; // 16 bytes
        std::fs::write(dir.join("v.mp4"), body).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            dir.join("v.mp4"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let confiner = confiner_over(&dir);
        let store = Arc::new(Mutex::new(CapStore::new()));
        let token = store
            .lock()
            .unwrap()
            .mint(dir.join("v.mp4"), CAP_TTL_SECS)
            .unwrap();

        let routes = asset_origin_routes(store, confiner);
        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .header("range", "bytes=4-7")
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.headers().get("content-range").unwrap(), "bytes 4-7/16");
        assert_eq!(
            resp.headers().get("cross-origin-resource-policy").unwrap(),
            "cross-origin"
        );
        assert_eq!(resp.body().as_ref(), b"4567");

        // An unsatisfiable range → 416 with the authoritative size.
        let bad = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .header("range", "bytes=99-200")
            .reply(&routes)
            .await;
        assert_eq!(bad.status(), 416);
        assert_eq!(bad.headers().get("content-range").unwrap(), "bytes */16");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cap_oversize_is_413() {
        let dir = temp_dir("big");
        let big = vec![b'x'; (confine::DEFAULT_MAX_FILE_SIZE + 1) as usize];
        std::fs::write(dir.join("big.bin"), &big).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            dir.join("big.bin"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let confiner = confiner_over(&dir);
        let store = Arc::new(Mutex::new(CapStore::new()));
        let token = store
            .lock()
            .unwrap()
            .mint(dir.join("big.bin"), CAP_TTL_SECS)
            .unwrap();

        let routes = asset_origin_routes(store, confiner);
        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 413);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_world_readable_asset_is_forbidden() {
        // Track-A hardening: a token for a now-private (mode 0o600) file is denied
        // even though the token itself is valid and the path confines.
        let dir = temp_dir("floor");
        std::fs::write(dir.join("secret.png"), b"\x89PNG").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            dir.join("secret.png"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let confiner = confiner_over(&dir);
        let store = Arc::new(Mutex::new(CapStore::new()));
        let token = store
            .lock()
            .unwrap()
            .mint(dir.join("secret.png"), CAP_TTL_SECS)
            .unwrap();

        let routes = asset_origin_routes(store, confiner);
        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 403, "non-world-readable asset must be denied");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cap_toctou_path_swapped_to_outside_symlink_is_not_served() {
        // Audit HIGH-2 regression: mint a token for a real in-root world-readable
        // file, then SWAP that path's final component to a symlink pointing at a
        // private file OUTSIDE the root (the classic stat-then-reopen TOCTOU).
        // Because serve_cap routes through confine_read — which opens the
        // canonical path ONCE with O_NOFOLLOW and confines the canonicalized
        // result — the swapped symlink is refused (ELOOP on open AND/OR escape on
        // confine): the private content is NEVER served.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("toctou-sym");
        let outside = temp_dir("toctou-sym-out");
        let secret = outside.join("secret.png");
        std::fs::write(&secret, b"TOPSECRET-PRIVATE-BYTES").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();

        let asset = dir.join("pic.png");
        std::fs::write(&asset, b"public").unwrap();
        std::fs::set_permissions(&asset, std::fs::Permissions::from_mode(0o644)).unwrap();

        let confiner = confiner_over(&dir);
        let store = Arc::new(Mutex::new(CapStore::new()));
        let token = store
            .lock()
            .unwrap()
            .mint(asset.clone(), CAP_TTL_SECS)
            .unwrap();

        // THE SWAP: replace the in-root file with a symlink to the outside secret
        // between mint and serve.
        std::fs::remove_file(&asset).unwrap();
        std::os::unix::fs::symlink(&secret, &asset).unwrap();

        let routes = asset_origin_routes(store, confiner);
        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .reply(&routes)
            .await;
        assert_ne!(resp.status(), 200, "must not serve the swapped-in target");
        assert_ne!(
            resp.body().as_ref(),
            b"TOPSECRET-PRIVATE-BYTES",
            "private outside-root bytes must never be served on the stale token"
        );
        // The O_NOFOLLOW open refuses the symlinked final component → flat 404.
        assert_eq!(resp.status(), 404);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn cap_toctou_mode_tightened_after_mint_is_forbidden() {
        // Audit HIGH-2 regression (floor arm): mint for a world-readable file,
        // then tighten its mode to 0o600 before serve. The floor is read from the
        // held fd's fstat (same descriptor the bytes would come from), so the
        // now-private file is denied with 403 — the swap cannot smuggle it past.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("toctou-mode");
        let asset = dir.join("pic.png");
        std::fs::write(&asset, b"\x89PNGsecret").unwrap();
        std::fs::set_permissions(&asset, std::fs::Permissions::from_mode(0o644)).unwrap();

        let confiner = confiner_over(&dir);
        let store = Arc::new(Mutex::new(CapStore::new()));
        let token = store
            .lock()
            .unwrap()
            .mint(asset.clone(), CAP_TTL_SECS)
            .unwrap();

        // THE SWAP: tighten the mode between mint and serve.
        std::fs::set_permissions(&asset, std::fs::Permissions::from_mode(0o600)).unwrap();

        let routes = asset_origin_routes(store, confiner);
        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 403, "floor on the held fd must deny the now-private file");
        assert_ne!(resp.body().as_ref(), b"\x89PNGsecret");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cap_sensitive_world_readable_path_is_rejected() {
        // Denylist-gap regression (finding #2): even a WORLD-READABLE file on the
        // sensitive denylist (e.g. under the registry home's `.ssh`) must be
        // rejected — confine_read applies `is_sensitive` unconditionally, which
        // the legacy FilePeer::within funnel did not. We register `.ssh` as a
        // root AND set the registry home so it is denylisted, then confirm the
        // (world-readable) key cannot be served.
        use std::os::unix::fs::PermissionsExt as _;
        let home = temp_dir("sensitive-home");
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let key = ssh.join("id_rsa");
        std::fs::write(&key, b"PRIVATE-KEY-EVEN-IF-CHMOD-644").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Build a confiner whose registry home is the tempdir home (so `.ssh` is
        // sensitive) and whose root is `.ssh` itself (so containment would pass).
        let mut reg = roots::Roots::new(&home);
        let _ = reg.register_root(&ssh, SystemTime::now());
        let confiner = Confiner::new(Arc::new(Mutex::new(reg)), Some(ssh.clone()));

        let store = Arc::new(Mutex::new(CapStore::new()));
        let token = store
            .lock()
            .unwrap()
            .mint(key.clone(), CAP_TTL_SECS)
            .unwrap();

        let routes = asset_origin_routes(store, confiner);
        let resp = warp::test::request()
            .method("GET")
            .path(&format!("/cap/{token}"))
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 404, "sensitive denylist must reject even a world-readable key");
        assert_ne!(resp.body().as_ref(), b"PRIVATE-KEY-EVEN-IF-CHMOD-644");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn svg_infers_svg_content_type() {
        assert_eq!(content_type_for(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(content_type_for(Path::new("v.mp4")), "video/mp4");
        assert_eq!(
            content_type_for(Path::new("x.unknown")),
            "application/octet-stream"
        );
    }

    // ---- range parser --------------------------------------------------------

    #[test]
    fn parse_range_handles_forms_and_rejects_bad() {
        assert_eq!(parse_range("bytes=0-3", 10), Some((0, 3)));
        assert_eq!(parse_range("bytes=4-", 10), Some((4, 9))); // to EOF
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 9))); // suffix
        assert_eq!(parse_range("bytes=5-100", 10), Some((5, 9))); // clamp end
        assert_eq!(parse_range("bytes=20-30", 10), None); // start past EOF
        assert_eq!(parse_range("bytes=0-1,4-5", 10), None); // multi-range
        assert_eq!(parse_range("items=0-1", 10), None); // wrong unit
        assert_eq!(parse_range("bytes=", 10), None); // empty
    }

    // ---- mint-URL helper -----------------------------------------------------

    #[test]
    fn cap_url_builds_full_static_origin_url() {
        let store = Mutex::new(CapStore::new());
        let url =
            cap_url(&store, "http://127.0.0.1:8081", PathBuf::from("/r/pic.png")).expect("url");
        assert!(url.starts_with("http://127.0.0.1:8081/cap/"));
        // Trailing slash on the base is normalized away (no `//cap`).
        let url2 =
            cap_url(&store, "http://127.0.0.1:8081/", PathBuf::from("/r/pic.png")).expect("url");
        assert!(url2.starts_with("http://127.0.0.1:8081/cap/"));
        assert!(!url2.contains("8081//cap"));
    }
}
