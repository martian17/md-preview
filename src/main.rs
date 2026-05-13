use pulldown_cmark::{html, Options, Parser};
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

    // 2. Setup GFM Options
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    // 3. Convert Markdown to HTML
    let parser = Parser::new_ext(&markdown_input, options);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);

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
