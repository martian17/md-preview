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
<!-- The untrusted renderer iframe is created/destroyed per navigation by the
     bootstrap (a fresh srcdoc + fresh nonce each time). It always carries
     sandbox="allow-scripts" ONLY (deliberately not granting the same-origin flag
     => null/opaque origin, no cookie/token). -->
<div id="render-host"></div>
<script nonce="{n}">
// Trusted shell bus + content fetch + WS subscribe + navigate/save (design §3).
// The parent is the SOLE authority on paths: it does every authenticated fetch
// (carrying the cookie same-origin, which is what makes SameSite=Strict work
// post-/claim), re-confines every navigation target server-side, and the iframe
// only ever REPORTS intent — it never resolves a path or touches the network.
(() => {{
  "use strict";
  const RENDER_WATCHDOG_MS = {watchdog};
  const IFRAME_SANDBOX = "{sandbox}";
  const host = document.getElementById("render-host");

  // Per-mount state. Each navigation destroys the iframe and rebuilds all of it,
  // so the channel/port/watchdog never leak across documents (no XSS bleed).
  let frame = null;       // the live <iframe>
  let channel = null;     // the MessageChannel to the live iframe
  let watchdog = null;    // teardown timer for the un-acked iframe
  let wsHandle = null;    // live /ws subscription handle
  let currentPath = null; // the document the shell is currently tracking

  function armWatchdog() {{
    clearTimeout(watchdog);
    // The iframe is disposable: if it doesn't ack {{rendered}} in time, tear it
    // down and rebuild it (bounds DoS from a malicious document, e.g. an
    // infinite Mermaid graph).
    watchdog = setTimeout(() => {{ if (currentPath) mount(currentPath); }},
      RENDER_WATCHDOG_MS);
  }}

  // Destroy the live iframe + its channel + watchdog. Disposable by design.
  function teardownFrame() {{
    clearTimeout(watchdog);
    if (channel) {{ try {{ channel.port1.close(); }} catch (e) {{}} channel = null; }}
    if (frame && frame.parentNode) frame.parentNode.removeChild(frame);
    frame = null;
  }}

  // parent -> iframe: hand the body HTML over the dedicated MessageChannel port.
  function postRender(html) {{
    if (!channel) return;
    armWatchdog();
    channel.port1.postMessage({{ type: "render", html }});
  }}

  // The shell does ALL authenticated fetches (it holds the cookie). Same-origin
  // so the cookie is carried (SameSite=Strict). connect-src 'self' permits this.
  async function loadDocument(path) {{
    const res = await fetch("./content?path=" + encodeURIComponent(path), {{
      credentials: "same-origin",
    }});
    if (!res.ok) throw new Error("content fetch failed: " + res.status);
    return res.text();
  }}

  // Fetch a FRESH /srcdoc (fresh nonce, fresh renderer CSP) for each mount, so
  // every navigation gets a brand-new null-origin sandbox with no shared state.
  async function fetchSrcdoc() {{
    const res = await fetch("./srcdoc", {{ credentials: "same-origin" }});
    if (!res.ok) throw new Error("srcdoc fetch failed: " + res.status);
    return res.text();
  }}

  // Build a fresh sandboxed iframe for `path`: destroy any prior frame, mint a
  // MessageChannel, mount a fresh /srcdoc, hand the iframe its private port on
  // load, then fetch + relay the rendered body and (re)subscribe to live updates.
  async function mount(path) {{
    currentPath = path;
    teardownFrame();
    if (wsHandle) {{ wsHandle.close(); wsHandle = null; }}

    let srcdoc;
    try {{ srcdoc = await fetchSrcdoc(); }} catch (e) {{ console.error("[shell]", e); return; }}
    // The mount may have been superseded while awaiting (rapid navigation).
    if (currentPath !== path) return;

    channel = new MessageChannel();
    frame = document.createElement("iframe");
    frame.setAttribute("sandbox", IFRAME_SANDBOX);
    frame.setAttribute("referrerpolicy", "no-referrer");
    frame.setAttribute("title", "rendered document");
    frame.id = "render-frame";

    // Inbound from the iframe arrives ONLY over the channel port we created and
    // handed it — never event.origin (an opaque sandbox origin is the string
    // "null", so an origin check is meaningless). The port IS the auth: only the
    // iframe we minted it for holds the other end.
    channel.port1.onmessage = (ev) => handleRendererMessage(ev.data);
    channel.port1.start && channel.port1.start();

    frame.addEventListener("load", () => {{
      // Hand the iframe its private port (transfer port2). The srcdoc keeps it
      // and replies only over it. armWatchdog before the first body relay.
      if (!frame || !channel) return;
      frame.contentWindow.postMessage({{ type: "__port" }}, "*", [channel.port2]);
    }});

    host.appendChild(frame);
    frame.srcdoc = srcdoc;

    // Fetch the rendered body and relay it once we have a frame + port. We post
    // on the next tick after load via the bus; the iframe buffers until ready by
    // virtue of the channel (queued port messages are delivered after start()).
    let html;
    try {{ html = await loadDocument(path); }} catch (e) {{ console.error("[shell]", e); return; }}
    if (currentPath !== path || !channel) return;
    postRender(html);

    // Live updates over a same-origin WS (connect-src 'self'); relayed verbatim
    // into the iframe as a fresh {{render}} on each new body fragment.
    wsHandle = subscribe(path, (body) => {{
      if (currentPath === path) postRender(body);
    }});
  }}

  // Live updates over a same-origin WebSocket (connect-src 'self').
  // Auto-reconnects on close/error with capped exponential backoff + jitter so
  // a tab whose daemon restarted self-heals without a manual page reload.
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

  // The PARENT is the sole path authority. The iframe reports a clicked link's
  // target as a STRING; the shell asks the server to re-confine it (server-side
  // authority — the iframe never gets path authority), then destroys + recreates
  // the iframe with a fresh /srcdoc for the new doc. Escapes -> the /outside path.
  async function navigate(target) {{
    if (typeof target !== "string" || !target) return;
    // Absolute in-root paths are tracked as the canonical `path=`; the server
    // re-confine is authoritative. We relay the click target the renderer saw;
    // the server resolves + confines it relative to the current document's root.
    let res;
    try {{
      res = await fetch("./navigate?path=" + encodeURIComponent(resolveTarget(target)), {{
        credentials: "same-origin",
      }});
    }} catch (e) {{ console.error("[shell]", e); return; }}
    if (res.status === 204) {{
      // In-root: re-point the shell URL and mount a fresh iframe for the doc.
      const next = resolveTarget(target);
      setPathInUrl(next);
      mount(next);
    }} else {{
      // Escape / sensitive / missing -> the /outside sentinel (server refused).
      location.href = "/outside";
    }}
  }}

  // Resolve a renderer-reported link target against the CURRENT document path so
  // the server receives an absolute candidate to re-confine. Already-absolute
  // targets pass through; relative ones are joined to the current doc's dir.
  // (The server is still the sole authority — this is just to form the query.)
  function resolveTarget(target) {{
    if (target.startsWith("/")) return target;
    const dir = currentPath ? currentPath.replace(/\/[^/]*$/, "") : "";
    return dir + "/" + target;
  }}

  function setPathInUrl(path) {{
    const u = new URL(location.href);
    u.searchParams.set("path", path);
    history.replaceState(null, "", u.toString());
  }}

  // The iframe asked to save the current document. The parent relays the content
  // to the authenticated same-origin /save endpoint (carrying the cookie). Edits
  // are SCOPED to the currently tracked path — the iframe cannot name another.
  async function saveCurrent(content) {{
    if (!currentPath || typeof content !== "string") return;
    try {{
      await fetch("./save?path=" + encodeURIComponent(currentPath), {{
        method: "POST",
        credentials: "same-origin",
        headers: {{ "content-type": "text/plain; charset=utf-8" }},
        body: content,
      }});
    }} catch (e) {{ console.error("[shell]", e); }}
  }}

  // Validated inbound from the iframe (delivered ONLY over the channel port).
  function handleRendererMessage(msg) {{
    if (!msg || typeof msg.type !== "string") return;
    switch (msg.type) {{
      case "rendered":
        clearTimeout(watchdog);               // ack: cancel the teardown timer
        break;
      case "save":
        saveCurrent(msg.content);             // scoped to the tracked doc
        break;
      case "navigate":
        navigate(msg.target);                 // server re-confine + remount
        break;
      case "error":
        console.error("[renderer]", msg.message);
        break;
    }}
  }}

  // The shell drives itself on load from the ?path= in its OWN (trusted) URL.
  function start() {{
    const path = new URLSearchParams(location.search).get("path");
    if (path) mount(path);
  }}

  // Exposed for tests / manual driving; the page also self-starts on load.
  window.__mdPreviewShell = {{
    loadDocument, subscribe, postRender, teardownFrame, mount, navigate, saveCurrent, start,
  }};

  if (document.readyState === "loading") {{
    document.addEventListener("DOMContentLoaded", start);
  }} else {{
    start();
  }}
}})();
</script>
</body>
</html>"#,
        sandbox = IFRAME_SANDBOX,
    )
}

/// Build the **sandboxed renderer document** (the iframe `srcdoc`).
///
/// `static_origin`/`asset_origin` are forwarded to [`renderer_csp`]. A **fresh
/// nonce is minted per call** ([`gen_nonce`]) for the inline bootstrap script, and
/// the matching CSP is embedded as a `<meta http-equiv>` so the policy travels with
/// the srcdoc (a srcdoc document has no response headers of its own).
///
/// The bootstrap is intentionally tiny: it first receives its **private
/// `MessageChannel` port** from the parent (a one-shot `{type:"__port"}` window
/// message carrying the transferred port — the *only* `window`-level message it
/// trusts, and only when it arrives from `parent` with a port attached). From then
/// on all traffic flows over that port: it waits for `{type:"render", html}`,
/// injects the body, and posts `{type:"rendered"}` (or `{type:"error", message}`)
/// back **over the port**. Save / navigate intents are reported up the port — the
/// renderer never resolves a path or touches the network (`connect-src 'none'`).
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
  // The dedicated MessageChannel port to the parent. Until it arrives there is no
  // bus at all (and never any network). Reply ONLY over this port — never
  // event.origin ("null" for an opaque sandbox) and never a broadcast postMessage.
  let port = null;

  function toParent(msg) {{ if (port) port.postMessage(msg); }}

  // Render a body fragment handed down over the port and ack it.
  function render(msg) {{
    if (!msg || msg.type !== "render" || typeof msg.html !== "string") return;
    try {{
      document.getElementById("doc").innerHTML = msg.html;
      toParent({{ type: "rendered" }});         // ack -> cancels parent watchdog
    }} catch (e) {{
      toParent({{ type: "error", message: String(e && e.message || e) }});
    }}
  }}

  // ONE-SHOT port handshake. The only window-level message we honour is the
  // parent handing us our private port; we bind the bus to it and stop listening
  // at the window level. We require ev.source === parent AND a transferred port
  // (the parent is the embedder; an opaque origin can't be checked, so the
  // source + the unforgeable transferred port ARE the authentication).
  window.addEventListener("message", function onPort(ev) {{
    if (ev.source !== parent) return;
    const msg = ev.data;
    if (!msg || msg.type !== "__port" || !ev.ports || !ev.ports[0]) return;
    window.removeEventListener("message", onPort);
    port = ev.ports[0];
    port.onmessage = (e) => render(e.data);
    if (port.start) port.start();
  }});

  // Link clicks are reported up the port; the parent re-confines the target and
  // remounts a fresh iframe. The renderer never navigates itself.
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

/// Length of a CSP nonce in base64url characters. 16 chars ≈ 96 bits of
/// entropy — more than sufficient for a per-page-load CSP nonce.
const NONCE_LEN: usize = 16;

/// Mint a fresh CSP nonce: a URL-safe-no-pad token (`A–Z a–z 0–9 - _`) valid
/// inside a `'nonce-…'` source and an HTML attribute without escaping.
///
/// Consolidated onto [`crate::auth::generate_token`] (the shared 256-bit
/// CSPRNG/base64url token helper) and truncated to [`NONCE_LEN`] chars, so there
/// is one token generator in the daemon (audit C §1h). No-panic: if the OS RNG
/// is unavailable we fall back to a non-secret placeholder so this builder never
/// aborts production (a missing OS RNG is fatal to the daemon elsewhere; this is
/// not the layer that should panic).
pub fn gen_nonce() -> String {
    match crate::auth::generate_token() {
        // The token is ≥ 43 base64url chars; take a NONCE_LEN-char prefix. Every
        // character is from the same URL-safe alphabet, so the prefix is itself a
        // valid CSP nonce / HTML-attribute value.
        Ok(mut token) => {
            token.truncate(NONCE_LEN);
            token
        }
        Err(_) => "nonce-unavailable".to_string(),
    }
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
        // The bus is a dedicated MessageChannel port (validated by port identity,
        // NOT by event.origin — an opaque sandbox origin is the string "null").
        assert!(page.contains("new MessageChannel()"));
        assert!(page.contains("channel.port1.onmessage"));
        // The private port (port2) is transferred to the iframe on load.
        assert!(page.contains("[channel.port2]"));
        // The shell must never GATE inbound iframe messages on the origin (it is
        // the string "null" for an opaque sandbox). The bus validation is the port
        // identity above; assert no origin-comparison anti-pattern is present.
        assert!(
            !page.contains(".origin ===") && !page.contains(".origin =="),
            "shell must validate by port, never by comparing origin"
        );
    }

    #[test]
    fn shell_bootstrap_wires_content_navigate_save_and_ws() {
        let page = render_shell_page(STATIC, "n");
        // Bootstrap: reads ?path= from its OWN (trusted) URL and mounts.
        assert!(page.contains(r#"new URLSearchParams(location.search).get("path")"#));
        // Authenticated same-origin content fetch (cookie carried -> SameSite=Strict).
        assert!(page.contains(r#"fetch("./content?path="#));
        assert!(page.contains(r#"credentials: "same-origin""#));
        // A FRESH /srcdoc is fetched per mount (fresh nonce per navigation).
        assert!(page.contains(r#"fetch("./srcdoc""#));
        // navigate() is server-authoritative: it hits /navigate, never resolves
        // the path in the iframe.
        assert!(page.contains(r#"fetch("./navigate?path="#));
        // 204 from /navigate => in-root: remount; otherwise the /outside sentinel.
        assert!(page.contains("204"));
        assert!(page.contains("/outside"));
        // saveCurrent() relays to the same-origin /save endpoint, scoped to the
        // CURRENTLY tracked path (the iframe can never name another doc).
        assert!(page.contains(r#"fetch("./save?path="#));
        assert!(page.contains(r#"method: "POST""#));
        assert!(page.contains("currentPath"));
        // Live updates subscribe over the same-origin WS and relay as {render}.
        assert!(page.contains("/ws?path="));
    }

    #[test]
    fn shell_handles_full_renderer_message_schema() {
        let page = render_shell_page(STATIC, "n");
        // The inbound dispatcher handles every iframe->parent message type.
        for ty in [r#"case "rendered""#, r#"case "save""#, r#"case "navigate""#, r#"case "error""#] {
            assert!(page.contains(ty), "shell must handle {ty}");
        }
        // The rendered ack cancels the watchdog teardown timer.
        assert!(page.contains("clearTimeout(watchdog)"));
    }

    #[test]
    fn shell_watchdog_tears_down_unacked_iframe() {
        let page = render_shell_page(STATIC, "n");
        // An un-acked iframe is rebuilt after RENDER_WATCHDOG_MS.
        assert!(page.contains("armWatchdog"));
        assert!(page.contains("setTimeout"));
        assert!(page.contains("teardownFrame"));
        // The watchdog re-mounts the current path (disposable iframe).
        assert!(page.contains("if (currentPath) mount(currentPath)"));
    }

    #[test]
    fn srcdoc_binds_bus_to_transferred_port_not_window() {
        let doc = render_srcdoc(STATIC, ASSET);
        // The renderer trusts ONLY the one-shot port handshake from the parent and
        // then talks over the transferred port — never a broadcast postMessage.
        assert!(doc.contains(r#"msg.type !== "__port""#));
        assert!(doc.contains("ev.ports[0]"));
        assert!(doc.contains("ev.source !== parent"));
        assert!(doc.contains("port.postMessage"));
        // It must NOT reply via an unscoped parent.postMessage broadcast.
        assert!(
            !doc.contains("parent.postMessage"),
            "renderer must reply only over its dedicated port"
        );
        // After binding the port it stops listening at the window level (one-shot).
        assert!(doc.contains("removeEventListener"));
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
