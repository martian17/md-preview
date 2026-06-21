//! Integration test: single-instance election + process reuse.
//!
//! Proves that a second `Open` request is served by the SAME daemon on the SAME
//! port — no rebind, no `AddrInUse` — and that each control round-trip returns
//! a usable (non-empty, distinct) `url` and `nonce`.
//!
//! The test spins up a minimal in-process daemon: a unix control listener (from
//! the election) plus a loopback TCP listener whose port is embedded in the
//! `Opened{url}` responses. It does not start the full HTTP server stack — it
//! only needs to prove that two `Open` requests are served by the SAME instance.

#![cfg(feature = "daemon")]

use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use md_preview::control::{Election, Request, Response, serve_connection};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Override XDG_RUNTIME_DIR for the lifetime of the closure so the test uses
/// an isolated socket path and does not collide with a running daemon.
fn isolated<F: FnOnce() -> R, R>(f: F) -> R {
    let tmp = tempfile::tempdir().expect("runtime_dir tempdir");
    // SAFETY: single-threaded test process section; env is restored after.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
    let result = f();
    unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    result
}

// ---------------------------------------------------------------------------
// Process-reuse test
// ---------------------------------------------------------------------------

#[test]
fn process_reuse_same_daemon_same_port() {
    isolated(|| {
        // --- Election 1: first invocation → Daemon ---------------------------
        let ctrl_listener = match md_preview::control::bind_or_detect().expect("election 1") {
            Election::Daemon(l) => l,
            Election::Client(_) => panic!("election 1 must be Daemon (fresh socket)"),
        };

        // Bind a loopback TCP port to represent "the daemon's HTTP address".
        // We hold the listener so the OS keeps the port alive; we extract the
        // port for use in Opened{url} responses.
        let http_listener = TcpListener::bind("127.0.0.1:0").expect("bind http port");
        let http_addr = http_listener.local_addr().expect("local_addr");
        let http_port = http_addr.port();

        // Nonce counter for the daemon stub — generates unique nonces.
        let nonce_counter = Arc::new(AtomicU64::new(0));

        // Spawn the control accept loop on a blocking thread (same pattern as
        // the real daemon in serve_with_control).
        let nonce_counter_thread = nonce_counter.clone();
        std::thread::spawn(move || {
            for conn in ctrl_listener.incoming() {
                match conn {
                    Ok(stream) => {
                        let nc = nonce_counter_thread.clone();
                        let port = http_port;
                        std::thread::spawn(move || {
                            let _ = serve_connection(stream, |req| match req {
                                Request::Ping | Request::Shutdown => Response::Pong {
                                    version: md_preview::control::build_version().to_string(),
                                    edit_mode: false,
                                },
                                Request::Open { path, root: _ } => {
                                    let n = nc.fetch_add(1, Ordering::SeqCst);
                                    let nonce = format!("nonce-{n}");
                                    let url =
                                        format!("http://127.0.0.1:{port}/view?path={path}");
                                    Response::Opened { url, nonce }
                                }
                            });
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        // Allow the daemon accept loop to start listening.
        std::thread::sleep(Duration::from_millis(20));

        // --- Election 2: second invocation → Client --------------------------
        let client = match md_preview::control::bind_or_detect().expect("election 2") {
            Election::Client(c) => c,
            Election::Daemon(_) => panic!("election 2 must be Client (daemon is live)"),
        };

        // First Open request.
        let resp1 = client
            .send_request_blocking(&Request::Open {
                path: "/tmp/a.md".into(),
                root: "/tmp".into(),
            })
            .expect("first Open round-trip");

        let (url1, nonce1) = match resp1 {
            Response::Opened { url, nonce } => (url, nonce),
            other => panic!("expected Opened, got {other:?}"),
        };
        assert!(!url1.is_empty(), "url1 must be non-empty");
        assert!(!nonce1.is_empty(), "nonce1 must be non-empty");

        // Second Open request — SAME client, same daemon.
        let resp2 = client
            .send_request_blocking(&Request::Open {
                path: "/tmp/b.md".into(),
                root: "/tmp".into(),
            })
            .expect("second Open round-trip");

        let (url2, nonce2) = match resp2 {
            Response::Opened { url, nonce } => (url, nonce),
            other => panic!("expected Opened, got {other:?}"),
        };
        assert!(!url2.is_empty(), "url2 must be non-empty");
        assert!(!nonce2.is_empty(), "nonce2 must be non-empty");

        // THE process-reuse assertion: both responses reference the SAME port.
        let port1 = extract_port(&url1);
        let port2 = extract_port(&url2);
        assert_eq!(
            port1, port2,
            "both Opened responses must embed the same daemon HTTP port"
        );
        assert_eq!(
            port1,
            http_port.to_string(),
            "the port must be the one the daemon bound — not a re-bound address"
        );

        // Each Open must produce a distinct nonce (single-use guarantee).
        assert_ne!(nonce1, nonce2, "each Open must mint a distinct nonce");

        // Third election against the live daemon: still a Client (no rebind).
        match md_preview::control::bind_or_detect().expect("election 3") {
            Election::Client(_) => {} // expected
            Election::Daemon(_) => panic!("election 3 must still be Client"),
        }

        // Keep http_listener alive until here so the port stays bound.
        drop(http_listener);
    });
}

fn extract_port(url: &str) -> String {
    // "http://127.0.0.1:<port>/view?path=..."
    url.split(':')
        .nth(2)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}
