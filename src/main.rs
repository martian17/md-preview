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

    // Tiny hand-rolled arg parsing — no clap dependency for four flags.
    //   md-preview <file.md> [--no-open]
    //   md-preview --warm-cache
    //   md-preview --daemon   (used by the systemd user unit)
    // Headless: pass `--no-open`, set `BROWSER=/bin/true`, or `MD_PREVIEW_NO_OPEN`.
    let mut file: Option<PathBuf> = None;
    let mut no_open = std::env::var_os("MD_PREVIEW_NO_OPEN").is_some();
    let mut warm_cache = false;
    let mut daemon_flag = false;

    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--no-open" => no_open = true,
            "--warm-cache" => warm_cache = true,
            // --daemon: start the server without opening a document; used by the
            // systemd user unit so the daemon is always-up even before `md <file>`
            // is first run.  The election still applies — if another instance is
            // already running this becomes a no-op client and exits cleanly.
            "--daemon" => daemon_flag = true,
            "-h" | "--help" => {
                eprintln!("Usage: md-preview <file.md> [--no-open]");
                eprintln!("       md-preview --warm-cache");
                eprintln!("       md-preview --daemon");
                eprintln!();
                eprintln!("md-preview <file.md> opens a live preview in your browser. The");
                eprintln!("first invocation auto-spawns a detached background daemon, then");
                eprintln!("returns immediately; later invocations reuse that daemon.");
                eprintln!();
                eprintln!("  --no-open     register the file and print the URL; do not open a");
                eprintln!("                browser (also via MD_PREVIEW_NO_OPEN=1).");
                eprintln!("  --warm-cache  pre-fetch + verify all pinned bundle assets, exit.");
                eprintln!("  --daemon      run the daemon in the foreground (for the systemd");
                eprintln!("                user unit). Exits cleanly if one is already running.");
                eprintln!();
                eprintln!("Environment:");
                eprintln!("  BROWSER   opener command (instead of the system default). Setting");
                eprintln!("            BROWSER=true (or /bin/true) intentionally opens nothing.");
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

    // --warm-cache: pre-fetch + verify + store every pinned bundle asset, then exit.
    // Requires the `daemon` feature (bundle cache is daemon-only).
    if warm_cache {
        let cache_dir = match md_preview::bundle::default_cache_dir() {
            Some(d) => d,
            None => {
                eprintln!("--warm-cache: cannot determine cache dir (neither XDG_CACHE_HOME nor HOME is set)");
                std::process::exit(1);
            }
        };
        let cache = md_preview::bundle::BundleCache::new(&cache_dir);
        let fetcher = md_preview::bundle::UreqFetcher;
        eprintln!("Warming bundle cache at {} …", cache_dir.display());
        let summary = md_preview::bundle::warm_all(&cache, &fetcher);
        for id in &summary.fetched {
            eprintln!("  fetched:  {id}");
        }
        for id in &summary.already_cached {
            eprintln!("  cached:   {id}");
        }
        for (id, err) in &summary.failed {
            eprintln!("  FAILED:   {id}: {err}");
        }
        if summary.all_ok() {
            eprintln!("All {} bundle assets verified and cached.", summary.fetched.len() + summary.already_cached.len());
            return;
        } else {
            eprintln!("{} asset(s) failed; daemon may degrade on those assets while offline.", summary.failed.len());
            std::process::exit(1);
        }
    }

    // --daemon without a file: start (or join) the always-on daemon, no document.
    if daemon_flag && file.is_none() {
        match md_preview::control::bind_or_detect() {
            Ok(md_preview::control::Election::Client(_handle)) => {
                // Another daemon instance is already running — nothing to do.
                return;
            }
            Ok(md_preview::control::Election::Daemon(ctrl_listener)) => {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("Failed to start runtime: {e}");
                        std::process::exit(1);
                    }
                };
                let code = rt.block_on(async move {
                    match md_preview::server::serve_daemon_only(ctrl_listener).await {
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

    // Auto-spawn is the default: the `<file>` path is ALWAYS a thin client. It
    // never becomes the long-lived foreground daemon itself — it either connects
    // to a running daemon, or spawns a DETACHED `md-preview --daemon` background
    // process and then connects to that. Either way this process returns promptly
    // and the terminal is freed.
    let req = md_preview::control::Request::Open {
        path: abs.to_string_lossy().into_owned(),
        root,
    };

    let handle = match connect_or_spawn_daemon() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Could not reach or start the md-preview daemon: {e}");
            std::process::exit(1);
        }
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

/// Connect to a running daemon, or spawn a detached one and connect to it.
///
/// 1. If a daemon is already listening on the control socket, return a client
///    handle to it immediately.
/// 2. Otherwise spawn `md-preview --daemon` as a **detached background process**
///    (new session via `setsid`, stdio redirected away from the terminal) and
///    poll the control socket until it is connectable (bounded ~5s timeout).
///
/// The spawned daemon's own [`md_preview::control::bind_or_detect`] performs the
/// single-instance election — so a spawn race (two `md-preview <file>` at once)
/// has exactly one winner; the loser's `--daemon` becomes a no-op client and
/// exits, and both `<file>` callers just connect to the survivor.
#[cfg(feature = "daemon")]
fn connect_or_spawn_daemon() -> std::io::Result<md_preview::control::ClientHandle> {
    use std::time::{Duration, Instant};

    // Fast path: a daemon is already up.
    match md_preview::control::connect_client() {
        Ok(Some(handle)) => return Ok(handle),
        Ok(None) => {}
        Err(e) => return Err(std::io::Error::other(e.to_string())),
    }

    // No daemon yet — spawn one, detached.
    spawn_detached_daemon()?;

    // Poll until the freshly-spawned daemon is accepting connections. A spawn
    // race or a stale-socket reclaim may add a beat, so we give it ~5s.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match md_preview::control::connect_client() {
            Ok(Some(handle)) => return Ok(handle),
            Ok(None) => {}
            Err(e) => return Err(std::io::Error::other(e.to_string())),
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for the spawned md-preview daemon to start listening",
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Spawn `md-preview --daemon` as a fully detached background process.
///
/// Detach mechanism (POSIX): `pre_exec(setsid)` puts the child in a new session
/// and process group so it survives the launching shell (no controlling TTY, not
/// in our process group), and stdio is redirected to a daemon log file under the
/// state/cache dir (falling back to `/dev/null`) so it never ties up the
/// terminal. We do not wait on the child — it lives independently.
#[cfg(feature = "daemon")]
fn spawn_detached_daemon() -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;

    // Where to send the daemon's stdout/stderr. Prefer a log file under the
    // cache dir so a crash leaves a breadcrumb; fall back to /dev/null.
    let (out, err): (Stdio, Stdio) = match daemon_log_target() {
        Some(file) => match file.try_clone() {
            Ok(clone) => (Stdio::from(file), Stdio::from(clone)),
            Err(_) => (Stdio::null(), Stdio::null()),
        },
        None => (Stdio::null(), Stdio::null()),
    };

    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);

    // SAFETY: `setsid()` is async-signal-safe and takes no arguments. We call it
    // in the forked child before exec to start a new session/process group; it
    // touches no memory we own and only fails (harmlessly) if we are already a
    // session leader, which the parent shell process is not.
    unsafe {
        cmd.pre_exec(|| {
            // Detach from the controlling terminal / our process group.
            if libc::setsid() == -1 {
                // Already a group leader is fine; only surface real failures.
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EPERM) {
                    return Err(e);
                }
            }
            Ok(())
        });
    }

    // Spawn and immediately drop the child handle — we do not wait on it.
    let _child = cmd.spawn()?;
    Ok(())
}

/// Open (creating if needed) the daemon log file under the cache/state dir.
///
/// Returns `None` (caller falls back to `/dev/null`) if no cache dir can be
/// determined or the file cannot be opened — never fatal.
#[cfg(feature = "daemon")]
fn daemon_log_target() -> Option<std::fs::File> {
    let dir = md_preview::bundle::default_cache_dir()?;
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))
        .ok()
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
