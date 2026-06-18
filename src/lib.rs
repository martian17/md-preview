//! Markdown → HTML rendering for md-preview.
//!
//! The crate is split so the rendering is usable on its own (tests, a future
//! daemon, other tools): [`render_markdown`] turns Markdown into an HTML body
//! fragment, and [`render_page`] wraps that fragment in the full standalone
//! HTML document (styles, KaTeX, copy-button script). The binary in `main.rs`
//! only handles argument parsing and serving.

use pulldown_cmark::Options;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::io::Cursor;
use syntect::highlighting::{Color, ThemeSet};
use syntect::html::{css_for_theme_with_class_style, ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

// Real-time collaborative editing (see ADR-0001 "CRDT over OT" and ADR-0003).
// The renderer above stays pure and reusable; these modules add the document
// model behind a stable contract (`doc::DocSession`).
pub mod doc;
pub mod diff;
pub mod file_peer;
pub mod session;

// The persistent daemon (warp + tokio live preview). Feature-gated so the lib
// stays usable on its own — `--no-default-features` builds the pure renderer and
// document core with ZERO web/server dependencies.
#[cfg(feature = "daemon")]
pub mod server;

/// Class prefix for syntect's highlight classes. Shared by the HTML generator
/// and the generated CSS so the markup and stylesheet stay in sync.
const HL_PREFIX: &str = "hl-";

/// GitHub's official TextMate themes (from primer/github-textmate-theme),
/// embedded so the binary stays self-contained. We only use them to generate
/// CSS — class-based highlighting itself is theme-independent.
const GITHUB_LIGHT_THEME: &str = include_str!("../themes/github-light.tmTheme");
const GITHUB_DARK_THEME: &str = include_str!("../themes/github-dark.tmTheme");

/// GitHub-style "copy code" button. Holds both the copy and check octicons; the
/// `.copied` class (toggled by JS on click) swaps which one is visible. The
/// click handler reads the sibling <pre><code> text, so no code is duplicated
/// into an attribute.
const COPY_BUTTON: &str = r##"<button class="copy-btn" type="button" aria-label="Copy code to clipboard" title="Copy">
<svg class="octicon octicon-copy" aria-hidden="true" height="16" width="16" viewBox="0 0 16 16"><path d="M0 6.75C0 5.784.784 5 1.75 5h1.5a.75.75 0 0 1 0 1.5h-1.5a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-1.5a.75.75 0 0 1 1.5 0v1.5A1.75 1.75 0 0 1 9.25 16h-7.5A1.75 1.75 0 0 1 0 14.25Z"></path><path d="M5 1.75C5 .784 5.784 0 6.75 0h7.5C15.216 0 16 .784 16 1.75v7.5A1.75 1.75 0 0 1 14.25 11h-7.5A1.75 1.75 0 0 1 5 9.25Zm1.75-.25a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-7.5a.25.25 0 0 0-.25-.25Z"></path></svg>
<svg class="octicon octicon-check" aria-hidden="true" height="16" width="16" viewBox="0 0 16 16"><path d="M13.78 4.22a.75.75 0 0 1 0 1.06l-7.25 7.25a.75.75 0 0 1-1.06 0L2.22 9.28a.751.751 0 0 1 .018-1.042.751.751 0 0 1 1.042-.018L6 10.94l6.72-6.72a.75.75 0 0 1 1.06 0Z"></path></svg>
</button>"##;

/// Format a syntect color as `#rrggbb`, or `inherit` if the theme omits it.
fn hex(c: Option<Color>) -> String {
    match c {
        Some(c) => format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b),
        None => "inherit".to_string(),
    }
}

/// Build the code-highlighting CSS: GitHub Light as the default, GitHub Dark
/// inside a `prefers-color-scheme: dark` media query. Both themes emit the same
/// `hl-` class names, so the dark block simply overrides in dark mode — one
/// server render, theme chosen by the browser (the way GitHub itself does it).
fn syntax_css() -> String {
    let style = ClassStyle::SpacedPrefixed { prefix: HL_PREFIX };
    let light = ThemeSet::load_from_reader(&mut Cursor::new(GITHUB_LIGHT_THEME))
        .expect("bundled light theme is valid");
    let dark = ThemeSet::load_from_reader(&mut Cursor::new(GITHUB_DARK_THEME))
        .expect("bundled dark theme is valid");
    let light_css = css_for_theme_with_class_style(&light, style).expect("generate light CSS");
    let dark_css = css_for_theme_with_class_style(&dark, style).expect("generate dark CSS");
    // Explicit <pre> background/foreground, specific enough to beat
    // github-markdown-css's `.markdown-body pre` rule (syntect's own `.hl-code`
    // rule has lower specificity and would otherwise lose).
    //
    // GitHub backs code blocks with --bgColor-muted (canvas-subtle), NOT the
    // editor canvas baked into the tmTheme: #f6f8fa light / #161b22 dark, per
    // github-markdown-css 5.x. We keep each theme's own text color.
    let l_fg = hex(light.settings.foreground);
    let d_fg = hex(dark.settings.foreground);
    let (l_bg, d_bg) = ("#f6f8fa", "#161b22");
    format!(
        "{light_css}\n\
         .markdown-body pre.hl-code {{ background-color: {l_bg}; color: {l_fg}; }}\n\
         @media (prefers-color-scheme: dark) {{\n{dark_css}\n\
         .markdown-body pre.hl-code {{ background-color: {d_bg}; color: {d_fg}; }}\n}}\n"
    )
}

/// Render a `mermaid` fenced block as the `<pre class="mermaid">` element the
/// Mermaid.js client picks up. The graph source is HTML-escaped (Mermaid reads
/// `textContent`, which decodes it back, so this is lossless and safe); the
/// block gets neither syntax highlighting nor a copy-button wrapper.
fn render_mermaid_block(code: &str) -> String {
    format!("<pre class=\"mermaid\">{}</pre>", escape_html(code))
}

/// Minimal HTML escaping for text placed inside the <math-renderer> element.
/// The frontend reads `this.textContent`, which decodes these back to raw TeX
/// before handing it to KaTeX, so escaping here is lossless.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a code block as GitHub-style class-based markup: a `.code-wrap`
/// holding the copy button and a `<pre class="hl-code"><code>` of highlighted
/// spans (no inline colors — the `hl-` CSS in <head> handles light/dark).
/// `lang` is the fence token, or "" for an indented block (→ plain text).
fn render_code_block(ps: &SyntaxSet, lang: &str, code: &str) -> String {
    let syntax = ps
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut hl = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        ps,
        ClassStyle::SpacedPrefixed { prefix: HL_PREFIX },
    );
    for line in LinesWithEndings::from(code) {
        let _ = hl.parse_html_for_line_which_includes_newline(line);
    }
    format!(
        "<div class=\"code-wrap\">{button}<pre class=\"hl-code\"><code>{code}</code></pre></div>",
        button = COPY_BUTTON,
        code = hl.finalize()
    )
}

/// Wrap raw TeX in the <math-renderer> custom element consumed by KaTeX.
fn math_renderer(tex: &str, display: bool) -> String {
    let escaped = escape_html(tex.trim());
    if display {
        format!(
            r#"<math-renderer class="js-display-math" style="display: block">{}</math-renderer>"#,
            escaped
        )
    } else {
        format!(
            r#"<math-renderer style="display: inline">{}</math-renderer>"#,
            escaped
        )
    }
}

/// Convert Markdown to an HTML **body fragment** (no surrounding document).
///
/// Beyond GitHub-flavored Markdown this handles two things specially:
/// math (`$...$`, `$$...$$`) becomes `<math-renderer>` elements for KaTeX, and
/// code blocks (fenced *or* indented) become syntax-highlighted `<pre>` blocks
/// with a copy button. Use [`render_page`] for a standalone HTML document.
pub fn render_markdown(markdown_input: &str) -> String {
    let ps = SyntaxSet::load_defaults_newlines();

    // GFM + math options. ENABLE_MATH turns `$...$` into Event::InlineMath
    // and `$$...$$` into Event::DisplayMath.
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_MATH);

    let parser = Parser::new_ext(markdown_input, options);
    let mut html_output = String::new();

    // Code blocks (fenced *or* indented) are intercepted: we suppress
    // pulldown's default <pre><code>, accumulate the body text across all the
    // Text events between Start and End, then emit our own highlighted markup
    // once on End. Accumulating (rather than rendering per Text event) keeps a
    // block whole even if the parser splits its body into several events.
    // `code_block` is Some((lang, buffer)) while inside a block; lang is the
    // fence token, or "" for an indented block.
    let mut code_block: Option<(String, String)> = None;

    let events = parser.filter_map(|event| {
        match event {
            // Inline `$...$` and display `$$...$$` math (from ENABLE_MATH).
            Event::InlineMath(tex) => Some(Event::Html(math_renderer(&tex, false).into())),
            Event::DisplayMath(tex) => Some(Event::Html(math_renderer(&tex, true).into())),
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                code_block = Some((lang, String::new()));
                None // suppress default tag; the whole block is emitted on End
            }
            Event::End(TagEnd::CodeBlock) => {
                let (lang, code) = code_block.take().expect("CodeBlock End without Start");
                // A ```mermaid fence is a live diagram, not highlighted code.
                let html = if lang == "mermaid" {
                    render_mermaid_block(&code)
                } else {
                    render_code_block(&ps, &lang, &code)
                };
                Some(Event::Html(html.into()))
            }
            // While inside a code block, route text into the buffer instead of
            // the document. (A ```math fence is just a code block here — only
            // inline $...$ and display $$...$$ get KaTeX-rendered.)
            Event::Text(text) if code_block.is_some() => {
                code_block.as_mut().unwrap().1.push_str(&text);
                None
            }
            other => Some(other),
        }
    });

    pulldown_cmark::html::push_html(&mut html_output, events);
    html_output
}

/// Render Markdown to a complete, standalone HTML document: the
/// [`render_markdown`] body plus the `<head>` styles (github-markdown-css,
/// KaTeX CSS, auto light/dark highlight CSS) and the scripts that drive KaTeX
/// and the copy-code buttons.
///
/// This is a thin wrapper over [`render_page_with`] with empty extras; its
/// output is byte-identical to assembling the document by hand.
pub fn render_page(markdown_input: &str) -> String {
    render_page_with(&render_markdown(markdown_input), "", "")
}

/// Assemble the full standalone HTML document around an already-rendered
/// `body` fragment, injecting `extra_head` just before `</head>` and
/// `extra_body` just before `</body>`.
///
/// This is the **pure** seam the daemon uses to add its live-reload WebSocket
/// client (and wrap the body in `<div id="doc">…</div>`) without duplicating
/// the bundled assets/styles, while keeping the renderer free of any web/server
/// dependency. With empty extras the output is byte-identical to the original
/// standalone page, so [`render_page`] and the existing snapshot tests are
/// unaffected.
pub fn render_page_with(body: &str, extra_head: &str, extra_body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
        <html>
        <head>
            <meta charset="utf-8">
            <meta name="viewport" content="width=device-width, initial-scale=1">
            <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/github-markdown-css/5.5.1/github-markdown.min.css">
            <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/katex@0.16.9/dist/katex.min.css">
            <style>
                .markdown-body {{
                    box-sizing: border-box;
                    min-width: 200px;
                    max-width: 980px;
                    margin: 0 auto;
                    padding: 45px;
                }}
                @media (max-width: 767px) {{ .markdown-body {{ padding: 15px; }} }}
                /* Page background follows the system theme, like the code blocks. */
                body {{ background-color: #ffffff; }}
                @media (prefers-color-scheme: dark) {{ body {{ background-color: #0d1117; }} }}
                /* Copy-code button, GitHub-style: top-right of each block,
                   fades in on hover, swaps to a green check when copied. */
                .code-wrap {{ position: relative; }}
                .copy-btn {{
                    position: absolute; top: 8px; right: 8px;
                    display: inline-flex; align-items: center; justify-content: center;
                    width: 28px; height: 28px; padding: 0;
                    border: 1px solid transparent; border-radius: 6px;
                    background: transparent; color: #656d76; cursor: pointer;
                    opacity: 0; transition: opacity .1s, background-color .1s, border-color .1s;
                }}
                .code-wrap:hover .copy-btn, .copy-btn:focus {{ opacity: 1; }}
                .copy-btn:hover {{ background-color: rgba(175,184,193,.2); border-color: rgba(175,184,193,.4); }}
                .copy-btn .octicon-check {{ display: none; color: #1a7f37; }}
                .copy-btn.copied {{ opacity: 1; }}
                .copy-btn.copied .octicon-copy {{ display: none; }}
                .copy-btn.copied .octicon-check {{ display: inline-block; }}
                @media (prefers-color-scheme: dark) {{
                    .copy-btn {{ color: #7d8590; }}
                    .copy-btn:hover {{ background-color: rgba(99,110,123,.25); border-color: rgba(99,110,123,.4); }}
                    .copy-btn .octicon-check {{ color: #3fb950; }}
                }}
                /* GitHub Light/Dark code highlighting, auto-switching. */
{syntax_css}
            </style>{extra_head}
        </head>
        <body class="markdown-body">
            {body}
        <script type="module">
import katex from 'https://cdn.jsdelivr.net/npm/katex@0.16.9/dist/katex.mjs';
import mermaid from 'https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs';

// Mermaid diagrams: <pre class="mermaid"> blocks. Theme follows the system
// light/dark like the rest of the page. startOnLoad renders the initial blocks;
// the daemon swaps `#doc.innerHTML` on every live update (no custom element to
// auto-fire), so a MutationObserver re-runs mermaid against any new <pre> nodes
// — the same "re-init against the swapped nodes" idea KaTeX gets for free.
const mermaidDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
mermaid.initialize({{ startOnLoad: true, theme: mermaidDark ? 'dark' : 'default' }});
{{
    let scheduled = false;
    const rerun = () => {{
        scheduled = false;
        const blocks = document.querySelectorAll('pre.mermaid:not([data-processed])');
        if (blocks.length) mermaid.run({{ nodes: blocks }});
    }};
    const doc = document.getElementById('doc');
    if (doc) new MutationObserver(() => {{
        if (scheduled) return;
        scheduled = true;
        queueMicrotask(rerun);
    }}).observe(doc, {{ childList: true }});
}}

class MathRenderer extends HTMLElement {{
    connectedCallback() {{
        // Get the raw TeX code inside the tag
        const rawTex = this.textContent.trim();
        const isDisplay = this.classList.contains('js-display-math');

        try {{
            // Render the math directly inside this element
            katex.render(rawTex, this, {{
                displayMode: isDisplay,
                throwOnError: false
            }});
        }} catch (err) {{
            console.error("Failed to render LaTeX:", err);
        }}
    }}
}}

// Register the custom element
customElements.define('math-renderer', MathRenderer);

// Copy-code buttons: copy the sibling <pre><code> text, then flash the check.
// navigator.clipboard works here because http://127.0.0.1 is a secure context.
document.addEventListener('click', async (e) => {{
    const btn = e.target.closest('.copy-btn');
    if (!btn) return;
    const code = btn.parentElement.querySelector('pre code');
    try {{
        await navigator.clipboard.writeText(code ? code.textContent : '');
        btn.classList.add('copied');
        btn.setAttribute('title', 'Copied!');
        setTimeout(() => {{
            btn.classList.remove('copied');
            btn.setAttribute('title', 'Copy');
        }}, 2000);
    }} catch (err) {{
        console.error('Copy failed:', err);
    }}
}});
        </script>{extra_body}
        </body>
        </html>"#,
        syntax_css = syntax_css(),
        body = body,
        extra_head = extra_head,
        extra_body = extra_body
    )
}

/// Serialize `s` as a JSON string literal (quotes included), safe to embed in a
/// `<script>`. Escapes the characters that would break out of the literal or
/// the script context (notably `<`/`>`/`&`, so a `path` like `a</script>b`
/// can't close the tag).
///
/// This mirrors the daemon's own `json_string` (in `server.rs`), but lives here
/// so the pure render layer stays free of any web/server dependency and still
/// compiles under `--no-default-features`.
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

/// Render the standalone HTML page for the **collaborative** Markdown editor
/// (Phase 3, ADR-0002). Pure render layer: like [`render_page_with`] it only
/// emits HTML+JS strings — zero web/server deps — so it builds under
/// `--no-default-features`. The daemon (a separate agent) wires the route that
/// calls this; we provide only the page.
///
/// ## Contract with the server agent (two load-bearing facts)
/// - **Shared `Y.Text` name: `"content"`.** The browser binds `ydoc.getText("content")`
///   to CodeMirror; the server must use the same name on its peer doc.
/// - **WebSocket URL: `ws[s]://<host>/collab?path=<encodeURIComponent(rel)>`.**
///   `wss` when the page is served over https, else `ws`.
///
/// ## Provider: custom, not y-websocket's `WebsocketProvider`
/// y-websocket@2's `WebsocketProvider(serverUrl, roomname, doc)` always composes
/// its URL as `serverUrl + '/' + roomname + '?...'` (with its own query params),
/// so it *cannot* produce our `/collab?path=<rel>` shape (no `/<room>` segment,
/// our own `path` query key). Per the spec's fallback we therefore ship a small
/// custom provider implementing the same wire protocol — a `varint(messageType)`
/// followed by the payload, where type 0 = sync (`y-protocols/sync`) and type 1
/// = awareness (`y-protocols/awareness`) — against the ephemeral doc. We still import
/// `y-websocket@2` so the version is pinned/available, but drive the socket
/// ourselves. `yCollab` consumes `provider.awareness` exactly as it would the
/// stock provider's.
///
/// The ephemeral doc (created here, bound to CodeMirror, flushed and discarded
/// on exit) is the editor's local doc — never a long-lived peer (ADR-0002).
pub fn render_editor_page(rel_path: &str) -> String {
    // The path is injected as a JSON-escaped literal and re-encoded with
    // encodeURIComponent in the browser, so no raw path text lands in the
    // script context (defends against a `path` like `a</script><b>.md`).
    let path_json = json_string(rel_path);
    // Visible label in the toolbar — HTML-escaped for text-node safety.
    let display_path = escape_html(rel_path);

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{display_path} — collaborative editor</title>
    <!-- Pinned ESM via importmap (esm.sh), matching the KaTeX/mermaid CDN house
         style. Stable Yjs-v13 line: yjs@13.6 + y-codemirror.next@0.3 (NOT the
         Yjs-v14 `@y/codemirror`). CodeMirror 6 core + markdown lang. y-websocket@2
         supplies the wire-protocol deps; we drive the socket via a custom provider. -->
    <script type="importmap">
    {{
      "imports": {{
        "yjs": "https://esm.sh/yjs@13.6",
        "y-protocols/sync": "https://esm.sh/y-protocols@1/sync",
        "y-protocols/awareness": "https://esm.sh/y-protocols@1/awareness",
        "y-codemirror.next": "https://esm.sh/y-codemirror.next@0.3",
        "y-websocket": "https://esm.sh/y-websocket@2",
        "lib0/encoding": "https://esm.sh/lib0@0.2/encoding",
        "lib0/decoding": "https://esm.sh/lib0@0.2/decoding",
        "@codemirror/state": "https://esm.sh/@codemirror/state@6",
        "@codemirror/view": "https://esm.sh/@codemirror/view@6",
        "@codemirror/commands": "https://esm.sh/@codemirror/commands@6",
        "@codemirror/lang-markdown": "https://esm.sh/@codemirror/lang-markdown@6"
      }}
    }}
    </script>
    <style>
        :root {{ color-scheme: light dark; }}
        body {{
            margin: 0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI",
                Helvetica, Arial, sans-serif;
            background-color: #ffffff; color: #1f2328;
        }}
        @media (prefers-color-scheme: dark) {{
            body {{ background-color: #0d1117; color: #e6edf3; }}
        }}
        #toolbar {{
            display: flex; align-items: center; gap: 12px;
            padding: 8px 16px; border-bottom: 1px solid rgba(128,128,128,.3);
            font-size: 14px;
        }}
        #toolbar .path {{ font-weight: 600; }}
        #toolbar .spacer {{ flex: 1; }}
        #toolbar label {{ color: #656d76; }}
        @media (prefers-color-scheme: dark) {{ #toolbar label {{ color: #7d8590; }} }}
        #username {{
            font: inherit; padding: 2px 6px; border-radius: 6px;
            border: 1px solid rgba(128,128,128,.4); background: transparent; color: inherit;
        }}
        #status {{ font-size: 12px; color: #656d76; }}
        #editor {{ max-width: 980px; margin: 0 auto; padding: 16px; }}
        .cm-editor {{ border: 1px solid rgba(128,128,128,.3); border-radius: 6px; }}
        .cm-editor.cm-focused {{ outline: none; }}
    </style>
</head>
<body>
    <div id="toolbar">
        <span class="path">{display_path}</span>
        <span class="spacer"></span>
        <label for="username">name</label>
        <input id="username" type="text" maxlength="40" autocomplete="off">
        <span id="status">connecting…</span>
    </div>
    <div id="editor"></div>

    <script type="module">
import * as Y from 'yjs';
import * as syncProtocol from 'y-protocols/sync';
import * as awarenessProtocol from 'y-protocols/awareness';
import * as encoding from 'lib0/encoding';
import * as decoding from 'lib0/decoding';
import {{ yCollab }} from 'y-codemirror.next';
import {{ EditorState }} from '@codemirror/state';
import {{ EditorView, keymap }} from '@codemirror/view';
import {{ defaultKeymap, history, historyKeymap }} from '@codemirror/commands';
import {{ markdown }} from '@codemirror/lang-markdown';

// (1) Server-injected path, JSON-escaped so it can't break out of <script>.
const path = {path_json};
const displayPath = path;

// CONTRACT (server agent must match): the shared Y.Text is named "content".
const Y_TEXT_NAME = 'content';

// (2) Ephemeral Y.Doc (ADR-0002): created on edit, bound to CodeMirror below,
// flushed and discarded on exit — never the long-lived peer.
const ydoc = new Y.Doc();
const awareness = new awarenessProtocol.Awareness(ydoc);

// y-websocket wire message types.
const MSG_SYNC = 0;
const MSG_AWARENESS = 1;

// Custom provider (~kept small): y-websocket@2's WebsocketProvider can only
// build `serverUrl + '/' + room + '?...'`, which can't express our `/collab?path=<rel>`
// endpoint, so we speak its protocol directly. Connects to EXACTLY
// `ws[s]://<host>/collab?path=<encodeURIComponent(path)>`.
class CollabProvider {{
    constructor(doc, awareness) {{
        this.doc = doc;
        this.awareness = awareness;
        this.ws = null;
        this.synced = false;
        this.shouldConnect = true;
        this._onStatus = () => {{}};
        doc.on('update', (update, origin) => {{
            if (origin === this) return; // don't echo applied remote updates
            const enc = encoding.createEncoder();
            encoding.writeVarUint(enc, MSG_SYNC);
            syncProtocol.writeUpdate(enc, update);
            this._send(enc);
        }});
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
    get connected() {{ return this.ws && this.ws.readyState === WebSocket.OPEN; }}
    connect() {{
        const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
        const url = scheme + '://' + location.host
            + '/collab?path=' + encodeURIComponent(path);
        const ws = new WebSocket(url);
        ws.binaryType = 'arraybuffer';
        this.ws = ws;
        ws.onopen = () => {{
            this._onStatus('connected');
            // Step 1 of sync: send our state vector so the server can reply with
            // the missing updates.
            const enc = encoding.createEncoder();
            encoding.writeVarUint(enc, MSG_SYNC);
            syncProtocol.writeSyncStep1(enc, this.doc);
            this._send(enc);
            // Announce our local awareness state.
            if (this.awareness.getLocalState() !== null) {{
                const aenc = encoding.createEncoder();
                encoding.writeVarUint(aenc, MSG_AWARENESS);
                encoding.writeVarUint8Array(aenc, awarenessProtocol.encodeAwarenessUpdate(
                    this.awareness, [this.doc.clientID]));
                this._send(aenc);
            }}
        }};
        ws.onmessage = (event) => this._onMessage(new Uint8Array(event.data));
        ws.onclose = () => {{
            this._onStatus('disconnected');
            // Drop our awareness entry locally; reconnect after a short backoff.
            awarenessProtocol.removeAwarenessStates(
                this.awareness, [...this.awareness.getStates().keys()]
                    .filter((id) => id !== this.doc.clientID), this);
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
            // readSyncMessage applies the update (origin = this, so we don't
            // echo it back) and may write a reply (e.g. syncStep2) into enc.
            const syncType = syncProtocol.readSyncMessage(dec, enc, this.doc, this);
            if (syncType === syncProtocol.messageYjsSyncStep2 && !this.synced) {{
                this.synced = true;
            }}
            if (encoding.length(enc) > 1) this._send(enc);
        }} else if (type === MSG_AWARENESS) {{
            awarenessProtocol.applyAwarenessUpdate(
                this.awareness, decoding.readVarUint8Array(dec), this);
        }}
    }}
    _send(enc) {{
        if (this.connected) this.ws.send(encoding.toUint8Array(enc));
    }}
    onStatus(fn) {{ this._onStatus = fn; }}
    destroy() {{
        this.shouldConnect = false;
        awarenessProtocol.removeAwarenessStates(
            this.awareness, [this.doc.clientID], 'window unload');
        if (this.ws) {{ try {{ this.ws.close(); }} catch (e) {{}} }}
    }}
}}

const provider = new CollabProvider(ydoc, awareness);
const statusEl = document.getElementById('status');
provider.onStatus((s) => {{ statusEl.textContent = s; }});

// (4) Awareness identity (D7): random color + editable display name, persisted
// in localStorage. yCollab renders remote cursors/labels from these fields.
function randomColor() {{
    const h = Math.floor(Math.random() * 360);
    return 'hsl(' + h + ', 70%, 50%)';
}}
let userColor = localStorage.getItem('collab-color');
if (!userColor) {{ userColor = randomColor(); localStorage.setItem('collab-color', userColor); }}
let userName = localStorage.getItem('collab-name')
    || ('anon-' + Math.floor(Math.random() * 1000));
const nameInput = document.getElementById('username');
nameInput.value = userName;
function applyIdentity() {{
    awareness.setLocalStateField('user', {{ name: userName, color: userColor, colorLight: userColor }});
}}
applyIdentity();
nameInput.addEventListener('input', () => {{
    userName = nameInput.value;
    localStorage.setItem('collab-name', userName);
    applyIdentity();
}});

// (3) Bind the shared Y.Text "content" to CodeMirror via yCollab + markdown.
const ytext = ydoc.getText(Y_TEXT_NAME);
const state = EditorState.create({{
    doc: ytext.toString(),
    extensions: [
        history(),
        keymap.of([...defaultKeymap, ...historyKeymap]),
        markdown(),
        yCollab(ytext, awareness),
        EditorView.lineWrapping,
    ],
}});
const view = new EditorView({{ state, parent: document.getElementById('editor') }});
view.focus();

// (5) Flush-on-exit (ADR-0002 gotcha): best-effort flush pending updates before
// teardown. Yjs applies edits synchronously and our doc-update handler sends
// each update immediately, so by `beforeunload` there's normally nothing queued;
// but if the socket isn't connected we can't flush — warn rather than silently
// drop the user's unsynced edits.
window.addEventListener('beforeunload', () => {{
    if (!provider.connected) {{
        console.warn('[collab] socket not connected on exit — unsynced edits may be lost');
    }}
    provider.destroy();
}});
    </script>
</body>
</html>"#,
        display_path = display_path,
        path_json = path_json,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_escapes_markup_chars() {
        assert_eq!(escape_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        // Order matters: an already-escaped entity must not be double-escaped
        // into something lossy. `&` is replaced first, so `<` -> `&lt;` stays.
        assert_eq!(escape_html("<"), "&lt;");
    }

    #[test]
    fn math_renderer_distinguishes_inline_and_display() {
        let inline = math_renderer("x^2", false);
        assert!(inline.contains(r#"style="display: inline""#));
        assert!(!inline.contains("js-display-math"));
        assert!(inline.contains("x^2"));

        let display = math_renderer("x^2", true);
        assert!(display.contains("js-display-math"));
        assert!(display.contains(r#"style="display: block""#));
    }

    #[test]
    fn math_renderer_trims_and_escapes() {
        let out = math_renderer("  a < b  ", false);
        assert!(out.contains("a &lt; b"));
        assert!(!out.contains("  a"));
    }

    #[test]
    fn inline_math_becomes_inline_math_renderer() {
        let html = render_markdown("An equation $a + b$ here.");
        assert!(html.contains(r#"<math-renderer style="display: inline">a + b</math-renderer>"#));
        assert!(!html.contains("js-display-math"));
    }

    #[test]
    fn display_math_becomes_display_math_renderer() {
        let html = render_markdown("$$\nx = y\n$$");
        assert!(html.contains("js-display-math"));
        assert!(html.contains("x = y"));
    }

    #[test]
    fn fenced_code_block_is_highlighted_with_copy_button() {
        let html = render_markdown("```rust\nfn main() {}\n```");
        assert!(html.contains(r#"<pre class="hl-code">"#));
        assert!(html.contains(r#"class="copy-btn""#));
        // Rust keyword picked up by the highlighter.
        assert!(html.contains("hl-storage"));
        // Exactly one block -> exactly one copy button.
        assert_eq!(html.matches(r#"class="copy-btn""#).count(), 1);
    }

    #[test]
    fn indented_code_block_is_also_a_code_block() {
        // Regression: indented blocks once leaked through as raw body text with
        // no <pre>/<code> and no copy button.
        let html = render_markdown("Intro:\n\n    let x = 42;\n");
        assert!(html.contains(r#"<pre class="hl-code">"#));
        assert!(html.contains(r#"class="copy-btn""#));
        assert!(html.contains("let x = 42;"));
    }

    #[test]
    fn mermaid_fence_becomes_a_mermaid_block_not_highlighted_code() {
        let html = render_markdown("```mermaid\ngraph TD; A-->B;\n```");
        // Rendered as the Mermaid.js target element, with no highlighting and no
        // copy-button wrapper.
        assert!(html.contains(r#"<pre class="mermaid">"#));
        assert!(!html.contains("hl-code"));
        assert!(!html.contains("copy-btn"));
        // The graph source survives inside the element.
        assert!(html.contains("graph TD; A--&gt;B;"));
    }

    #[test]
    fn mermaid_block_escapes_its_source() {
        let html = render_mermaid_block("a --> b & <c>");
        assert!(html.contains("a --&gt; b &amp; &lt;c&gt;"));
        assert!(!html.contains("<c>"));
    }

    #[test]
    fn math_fence_is_a_code_block_not_katex() {
        // ```math is a *code block* showing raw TeX, not a rendered equation.
        let html = render_markdown("```math\nx = y\n```");
        assert!(html.contains(r#"<pre class="hl-code">"#));
        assert!(!html.contains("math-renderer"));
    }

    #[test]
    fn plain_markdown_passes_through() {
        let html = render_markdown("# Title\n\nSome **bold** text.");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
    }

    #[test]
    fn gfm_tables_are_enabled() {
        let html = render_markdown("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(html.contains("<table>"));
    }

    #[test]
    fn render_page_is_a_full_document_with_assets() {
        let page = render_page("# Hi");
        assert!(page.starts_with("<!DOCTYPE html>"));
        assert!(page.contains("github-markdown.min.css"));
        assert!(page.contains("katex.min.css"));
        assert!(page.contains("customElements.define('math-renderer'"));
        // The highlight CSS is injected.
        assert!(page.contains("prefers-color-scheme: dark"));
        // And the rendered body is embedded.
        assert!(page.contains("<h1>Hi</h1>"));
    }

    #[test]
    fn render_page_with_empty_extras_is_byte_identical_to_render_page() {
        // The pure seam must not perturb the standalone page when no extras are
        // injected — this is what keeps the existing snapshot/asset tests and any
        // byte-for-byte consumers stable.
        for md in ["", "# Hi", "Inline $a^2$ and\n\n```rust\nfn x() {}\n```\n"] {
            let body = render_markdown(md);
            assert_eq!(
                render_page_with(&body, "", ""),
                render_page(md),
                "render_page_with(.., \"\", \"\") must equal render_page for {md:?}"
            );
        }
    }

    #[test]
    fn render_page_with_injects_extras_in_the_right_places() {
        let body = render_markdown("# Hi");
        let page = render_page_with(&body, "<!--HEADMARK-->", "<!--BODYMARK-->");
        // extra_head lands inside <head> (before the closing tag).
        let head_pos = page.find("<!--HEADMARK-->").expect("head extra present");
        let head_close = page.find("</head>").expect("</head> present");
        assert!(head_pos < head_close, "extra_head must be inside <head>");
        // extra_body lands just before </body>.
        let body_pos = page.find("<!--BODYMARK-->").expect("body extra present");
        let body_close = page.find("</body>").expect("</body> present");
        assert!(body_pos < body_close, "extra_body must be before </body>");
        assert!(head_close < body_pos, "head extra precedes body extra");
        // The original body fragment is still embedded.
        assert!(page.contains("<h1>Hi</h1>"));
    }

    #[test]
    fn render_editor_page_targets_collab_endpoint_for_the_path() {
        let page = render_editor_page("notes.md");
        assert!(page.starts_with("<!DOCTYPE html>"));
        // Connects to our /collab endpoint (path is appended via encodeURIComponent).
        assert!(page.contains("/collab?path="));
        // The path appears (injected literal + visible toolbar label).
        assert!(page.contains("notes.md"));
    }

    #[test]
    fn render_editor_page_pins_versions_and_binds_ycollab() {
        let page = render_editor_page("notes.md");
        // Importmap with the pinned, exact versions (esm.sh house style).
        assert!(page.contains(r#"<script type="importmap">"#));
        assert!(page.contains("yjs@13.6"));
        assert!(page.contains("y-codemirror.next@0.3"));
        assert!(page.contains("y-websocket@2"));
        assert!(page.contains("@codemirror/state@6"));
        assert!(page.contains("@codemirror/view@6"));
        assert!(page.contains("@codemirror/commands@6"));
        assert!(page.contains("@codemirror/lang-markdown@6"));
        // The CodeMirror <-> Yjs binding.
        assert!(page.contains("yCollab(ytext, awareness)"));
    }

    #[test]
    fn render_editor_page_names_the_shared_text_content() {
        // The shared type name is the contract with the server agent.
        let page = render_editor_page("notes.md");
        assert!(page.contains("'content'"));
        assert!(page.contains(r#"getText(Y_TEXT_NAME)"#));
    }

    #[test]
    fn render_editor_page_escapes_path_so_it_cannot_break_the_script() {
        // A path crafted to close the <script> tag must be neutralized: the
        // literal </script> from user input is JSON-unicode-escaped, mirroring
        // the json_string escaping test in server.rs.
        let page = render_editor_page("a</script><b>.md");
        // The injected JS literal carries the escaped form, never a raw </script>.
        assert!(page.contains("a\\u003c/script\\u003e\\u003cb\\u003e.md"));
        // And no raw breakout sequence from the user input survives in the page.
        assert!(!page.contains("a</script>"));
    }

    #[test]
    fn json_string_escapes_script_breakers() {
        // Mirrors server.rs's json_string contract so the two stay in lockstep.
        let out = json_string("a</script>b");
        assert!(out.contains("\\u003c/script\\u003e"));
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
