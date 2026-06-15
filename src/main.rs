use pulldown_cmark::{Parser, Event, Tag, TagEnd};
use pulldown_cmark::Options;
use syntect::parsing::SyntaxSet;
use syntect::html::highlighted_html_for_string;
use syntect::highlighting::ThemeSet;

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
    let ts = ThemeSet::load_defaults();
    // Use a theme like "Base16 Ocean" or a custom GitHub-like theme
    let theme = &ts.themes["base16-ocean.dark"];

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
                            // Server-computed syntax highlighting
                            let syntax = ps.find_syntax_by_token(lang).unwrap_or_else(|| ps.find_syntax_plain_text());
                            let highlighted = highlighted_html_for_string(&text, &ps, syntax, theme).unwrap();
                            Event::Html(highlighted.into())
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
                body {{ background-color: #0d1117; }} /* GitHub Dark Mode match */
            </style>
        </head>
        <body class="markdown-body">
            {}
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
        </script>
        </body>
        </html>"#,
        html_output
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
