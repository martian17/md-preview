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
/// - `style-src 'unsafe-inline' {static}` — inline styles + bundle CSS. We use
///   **`'unsafe-inline'` with NO nonce on style** (deliberately, unlike `script`),
///   because a `srcdoc` iframe **inherits the embedder's CSP** and the untrusted
///   renderer body (KaTeX output, mermaid per-node fills, `<math-renderer style=…>`)
///   carries inline `style="…"` *attributes* that nonces cannot cover — AND per
///   the CSP spec a nonce in `style-src` makes `'unsafe-inline'` **ignored even
///   for those attributes** (a style nonce would self-defeat and block them). So
///   style is allowed inline; **`script` stays strictly nonce-pinned** (no
///   `'unsafe-inline'` there — that is the load-bearing XSS boundary). This is the
///   ONE deliberate relaxation vs. the original (style only, never script);
///   security re-review item. The trusted shell page emits only one inline
///   `<style>` (its own chrome) and zero untrusted style; the permissiveness
///   exists so the inherited policy lets the sandboxed renderer's markup style.
/// - `font-src {static}` / `img-src {static}` — fonts and chrome images come from
///   the bundle origin. Inherited by the srcdoc: KaTeX fonts (`{static}/bundle/
///   fonts/…`) and the renderer's per-doc capability images (`{static}/cap/…`,
///   same static origin) load through this inherited allowance.
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
         style-src 'unsafe-inline' {s}; \
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
    // `static_origin` is still part of the public signature (the shell's CSP is
    // built from it by the caller) but the shell page itself no longer embeds a
    // cross-origin <link>: its chrome CSS is inline + nonce'd (see <style> below).
    let _ = static_origin;
    let n = escape_attr(nonce);
    let watchdog = RENDER_WATCHDOG_MS;
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>md-preview</title>
<style>
/* Trusted-shell chrome only (NO document styling — that lives in the sandboxed
   renderer). Inline: the shell CSP allows `style-src 'unsafe-inline'` (style, not
   script), so we need no cross-origin shell stylesheet (there is no such bundle
   asset, and a cross-origin link would need CORP handling for nothing). */
html, body {{ margin: 0; height: 100%; background: #ffffff; }}
@media (prefers-color-scheme: dark) {{ html, body {{ background: #0d1117; }} }}
#render-host, #render-frame {{ display: block; border: 0; width: 100%; height: 100vh; }}
</style>
</head>
<body>
<!-- The untrusted renderer iframe is created/destroyed per navigation by the
     bootstrap (a fresh srcdoc per navigation). It always carries
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
  // This page's CSP nonce. A srcdoc iframe INHERITS this page's CSP, so the
  // srcdoc's inline bootstrap <script>/<style> must carry THIS nonce to pass the
  // inherited policy; we thread it to /srcdoc (?n=) on every mount.
  const SHELL_NONCE = "{n}";
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

  // Fetch a FRESH /srcdoc for each mount, so every navigation gets a brand-new
  // null-origin sandbox with no shared state. We pass this page's nonce so the
  // srcdoc's inline bootstrap carries the nonce the INHERITED shell CSP allows
  // (the srcdoc inherits + enforces this page's CSP atop its own <meta> CSP).
  async function fetchSrcdoc() {{
    const res = await fetch("./srcdoc?n=" + encodeURIComponent(SHELL_NONCE), {{
      credentials: "same-origin",
    }});
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

    // A MessagePort can be transferred EXACTLY ONCE — a second transfer throws
    // "Port already neutered". The iframe `load` event can fire more than once, so
    // guard the transfer with a per-mount latch so port2 is handed over once and
    // only once. CRUCIAL ordering: set `srcdoc` BEFORE appending to the DOM, so the
    // ONLY load this frame ever fires is the srcdoc document itself — appending an
    // iframe with no `src`/`srcdoc` first would fire a load for the throwaway
    // `about:blank`, and the once-latch would then burn the transfer on that
    // (wrong) document, leaving the real srcdoc bootstrap waiting for a port that
    // never arrives. With srcdoc set first, `load` == the renderer is ready.
    let portSent = false;
    frame.addEventListener("load", () => {{
      // Hand the iframe its private port (transfer port2). The srcdoc keeps it
      // and replies only over it.
      if (!frame || !channel || portSent) return;
      portSent = true;
      frame.contentWindow.postMessage({{ type: "__port" }}, "*", [channel.port2]);
    }});

    frame.srcdoc = srcdoc;
    host.appendChild(frame);

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

  // Resolve a renderer-reported link target into the absolute candidate PATH the
  // server should re-confine. The server is still the SOLE authority — this only
  // forms the `path=` query; it never grants the iframe path authority.
  //
  // In-root `.md` links are rewritten server-side to `/view?path=<abs>` SHELL
  // links (see rewrite_doc_caps), so the renderer reports that whole string. We
  // UNWRAP `/view?path=<p>` back to its `<p>` (decoded) — otherwise we'd send the
  // server `/navigate?path=/view?path=…`, which can't confine and bounces every
  // cross-link to /outside. Already-absolute targets pass through; relative ones
  // are joined to the current doc's dir.
  function resolveTarget(target) {{
    // Unwrap a server-rewritten shell link `/view?path=<p>` (possibly with a
    // #fragment) to the bare confined path it points at.
    const m = /^\/view\?path=([^&#]*)/.exec(target);
    if (m) {{
      try {{ return decodeURIComponent(m[1]); }} catch (e) {{ return m[1]; }}
    }}
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
/// `static_origin`/`asset_origin` are forwarded to [`renderer_csp`]. `nonce` is
/// the **shell page's nonce**, passed in (NOT minted here): a `srcdoc` iframe
/// **inherits the embedder shell's CSP** and enforces it on top of its own
/// `<meta>` CSP, so the srcdoc's inline `<script>`/`<style>` MUST carry the exact
/// nonce the inherited shell policy (`'nonce-…'`) allows, or they are blocked and
/// the renderer never boots. The shell mints the nonce once per page load and
/// threads it to `/srcdoc` (`?n=`), so every per-navigation srcdoc this shell
/// mounts shares that one nonce. The same nonce is embedded in the srcdoc's own
/// `<meta>` CSP (via [`renderer_csp`]) so both stacked policies permit the inline
/// bootstrap. The renderer is still a fresh null-origin iframe per navigation
/// (fresh MessageChannel, no shared JS state); only the inline-auth nonce — whose
/// sole job is to vouch for the *trusted* bootstrap markup — is shared.
///
/// The bootstrap is intentionally tiny: it first receives its **private
/// `MessageChannel` port** from the parent (a one-shot `{type:"__port"}` window
/// message carrying the transferred port — the *only* `window`-level message it
/// trusts, and only when it arrives from `parent` with a port attached). From then
/// on all traffic flows over that port: it waits for `{type:"render", html}`,
/// injects the body, and posts `{type:"rendered"}` (or `{type:"error", message}`)
/// back **over the port**. Save / navigate intents are reported up the port — the
/// renderer never resolves a path or touches the network (`connect-src 'none'`).
pub fn render_srcdoc(static_origin: &str, asset_origin: &str, nonce: &str) -> String {
    let csp = escape_attr(&renderer_csp(static_origin, asset_origin, nonce));
    let s = escape_attr(static_origin);
    let n = escape_attr(nonce);
    // The document chrome + syntax-highlight CSS (the `.markdown-body`, copy-button
    // and syntect `.hl-` rules that `render_page` bakes into a standalone doc),
    // inlined into a plain `<style>` (NO nonce). Both the inherited shell CSP and
    // the renderer's own <meta> CSP use `style-src 'unsafe-inline' {static}` with
    // no style nonce, so this `<style>` element AND the rendered body's inline
    // style ATTRIBUTES (KaTeX spacing, mermaid per-node fill/stroke) are allowed.
    // (A style nonce would self-defeat: CSP ignores 'unsafe-inline' when a nonce is
    // present, blocking the attributes.) The KaTeX + github-markdown stylesheets
    // come cross-origin from the bundle origin (CORP under the embedder's COEP).
    let doc_css = document_css();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="{csp}">
<link rel="stylesheet" href="{s}/bundle/github-markdown.css">
<link rel="stylesheet" href="{s}/bundle/katex.min.css">
<style>{doc_css}</style>
</head>
<body>
<!-- `.markdown-body` is the github-markdown-css scope (same wrapper class
     `render_page` uses around the body). The parent swaps this node's innerHTML
     per render; the bootstrap re-runs mermaid/KaTeX against the swapped nodes. -->
<div id="doc" class="markdown-body"></div>
<!-- Origin-pinned render libs, loaded cross-origin from the BUNDLE origin only
     (governed by `script-src 'nonce-…' {{static}}`; never `connect-src`, which
     stays 'none'). These are the UMD single-file builds => `window.mermaid` /
     `window.katex`. Loaded NO-CORS: the bundle's `Cross-Origin-Resource-Policy:
     cross-origin` satisfies the embedder COEP, and since the bundle sends no
     `Access-Control-Allow-Origin` a CORS load would fail. They self-register the
     globals the bootstrap below drives. -->
<script nonce="{n}" src="{s}/bundle/mermaid.min.js"></script>
<script nonce="{n}" src="{s}/bundle/katex.min.js"></script>
<script nonce="{n}">
// Sandboxed renderer bootstrap (null origin, connect-src 'none' => zero egress).
// Receives body HTML from the parent over postMessage; never fetches or resolves
// paths itself (the parent is the sole path authority). The only network it does
// is the `<script>`/`<link>` bundle loads above, governed by script-src/style-src
// (the bundle origin) — connect-src stays 'none', so no fetch/XHR/socket is ever
// possible from here.
(() => {{
  "use strict";
  // The dedicated MessageChannel port to the parent. Until it arrives there is no
  // bus at all (and never any network). Reply ONLY over this port — never
  // event.origin ("null" for an opaque sandbox) and never a broadcast postMessage.
  let port = null;
  const docEl = document.getElementById("doc");

  function toParent(msg) {{ if (port) port.postMessage(msg); }}

  // --- client-side render of the swapped body (mermaid + KaTeX, from the bundle).
  // Both libs are loaded as classic UMD scripts above; on first render they may
  // not have parsed yet, so the body is injected + ack'd immediately (text shows
  // and the parent watchdog is cancelled) and the heavy render runs whenever the
  // libs are ready AND re-runs on every subsequent body swap.

  // Mermaid: initialise once (theme follows the system), then run against any
  // not-yet-processed `<pre class="mermaid">` nodes in the freshly swapped body.
  let mermaidReady = false;
  function ensureMermaid() {{
    if (mermaidReady || typeof window.mermaid === "undefined") return mermaidReady;
    try {{
      const dark = window.matchMedia
        && window.matchMedia("(prefers-color-scheme: dark)").matches;
      window.mermaid.initialize({{ startOnLoad: false, theme: dark ? "dark" : "default" }});
      mermaidReady = true;
    }} catch (e) {{ /* leave not-ready; a later render retries */ }}
    return mermaidReady;
  }}
  function runMermaid() {{
    if (!ensureMermaid()) return;
    const nodes = docEl.querySelectorAll("pre.mermaid:not([data-processed])");
    if (nodes.length) {{
      try {{ window.mermaid.run({{ nodes }}); }} catch (e) {{ /* malformed graph */ }}
    }}
  }}

  // KaTeX: typeset each `<math-renderer>` (the custom element `render_markdown`
  // emits) in place, display vs inline per its `js-display-math` class.
  function runKaTeX() {{
    if (typeof window.katex === "undefined") return;
    const nodes = docEl.querySelectorAll("math-renderer:not([data-typeset])");
    nodes.forEach((el) => {{
      const tex = el.textContent.trim();
      const display = el.classList.contains("js-display-math");
      try {{
        window.katex.render(tex, el, {{ displayMode: display, throwOnError: false }});
        el.setAttribute("data-typeset", "");
      }} catch (e) {{ /* leave the raw TeX visible */ }}
    }});
  }}

  function renderRich() {{ runMermaid(); runKaTeX(); }}

  // Render a body fragment handed down over the port: swap it in, ack at once
  // (cancels the parent watchdog), then run the rich render. If the libs are not
  // parsed yet, their `load` handlers below re-run renderRich once they are.
  function render(msg) {{
    if (!msg || msg.type !== "render" || typeof msg.html !== "string") return;
    try {{
      docEl.innerHTML = msg.html;
      toParent({{ type: "rendered" }});         // ack -> cancels parent watchdog
      renderRich();
    }} catch (e) {{
      toParent({{ type: "error", message: String(e && e.message || e) }});
    }}
  }}

  // If a lib finishes loading AFTER the first body swap, render the now-present
  // body. (Scripts may still be parsing when the first {{render}} arrives.)
  function onLibLoad() {{ renderRich(); }}
  document.querySelectorAll('script[src]').forEach((sc) => {{
    sc.addEventListener("load", onLibLoad);
  }});

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
  // remounts a fresh iframe. The renderer never navigates itself. The copy-button
  // click is handled locally (clipboard write of the sibling code), never up the
  // port — it is a same-frame UI action, not a navigation.
  document.addEventListener("click", (ev) => {{
    const btn = ev.target.closest && ev.target.closest(".copy-btn");
    if (btn) {{
      const code = btn.parentElement && btn.parentElement.querySelector("pre code");
      try {{
        navigator.clipboard && navigator.clipboard.writeText(code ? code.textContent : "");
        btn.classList.add("copied");
        setTimeout(() => btn.classList.remove("copied"), 2000);
      }} catch (e) {{ /* clipboard unavailable in a null-origin frame: ignore */ }}
      return;
    }}
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

/// The document chrome + syntax-highlight CSS the sandboxed renderer inlines:
/// the `.markdown-body` layout, the copy-button styling, and the syntect `.hl-`
/// highlight rules (light/dark) — exactly the `<style>` block
/// [`md_render::render_page`] bakes into a standalone document. We slice it out of
/// a one-time `render_page("")` render (cached in a [`OnceLock`]) so the renderer
/// stays byte-identical to the canonical standalone styling without duplicating
/// the syntect theme generation here. It carries **no** CDN/`<link>` refs (those
/// live outside the `<style>` block); the cross-origin stylesheets (KaTeX +
/// github-markdown) are loaded from the bundle origin instead.
fn document_css() -> &'static str {
    use std::sync::OnceLock;
    static CSS: OnceLock<String> = OnceLock::new();
    CSS.get_or_init(|| {
        let page = crate::render_page("");
        // The single `<style>…</style>` block in `render_page`'s output holds the
        // chrome + syntax CSS. Slice it; if the upstream layout ever changes the
        // empty fallback degrades gracefully to "unstyled but functional".
        match (page.find("<style>"), page.find("</style>")) {
            (Some(a), Some(b)) if b > a => page[a + "<style>".len()..b].to_string(),
            _ => String::new(),
        }
    })
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
        // SCRIPT must NEVER carry 'unsafe-inline' (scripts stay strictly nonce-pinned;
        // that is the load-bearing XSS boundary).
        assert!(
            !csp.contains("script-src 'nonce-abc123' http://127.0.0.1:7001 'unsafe-inline'"),
            "script-src must never allow unsafe-inline"
        );
        // STYLE uses 'unsafe-inline' with NO nonce — REQUIRED so the inherited shell
        // CSP lets the sandboxed renderer's body STYLE ATTRIBUTES (KaTeX spacing,
        // mermaid per-node fills, <math-renderer style=>) apply. A style nonce would
        // self-defeat: CSP ignores 'unsafe-inline' when a nonce is present, blocking
        // those attributes. Deliberate, documented (style only, never script).
        assert!(
            csp.contains("style-src 'unsafe-inline' http://127.0.0.1:7001"),
            "style-src must be 'unsafe-inline' {{static}} with no style nonce: {csp}"
        );
        assert!(
            !csp.contains("style-src 'nonce-"),
            "style-src must carry NO nonce (it would disable 'unsafe-inline')"
        );
        // The single 'unsafe-inline' is ONLY in style-src — never where script runs.
        assert_eq!(csp.matches("'unsafe-inline'").count(), 1);
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
        // The shell chrome CSS is INLINE (style-src 'unsafe-inline'; no cross-origin
        // shell.css, which the daemon never served — old broken <link> 404'd+COEP-blocked).
        assert!(page.contains("<style>"));
        assert!(!page.contains("shell.css"));
        // The port is transferred EXACTLY ONCE per mount (the load event can fire
        // more than once with srcdoc → "Port already neutered"); guard with a latch.
        assert!(page.contains("portSent"));
        // CRUCIAL ordering: `srcdoc` is set BEFORE the frame is appended, so the
        // only `load` the frame fires is the real srcdoc document — not a throwaway
        // about:blank that would burn the once-latch on the wrong document and leave
        // the renderer's bootstrap waiting for a port that never arrives.
        let srcdoc_at = page.find("frame.srcdoc = srcdoc").expect("sets srcdoc");
        let append_at = page.find("host.appendChild(frame)").expect("appends frame");
        assert!(
            srcdoc_at < append_at,
            "frame.srcdoc must be set BEFORE host.appendChild (single, correct load)"
        );
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
        // A FRESH /srcdoc is fetched per mount, carrying THIS page's nonce so the
        // srcdoc's inline bootstrap passes the inherited shell CSP.
        assert!(page.contains(r#"fetch("./srcdoc?n=""#));
        assert!(page.contains("SHELL_NONCE"));
        // navigate() is server-authoritative: it hits /navigate, never resolves
        // the path in the iframe.
        assert!(page.contains(r#"fetch("./navigate?path="#));
        // 204 from /navigate => in-root: remount; otherwise the /outside sentinel.
        assert!(page.contains("204"));
        assert!(page.contains("/outside"));
        // A server-rewritten in-root `.md` link is `/view?path=<abs>`; resolveTarget
        // must UNWRAP that to the bare path before forming the /navigate query (else
        // every cross-link double-wraps to /navigate?path=/view?path=… and bounces
        // to /outside). Assert the unwrap is present.
        assert!(page.contains(r#"/^\/view\?path="#));
        assert!(page.contains("decodeURIComponent"));
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
        let doc = render_srcdoc(STATIC, ASSET, "n0nce");
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
    fn srcdoc_embeds_renderer_csp_and_the_shell_nonce() {
        let doc = render_srcdoc(STATIC, ASSET, "shellNonce123");
        assert!(doc.starts_with("<!DOCTYPE html>"));
        // CSP travels in the srcdoc via meta http-equiv (no response headers).
        assert!(doc.contains(r#"<meta http-equiv="Content-Security-Policy""#));
        assert!(doc.contains("connect-src 'none'"));
        // Renderer reports navigation up rather than navigating itself.
        assert!(doc.contains(r#"type: "navigate""#));
        assert!(doc.contains(r#"type: "rendered""#));
        // The srcdoc carries the SHELL's nonce (passed in, NOT minted here) on its
        // inline <script> AND in its own <meta> CSP `script-src` — so the inline
        // bootstrap passes BOTH the inherited shell CSP and its own policy. (Style
        // is 'unsafe-inline' with NO nonce — see srcdoc_runs_mermaid_and_katex.)
        assert!(doc.contains(r#"<script nonce="shellNonce123">"#));
        assert!(doc.contains("<style>"), "the inline <style> carries no nonce");
        // The <meta> CSP is attribute-escaped, so the quotes render as &#x27;.
        assert!(
            doc.contains("&#x27;nonce-shellNonce123&#x27;")
                || doc.contains("'nonce-shellNonce123'")
        );
    }

    #[test]
    fn srcdoc_loads_bundle_assets_from_the_real_served_paths() {
        let doc = render_srcdoc(STATIC, ASSET, "n0nce");
        // The OLD broken markup linked `…/bundle.css` (a 404, no such route). The
        // corrected srcdoc links the REAL bundle paths served at `/bundle/<id>`.
        assert!(
            !doc.contains("/bundle.css\""),
            "must not link the non-existent /bundle.css"
        );
        assert!(doc.contains(&format!(r#"href="{STATIC}/bundle/github-markdown.css""#)));
        assert!(doc.contains(&format!(r#"href="{STATIC}/bundle/katex.min.css""#)));
        // The render libs load from the bundle origin (UMD single-file builds).
        assert!(doc.contains(&format!(r#"src="{STATIC}/bundle/mermaid.min.js""#)));
        assert!(doc.contains(&format!(r#"src="{STATIC}/bundle/katex.min.js""#)));
        // No `crossorigin` on the bundle subresources: a no-cors load is satisfied
        // by the bundle's CORP under COEP; the bundle sends no ACAO, so a CORS
        // (crossorigin) load would FAIL. Keep them no-cors.
        assert!(
            !doc.contains("crossorigin"),
            "bundle subresources load no-cors (CORP), never crossorigin/CORS"
        );
    }

    #[test]
    fn srcdoc_runs_mermaid_and_katex_after_each_body_swap() {
        let doc = render_srcdoc(STATIC, ASSET, "n0nce");
        // After innerHTML swap the bootstrap drives the render libs.
        assert!(doc.contains("mermaid.run"), "must invoke mermaid.run");
        assert!(doc.contains("katex.render"), "must invoke katex.render");
        // Re-runs on every swap (renderRich is called from render()).
        assert!(doc.contains("renderRich"));
        // Targets the markup the renderer emits: pre.mermaid + <math-renderer>.
        assert!(doc.contains("pre.mermaid"));
        assert!(doc.contains("math-renderer"));
        // The document is scoped with the github-markdown `.markdown-body` class.
        assert!(doc.contains(r#"id="doc" class="markdown-body""#));
        // The chrome + syntect highlight CSS is inlined in a plain <style> (no
        // nonce: style-src is 'unsafe-inline' so the body's inline style ATTRIBUTES
        // — mermaid fills, KaTeX spacing — also apply; a style nonce would block them).
        assert!(doc.contains("<style>"));
        assert!(!doc.contains("<style nonce"), "style carries no nonce");
        assert!(
            doc.contains(".hl-") || doc.contains("copy-btn"),
            "inlined document CSS (syntax/chrome) must be present"
        );
        // Inlining the CSS does NOT loosen the renderer CSP: still zero-egress.
        assert!(doc.contains("connect-src 'none'"));
    }

    #[test]
    fn document_css_extracts_chrome_and_syntax_css() {
        let css = document_css();
        assert!(!css.is_empty(), "document CSS must be extracted from render_page");
        // It carries the chrome + syntax rules but NO CDN <link>/import refs.
        assert!(css.contains("markdown-body") || css.contains("copy-btn"));
        assert!(!css.contains("cdn"), "inlined CSS must carry no CDN refs");
    }

    #[test]
    fn srcdoc_uses_the_passed_in_shell_nonce_verbatim() {
        // The srcdoc no longer mints its own nonce: it must use the SHELL nonce the
        // caller passes (so it satisfies the inherited shell CSP). Same nonce in =>
        // identical docs; different nonces => different docs (and each appears).
        let a = render_srcdoc(STATIC, ASSET, "AAAA1111");
        let a2 = render_srcdoc(STATIC, ASSET, "AAAA1111");
        let b = render_srcdoc(STATIC, ASSET, "BBBB2222");
        assert_eq!(a, a2, "deterministic in the nonce — no internal minting");
        assert_ne!(a, b);
        assert!(a.contains("AAAA1111") && !a.contains("BBBB2222"));
        assert!(b.contains("BBBB2222") && !b.contains("AAAA1111"));
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
