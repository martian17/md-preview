//! Render-isolation HTML/CSP layer (ADR-0007; design §3).
//!
//! This module is the **pure builder** for the two-context render-isolation
//! model. It returns HTML and CSP **strings** and performs **no I/O** — origins
//! are parameters, never baked in, so the same builders serve any loopback /
//! asset-origin pair the daemon chooses. The daemon (rebuild Wave 6) owns route
//! wiring, capability minting, and path confinement; here we only assemble the
//! markup and headers and document the postMessage contract between the two
//! contexts.
//!
//! ## Two contexts (design §3 "Two contexts")
//! - **Trusted shell / parent** — an intact `http://127.0.0.1` SPA. It holds the
//!   cookie/capability ([ADR-0006]), does **all** authenticated fetches (markdown
//!   + asset authorization), and loads **no** untrusted content and **no**
//!     third-party code (its own tiny, origin-pinned bundle). This is the minimal,
//!     audited TCB. Served with [`shell_csp`] + COOP/COEP ([`SHELL_COOP`] /
//!     [`SHELL_COEP`]) so the iframe runs in its own process.
//! - **Untrusted renderer** — an `<iframe srcdoc=…>` with
//!   **[`IFRAME_SANDBOX`] only** (`allow-scripts`, *not* `allow-same-origin`), so
//!   it is a **null/opaque origin** with no cookie, no token and (via
//!   [`renderer_csp`]) **no network**. It receives content only over the
//!   postMessage bus and can never exfiltrate the bytes it renders.
//!
//! ## postMessage bus schema (design §3 "postMessage bus")
//! The parent is the **sole authority on paths** — the iframe may *report* intent
//! but never resolves a path itself; the parent re-resolves + confines every path
//! (the §1 confinement funnel) before acting. Validate the message **source /
//! channel port**, never `event.origin` (it is the string `"null"` for every
//! opaque origin).
//!
//! - **parent → iframe**: `{ type: "render", html }` — body HTML to render.
//! - **iframe → parent**:
//!   - `{ type: "rendered" }` — ack; cancels the parent's [`RENDER_WATCHDOG_MS`]
//!     teardown timer.
//!   - `{ type: "save", content }` — user asked to save; the parent writes the
//!     *current tracked* document only (edits are scoped to the open doc).
//!   - `{ type: "navigate", target }` — user clicked a link; the parent destroys
//!     this iframe, re-resolves + confines `target`, then mounts a fresh srcdoc
//!     iframe (no cross-document state / XSS bleed). Wave 6 owns the re-confine.
//!   - `{ type: "error", message }` — the renderer hit a fatal error.
//!
//! [ADR-0006]: ../../hq/products/md-preview/decisions/ADR-0006-localhost-trust-and-auth.md
//! [ADR-0007]: ../../hq/products/md-preview/decisions/ADR-0007-render-isolation-model.md

/// `sandbox` attribute for the renderer iframe. **`allow-scripts` only** —
/// deliberately *no* `allow-same-origin` (keeps the frame a null/opaque origin so
/// it can't share the shell's cookie), *no* `allow-popups`, *no*
/// `allow-top-navigation`, *no* `allow-forms`, *no* `allow-modals` (design §3
/// "Two contexts"). The scripts the renderer does run are confined by
/// [`renderer_csp`].
pub const IFRAME_SANDBOX: &str = "allow-scripts";

/// `Cross-Origin-Opener-Policy` for the shell. With [`SHELL_COEP`] this opts the
/// shell into cross-origin isolation so the browser puts the null-origin iframe in
/// its **own process** — the Spectre-class cross-origin-read backstop from design
/// §3 "Hardening backstops".
pub const SHELL_COOP: &str = "same-origin";

/// `Cross-Origin-Embedder-Policy` for the shell. Pairs with [`SHELL_COOP`] to
/// enable cross-origin isolation / Site Isolation (design §3 "Hardening
/// backstops").
pub const SHELL_COEP: &str = "require-corp";

/// Renderer watchdog budget (milliseconds). The parent tears down any iframe that
/// hasn't acked `{type:"rendered"}` within this window, bounding DoS from a
/// malicious document (e.g. an infinite Mermaid graph). The iframe is disposable
/// (design §3 "Hardening backstops").
pub const RENDER_WATCHDOG_MS: u32 = 5000;

/// Build the **Content-Security-Policy for the trusted shell origin**.
///
/// `static_origin` is the separate *local* origin that serves the origin-pinned
/// JS/CSS/font bundle (mermaid, KaTeX, …; design §3 "JS/CSS bundle"). `nonce` is a
/// freshly minted nonce for the shell's own inline bus/fetch/WS script — generate
/// one per page load with [`gen_nonce`].
///
/// Directives (each documented inline):
/// - `default-src 'none'` — deny by default; everything else is opt-in.
/// - `frame-ancestors 'none'` — the shell may never itself be framed (clickjacking
///   / nested-frame confusion backstop).
/// - `script-src 'nonce-…' {static}` — only the shell's own nonce'd inline bus JS
///   and the origin-pinned bundle from the static origin; **no `'self'`** for
///   arbitrary same-origin scripts, **no** CDN.
/// - `style-src 'nonce-…' {static}` — the shell's nonce'd inline styles + bundle CSS.
/// - `font-src {static}` / `img-src {static}` — fonts and chrome images come from
///   the bundle origin.
/// - `connect-src 'self'` — fetch/XHR/WebSocket limited to **same-origin only**:
///   the content API and the live-reload/collab WS. The shell does the
///   authenticated fetching; nothing leaves loopback.
/// - `frame-src 'self'` — permit the `srcdoc` renderer iframe (a srcdoc frame
///   inherits the embedder's `frame-src` allowance).
/// - `base-uri 'none'` / `form-action 'none'` / `object-src 'none'` — close the
///   remaining injection avenues.
pub fn shell_csp(static_origin: &str, nonce: &str) -> String {
    let s = escape_csp_token(static_origin);
    let n = escape_csp_token(nonce);
    format!(
        "default-src 'none'; \
         frame-ancestors 'none'; \
         script-src 'nonce-{n}' {s}; \
         style-src 'nonce-{n}' {s}; \
         font-src {s}; \
         img-src {s}; \
         connect-src 'self'; \
         frame-src 'self'; \
         base-uri 'none'; \
         form-action 'none'; \
         object-src 'none'"
    )
}

/// Build the **Content-Security-Policy for the sandboxed null-origin renderer**
/// (the `srcdoc` document). This CSP — not the `sandbox` attribute — is the actual
/// containment (design §3 "Renderer CSP").
///
/// `static_origin` is the bundle origin; `asset_origin` is the separate *capability*
/// origin that serves per-doc images/media (cross-origin & CORS-less, so pixels
/// stay canvas-tainted and unreadable by script). `nonce` is a fresh per-call nonce
/// for the renderer's own inline bootstrap script.
///
/// Directives:
/// - `default-src 'none'` — deny everything by default.
/// - **`connect-src 'none'`** — the renderer can **NEVER** fetch / XHR / open a
///   socket. This is what makes "a script can read the document bytes" inert:
///   there is no egress.
/// - `script-src 'nonce-…' {static}` — only the renderer's nonce'd bootstrap +
///   the origin-pinned bundle; **no `'self'`** (meaningless in an opaque origin
///   and would allow `<script src="./x">`).
/// - `style-src 'unsafe-inline' {static}` — inline styles (rendered markdown
///   carries them) + bundle CSS. `unsafe-inline` is acceptable here: it is
///   *style*, not script, inside a null-origin, zero-egress frame.
/// - `font-src {static}` — bundle fonts (KaTeX, …).
/// - `img-src {asset}` / `media-src {asset}` — images & video come **only** from
///   the capability asset origin (visible to the human, opaque to script,
///   unexfiltratable). No `data:` (no giant inline blobs).
/// - `base-uri 'none'` / `form-action 'none'` / `object-src 'none'` — close the
///   remaining avenues.
pub fn renderer_csp(static_origin: &str, asset_origin: &str, nonce: &str) -> String {
    let s = escape_csp_token(static_origin);
    let a = escape_csp_token(asset_origin);
    let n = escape_csp_token(nonce);
    format!(
        "default-src 'none'; \
         connect-src 'none'; \
         script-src 'nonce-{n}' {s}; \
         style-src 'unsafe-inline' {s}; \
         font-src {s}; \
         img-src {a}; \
         media-src {a}; \
         base-uri 'none'; \
         form-action 'none'; \
         object-src 'none'"
    )
}

/// Build the **trusted SPA shell page** (parent document).
///
/// This is the privileged origin that holds the cookie, hosts the sandboxed
/// renderer iframe, and runs the tiny nonce'd inline **bus** (postMessage),
/// **content fetch**, and **WS subscribe** logic. It loads no untrusted content;
/// the bundle is loaded from `static_origin`. `nonce` MUST match the one passed to
/// the matching [`shell_csp`] header (same page load); mint it with [`gen_nonce`].
///
/// The page mounts an empty sandboxed iframe ([`IFRAME_SANDBOX`]) and arms the
/// [`RENDER_WATCHDOG_MS`] teardown timer; the inline script fetches the document
/// body, posts `{type:"render", html}` into the frame, and relays
/// `save`/`navigate`/`error`/`rendered` back to the daemon. Path authority stays
/// in the parent — the iframe only *reports* intent (see the module-level schema).
pub fn render_shell_page(static_origin: &str, nonce: &str) -> String {
    let s = escape_attr(static_origin);
    let n = escape_attr(nonce);
    let sandbox = IFRAME_SANDBOX;
    let watchdog = RENDER_WATCHDOG_MS;
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>md-preview</title>
<link rel="stylesheet" href="{s}/shell.css" nonce="{n}">
</head>
<body>
<!-- The untrusted renderer: sandbox is allow-scripts ONLY (deliberately not
     granting the same-origin flag => null/opaque origin, no cookie/token). Its
     srcdoc is set per document. -->
<iframe id="render-frame" sandbox="{sandbox}" referrerpolicy="no-referrer"
        title="rendered document"></iframe>
<script nonce="{n}">
// Trusted shell bus + content fetch + WS subscribe (design §3). The parent is the
// sole authority on paths: it fetches/authorizes content and re-confines every
// navigation target; the iframe only reports intent.
(() => {{
  "use strict";
  const RENDER_WATCHDOG_MS = {watchdog};
  const frame = document.getElementById("render-frame");
  let watchdog = null;

  // Validate by source (the frame's contentWindow), NEVER event.origin: an opaque
  // sandbox origin is always the string "null", so origin checks are meaningless.
  function fromRenderer(ev) {{ return ev.source === frame.contentWindow; }}

  function armWatchdog() {{
    clearTimeout(watchdog);
    // The iframe is disposable: if it doesn't ack "rendered" in time, tear it
    // down (bounds DoS from a malicious document).
    watchdog = setTimeout(() => teardownFrame(), RENDER_WATCHDOG_MS);
  }}

  function teardownFrame() {{
    clearTimeout(watchdog);
    frame.removeAttribute("srcdoc");
  }}

  // parent -> iframe: hand the body HTML over for rendering.
  function postRender(html) {{
    armWatchdog();
    frame.contentWindow.postMessage({{ type: "render", html }}, "*");
  }}

  // The shell does ALL authenticated fetches (it holds the cookie). Wave 6 wires
  // the concrete content endpoint; here we keep the same-origin fetch shape that
  // shell_csp's `connect-src 'self'` permits.
  async function loadDocument(path) {{
    const res = await fetch("./content?path=" + encodeURIComponent(path), {{
      credentials: "same-origin",
    }});
    if (!res.ok) throw new Error("content fetch failed: " + res.status);
    return res.text();
  }}

  // Live updates over a same-origin WebSocket (connect-src 'self').
  // Auto-reconnects on close/error with capped exponential backoff + jitter so
  // a tab whose daemon restarted self-heals without a manual page reload.
  // CSP: connect-src 'self' already permits this same-origin WS — unchanged.
  function subscribe(path, onUpdate) {{
    const WS_BASE_MS   = 500;   // first reconnect delay
    const WS_MAX_MS    = 10000; // cap (~10 s)
    let delay = WS_BASE_MS;
    let ws;
    let stopped = false;

    function connect() {{
      if (stopped) return;
      const scheme = location.protocol === "https:" ? "wss" : "ws";
      ws = new WebSocket(scheme + "://" + location.host
        + "/ws?path=" + encodeURIComponent(path));
      ws.addEventListener("message", (ev) => onUpdate(ev.data));
      ws.addEventListener("open", () => {{ delay = WS_BASE_MS; }}); // reset on success
      ws.addEventListener("close", () => {{ if (!stopped) scheduleReconnect(); }});
      ws.addEventListener("error", () => {{ /* close fires after error; handled there */ }});
    }}

    function scheduleReconnect() {{
      // Jitter: ±20 % of current delay to spread reconnect storms.
      const jitter = (Math.random() * 0.4 - 0.2) * delay;
      const wait = Math.min(delay + jitter, WS_MAX_MS);
      delay = Math.min(delay * 2, WS_MAX_MS);
      setTimeout(connect, wait);
    }}

    connect();

    // Return a handle the caller can use to stop reconnecting (e.g. on navigate).
    return {{ close() {{ stopped = true; ws && ws.close(); }} }};
  }}

  window.addEventListener("message", (ev) => {{
    if (!fromRenderer(ev)) return;            // source check, not origin
    const msg = ev.data;
    if (!msg || typeof msg.type !== "string") return;
    switch (msg.type) {{
      case "rendered":
        clearTimeout(watchdog);               // ack: cancel the teardown timer
        break;
      case "save":
        // Parent writes only the current tracked document (scoped edits).
        saveCurrent(msg.content);
        break;
      case "navigate":
        // Parent re-resolves + confines msg.target, destroys this frame, and
        // mounts a fresh srcdoc iframe (Wave 6 owns the confinement).
        navigate(msg.target);
        break;
      case "error":
        console.error("[renderer]", msg.message);
        break;
    }}
  }});

  // Wave-6 wiring stubs (kept inert so the shell is self-contained & no-panic):
  function saveCurrent(_content) {{ /* parent POSTs to the save endpoint */ }}
  function navigate(_target) {{ teardownFrame(); /* re-confine + remount */ }}

  // Exposed for the Wave-6 bootstrap to drive once a document/path is known.
  window.__mdPreviewShell = {{ loadDocument, subscribe, postRender, teardownFrame }};
}})();
</script>
</body>
</html>"#
    )
}

/// Build the **sandboxed renderer document** (the iframe `srcdoc`).
///
/// `static_origin`/`asset_origin` are forwarded to [`renderer_csp`]. A **fresh
/// nonce is minted per call** ([`gen_nonce`]) for the inline bootstrap script, and
/// the matching CSP is embedded as a `<meta http-equiv>` so the policy travels with
/// the srcdoc (a srcdoc document has no response headers of its own).
///
/// The bootstrap is intentionally tiny: it waits for `{type:"render", html}` from
/// the parent, injects the body, and posts `{type:"rendered"}` (or
/// `{type:"error", message}`). Save / navigate intents are reported up to the
/// parent — the renderer never resolves a path or touches the network
/// (`connect-src 'none'`).
pub fn render_srcdoc(static_origin: &str, asset_origin: &str) -> String {
    let nonce = gen_nonce();
    let csp = escape_attr(&renderer_csp(static_origin, asset_origin, &nonce));
    let s = escape_attr(static_origin);
    let n = escape_attr(&nonce);
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="{csp}">
<link rel="stylesheet" href="{s}/bundle.css" nonce="{n}">
</head>
<body>
<div id="doc"></div>
<script nonce="{n}">
// Sandboxed renderer bootstrap (null origin, connect-src 'none' => zero egress).
// Receives body HTML from the parent over postMessage; never fetches or resolves
// paths itself (the parent is the sole path authority).
(() => {{
  "use strict";
  // Reply only to the embedder (parent). Do NOT trust event.origin ("null").
  function toParent(msg) {{ parent.postMessage(msg, "*"); }}

  window.addEventListener("message", (ev) => {{
    if (ev.source !== parent) return;
    const msg = ev.data;
    if (!msg || msg.type !== "render" || typeof msg.html !== "string") return;
    try {{
      document.getElementById("doc").innerHTML = msg.html;
      toParent({{ type: "rendered" }});         // ack -> cancels parent watchdog
    }} catch (e) {{
      toParent({{ type: "error", message: String(e && e.message || e) }});
    }}
  }});

  // Link clicks are reported up; the parent re-confines the target and remounts a
  // fresh iframe. The renderer never navigates itself.
  document.addEventListener("click", (ev) => {{
    const a = ev.target.closest && ev.target.closest("a[href]");
    if (!a) return;
    ev.preventDefault();
    toParent({{ type: "navigate", target: a.getAttribute("href") }});
  }});
}})();
</script>
</body>
</html>"#
    )
}

/// Escape a string for safe interpolation into an HTML **attribute** value (the
/// builders quote attributes with `"`). Neutralizes attribute breakout and the
/// markup metacharacters; lossless for the origin/nonce/CSP strings we embed.
fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// Sanitize a token (origin / nonce) for inclusion in a **CSP directive**. A CSP
/// source token must not contain `;` (directive separator), whitespace (token
/// separator), or quotes/`<`/`>` — any such char would let a crafted origin inject
/// extra directives or break the policy. We drop disallowed bytes rather than
/// escape them (CSP has no escape syntax); callers pass well-formed origins, so
/// this is a defense-in-depth backstop, not a transform.
fn escape_csp_token(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(c, ';' | ',' | '"' | '\'' | '<' | '>' | '`' | '(' | ')')
        })
        .collect()
}

/// Mint a fresh CSP nonce: 16 CSPRNG bytes mapped to a URL-safe alphabet
/// (`A–Z a–z 0–9 - _`), which is valid inside a `'nonce-…'` source and an HTML
/// attribute without escaping. No-panic: if the OS RNG is unavailable we fall back
/// to a non-secret placeholder so this builder never aborts production (a missing
/// OS RNG is fatal to the daemon elsewhere; this is not the layer that should
/// panic).
pub fn gen_nonce() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        return "nonce-unavailable".to_string();
    }
    bytes
        .iter()
        .map(|b| ALPHABET[(b & 0x3f) as usize] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATIC: &str = "http://127.0.0.1:7001";
    const ASSET: &str = "http://127.0.0.1:7002";

    #[test]
    fn constants_match_the_isolation_model() {
        // allow-scripts ONLY — never allow-same-origin (would re-grant the origin).
        assert_eq!(IFRAME_SANDBOX, "allow-scripts");
        assert!(!IFRAME_SANDBOX.contains("allow-same-origin"));
        // Cross-origin isolation pair (Site Isolation backstop).
        assert_eq!(SHELL_COOP, "same-origin");
        assert_eq!(SHELL_COEP, "require-corp");
        assert_eq!(RENDER_WATCHDOG_MS, 5000);
    }

    #[test]
    fn shell_csp_has_required_directives() {
        let csp = shell_csp(STATIC, "abc123");
        assert!(
            csp.contains("frame-ancestors 'none'"),
            "shell must never be framed"
        );
        assert!(csp.contains("default-src 'none'"));
        // Shell connects same-origin only (content API + WS) — NOT 'none'.
        assert!(csp.contains("connect-src 'self'"));
        // Inline bus JS is nonce'd; the bundle origin is allowed; no bare 'self' script.
        assert!(csp.contains("script-src 'nonce-abc123'"));
        assert!(csp.contains(STATIC));
        assert!(!csp.contains("script-src 'self'"));
        assert!(
            !csp.contains("'unsafe-inline'"),
            "shell never uses unsafe-inline"
        );
    }

    #[test]
    fn renderer_csp_is_zero_egress() {
        let csp = renderer_csp(STATIC, ASSET, "n0nce");
        assert!(csp.contains("default-src 'none'"));
        // The load-bearing containment: the renderer can NEVER fetch.
        assert!(csp.contains("connect-src 'none'"));
        // Script is nonce-pinned, no 'self'.
        assert!(csp.contains("script-src 'nonce-n0nce'"));
        assert!(!csp.contains("script-src 'self'"));
        // Images/media come only from the capability asset origin.
        assert!(csp.contains(&format!("img-src {ASSET}")));
        assert!(csp.contains(&format!("media-src {ASSET}")));
        // Inline styles are allowed (style, not script, in a null-origin frame).
        assert!(csp.contains("style-src 'unsafe-inline'"));
        // The bundle origin serves script/style/font.
        assert!(csp.contains(STATIC));
    }

    #[test]
    fn renderer_never_allows_connect_to_asset_or_static() {
        // Even though img/media use the asset origin, connect-src stays 'none'.
        let csp = renderer_csp(STATIC, ASSET, "n");
        assert!(csp.contains("connect-src 'none'"));
        assert!(!csp.contains("connect-src 'self'"));
        assert!(!csp.contains(&format!("connect-src {ASSET}")));
    }

    #[test]
    fn shell_page_mounts_sandboxed_iframe_with_correct_attr() {
        let page = render_shell_page(STATIC, "nonceX");
        assert!(page.starts_with("<!DOCTYPE html>"));
        // Sandbox attr is exactly allow-scripts (no same-origin).
        assert!(page.contains(&format!(r#"sandbox="{IFRAME_SANDBOX}""#)));
        assert!(!page.contains("allow-same-origin"));
        // Inline bus script carries the nonce.
        assert!(page.contains(r#"<script nonce="nonceX">"#));
        // Watchdog budget injected.
        assert!(page.contains("5000"));
        // Source-based bus check, not origin.
        assert!(page.contains("ev.source === frame.contentWindow"));
    }

    // Wave 7: WS auto-reconnect / backoff assertions.
    #[test]
    fn ws_reconnect_code_is_present_in_shell_page() {
        let page = render_shell_page(STATIC, "nonceR");
        // The reconnect plumbing must be present in the inline script.
        assert!(
            page.contains("scheduleReconnect"),
            "shell page must contain WS reconnect logic"
        );
        assert!(
            page.contains("WS_BASE_MS"),
            "shell page must define base reconnect delay"
        );
        assert!(
            page.contains("WS_MAX_MS"),
            "shell page must define max reconnect delay cap"
        );
        // Jitter must be applied (Math.random()).
        assert!(
            page.contains("Math.random()"),
            "shell page must apply jitter to the reconnect delay"
        );
        // The reconnect loop must hook on 'close' (and error fires before close).
        assert!(
            page.contains(r#""close""#),
            "shell page must attach close listener for reconnect"
        );
    }

    #[test]
    fn ws_reconnect_does_not_loosen_csp() {
        // CSP is built independently of the inline JS, so just check it still
        // carries connect-src 'self' and nothing wider.
        let csp = shell_csp(STATIC, "n");
        assert!(csp.contains("connect-src 'self'"), "CSP must still be connect-src 'self'");
        assert!(!csp.contains("connect-src *"), "CSP must not be wildened");
        assert!(!csp.contains("connect-src 'unsafe'"), "CSP must not be wildened");
    }

    #[test]
    fn srcdoc_embeds_renderer_csp_and_fresh_nonce() {
        let doc = render_srcdoc(STATIC, ASSET);
        assert!(doc.starts_with("<!DOCTYPE html>"));
        // CSP travels in the srcdoc via meta http-equiv (no response headers).
        assert!(doc.contains(r#"<meta http-equiv="Content-Security-Policy""#));
        assert!(doc.contains("connect-src 'none'"));
        // Renderer reports navigation up rather than navigating itself.
        assert!(doc.contains(r#"type: "navigate""#));
        assert!(doc.contains(r#"type: "rendered""#));
    }

    #[test]
    fn srcdoc_mints_a_distinct_nonce_each_call() {
        // Fresh nonce per call: the two documents must not share a nonce.
        let a = render_srcdoc(STATIC, ASSET);
        let b = render_srcdoc(STATIC, ASSET);
        assert_ne!(a, b);
    }

    #[test]
    fn gen_nonce_is_url_safe_and_sized() {
        let n = gen_nonce();
        assert_eq!(n.len(), 16);
        assert!(n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn escape_attr_neutralizes_attribute_breakout() {
        let out = escape_attr(r#"x" onload="alert(1)"#);
        assert!(!out.contains('"'));
        assert!(out.contains("&quot;"));
        // A crafted origin can't inject markup into the page either.
        assert_eq!(escape_attr("<b>&'"), "&lt;b&gt;&amp;&#x27;");
    }

    #[test]
    fn escape_csp_token_strips_directive_injection() {
        // A malicious "origin" trying to add directives or break the policy is
        // stripped of the dangerous separators.
        let out = escape_csp_token("http://x; script-src 'unsafe-inline'");
        assert!(!out.contains(';'));
        assert!(!out.contains(' '));
        assert!(!out.contains('\''));
        // A clean origin is preserved verbatim.
        assert_eq!(
            escape_csp_token("http://127.0.0.1:7001"),
            "http://127.0.0.1:7001"
        );
    }

    #[test]
    fn crafted_origin_cannot_inject_a_csp_directive() {
        let csp = shell_csp("http://x;default-src *", "n");
        // The injected `;default-src *` must not have created a second directive.
        assert!(!csp.contains("default-src *"));
        assert!(csp.contains("default-src 'none'"));
    }
}
