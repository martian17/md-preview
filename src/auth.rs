//! Localhost trust & auth primitives for the always-on preview daemon.
//!
//! This module implements the security model from
//! `design/always-on-secure-preview.md` §2 and `ADR-0006`. It is the
//! *primitive* layer: the route enforcement (threading the `authenticated`
//! bit, calling [`floor_deny`], gating `/ws` & `/collab`) is wired in a later
//! wave (W3); this module provides the correct, hardened building blocks and
//! their tests.
//!
//! ## Threat model
//! The dangerous callers are *lower-privilege than the user* — a malicious web
//! page (incl. DNS rebinding), another local user (loopback TCP is host-wide),
//! a sandboxed process. A same-uid process is **not** in scope. Every primitive
//! here targets that confused-deputy problem.
//!
//! ## Hardening baked in
//! - **Constant-time comparison of all secrets.** Nonces, session tokens, and
//!   any other bearer secret are compared with [`subtle::ConstantTimeEq`] —
//!   never `==`. Plain `==` short-circuits on the first differing byte and
//!   leaks length/prefix information through timing. See [`ct_eq`].
//! - **CSPRNG entropy.** All tokens/nonces come from [`getrandom`] (the OS
//!   CSPRNG), encoded URL-safe-no-pad. 256 bits each.
//! - **Injected clock.** No primitive reads the wall clock directly; a [`Clock`]
//!   closure is threaded in, so TTL/sliding-expiry logic is deterministically
//!   testable.
//! - **No `unwrap`/`expect`/`panic`** in production paths.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use subtle::ConstantTimeEq;

// ---------------------------------------------------------------------------
// Clock injection
// ---------------------------------------------------------------------------

/// An injected "now" in **milliseconds** since an arbitrary but fixed epoch.
/// We use `u64` millis (not [`std::time::Instant`]) so the value is trivially
/// injectable in tests and serialisable for persistence.
pub type Millis = u64;

/// An injected clock. Production wires this to the real wall clock; tests pass
/// a closure over a shared counter for deterministic TTL testing.
///
/// Kept as a boxed closure (rather than a trait-object zoo) so the stores own
/// their time source and no logic ever touches the real clock directly.
pub struct Clock(Box<dyn Fn() -> Millis + Send + Sync>);

impl Clock {
    /// Build a clock from any `Fn() -> Millis`.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn() -> Millis + Send + Sync + 'static,
    {
        Clock(Box::new(f))
    }

    /// Current time in milliseconds.
    fn now(&self) -> Millis {
        (self.0)()
    }

    /// A clock backed by the real wall clock (`SystemTime`). Falls back to `0`
    /// if the system clock is before the Unix epoch (never panics).
    pub fn system() -> Self {
        Clock::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as Millis)
                .unwrap_or(0)
        })
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Clock(<injected>)")
    }
}

// ---------------------------------------------------------------------------
// Secret comparison & token generation
// ---------------------------------------------------------------------------

/// Constant-time equality for two byte slices.
///
/// This is the single chokepoint for **all** secret comparisons in the daemon
/// (nonces, session tokens, capability tokens). It uses
/// [`subtle::ConstantTimeEq`]; the comparison time depends only on the input
/// *lengths*, not their contents, so it leaks no byte-position information
/// through a timing side-channel. Never compare a secret with `==`.
///
/// Slices of differing length are unequal; lengths are not secret (the protocol
/// fixes the token width), so the early length check leaks nothing a caller
/// couldn't already observe.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Constant-time equality for two secret strings (UTF-8 bytes). See [`ct_eq`].
#[must_use]
pub fn ct_eq_str(a: &str, b: &str) -> bool {
    ct_eq(a.as_bytes(), b.as_bytes())
}

/// Number of random bytes behind every token/nonce (256 bits).
const TOKEN_BYTES: usize = 32;

/// Generate a fresh URL-safe-no-pad token from 256 bits of OS CSPRNG entropy.
///
/// Returns `Err` only if the OS entropy source fails (extremely rare; never
/// panics). Used for session tokens and bootstrap nonces alike.
pub fn generate_token() -> Result<String, getrandom::Error> {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut buf)?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

// ---------------------------------------------------------------------------
// NonceStore — single-use, ~5s-TTL bootstrap claim nonces
// ---------------------------------------------------------------------------

/// Default bootstrap-nonce time-to-live (~5 seconds, per §2 step 1).
pub const NONCE_TTL: Duration = Duration::from_millis(5_000);

#[derive(Clone)]
struct NonceEntry {
    value: String,
    /// Absolute millis after which the nonce is expired.
    expires_at: Millis,
}

/// A store of **single-use, short-TTL** bootstrap claim nonces.
///
/// Flow (design §2): the socket-authenticated CLI asks the daemon to
/// [`mint`](Self::mint) a nonce; the browser bootstrap page POSTs it to
/// `/claim`; the daemon calls [`verify_and_consume`](Self::verify_and_consume),
/// which validates **in constant time** and **burns** the nonce (single-use).
/// Expired or unknown nonces are rejected.
pub struct NonceStore {
    nonces: HashMap<String, NonceEntry>,
    ttl: Duration,
    clock: Clock,
}

impl NonceStore {
    /// New store with the default [`NONCE_TTL`].
    pub fn new(clock: Clock) -> Self {
        Self::with_ttl(clock, NONCE_TTL)
    }

    /// New store with an explicit TTL (used by tests).
    pub fn with_ttl(clock: Clock, ttl: Duration) -> Self {
        NonceStore {
            nonces: HashMap::new(),
            ttl,
            clock,
        }
    }

    /// Mint a fresh single-use nonce and arm it. Returns the nonce string to
    /// hand back over the socket. Lazily evicts expired entries first.
    pub fn mint(&mut self) -> Result<String, getrandom::Error> {
        let now = self.clock.now();
        self.evict_expired(now);
        let value = generate_token()?;
        let expires_at = now.saturating_add(self.ttl.as_millis() as Millis);
        self.nonces.insert(
            value.clone(),
            NonceEntry {
                value: value.clone(),
                expires_at,
            },
        );
        Ok(value)
    }

    /// Validate `candidate` against the armed nonces in **constant time**, and
    /// **consume** (burn) it on success. Returns `true` iff a live nonce
    /// matched. Single-use: a second call with the same value fails.
    ///
    /// We compare against every non-expired entry (rather than a hash-map
    /// lookup keyed on the secret — a keyed lookup would itself be a timing
    /// oracle on the key bytes).
    #[must_use]
    pub fn verify_and_consume(&mut self, candidate: &str) -> bool {
        let now = self.clock.now();
        let mut matched: Option<String> = None;
        for entry in self.nonces.values() {
            let live = entry.expires_at > now;
            if live && ct_eq_str(&entry.value, candidate) {
                matched = Some(entry.value.clone());
            }
        }
        match matched {
            Some(key) => {
                // Burn: single-use regardless of remaining TTL.
                self.nonces.remove(&key);
                true
            }
            None => false,
        }
    }

    /// Drop all expired nonces. Called lazily on `mint`; exposed for a periodic
    /// sweep by the daemon if desired.
    pub fn evict_expired(&mut self, now: Millis) {
        self.nonces.retain(|_, e| e.expires_at > now);
    }

    /// Number of currently-armed (not-yet-evicted) nonces. For tests/metrics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nonces.len()
    }

    /// Whether the store holds no armed nonces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nonces.is_empty()
    }
}

// ---------------------------------------------------------------------------
// SessionStore — sliding-expiry session tokens, persisted 0600
// ---------------------------------------------------------------------------

/// Default session sliding-expiry idle window (30 days). Active tabs renew on
/// activity and on each `md` run, so a live session never expires (§2).
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Clone)]
struct SessionEntry {
    value: String,
    /// Absolute millis after which the session is considered idle-expired.
    expires_at: Millis,
}

/// A store of **session tokens with sliding renewal**, persisted to disk `0600`.
///
/// Per §2/ADR-0006 there is a single per-origin token shared by all tabs;
/// validity slides on every authenticated request and on each `md` run.
/// Persistence keeps cookies valid across daemon restarts/reboots.
///
/// The on-disk format is a tiny line-oriented text file (`token<TAB>expires_ms`
/// per line) written atomically with `0600` perms — no third-party serializer
/// pulled in for two fields.
pub struct SessionStore {
    sessions: HashMap<String, SessionEntry>,
    ttl: Duration,
    clock: Clock,
}

impl SessionStore {
    /// New empty store with the default [`SESSION_TTL`].
    pub fn new(clock: Clock) -> Self {
        Self::with_ttl(clock, SESSION_TTL)
    }

    /// New empty store with an explicit TTL (used by tests).
    pub fn with_ttl(clock: Clock, ttl: Duration) -> Self {
        SessionStore {
            sessions: HashMap::new(),
            ttl,
            clock,
        }
    }

    /// Mint, arm, and return a fresh session token.
    pub fn issue(&mut self) -> Result<String, getrandom::Error> {
        let now = self.clock.now();
        self.evict_expired(now);
        let value = generate_token()?;
        let expires_at = now.saturating_add(self.ttl.as_millis() as Millis);
        self.sessions.insert(
            value.clone(),
            SessionEntry {
                value: value.clone(),
                expires_at,
            },
        );
        Ok(value)
    }

    /// Validate `candidate` in **constant time** and, on success, **slide** its
    /// expiry forward (renew-on-activity). Returns `true` iff a live session
    /// matched. Constant-time scan over entries (a keyed lookup would be a
    /// timing oracle on the token bytes).
    #[must_use]
    pub fn validate_and_renew(&mut self, candidate: &str) -> bool {
        let now = self.clock.now();
        let mut matched: Option<String> = None;
        for entry in self.sessions.values() {
            let live = entry.expires_at > now;
            if live && ct_eq_str(&entry.value, candidate) {
                matched = Some(entry.value.clone());
            }
        }
        match matched {
            Some(key) => {
                let new_exp = now.saturating_add(self.ttl.as_millis() as Millis);
                if let Some(e) = self.sessions.get_mut(&key) {
                    e.expires_at = new_exp;
                }
                true
            }
            None => false,
        }
    }

    /// Validate without renewing (read-only check). Constant-time.
    #[must_use]
    pub fn is_valid(&self, candidate: &str) -> bool {
        let now = self.clock.now();
        let mut hit = false;
        for entry in self.sessions.values() {
            let live = entry.expires_at > now;
            if live && ct_eq_str(&entry.value, candidate) {
                hit = true;
            }
        }
        hit
    }

    /// Explicitly revoke a session (deliberate rotation / logout).
    /// Constant-time match, then remove.
    pub fn revoke(&mut self, candidate: &str) {
        let mut matched: Option<String> = None;
        for entry in self.sessions.values() {
            if ct_eq_str(&entry.value, candidate) {
                matched = Some(entry.value.clone());
            }
        }
        if let Some(key) = matched {
            self.sessions.remove(&key);
        }
    }

    /// Drop all idle-expired sessions.
    pub fn evict_expired(&mut self, now: Millis) {
        self.sessions.retain(|_, e| e.expires_at > now);
    }

    /// Number of live (un-evicted) sessions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the store holds no sessions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Persist the (non-expired) sessions to `path` with `0600` permissions,
    /// written atomically via a temp file + rename in the same directory.
    ///
    /// Format: one `<token>\t<expires_ms>` line per session. Tokens are
    /// URL-safe-no-pad (no tab/newline), so the framing is unambiguous.
    pub fn persist(&self, path: &Path) -> io::Result<()> {
        let now = self.clock.now();
        let mut body = String::new();
        for e in self.sessions.values() {
            if e.expires_at > now {
                body.push_str(&e.value);
                body.push('\t');
                body.push_str(&e.expires_at.to_string());
                body.push('\n');
            }
        }
        write_private_atomic(path, body.as_bytes())
    }

    /// Load sessions from a file previously written by [`persist`](Self::persist).
    /// Expired entries are dropped on load. A missing file yields an empty
    /// store (not an error). Malformed lines are skipped (best-effort).
    pub fn load(path: &Path, clock: Clock) -> io::Result<Self> {
        Self::load_with_ttl(path, clock, SESSION_TTL)
    }

    /// Like [`load`](Self::load) with an explicit TTL.
    pub fn load_with_ttl(path: &Path, clock: Clock, ttl: Duration) -> io::Result<Self> {
        let mut store = SessionStore::with_ttl(clock, ttl);
        let contents = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(store),
            Err(e) => return Err(e),
        };
        let now = store.clock.now();
        for line in contents.lines() {
            let mut parts = line.splitn(2, '\t');
            let value = match parts.next() {
                Some(v) if !v.is_empty() => v.to_string(),
                _ => continue,
            };
            let expires_at = match parts.next().and_then(|s| s.parse::<Millis>().ok()) {
                Some(t) => t,
                None => continue,
            };
            if expires_at > now {
                store
                    .sessions
                    .insert(value.clone(), SessionEntry { value, expires_at });
            }
        }
        Ok(store)
    }
}

/// Write `bytes` to `path` atomically with owner-only (`0600`) permissions.
///
/// Writes to a sibling temp file created with mode `0600`, then renames over
/// the target (atomic on the same filesystem). On Unix the mode is enforced via
/// `OpenOptions::mode`; on other platforms the rename still happens (the daemon
/// is Linux-only, but we avoid a hard `#[cfg]` panic path).
fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use io::Write as _;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = match path.file_name() {
        Some(name) => {
            let mut t = name.to_os_string();
            t.push(".tmp");
            dir.join(t)
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session path has no file name",
            ));
        }
    };

    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }

    // Belt-and-suspenders: re-assert 0600 in case the temp file pre-existed
    // with a looser mode (`create` does not reset an existing file's perms).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }

    fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Network defenses — Host allowlist, Origin / Sec-Fetch checks
// ---------------------------------------------------------------------------

/// Returns `true` iff `host` (the `Host` request header value) is an allowed
/// loopback authority — the **DNS-rebinding defense** (§2). Only
/// `127.0.0.1[:port]` and `localhost[:port]` are accepted; any other name,
/// `0.0.0.0`, wildcard, IPv6, or suffix trick is rejected. The port, if
/// present, must be ASCII digits with no leading zero (so `:08080` and `:0080`
/// are rejected as canonicalisation tricks).
#[must_use]
pub fn host_allowed(host: &str) -> bool {
    let (name, port) = match host.rsplit_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (host, None),
    };
    if name != "127.0.0.1" && name != "localhost" {
        return false;
    }
    match port {
        None => true,
        Some(p) => valid_port(p),
    }
}

/// A port string is valid iff it is 1..=5 ASCII digits, parses in range, and
/// has no leading zero (rejecting `0080`-style equivalence tricks). `0` itself
/// is rejected (never a real listen port for the daemon).
fn valid_port(p: &str) -> bool {
    if p.is_empty() || p.len() > 5 {
        return false;
    }
    if !p.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if p.len() > 1 && p.starts_with('0') {
        return false;
    }
    match p.parse::<u32>() {
        Ok(n) => (1..=65_535).contains(&n),
        Err(_) => false,
    }
}

/// The outcome of an [`origin_allowed`] check, distinguishing "no usable Origin
/// present" (allowed by design for Origin-less GETs and the `Origin: null`
/// bootstrap POST) from an explicit cross-origin rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginCheck {
    /// Same-origin (or an acceptably Origin-less request) — allow.
    Allow,
    /// An `Origin`/`Sec-Fetch-Site` header was present and indicated a
    /// cross-origin / cross-site request — reject.
    Deny,
}

/// `Origin` + `Sec-Fetch-*` checks (§2 "always-on network defenses").
///
/// `origin` / `sec_fetch_site` are the corresponding header values, if present.
/// Policy:
/// - A present `Sec-Fetch-Site` must be `same-origin` or `none`; `same-site` /
///   `cross-site` → **Deny**.
/// - A present `Origin` must be an allowed loopback origin (`http://127.0.0.1`
///   / `http://localhost`, optional port), else **Deny**.
/// - A missing `Origin` is **allowed** (Origin-less GETs and `Origin: null`
///   bootstrap POSTs are legitimate); [`host_allowed`] is the real rebinding
///   stopgap, so this is genuine defense-in-depth. The literal string `"null"`
///   (opaque origin) is treated as "no usable Origin" → Allow, since the
///   bootstrap POST carries exactly that.
///
/// Note the scheme assumption: today the daemon is `http://` on loopback. The
/// scheme is matched explicitly so a future `https`/WAN exposure is a
/// deliberate edit, not an accident.
#[must_use]
pub fn origin_allowed(origin: Option<&str>, sec_fetch_site: Option<&str>) -> OriginCheck {
    if let Some(site) = sec_fetch_site {
        match site {
            "same-origin" | "none" => {}
            _ => return OriginCheck::Deny,
        }
    }
    match origin {
        None => OriginCheck::Allow,
        Some("null") => OriginCheck::Allow,
        Some(o) => {
            if origin_is_loopback(o) {
                OriginCheck::Allow
            } else {
                OriginCheck::Deny
            }
        }
    }
}

/// `true` iff `origin` is `http://127.0.0.1[:port]` or `http://localhost[:port]`.
fn origin_is_loopback(origin: &str) -> bool {
    let rest = match origin.strip_prefix("http://") {
        Some(r) => r,
        None => return false,
    };
    // Reject any path/query/userinfo — an origin is scheme+host+port only.
    if rest.contains('/') || rest.contains('@') {
        return false;
    }
    host_allowed(rest)
}

// ---------------------------------------------------------------------------
// Permission floor — defense in depth (§2)
// ---------------------------------------------------------------------------

/// Returns `true` iff `mode` (the canonical target's Unix permission bits) has
/// the **world-readable** bit (`o+r`, octal `0o004`) set.
///
/// The floor invariant (§2): "never serve what the caller couldn't already read
/// itself." A world-readable file is readable by every local user, so serving
/// it escalates nothing. Non-world-readable files require an authenticated
/// session (the token "unlocks your own private docs").
#[must_use]
pub fn is_world_readable(mode: u32) -> bool {
    mode & 0o004 != 0
}

/// Extract the Unix permission bits from filesystem metadata.
///
/// Per §2 this must be called on the **canonical target** (after symlink
/// resolution) and **re-stated at read time** (TOCTOU) — ideally on the very
/// `fstat` of the held descriptor the bytes are read from. This helper only
/// extracts the mode; the caller owns the "same fd" discipline (wired in W3).
///
/// On non-Unix platforms (the daemon is Linux-only) this returns `0`
/// (no world-read bit), i.e. the *safe* default of "requires auth".
#[must_use]
pub fn mode_of(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        metadata.mode()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

/// The permission-floor decision primitive.
///
/// Returns `true` iff serving a file with permission bits `mode` is permitted
/// for a caller whose authenticated state is `authenticated`:
/// - world-readable (`o+r`) → allowed **always** (even unauthenticated);
/// - non-world-readable → allowed **only** when `authenticated` (the session
///   token unlocks the user's own private docs).
#[must_use]
pub fn floor_allows(mode: u32, authenticated: bool) -> bool {
    is_world_readable(mode) || authenticated
}

/// The inverse of [`floor_allows`], named for the route-enforcement call site
/// ("deny if the floor is not met"). Returns `true` iff the request **must be
/// rejected** under the permission floor. W3 wires this into `/view`, `/asset`,
/// `/raw`, `/save`, `/ws`, `/collab`, and `/cap` on the `fstat` of the held fd.
#[must_use]
pub fn floor_deny(mode: u32, authenticated: bool) -> bool {
    !floor_allows(mode, authenticated)
}

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

/// Name of the per-origin session cookie (§2: one cookie shared by all tabs).
pub const SESSION_COOKIE_NAME: &str = "mdp_session";

/// Build a `Set-Cookie` header value carrying the session `token`.
///
/// Always emits `HttpOnly`, `SameSite=Strict`, `Path=/`. The `secure`
/// parameter controls the `Secure` attribute:
/// - `false` today (loopback `http://` — `Secure` would prevent the cookie from
///   ever being sent over plain http);
/// - `true` for a future `https`/WAN exposure.
///
/// The option is deliberately a parameter (not hardcoded) so the WAN path is a
/// one-line caller change rather than a code edit here. An optional `max_age`
/// (seconds) is appended when `Some` (persistence across restarts; `None` → a
/// session cookie).
#[must_use]
pub fn build_set_cookie(token: &str, secure: bool, max_age: Option<u64>) -> String {
    let mut s = format!(
        "{name}={token}; HttpOnly; SameSite=Strict; Path=/",
        name = SESSION_COOKIE_NAME,
    );
    if secure {
        s.push_str("; Secure");
    }
    if let Some(age) = max_age {
        s.push_str("; Max-Age=");
        s.push_str(&age.to_string());
    }
    s
}

/// Build a `Set-Cookie` header value that **clears** the session cookie
/// (deliberate rotation / logout): empty value + `Max-Age=0`.
#[must_use]
pub fn build_clear_cookie(secure: bool) -> String {
    let mut s = format!(
        "{name}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        name = SESSION_COOKIE_NAME,
    );
    if secure {
        s.push_str("; Secure");
    }
    s
}

/// Parse the session token out of a `Cookie` request header value.
///
/// Returns the value of the [`SESSION_COOKIE_NAME`] cookie if present. Handles
/// multiple `; `-separated cookie pairs and surrounding whitespace; ignores
/// other cookies. Returns `None` if the session cookie is absent or empty.
///
/// Cookie *names* are compared with plain `==` (they are not secret); the
/// returned token *value* must be checked against the store with the
/// constant-time [`SessionStore::validate_and_renew`].
#[must_use]
pub fn parse_session_cookie(cookie_header: &str) -> Option<String> {
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some((name, value)) = pair.split_once('=')
            && name.trim() == SESSION_COOKIE_NAME
        {
            let v = value.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A controllable test clock. Returns whatever millis we set; advanceable.
    fn test_clock() -> (Clock, Arc<Mutex<Millis>>) {
        let t = Arc::new(Mutex::new(0u64));
        let t2 = Arc::clone(&t);
        let clock = Clock::new(move || *t2.lock().unwrap_or_else(|e| e.into_inner()));
        (clock, t)
    }

    fn set(t: &Arc<Mutex<Millis>>, v: Millis) {
        *t.lock().unwrap_or_else(|e| e.into_inner()) = v;
    }

    // --- ct_eq -------------------------------------------------------------

    #[test]
    fn ct_eq_matches_and_rejects() {
        assert!(ct_eq(b"hunter2", b"hunter2"));
        assert!(!ct_eq(b"hunter2", b"hunter3"));
        assert!(!ct_eq(b"short", b"longer"));
        assert!(ct_eq(b"", b""));
        assert!(ct_eq_str("tok-abc", "tok-abc"));
        assert!(!ct_eq_str("tok-abc", "tok-abd"));
        assert!(!ct_eq_str("a", "aa"));
    }

    // --- token generation --------------------------------------------------

    #[test]
    fn generated_tokens_are_unique_and_urlsafe() {
        let a = generate_token().expect("entropy");
        let b = generate_token().expect("entropy");
        assert_ne!(a, b, "two CSPRNG tokens collided");
        // 32 bytes base64-url-no-pad => 43 chars
        assert_eq!(a.len(), 43);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "token not url-safe: {a}"
        );
    }

    // --- NonceStore --------------------------------------------------------

    #[test]
    fn nonce_mint_verify_consume_single_use() {
        let (clock, _t) = test_clock();
        let mut store = NonceStore::new(clock);
        let n = store.mint().expect("mint");
        assert_eq!(store.len(), 1);
        assert!(store.verify_and_consume(&n));
        // single-use: second fails
        assert!(!store.verify_and_consume(&n));
        assert!(store.is_empty());
    }

    #[test]
    fn nonce_unknown_value_rejected() {
        let (clock, _t) = test_clock();
        let mut store = NonceStore::new(clock);
        let _ = store.mint().expect("mint");
        assert!(!store.verify_and_consume("not-the-nonce"));
    }

    #[test]
    fn nonce_expires_after_ttl() {
        let (clock, t) = test_clock();
        let mut store = NonceStore::with_ttl(clock, Duration::from_millis(5_000));
        let n = store.mint().expect("mint");
        // just before expiry: still valid (and consumed)
        set(&t, 4_999);
        assert!(store.verify_and_consume(&n));

        // mint a new one and roll past its TTL
        let n2 = store.mint().expect("mint2");
        set(&t, 4_999 + 5_001);
        assert!(!store.verify_and_consume(&n2), "expired nonce accepted");
    }

    #[test]
    fn nonce_evict_expired_drops_stale() {
        let (clock, t) = test_clock();
        let mut store = NonceStore::with_ttl(clock, Duration::from_millis(1_000));
        let _ = store.mint().expect("mint");
        let _ = store.mint().expect("mint");
        assert_eq!(store.len(), 2);
        set(&t, 2_000);
        store.evict_expired(2_000);
        assert!(store.is_empty());
    }

    #[test]
    fn nonce_mint_evicts_expired_lazily() {
        let (clock, t) = test_clock();
        let mut store = NonceStore::with_ttl(clock, Duration::from_millis(1_000));
        let _ = store.mint().expect("mint");
        set(&t, 5_000);
        // minting at t=5000 should evict the stale one first
        let _ = store.mint().expect("mint");
        assert_eq!(store.len(), 1);
    }

    // --- SessionStore ------------------------------------------------------

    #[test]
    fn session_issue_validate_renew() {
        let (clock, t) = test_clock();
        let mut store = SessionStore::with_ttl(clock, Duration::from_millis(10_000));
        let tok = store.issue().expect("issue");
        assert!(store.is_valid(&tok));

        // advance to just before expiry; renew slides it forward
        set(&t, 9_000);
        assert!(store.validate_and_renew(&tok));
        // new expiry is now 19000; at t=18000 still valid
        set(&t, 18_000);
        assert!(store.is_valid(&tok), "sliding renewal did not extend expiry");
    }

    #[test]
    fn session_idle_expiry_without_renewal() {
        let (clock, t) = test_clock();
        let mut store = SessionStore::with_ttl(clock, Duration::from_millis(10_000));
        let tok = store.issue().expect("issue");
        set(&t, 10_001);
        assert!(!store.is_valid(&tok), "idle-expired session still valid");
        assert!(!store.validate_and_renew(&tok));
    }

    #[test]
    fn session_unknown_token_rejected() {
        let (clock, _t) = test_clock();
        let mut store = SessionStore::new(clock);
        let _ = store.issue().expect("issue");
        assert!(!store.is_valid("bogus"));
        assert!(!store.validate_and_renew("bogus"));
    }

    #[test]
    fn session_revoke() {
        let (clock, _t) = test_clock();
        let mut store = SessionStore::new(clock);
        let tok = store.issue().expect("issue");
        assert!(store.is_valid(&tok));
        store.revoke(&tok);
        assert!(!store.is_valid(&tok));
        assert!(store.is_empty());
    }

    #[test]
    fn session_persist_and_load_roundtrip() {
        let (clock, _t) = test_clock();
        let mut store = SessionStore::with_ttl(clock, Duration::from_millis(100_000));
        let tok = store.issue().expect("issue");

        let dir = std::env::temp_dir().join(format!("mdp-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("sessions");
        store.persist(&path).expect("persist");

        // 0600 perms enforced
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "session file not 0600");
        }

        let (clock2, _t2) = test_clock();
        let loaded =
            SessionStore::load_with_ttl(&path, clock2, Duration::from_millis(100_000))
                .expect("load");
        assert!(loaded.is_valid(&tok), "token did not survive persist/load");
        assert_eq!(loaded.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_load_missing_file_is_empty() {
        let (clock, _t) = test_clock();
        let path = std::env::temp_dir().join("mdp-auth-definitely-missing-xyz/sessions");
        let store = SessionStore::load(&path, clock).expect("missing file ok");
        assert!(store.is_empty());
    }

    #[test]
    fn session_load_drops_expired() {
        let dir = std::env::temp_dir().join(format!("mdp-auth-exp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("sessions");
        std::fs::write(&path, "expiredtoken\t500\nlivetoken\t100000\n").expect("write");

        let (clock, t) = test_clock();
        set(&t, 1_000);
        let store = SessionStore::load(&path, clock).expect("load");
        assert!(!store.is_valid("expiredtoken"));
        assert!(store.is_valid("livetoken"));
        assert_eq!(store.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- host_allowed ------------------------------------------------------

    #[test]
    fn host_allowlist_accepts_loopback() {
        assert!(host_allowed("127.0.0.1"));
        assert!(host_allowed("localhost"));
        assert!(host_allowed("127.0.0.1:8080"));
        assert!(host_allowed("localhost:3000"));
        assert!(host_allowed("127.0.0.1:1"));
        assert!(host_allowed("127.0.0.1:65535"));
    }

    #[test]
    fn host_allowlist_rejects_rebinding_and_tricks() {
        assert!(!host_allowed("evil.com"));
        assert!(!host_allowed("evil.com:8080"));
        assert!(!host_allowed("0.0.0.0:8080"));
        assert!(!host_allowed("127.0.0.1.evil.com"));
        assert!(!host_allowed("localhost.evil.com"));
        assert!(!host_allowed("[::1]:8080")); // no IPv6
        assert!(!host_allowed("127.0.0.1:0")); // port 0
        assert!(!host_allowed("127.0.0.1:08080")); // leading zero
        assert!(!host_allowed("127.0.0.1:0080")); // leading zero
        assert!(!host_allowed("127.0.0.1:99999")); // out of range
        assert!(!host_allowed("127.0.0.1:notaport"));
        assert!(!host_allowed("127.0.0.1:"));
    }

    // --- origin_allowed ----------------------------------------------------

    #[test]
    fn origin_check_same_origin_and_missing() {
        assert_eq!(origin_allowed(None, None), OriginCheck::Allow);
        assert_eq!(
            origin_allowed(Some("http://127.0.0.1:8080"), Some("same-origin")),
            OriginCheck::Allow
        );
        assert_eq!(
            origin_allowed(Some("http://localhost:3000"), None),
            OriginCheck::Allow
        );
        // null origin (bootstrap POST) allowed
        assert_eq!(
            origin_allowed(Some("null"), Some("none")),
            OriginCheck::Allow
        );
    }

    #[test]
    fn origin_check_rejects_cross_origin() {
        assert_eq!(
            origin_allowed(Some("http://evil.com"), None),
            OriginCheck::Deny
        );
        assert_eq!(
            origin_allowed(Some("https://127.0.0.1:8080"), None),
            OriginCheck::Deny,
            "https scheme must be a deliberate edit, not auto-allowed"
        );
        // cross-site Sec-Fetch-Site rejected even with a good origin
        assert_eq!(
            origin_allowed(Some("http://127.0.0.1:8080"), Some("cross-site")),
            OriginCheck::Deny
        );
        assert_eq!(origin_allowed(None, Some("same-site")), OriginCheck::Deny);
        // origin with a path is not a bare origin
        assert_eq!(
            origin_allowed(Some("http://127.0.0.1:8080/evil"), None),
            OriginCheck::Deny
        );
    }

    // --- permission floor --------------------------------------------------

    #[test]
    fn floor_world_readable_always_allowed() {
        assert!(is_world_readable(0o644));
        assert!(floor_allows(0o644, false));
        assert!(floor_allows(0o644, true));
        assert!(!floor_deny(0o644, false));
    }

    #[test]
    fn floor_private_requires_auth() {
        assert!(!is_world_readable(0o600));
        assert!(!is_world_readable(0o640));
        // unauthenticated: denied
        assert!(!floor_allows(0o600, false));
        assert!(floor_deny(0o600, false));
        // authenticated: token unlocks own private docs
        assert!(floor_allows(0o600, true));
        assert!(!floor_deny(0o600, true));
    }

    #[test]
    fn mode_of_reads_real_metadata() {
        // A world-readable temp file should report o+r through mode_of.
        let dir = std::env::temp_dir().join(format!("mdp-auth-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let p = dir.join("f");
        std::fs::write(&p, b"x").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644))
                .expect("chmod");
            let md = std::fs::metadata(&p).expect("stat");
            assert!(is_world_readable(mode_of(&md)));
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))
                .expect("chmod");
            let md = std::fs::metadata(&p).expect("stat");
            assert!(!is_world_readable(mode_of(&md)));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- cookies -----------------------------------------------------------

    #[test]
    fn set_cookie_flags() {
        let c = build_set_cookie("tok123", false, None);
        assert!(c.starts_with("mdp_session=tok123"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Strict"));
        assert!(c.contains("Path=/"));
        assert!(!c.contains("Secure"), "loopback cookie must not be Secure");
        assert!(!c.contains("Max-Age"));
    }

    #[test]
    fn set_cookie_secure_and_max_age() {
        let c = build_set_cookie("tok123", true, Some(2_592_000));
        assert!(c.contains("Secure"), "WAN cookie should be Secure");
        assert!(c.contains("Max-Age=2592000"));
    }

    #[test]
    fn clear_cookie_expires_immediately() {
        let c = build_clear_cookie(false);
        assert!(c.starts_with("mdp_session=;"));
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("HttpOnly"));
    }

    #[test]
    fn parse_cookie_extracts_session() {
        assert_eq!(
            parse_session_cookie("mdp_session=abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_session_cookie("other=1; mdp_session=abc123; foo=bar"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_session_cookie("  mdp_session = spaced  "),
            Some("spaced".to_string())
        );
        assert_eq!(parse_session_cookie("other=1; foo=bar"), None);
        assert_eq!(parse_session_cookie("mdp_session="), None);
        assert_eq!(parse_session_cookie(""), None);
    }

    #[test]
    fn cookie_roundtrip_through_store() {
        let (clock, _t) = test_clock();
        let mut store = SessionStore::new(clock);
        let tok = store.issue().expect("issue");
        let set_hdr = build_set_cookie(&tok, false, Some(100));
        // simulate the browser echoing just the name=value pair back
        let cookie_hdr = set_hdr.split(';').next().unwrap_or("").to_string();
        let parsed = parse_session_cookie(&cookie_hdr).expect("parse");
        assert!(
            store.validate_and_renew(&parsed),
            "roundtripped token invalid"
        );
    }
}
