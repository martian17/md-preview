//! The `md-preview` binary.
//!
//! With the default `daemon` feature this starts the persistent live-preview
//! server ([`md_preview::server`]) and opens the browser at `/view`. Without the
//! feature it falls back to a minimal one-shot render to stdout, so the crate
//! still builds and is useful with zero web dependencies.

#[cfg(feature = "daemon")]
fn main() {
    use std::path::PathBuf;
    use std::process::ExitCode;

    // Tiny hand-rolled arg parsing — no clap dependency for two flags.
    //   md-preview <file.md> [--no-open]
    // Headless: pass `--no-open`, set `BROWSER=/bin/true`, or `MD_PREVIEW_NO_OPEN`.
    let mut file: Option<PathBuf> = None;
    let mut no_open = std::env::var_os("MD_PREVIEW_NO_OPEN").is_some();

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--no-open" => no_open = true,
            "-h" | "--help" => {
                eprintln!("Usage: md-preview <file.md> [--no-open]");
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
            eprintln!("Usage: md-preview <file.md> [--no-open]");
            std::process::exit(2);
        }
    };

    let abs = match std::fs::canonicalize(&file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Cannot resolve {}: {e}", file.display());
            std::process::exit(1);
        }
    };

    // Infer the confinement root for this file (parent dir; daemon refines it).
    let root = abs
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());

    // Single-instance election: first invocation becomes the daemon; all later
    // invocations are thin clients that delegate over the unix control socket.
    match md_preview::control::bind_or_detect() {
        Ok(md_preview::control::Election::Client(handle)) => {
            let req = md_preview::control::Request::Open {
                path: abs.to_string_lossy().into_owned(),
                root,
            };
            match handle.send_request_blocking(&req) {
                Ok(md_preview::control::Response::Opened { url, nonce }) => {
                    if no_open {
                        println!("Preview ready (headless): {url}");
                    } else if let Err(e) = open_via_bootstrap(&url, &nonce) {
                        eprintln!("Could not open browser: {e}");
                        eprintln!("Open this URL in your browser: {url}");
                    }
                }
                Ok(md_preview::control::Response::Error { message }) => {
                    eprintln!("Daemon error: {message}");
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("Unexpected daemon response: {other:?}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Control channel error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Ok(md_preview::control::Election::Daemon(ctrl_listener)) => {
            // We are the daemon: spin up the server with the control accept loop.
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("Failed to start runtime: {e}");
                    std::process::exit(1);
                }
            };

            let code = rt.block_on(async move {
                match md_preview::server::serve_with_control(&file, ctrl_listener).await {
                    Ok(()) => ExitCode::SUCCESS,
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
        Err(e) => {
            eprintln!("Control socket error: {e}");
            std::process::exit(1);
        }
    }
}

/// Write a `0600` bootstrap HTML file and open the browser at it.
///
/// The bootstrap page auto-POSTs the nonce to `/claim` (never in argv or URL),
/// receives a session cookie, then redirects to the real `/view?path=...` URL.
/// Per ADR-0006: the nonce lives in a `0600` file inside the `0700` runtime dir;
/// only the `file://` path (never the nonce) enters the browser's argv.
#[cfg(feature = "daemon")]
fn open_via_bootstrap(target_url: &str, nonce: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // Build /claim URL from the target URL's origin.
    let claim_url = {
        let rest = target_url
            .strip_prefix("http://")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad scheme"))?;
        let host_end = rest.find('/').unwrap_or(rest.len());
        format!("http://{}/claim", &rest[..host_end])
    };

    // Auto-submitting script: POST nonce → get cookie → redirect to view.
    let html = format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<script>(function(){{",
            "var n={nonce_json};var t={target_json};var c={claim_json};",
            "fetch(c,{{method:'POST',headers:{{'Content-Type':'text/plain'}},body:n}})",
            ".then(function(r){{window.location.replace(t);}},",
            "function(e){{document.body.textContent='Error: '+e;}});",
            "}})();</script>",
            "</head><body>Authenticating\u{2026}</body></html>"
        ),
        nonce_json = json_lit(nonce),
        target_json = json_lit(target_url),
        claim_json = json_lit(&claim_url),
    );

    // Place bootstrap file inside the already-0700 control dir.
    let dir = md_preview::control::socket_path()
        .map_err(|e| std::io::Error::other(e.to_string()))?
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| std::io::Error::other("no parent dir"))?;
    let bootstrap_path = dir.join("bootstrap.html");
    let _ = std::fs::remove_file(&bootstrap_path);

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&bootstrap_path)?;
        f.write_all(html.as_bytes())?;
    }

    let file_url = format!("file://{}", bootstrap_path.display());
    println!("Preview ready: {target_url}");

    if let Some(browser) = std::env::var_os("BROWSER") {
        if let Err(e) = std::process::Command::new(&browser).arg(&file_url).spawn() {
            eprintln!("Could not run $BROWSER ({browser:?}): {e}");
            eprintln!("Open this URL in your browser: {target_url}");
        }
    } else if let Err(e) = webbrowser::open(&file_url) {
        eprintln!("Could not open browser automatically: {e}");
        eprintln!("Open this URL in your browser: {target_url}");
    }

    Ok(())
}

/// Produce a JSON string literal (with surrounding quotes) safe to embed in a
/// `<script>`. Escapes `<`, `>`, `&`, `"`, `\`, and control chars.
#[cfg(feature = "daemon")]
fn json_lit(s: &str) -> String {
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
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Fallback build with no web dependencies: render the file to stdout.
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
