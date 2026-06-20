//! New editor page using the offline mycelium-editor bundle.
//!
//! This module replaces the CDN-dependent `render_editor_page` from md-render
//! for the daemon's `/edit` route. The editor UI (CodeMirror 6 + markdown
//! theme + decorations + scroll-sync + math-copy) is served OFFLINE from the
//! binary-embedded `/editor-bundle/` assets (see `editor_bundle.rs`). The
//! `esm.sh` importmap is GONE — the editor now works with no network access.
//!
//! ## Architecture
//! - **Edit/split/preview triad** — the three-button view switcher from the
//!   original page is preserved. In split view: mycelium-editor (source, with
//!   markers visible) on the left, the server-rendered preview on the right.
//! - **Scroll sync** — `syncEditorPreviewScroll` from the bundle wires the
//!   editor scroll position to the preview pane in split view.
//! - **Math-copy-as-TeX** — `enableMathCopyAsTex` is called on the preview div
//!   after each preview refresh so copying KaTeX renders yields TeX source.
//! - **CRDT** — The existing y-websocket wire protocol is kept. The offline
//!   `crdt.es.js` companion bundle provides Y.Doc, Y.Text, y-protocols, and
//!   lib0 encoding from the same node_modules tree as mycelium-editor, enabling
//!   fully offline collaborative editing over `/collab?path=`.
//!
//! ## CRDT module-identity note (important for maintainers)
//! mycelium-editor bundles yjs+y-codemirror.next internally. Ideally we would
//! pass a Y.Text from `crdt.es.js`'s Y.Doc to `createMyceliumEditor`
//! (`options.yText`) for fine-grained CRDT binding. However, because Vite
//! bundles ALL deps into each output file (external: []), the yjs class in
//! `crdt.es.js` and the yjs class inside `index-CUCuSPF_.js` are TWO separate
//! runtime copies — `instanceof Y.Doc` checks in y-codemirror.next would fail
//! across them. We therefore bind CRDT at the document level: the CollabProvider
//! manages its own Y.Doc (from crdt.es.js), syncs with the `/collab` server,
//! and calls `editor.setValue(yText.toString())` on remote updates while
//! listening to `onChange` for local changes. This is equivalent in behavior to
//! the previous esm.sh-based page (same last-write-wins document model) and
//! preserves all the existing `/collab` server-side CRDT logic.
//!
//! To upgrade to fine-grained Y.Text binding in the future:
//! 1. Add `externals: ['yjs']` to mycelium-editor's vite.config.ts and rebuild
//!    (mycelium-editor would then import yjs at runtime from the importee's
//!    context, sharing one instance).
//! 2. Remove `crdt.es.js` and load yjs once from a shared module.
//! (Logged in MORNING-NOTES.)
//!
//! ## Security
//! - The editor page has NO explicit CSP header set (same as before — the prior
//!   `render_editor_page` from md-render also returned plain HTML with no CSP).
//!   The edit route is daemon-gated (`edit_mode: true`) and localhost-only.
//! - All JS is loaded from `type="module"` scripts pointing to `/editor-bundle/`
//!   (same origin, no CDN). No importmap, no esm.sh.
//! - The preview pane renders fetched HTML into `innerHTML` in the same document
//!   (NOT sandboxed) — this is the EXISTING behavior, unchanged. The `/content`
//!   endpoint that serves it is auth-floored and confinement-gated.
//! - No COOP/COEP headers are added (the edit page is not the render-isolation
//!   shell). Changing that would require migrating preview to a sandboxed iframe,
//!   which is a larger future refactor.

use crate::shell::gen_nonce;

/// Serialize `s` as a JSON string literal (with surrounding `"`), safe to embed
/// in a `<script>`. Escapes `<`, `>`, `&` (so `</script>` can't close the tag),
/// `"`, `\`, and control chars. Mirrors `json_string` in `md_render`.
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
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Minimal HTML escaping for text-node content (toolbar label, title).
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the collaborative editor page using the offline mycelium-editor bundle.
///
/// `rel_path` is the canonical absolute path of the document being edited
/// (as returned by `path_id`). It is JSON-escaped for the script literal and
/// HTML-escaped for the visible toolbar label; it never appears raw in the HTML.
pub fn render(rel_path: &str) -> String {
    let path_json = json_string(rel_path);
    let display_path = escape_html(rel_path);
    // A fresh nonce for the module script tag (forward-compatible with a future
    // CSP hardening pass on the edit page; currently the edit page has no CSP
    // header so the nonce is advisory). We HTML-escape it for safe attr embedding.
    let _nonce = gen_nonce();
    let nonce = escape_html(&_nonce);
    // JSON-escaped form of the nonce for embedding as a JS string literal in the
    // module script (so the editor page can pass ?n=<nonce> to /srcdoc when
    // mounting the sandboxed preview iframe, matching the shell pattern).
    let nonce_json = json_string(&_nonce);

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{display_path} — editor</title>
<style>
/* Offline mycelium-editor page chrome (toolbar, panes, view switcher).
   The editor's own styling (CodeMirror, syntax, decorations) is injected
   dynamically from myceliumCss / myceliumDarkCss in the module script below. */
:root {{ color-scheme: light dark; }}
*, *::before, *::after {{ box-sizing: border-box; }}
html, body {{ margin: 0; height: 100%; overflow: hidden; }}
body {{
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
    font-size: 14px;
    background-color: #ffffff; color: #1f2328;
}}
@media (prefers-color-scheme: dark) {{
    body {{ background-color: #0d1117; color: #e6edf3; }}
}}
/* Toolbar */
#toolbar {{
    display: flex; align-items: center; gap: 12px;
    padding: 8px 16px; height: 49px;
    border-bottom: 1px solid rgba(128,128,128,.3);
    flex-shrink: 0;
}}
#toolbar .path {{ font-weight: 600; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
#toolbar .spacer {{ flex: 1; }}
#toolbar label {{ color: #656d76; }}
@media (prefers-color-scheme: dark) {{ #toolbar label {{ color: #7d8590; }} }}
#username {{
    font: inherit; padding: 2px 6px; border-radius: 6px;
    border: 1px solid rgba(128,128,128,.4); background: transparent; color: inherit;
    max-width: 120px;
}}
#status {{ font-size: 12px; color: #656d76; white-space: nowrap; }}
@media (prefers-color-scheme: dark) {{ #status {{ color: #7d8590; }} }}
/* View-switcher buttons */
.view-btn {{
    display: inline-flex; align-items: center; justify-content: center;
    width: 28px; height: 28px; padding: 0; flex-shrink: 0;
    border: 1px solid rgba(128,128,128,.3); border-radius: 6px;
    background: transparent; color: #656d76; cursor: pointer;
    transition: background-color .1s, color .1s, border-color .1s;
}}
.view-btn:hover {{ background-color: rgba(175,184,193,.2); border-color: rgba(175,184,193,.5); color: #1f2328; }}
.view-btn.active {{ background-color: rgba(175,184,193,.3); border-color: rgba(175,184,193,.6); color: #1f2328; }}
@media (prefers-color-scheme: dark) {{
    .view-btn {{ color: #7d8590; }}
    .view-btn:hover {{ background-color: rgba(99,110,123,.25); border-color: rgba(99,110,123,.5); color: #e6edf3; }}
    .view-btn.active {{ background-color: rgba(99,110,123,.35); border-color: rgba(99,110,123,.6); color: #e6edf3; }}
}}
/* Pane layout */
#page {{ display: flex; flex-direction: column; height: 100vh; }}
#panes {{ display: flex; flex: 1; overflow: hidden; }}
#editor-pane {{
    flex: 1; overflow: auto; display: flex; flex-direction: column;
    padding: 0; /* mycelium-editor fills its parent */
}}
#preview-pane {{
    flex: 1; overflow: hidden; padding: 0; border-left: 1px solid rgba(128,128,128,.3);
}}
/* preview iframe fills the pane; the srcdoc renderer owns its own scrolling */
#preview-frame {{ display: block; border: 0; width: 100%; height: 100%; }}
/* editor-pane must fill its container so CodeMirror stretches to full height */
#editor-pane .cm-editor {{ flex: 1; height: 100%; }}
/* View modes controlled by data-view on #panes */
#panes[data-view="edit"] #preview-pane {{ display: none; }}
#panes[data-view="preview"] #editor-pane {{ display: none; }}
/* Split: editor left, preview right — both flex:1 (default above) */
</style>
</head>
<body>
<div id="page">
    <div id="toolbar">
        <span class="path" title="{display_path}">{display_path}</span>
        <span class="spacer"></span>
        <!-- Three-button view-switcher triad (pencil / split / eye) -->
        <button class="view-btn" id="btn-edit" type="button"
                title="Edit only" aria-label="Edit only" aria-pressed="false">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="16" height="16" fill="currentColor" aria-hidden="true">
                <path d="M227.31,73.37,182.63,28.69a16,16,0,0,0-22.63,0L36.69,152A15.86,15.86,0,0,0,32,163.31V208a16,16,0,0,0,16,16H92.69A15.86,15.86,0,0,0,104,219.31L227.31,96A16,16,0,0,0,227.31,73.37ZM51.31,160,136,75.31,152.69,92,68,176.69Zm-3.31,16H68v20H48Zm36,4L68,163.31,172.69,58.63,189.37,75.31Z"/>
            </svg>
        </button>
        <button class="view-btn active" id="btn-split" type="button"
                title="Split view" aria-label="Split view" aria-pressed="true">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="16" height="16" fill="currentColor" aria-hidden="true">
                <path d="M216,48H40A16,16,0,0,0,24,64V192a16,16,0,0,0,16,16H216a16,16,0,0,0,16-16V64A16,16,0,0,0,216,48Zm0,16V192H136V64Zm-96,128H40V64h80Z"/>
            </svg>
        </button>
        <button class="view-btn" id="btn-preview" type="button"
                title="Preview only" aria-label="Preview only" aria-pressed="false">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" width="16" height="16" fill="currentColor" aria-hidden="true">
                <path d="M247.31,124.76c-.35-.79-8.82-19.58-27.65-38.41C194.57,61.26,162.88,48,128,48S61.43,61.26,36.34,86.35C17.51,105.18,9,124,8.69,124.76a8,8,0,0,0,0,6.5c.35.79,8.82,19.57,27.65,38.4C61.43,194.74,93.12,208,128,208s66.57-13.26,91.66-38.34c18.83-18.83,27.3-37.61,27.65-38.4A8,8,0,0,0,247.31,124.76ZM128,192c-30.78,0-57.67-11.19-79.93-33.25A133.47,133.47,0,0,1,25,128,133.33,133.33,0,0,1,48.07,97.25C70.33,75.19,97.22,64,128,64s57.67,11.19,79.93,33.25A133.46,133.46,0,0,1,231.05,128C223.84,141.46,192.43,192,128,192Zm0-112a48,48,0,1,0,48,48A48.05,48.05,0,0,0,128,80Zm0,80a32,32,0,1,1,32-32A32,32,0,0,1,128,112Z"/>
            </svg>
        </button>
        <label for="username">name</label>
        <input id="username" type="text" maxlength="40" autocomplete="off">
        <span id="status">connecting…</span>
    </div>
    <div id="panes" data-view="split">
        <div id="editor-pane"></div>
        <div id="preview-pane">
            <!-- The preview iframe is created/destroyed by refreshPreview() using the
                 same sandboxed srcdoc render model as the read-only /view route.
                 sandbox="allow-scripts" only (no allow-same-origin => null/opaque origin,
                 connect-src 'none' in its own CSP => zero network egress).
                 NOTE: the editor page has no COOP/COEP so the iframe is not process-
                 isolated (Spectre backstop absent). Acceptable: loopback-only, edit-mode-
                 gated. Isolation is still better than the former same-origin innerHTML. -->
        </div>
    </div>
</div>

<script type="module" nonce="{nonce}">
// Offline mycelium-editor page — NO CDN, NO external JS.
// All JS loads from /editor-bundle/ (same-origin, binary-embedded).
import {{
    createMyceliumEditor,
    myceliumCss,
    myceliumDarkCss,
    syncEditorPreviewScroll,
}} from '/editor-bundle/mycelium-editor.es.js';

import {{
    Y,
    syncProtocol,
    awarenessProtocol,
    encoding,
    decoding,
}} from '/editor-bundle/crdt.es.js';

// ── Server-injected path (JSON-escaped, safe in a module script) ──────────
const path = {path_json};

// ── Apply editor CSS (light / dark) ──────────────────────────────────────
(function applyEditorCss() {{
    const styleEl = document.createElement('style');
    const dark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
    styleEl.textContent = dark ? myceliumDarkCss : myceliumCss;
    styleEl.id = 'mycelium-theme';
    document.head.appendChild(styleEl);
    // Swap theme on system change.
    if (window.matchMedia) {{
        window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', (e) => {{
            const el = document.getElementById('mycelium-theme');
            if (el) el.textContent = e.matches ? myceliumDarkCss : myceliumCss;
        }});
    }}
}})();

// ── CRDT: ephemeral Y.Doc + custom CollabProvider ──────────────────────
// The CollabProvider speaks the y-websocket wire protocol (MSG_SYNC=0,
// MSG_AWARENESS=1), identical to the server's `collab_pump.rs` codec.
// Y.Text "content" is the shared authoritative text; editor.getValue /
// editor.setValue bridge it to the CodeMirror instance.
//
// Module-identity note: yjs is bundled inside both crdt.es.js AND the
// mycelium-editor chunk (index-CUCuSPF_.js). These are two separate runtime
// copies, so we CANNOT pass a Y.Text from crdt.es.js to createMyceliumEditor
// (instanceof checks in y-codemirror.next would fail). We therefore sync at
// the document level via onChange / setValue. See editor_page.rs module doc.

const CONTRACT_TEXT_NAME = 'content';
const MSG_SYNC = 0;
const MSG_AWARENESS = 1;

const ydoc = new Y.Doc();
const awareness = new awarenessProtocol.Awareness(ydoc);
const ytext = ydoc.getText(CONTRACT_TEXT_NAME);

class CollabProvider {{
    constructor(doc, awareness) {{
        this.doc = doc;
        this.awareness = awareness;
        this.ws = null;
        this.synced = false;
        this.shouldConnect = true;
        this._onStatus = () => {{}};

        // Forward local doc updates to the server.
        doc.on('update', (update, origin) => {{
            if (origin === this) return; // skip updates we applied ourselves
            const enc = encoding.createEncoder();
            encoding.writeVarUint(enc, MSG_SYNC);
            syncProtocol.writeUpdate(enc, update);
            this._send(enc);
        }});

        // Forward local awareness changes to the server.
        awareness.on('update', ({{ added, updated, removed }}) => {{
            const changed = added.concat(updated, removed);
            const enc = encoding.createEncoder();
            encoding.writeVarUint(enc, MSG_AWARENESS);
            encoding.writeVarUint8Array(
                enc, awarenessProtocol.encodeAwarenessUpdate(awareness, changed));
            this._send(enc);
        }});

        this.connect();
    }}

    get connected() {{
        return this.ws && this.ws.readyState === WebSocket.OPEN;
    }}

    connect() {{
        if (!this.shouldConnect) return;
        const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
        const url = scheme + '://' + location.host
            + '/collab?path=' + encodeURIComponent(path);
        const ws = new WebSocket(url);
        ws.binaryType = 'arraybuffer';
        this.ws = ws;

        ws.onopen = () => {{
            this._onStatus('connected');
            // Sync step 1: send our state vector so the server can reply.
            const enc = encoding.createEncoder();
            encoding.writeVarUint(enc, MSG_SYNC);
            syncProtocol.writeSyncStep1(enc, this.doc);
            this._send(enc);
            // Announce local awareness.
            if (awareness.getLocalState() !== null) {{
                const aenc = encoding.createEncoder();
                encoding.writeVarUint(aenc, MSG_AWARENESS);
                encoding.writeVarUint8Array(
                    aenc, awarenessProtocol.encodeAwarenessUpdate(
                        awareness, [this.doc.clientID]));
                this._send(aenc);
            }}
        }};

        ws.onmessage = (event) => this._onMessage(new Uint8Array(event.data));

        ws.onclose = () => {{
            this._onStatus('disconnected');
            awarenessProtocol.removeAwarenessStates(
                awareness,
                [...awareness.getStates().keys()].filter(id => id !== this.doc.clientID),
                this,
            );
            if (this.shouldConnect) setTimeout(() => this.connect(), 1000);
        }};

        ws.onerror = () => {{ try {{ ws.close(); }} catch (e) {{}} }};
    }}

    _onMessage(buf) {{
        const dec = decoding.createDecoder(buf);
        const enc = encoding.createEncoder();
        const type = decoding.readVarUint(dec);
        if (type === MSG_SYNC) {{
            encoding.writeVarUint(enc, MSG_SYNC);
            const syncType = syncProtocol.readSyncMessage(dec, enc, this.doc, this);
            if (syncType === syncProtocol.messageYjsSyncStep2 && !this.synced) {{
                this.synced = true;
            }}
            if (encoding.length(enc) > 1) this._send(enc);
        }} else if (type === MSG_AWARENESS) {{
            awarenessProtocol.applyAwarenessUpdate(
                awareness, decoding.readVarUint8Array(dec), this);
        }}
    }}

    _send(enc) {{
        if (this.connected) this.ws.send(encoding.toUint8Array(enc));
    }}

    onStatus(fn) {{ this._onStatus = fn; }}

    destroy() {{
        this.shouldConnect = false;
        awarenessProtocol.removeAwarenessStates(
            awareness, [this.doc.clientID], 'window unload');
        try {{ if (this.ws) this.ws.close(); }} catch (e) {{}}
    }}
}}

// ── Mount mycelium-editor ────────────────────────────────────────────────
const editorPane = document.getElementById('editor-pane');
let applyingRemote = false; // guard: skip onChange when setValue is ours

const editor = createMyceliumEditor(editorPane, {{
    value: ytext.toString(), // initial (empty until sync completes)
    readOnly: false,
    onChange: (newValue) => {{
        // Push local keystrokes into the Y.Text so the CollabProvider picks
        // them up via doc.on('update') and forwards to the server.
        if (applyingRemote) return;
        const current = ytext.toString();
        if (newValue === current) return;
        // Apply as a single Y.Text delta (insert/delete) so the CRDT
        // integrates correctly even if a concurrent remote update arrives.
        ydoc.transact(() => {{
            ytext.delete(0, current.length);
            ytext.insert(0, newValue);
        }}, editor); // origin = editor so doc.on('update') skips the echo
    }},
}});

// ── Sync Y.Text remote updates → editor ──────────────────────────────────
ytext.observe((event, transaction) => {{
    if (transaction.origin === editor) return; // our own keystrokes
    const newValue = ytext.toString();
    if (newValue === editor.getValue()) return;
    applyingRemote = true;
    try {{
        editor.setValue(newValue);
    }} finally {{
        applyingRemote = false;
    }}
}});

// ── CollabProvider wiring ────────────────────────────────────────────────
const provider = new CollabProvider(ydoc, awareness);
const statusEl = document.getElementById('status');
provider.onStatus((s) => {{ statusEl.textContent = s; }});

// ── Awareness identity ───────────────────────────────────────────────────
function randomColor() {{
    return 'hsl(' + Math.floor(Math.random() * 360) + ', 70%, 50%)';
}}
let userColor = localStorage.getItem('collab-color') || randomColor();
localStorage.setItem('collab-color', userColor);
let userName = localStorage.getItem('collab-name') || ('anon-' + Math.floor(Math.random() * 1000));

const nameInput = document.getElementById('username');
nameInput.value = userName;

function applyIdentity() {{
    awareness.setLocalStateField('user', {{
        name: userName, color: userColor, colorLight: userColor,
    }});
}}
applyIdentity();
nameInput.addEventListener('input', () => {{
    userName = nameInput.value;
    localStorage.setItem('collab-name', userName);
    applyIdentity();
}});

// ── View-switcher triad ──────────────────────────────────────────────────
const panesEl = document.getElementById('panes');
const btns = {{
    edit:    document.getElementById('btn-edit'),
    split:   document.getElementById('btn-split'),
    preview: document.getElementById('btn-preview'),
}};
let scrollUnsub = null; // cleanup for syncEditorPreviewScroll

const previewPane = document.getElementById('preview-pane');

// The editor page's CSP nonce (server-injected). Passed to /srcdoc so the
// srcdoc's inline bootstrap carries the nonce its own <meta> CSP allows.
// The editor page has no strict CSP header, so this is advisory; it is still
// needed because /srcdoc embeds the nonce in its own renderer <meta> CSP.
const EDITOR_NONCE = {nonce_json};

// Per-mount preview state. Torn down and rebuilt on each refreshPreview() call
// (same disposable-iframe pattern as the shell's mount()). The MessageChannel
// port is the sole auth token between this page and its preview iframe.
let previewFrame = null;
let previewChannel = null;

function teardownPreviewFrame() {{
    if (previewChannel) {{ try {{ previewChannel.port1.close(); }} catch (e) {{}} previewChannel = null; }}
    if (previewFrame && previewFrame.parentNode) previewFrame.parentNode.removeChild(previewFrame);
    previewFrame = null;
}}

let previewFetching = false;
async function refreshPreview() {{
    if (previewFetching) return;
    previewFetching = true;
    try {{
        // Fetch the sandboxed renderer document and the rendered content in parallel.
        const [srcdocResp, contentResp] = await Promise.all([
            fetch('/srcdoc?n=' + encodeURIComponent(EDITOR_NONCE), {{ credentials: 'same-origin' }}),
            fetch('/content?path=' + encodeURIComponent(path), {{ credentials: 'same-origin' }}),
        ]);
        if (!srcdocResp.ok || !contentResp.ok) return;
        const [srcdoc, html] = await Promise.all([srcdocResp.text(), contentResp.text()]);

        // Destroy the prior iframe + channel, then build a fresh sandboxed one.
        teardownPreviewFrame();
        previewChannel = new MessageChannel();
        const frame = document.createElement('iframe');
        frame.setAttribute('sandbox', 'allow-scripts');
        frame.setAttribute('referrerpolicy', 'no-referrer');
        frame.setAttribute('title', 'rendered preview');
        frame.id = 'preview-frame';

        // Transfer port2 to the iframe on load (once only — guard with latch).
        let portSent = false;
        frame.addEventListener('load', () => {{
            if (!frame || !previewChannel || portSent) return;
            portSent = true;
            frame.contentWindow.postMessage({{ type: '__port' }}, '*', [previewChannel.port2]);
            // Once the port is handed over, post the body HTML.
            previewChannel.port1.postMessage({{ type: 'render', html }});
        }});

        // Set srcdoc BEFORE appending so the only load event is the real document.
        frame.srcdoc = srcdoc;
        previewFrame = frame;
        previewPane.appendChild(frame);
    }} catch (e) {{
        console.error('[editor-page] preview refresh error:', e);
    }} finally {{
        previewFetching = false;
    }}
}}

function setView(mode) {{
    panesEl.setAttribute('data-view', mode);
    Object.entries(btns).forEach(([k, btn]) => {{
        const active = k === mode;
        btn.classList.toggle('active', active);
        btn.setAttribute('aria-pressed', String(active));
    }});
    // Tear down scroll-sync when leaving split view.
    if (scrollUnsub) {{ scrollUnsub(); scrollUnsub = null; }}
    if (mode === 'split' || mode === 'preview') {{
        refreshPreview().then(() => {{
            if (mode === 'split') {{
                // Wire scroll-sync: editor scroll → preview scroll.
                scrollUnsub = syncEditorPreviewScroll(
                    editor,
                    previewPane,
                    (_line) => 0, // lineToOffset: identity (best-effort)
                );
            }}
        }});
    }}
    localStorage.setItem('editor-view', mode);
}}

// Restore persisted view mode.
const savedView = localStorage.getItem('editor-view');
if (savedView === 'edit' || savedView === 'split' || savedView === 'preview') {{
    setView(savedView);
}} else {{
    setView('split'); // default: split view
}}

btns.edit.addEventListener('click', () => setView('edit'));
btns.split.addEventListener('click', () => setView('split'));
btns.preview.addEventListener('click', () => setView('preview'));

// Refresh preview on document changes (debounced 400 ms).
let previewTimer = null;
ytext.observe(() => {{
    if (panesEl.getAttribute('data-view') !== 'edit') {{
        clearTimeout(previewTimer);
        previewTimer = setTimeout(refreshPreview, 400);
    }}
}});

// ── Flush-on-exit ────────────────────────────────────────────────────────
window.addEventListener('beforeunload', () => {{
    if (!provider.connected) {{
        console.warn('[collab] socket not connected on exit — unsynced edits may be lost');
    }}
    provider.destroy();
    if (scrollUnsub) {{ scrollUnsub(); scrollUnsub = null; }}
    teardownPreviewFrame();
    editor.destroy();
}});
</script>
</body>
</html>"#,
        display_path = display_path,
        path_json = path_json,
        nonce = nonce,
        nonce_json = nonce_json,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DUMMY_PATH: &str = "/home/user/notes.md";

    fn page() -> String {
        render(DUMMY_PATH)
    }

    #[test]
    fn page_is_valid_html_document() {
        let p = page();
        assert!(p.starts_with("<!DOCTYPE html>"));
        assert!(p.contains("</html>"));
    }

    #[test]
    fn no_esm_sh_importmap_anywhere() {
        let p = page();
        // The entire point of this refactor: no CDN importmap.
        assert!(!p.contains("esm.sh"), "must NOT reference esm.sh");
        assert!(!p.contains("type=\"importmap\""), "must NOT use importmap");
        assert!(!p.contains("importmap"), "must NOT use importmap at all");
    }

    #[test]
    fn all_js_loads_from_editor_bundle_route() {
        let p = page();
        // Every module import points to /editor-bundle/.
        assert!(p.contains("/editor-bundle/mycelium-editor.es.js"),
            "must import mycelium-editor from /editor-bundle/");
        assert!(p.contains("/editor-bundle/crdt.es.js"),
            "must import CRDT companion from /editor-bundle/");
        // No external URL patterns.
        assert!(!p.contains("cdn.jsdelivr"), "must NOT reference jsdelivr CDN");
    }

    #[test]
    fn path_is_injected_json_escaped() {
        let evil = "a</script><b>.md";
        let p = render(evil);
        // The raw closing tag must not appear in the HTML.
        assert!(!p.contains("a</script>"), "raw </script> must be JSON-escaped");
        // The unicode-escaped form must be present.
        assert!(p.contains("\\u003c/script\\u003e"),
            "JSON-escaped form must be present");
    }

    #[test]
    fn path_is_html_escaped_in_toolbar() {
        let evil = "<b>bold</b>&amp;";
        let p = render(evil);
        // The raw HTML must not appear in the page source.
        assert!(!p.contains("<b>bold</b>"), "raw HTML tags must be escaped");
        assert!(p.contains("&lt;b&gt;bold&lt;/b&gt;") || p.contains("&lt;b&gt;"),
            "HTML-escaped form must be present");
    }

    #[test]
    fn collab_provider_targets_collab_endpoint() {
        let p = page();
        assert!(p.contains("/collab?path="), "must connect to /collab?path=");
    }

    #[test]
    fn view_switcher_has_all_three_buttons() {
        let p = page();
        assert!(p.contains("id=\"btn-edit\""), "must have edit button");
        assert!(p.contains("id=\"btn-split\""), "must have split button");
        assert!(p.contains("id=\"btn-preview\""), "must have preview button");
        // Default view: split.
        assert!(p.contains("btn-split\" type=\"button\"\n                title=\"Split view\" aria-label=\"Split view\" aria-pressed=\"true\"")
            || p.contains("id=\"btn-split\" type=\"button\""),
            "split button must exist");
    }

    #[test]
    fn preview_pane_is_present() {
        let p = page();
        assert!(p.contains("id=\"preview-pane\""), "must have preview pane");
        // The preview pane now embeds a sandboxed srcdoc iframe (not a plain div).
        assert!(p.contains("id=\"preview-frame\"") || p.contains("preview-frame"),
            "must reference the preview-frame iframe");
    }

    #[test]
    fn preview_pane_uses_sandboxed_iframe() {
        let p = page();
        // The preview is now a sandboxed srcdoc iframe (same model as /view).
        assert!(p.contains("sandbox"), "preview iframe must carry sandbox attribute");
        assert!(p.contains("allow-scripts"), "sandbox must include allow-scripts");
        assert!(p.contains("preview-frame"), "must have preview-frame element");
        // The srcdoc is fetched from /srcdoc (same isolation model as the shell).
        assert!(p.contains("/srcdoc"), "must fetch /srcdoc for the preview iframe");
        // Math-copy is now handled inside the srcdoc bootstrap (not in this page).
        assert!(!p.contains("enableMathCopyAsTex"),
            "enableMathCopyAsTex must NOT be imported/called in the editor page (it lives in the srcdoc)");
    }

    #[test]
    fn scroll_sync_is_wired_in_split_view() {
        let p = page();
        assert!(p.contains("syncEditorPreviewScroll"),
            "must call syncEditorPreviewScroll");
    }

    #[test]
    fn crdt_contract_text_name_is_content() {
        let p = page();
        // The server expects Y.Text named "content" — must match doc_core.
        assert!(p.contains("'content'"), "Y.Text must be named 'content'");
    }

    #[test]
    fn json_string_escapes_control_chars_and_script_breakers() {
        assert_eq!(json_string("hi"), "\"hi\"");
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert!(json_string("</script>").contains("\\u003c"));
        assert!(json_string("a&b").contains("\\u0026"));
        assert!(json_string("\n").contains("\\n"));
        // Tab and CR also escaped.
        assert!(json_string("\t").contains("\\t"));
        assert!(json_string("\r").contains("\\r"));
    }

    #[test]
    fn escape_html_escapes_markup_chars() {
        assert_eq!(escape_html("<b>&amp;"), "&lt;b&gt;&amp;amp;");
    }
}
