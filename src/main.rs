use pulldown_cmark::{html, Options, Parser};
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

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    let parser = Parser::new_ext(&markdown_input, options);
    let mut html_output = String::new();
    html::push_html(&mut html_output, parser);

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
        body {{ background-color: #0d1117; }}
    </style>
</head>
<body class="markdown-body">
    {}
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
