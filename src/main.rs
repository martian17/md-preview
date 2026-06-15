use md_preview::render_page;
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

    let full_html = render_page(&markdown_input);

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

    // Serve requests until the root page has been delivered. Browsers send side
    // requests (e.g. /favicon.ico) before the main page; answer those with 204
    // No Content and keep waiting for the root request.
    for request in server.incoming_requests() {
        let path = request.url();
        if path == "/" || path.starts_with("/?") {
            let response = Response::from_string(full_html.clone()).with_header(
                Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
            );
            let _ = request.respond(response);
            println!("Preview served. Shutting down.");
            break;
        }
        let _ = request.respond(Response::empty(204));
    }
}
