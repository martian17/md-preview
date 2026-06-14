use pulldown_cmark::{Parser, Event, Tag, TagEnd};
use pulldown_cmark::{html, Options};
use syntect::parsing::SyntaxSet;
use syntect::html::highlighted_html_for_string;
use syntect::highlighting::ThemeSet;

fn render_markdown(markdown_input: &str) -> String {
    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();
    // Use a theme like "Base16 Ocean" or a custom GitHub-like theme
    let theme = &ts.themes["base16-ocean.dark"]; 


    // // 2. Setup GFM Options
    // let mut options = Options::empty();
    // options.insert(Options::ENABLE_TABLES);
    // options.insert(Options::ENABLE_FOOTNOTES);
    // options.insert(Options::ENABLE_STRIKETHROUGH);
    // options.insert(Options::ENABLE_TASKLISTS);
    // options.insert(Options::ENABLE_SMART_PUNCTUATION);

    // let parser = Parser::new_ext(markdown_input, options);

    let parser = Parser::new(markdown_input);
    let mut html_output = String::new();
    
    // Track if we are inside a math block if your parser supports it,
    // or intercept standard code blocks labeled as "math"
    let mut in_code_block = None;

    let events = parser.map(|event| {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                println!("{:?}", kind);
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
                        pulldown_cmark::CodeBlockKind::Fenced(lang) if lang.as_ref() == "math" => {
                            // Target GitHub's exact block math wrapper
                            let raw_tex = text.trim();
                            Event::Html(format!(
                                r#"<math-renderer class="js-display-math" style="display: block" data-run-id="unique-id">{}</math-renderer>"#, 
                                raw_tex
                            ).into())
                        }
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
use std::net::TcpListener;
use tiny_http::{Header, Response, Server};

fn main() {
    // 1. Get the filename from args
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: md <file.md>");
        return;
    }
    let filename = &args[1];
    let markdown_input = fs::read_to_string(filename).expect("Failed to read file");

    let html_output = render_markdown(&markdown_input);

    // 4. Wrap in GitHub-styled Template
    let full_html = format!(
        r#"<!DOCTYPE html>
        <html>
        <head>
            <meta charset="utf-8">
            <meta name="viewport" content="width=device-width, initial-scale=1">
            <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/github-markdown-css/5.5.1/github-markdown.min.css">
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

    // 5. Start Server on an ephemeral port
    let server = Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let url = format!("http://127.0.0.1:{}", port);

    println!("Serving preview at {}", url);
    webbrowser::open(&url).expect("Failed to open browser");

    // 6. Handle exactly one request then kill the process
    if let Some(request) = server.incoming_requests().next() {
        let response = Response::from_string(full_html)
            .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..]).unwrap());
        let _ = request.respond(response);
        println!("Preview served. Shutting down.");
    }
}
