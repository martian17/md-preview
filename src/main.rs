//! The `md-preview` binary.
//!
//! With the default `daemon` feature this starts the persistent live-preview
//! server ([`md_preview::server`]) and opens the browser at `/view`. Without the
//! feature it falls back to a minimal one-shot render to stdout, so the crate
//! still builds and is useful with zero web dependencies.

#[cfg(feature = "daemon")]
fn main() {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::process::ExitCode;

    // Tiny hand-rolled arg parsing — no clap dependency for three flags.
    //   md-preview <file.md> [--port N] [--no-open]
    // Headless operation: pass `--no-open`, or set `BROWSER` (honoured below as
    // the opener command, so `BROWSER=/bin/true` opens nothing), or set
    // `MD_PREVIEW_NO_OPEN`.
    let mut file: Option<PathBuf> = None;
    let mut port: u16 = default_port();
    let mut no_open = std::env::var_os("MD_PREVIEW_NO_OPEN").is_some();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-open" => no_open = true,
            "--port" => {
                match args.next().and_then(|p| p.parse::<u16>().ok()) {
                    Some(p) => port = p,
                    None => {
                        eprintln!("--port requires a number");
                        std::process::exit(2);
                    }
                }
            }
            "-h" | "--help" => {
                eprintln!("Usage: md-preview <file.md> [--port N] [--no-open]");
                return;
            }
            other if file.is_none() && !other.starts_with('-') => {
                file = Some(PathBuf::from(other));
            }
            other => {
                eprintln!("Unexpected argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let file = match file {
        Some(f) => f,
        None => {
            eprintln!("Usage: md-preview <file.md> [--port N] [--no-open]");
            std::process::exit(2);
        }
    };

    let addr: SocketAddr = ([127, 0, 0, 1], port).into();

    // Build the preview URL: /view with the file's CANONICAL ABSOLUTE path as the
    // (URL-encoded) `path` query parameter. A document is identified by its
    // canonical absolute path; the daemon confines it against the active roots.
    let abs = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
    let url = format!(
        "http://{addr}/view?path={}",
        encode_path_query(&abs.to_string_lossy())
    );

    // A multi-threaded runtime: the blocking watch loop runs on its own OS
    // thread, but the WS forwarding + HTTP serving want async workers.
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to start runtime: {e}");
            std::process::exit(1);
        }
    };

    let code = rt.block_on(async move {
        // Bring the server up; serve() validates + confines the file and spawns
        // the watch thread before it starts accepting connections.
        let server = md_preview::server::serve(&file, addr);

        if no_open {
            println!("Preview ready (headless): {url}");
        } else if let Some(browser) = std::env::var_os("BROWSER") {
            // Honour an explicit opener command (lets CI use BROWSER=/bin/true).
            println!("Preview ready: {url}");
            if let Err(e) = std::process::Command::new(&browser).arg(&url).spawn() {
                eprintln!("Could not run $BROWSER ({browser:?}): {e}");
                eprintln!("Open this URL in your browser: {url}");
            }
        } else {
            println!("Preview ready: {url}");
            if let Err(e) = webbrowser::open(&url) {
                eprintln!("Could not open browser automatically: {e}");
                eprintln!("Open this URL in your browser: {url}");
            }
        }

        match server.await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                eprintln!(
                    "md-preview is already using port {port} — \
                     open http://127.0.0.1:{port}/view?path=... \
                     or pass --port N to run a second instance"
                );
                ExitCode::FAILURE
            }
            Err(e) => {
                eprintln!("Server error: {e}");
                ExitCode::FAILURE
            }
        }
    });

    std::process::exit(match code {
        c if c == ExitCode::SUCCESS => 0,
        _ => 1,
    });
}

/// Minimal percent-encoding for the `path=` query value (an absolute path):
/// keep the unreserved set plus `/`/`.`/`-`/`_`, percent-encode the rest. Mirrors
/// the server-side encoder; avoids a URL-encoding crate for one call site.
#[cfg(feature = "daemon")]
fn encode_path_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Default port for the daemon, overridable via `--port` or the `MD_PREVIEW_PORT`
/// env var. Each invocation starts its own server on this port; if the port is
/// already in use the process exits with a clear message and a non-zero status.
#[cfg(feature = "daemon")]
fn default_port() -> u16 {
    std::env::var("MD_PREVIEW_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7878)
}

/// Fallback build with no web dependencies: render the file to stdout. Keeps the
/// crate buildable and useful under `--no-default-features`.
#[cfg(not(feature = "daemon"))]
fn main() {
    use std::path::PathBuf;

    let file = std::env::args().nth(1).map(PathBuf::from);
    let Some(file) = file else {
        eprintln!("Usage: md-preview <file.md>");
        eprintln!("(built without the `daemon` feature: renders to stdout)");
        std::process::exit(2);
    };

    match std::fs::read_to_string(&file) {
        Ok(md) => print!("{}", md_preview::render_page(&md)),
        Err(e) => {
            eprintln!("Failed to read {}: {e}", file.display());
            std::process::exit(1);
        }
    }
}
