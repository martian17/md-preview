use pulldown_cmark::{Parser, Event, Tag, TagEnd};
use pulldown_cmark::Options;
use std::io::Cursor;
use syntect::parsing::SyntaxSet;
use syntect::highlighting::{Color, ThemeSet};
use syntect::html::{css_for_theme_with_class_style, ClassStyle, ClassedHTMLGenerator};
use syntect::util::LinesWithEndings;

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
    let light_css =
        css_for_theme_with_class_style(&light, style).expect("generate light CSS");
    let dark_css =
        css_for_theme_with_class_style(&dark, style).expect("generate dark CSS");
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

/// Minimal HTML escaping for text placed inside the <math-renderer> element.
/// The frontend reads `this.textContent`, which decodes these back to raw TeX
/// before handing it to KaTeX, so escaping here is lossless.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

fn render_markdown(markdown_input: &str) -> String {
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
    
    // Track if we are inside a math block if your parser supports it,
    // or intercept standard code blocks labeled as "math"
    let mut in_code_block = None;

    let events = parser.map(|event| {
        match event {
            // Inline `$...$` and display `$$...$$` math (from ENABLE_MATH).
            Event::InlineMath(tex) => Event::Html(math_renderer(&tex, false).into()),
            Event::DisplayMath(tex) => Event::Html(math_renderer(&tex, true).into()),
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = Some(kind);
                Event::Text("".into()) // Skip default tag emission
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = None;
                Event::Text("".into())
            }
            Event::Text(text) => {
                if let Some(ref kind) = in_code_block {
                    match kind {
                        // A ```math fenced block is a *code block*: show the raw
                        // TeX as text, not KaTeX. (Inline $...$ / display $$...$$
                        // are what get math-rendered.)
                        pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                            // GitHub-style class-based highlighting: emit
                            // <span class="hl-..."> markup with NO inline colors.
                            // The colors live in the light/dark CSS injected in
                            // <head>, so code blocks follow prefers-color-scheme.
                            let syntax = ps
                                .find_syntax_by_token(lang)
                                .unwrap_or_else(|| ps.find_syntax_plain_text());
                            let mut hl = ClassedHTMLGenerator::new_with_class_style(
                                syntax,
                                &ps,
                                ClassStyle::SpacedPrefixed { prefix: HL_PREFIX },
                            );
                            for line in LinesWithEndings::from(&text) {
                                let _ = hl.parse_html_for_line_which_includes_newline(line);
                            }
                            // .code-wrap positions the copy button; the button's
                            // click handler reads this <pre><code>'s text.
                            Event::Html(
                                format!(
                                    "<div class=\"code-wrap\">{button}<pre class=\"hl-code\"><code>{code}</code></pre></div>",
                                    button = COPY_BUTTON,
                                    code = hl.finalize()
                                )
                                .into(),
                            )
                        }
                        _ => Event::Text(text)
                    }
                } else {
                    Event::Text(text)
                }
            }
            _ => event
        }
    });

    pulldown_cmark::html::push_html(&mut html_output, events);
    html_output
}

use std::env;
use std::fs;
use tiny_http::{Header, Response, Server};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: md-preview <file.md>");
        return;
    }
    let filename = &args[1];
    let markdown_input = fs::read_to_string(filename).expect("Failed to read file");

    let html_output = render_markdown(&markdown_input);
    let full_html = format!(
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
            </style>
        </head>
        <body class="markdown-body">
            {content}
        <script type="module">
import katex from 'https://cdn.jsdelivr.net/npm/katex@0.16.9/dist/katex.mjs';

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
        </script>
        </body>
        </html>"#,
        syntax_css = syntax_css(),
        content = html_output
    );

    let server = Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let url = format!("http://127.0.0.1:{}", port);

    println!("Preview ready: {}", url);

    // Try to open browser; on WSL the webbrowser crate uses explorer.exe automatically.
    // Don't panic on failure — user can open the URL manually.
    if let Err(e) = webbrowser::open(&url) {
        eprintln!("Could not open browser automatically: {}", e);
        eprintln!("Open this URL in your browser: {}", url);
    }

    // Serve requests until the root page has been delivered, skipping side requests
    // like /favicon.ico that browsers send before the main page.
    for request in server.incoming_requests() {
        let url_path = request.url().to_string();
        let response = if url_path == "/" || url_path.starts_with("/?") {
            let r = Response::from_string(full_html.clone())
                .with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                        .unwrap(),
                );
            let _ = request.respond(r);
            println!("Preview served. Shutting down.");
            break;
        } else {
            // Answer side requests (favicon etc.) with 204 No Content and keep waiting.
            Response::empty(204)
        };
        let _ = request.respond(response);
    }
}
