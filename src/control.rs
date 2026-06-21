//! Unix-socket control plane for the always-on preview daemon.
//!
//! This module implements the CLI↔daemon control channel from
//! `design/always-on-secure-preview.md` §2 ("Channels") and `ADR-0006`. It is
//! the **single-instance coordination layer**: the first `md` invocation binds
//! the socket and *becomes the daemon*; every later invocation detects the
//! running daemon over that same socket and *becomes a thin client*.
//!
//! ## Why a unix socket (not loopback TCP)
//! The control plane is authenticated **by the operating system**, so the CLI
//! needs no token at all:
//! - The socket is a `0600` file inside a `0700` directory under
//!   `$XDG_RUNTIME_DIR` — filesystem perms keep other users out.
//! - Every accepted connection is additionally checked with `SO_PEERCRED`: a
//!   peer whose uid != our uid is rejected. This is *belt-and-suspenders*
//!   against a future perms misconfiguration (the audit calls it legitimate
//!   defense-in-depth, not redundant with the file perms — it asserts the
//!   invariant in code). Loopback TCP, by contrast, is host-wide and exposed to
//!   DNS rebinding, so it is never used for control.
//!
//! ## Protocol
//! Newline-delimited JSON. Each request is one line; each response is one line.
//! The enums are *internally tagged* and deliberately **do not**
//! `deny_unknown_fields` — a newer client/daemon may add fields and the older
//! peer must tolerate them (forward-compat).
//!
//! ## Threat model
//! The dangerous caller is *lower-privilege than the user*; a same-uid process
//! is not in scope (it can already read the files). The peer-uid check exists
//! solely to fail closed if the directory/socket perms are ever wrong.
//!
//! ## No panics
//! No `unwrap`/`expect`/`panic` on any production path. Errors are surfaced via
//! [`ControlError`].

use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------

/// The build version embedded at compile time: `CARGO_PKG_VERSION`.
/// Exposed so both the daemon (in its `Pong`) and the client (for comparison)
/// can reference the same constant without duplicating the `env!` call.
pub fn build_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A control-plane request from the CLI to the daemon.
///
/// Internally tagged on `"type"`; unknown fields are tolerated for
/// forward-compatibility (no `deny_unknown_fields`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Ask the daemon to open `path` (resolved within `root`) and arm a
    /// browser-bootstrap claim. `root` is the registered root the path belongs
    /// to (ADR-0005); the daemon does the confinement + claim-nonce work.
    Open {
        /// The document path the CLI was invoked on.
        path: String,
        /// The registered root the path is expected to live under.
        root: String,
        /// Whether the client wants to open the collaborative editor (`--edit`
        /// flag on the CLI). When `true` the daemon's `Opened.url` points to
        /// `/edit?path=…`; when `false` (or absent — older clients) it points
        /// to `/view?path=…`. Ignored if the daemon is not in edit mode.
        #[serde(default)]
        want_edit: bool,
    },
    /// Liveness probe — used by [`bind_or_detect`] to confirm a peer that owns
    /// the socket is actually a live daemon.
    Ping,
    /// Ask the daemon to exit cleanly. Used by the thin client when it detects
    /// a version mismatch or a mode conflict (read-only vs. `--edit`), so it
    /// can respawn a fresh daemon at the correct version/mode. The daemon
    /// responds with [`Response::Pong`] (the current version) then exits.
    Shutdown,
}

/// A control-plane response from the daemon to the CLI.
///
/// Internally tagged on `"type"`; unknown fields are tolerated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    /// The document is being served; `url` is the `file://` bootstrap path the
    /// CLI should open, and `nonce` is the single-use claim nonce (per §2 the
    /// secret travels over the socket, never argv/URL).
    Opened {
        /// The `file://` bootstrap path to hand to the browser.
        url: String,
        /// The single-use, short-TTL claim nonce.
        nonce: String,
    },
    /// Reply to [`Request::Ping`] or [`Request::Shutdown`].
    ///
    /// Carries the daemon's build version and edit-mode flag so the client can
    /// detect a stale daemon (version mismatch) or a mode conflict (read-only
    /// vs. `--edit`) and respawn. Old daemons return `{"type":"Pong"}` with no
    /// extra fields; `#[serde(default)]` makes them deserialize as empty strings
    /// / false — the client treats a missing version as a mismatch.
    Pong {
        /// Daemon's `CARGO_PKG_VERSION` at compile time.
        #[serde(default)]
        version: String,
        /// Whether the daemon was started with `--edit` (collaborative editor
        /// routes enabled).
        #[serde(default)]
        edit_mode: bool,
    },
    /// The request could not be served; `message` is a human-readable reason.
    Error {
        /// Human-readable failure reason.
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised by the control plane.
#[derive(Debug)]
pub enum ControlError {
    /// A filesystem / socket I/O error.
    Io(io::Error),
    /// JSON (de)serialization of a frame failed.
    Protocol(serde_json::Error),
    /// The peer connected over the socket but its uid does not match ours.
    PeerUidMismatch {
        /// The uid reported by `SO_PEERCRED`.
        peer: u32,
        /// Our own effective uid.
        ours: u32,
    },
    /// The peer closed the connection before sending a complete frame.
    UnexpectedEof,
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::Io(e) => write!(f, "control socket I/O error: {e}"),
            ControlError::Protocol(e) => write!(f, "control protocol error: {e}"),
            ControlError::PeerUidMismatch { peer, ours } => {
                write!(f, "rejecting peer uid {peer} (daemon uid is {ours})")
            }
            ControlError::UnexpectedEof => write!(f, "peer closed connection mid-frame"),
        }
    }
}

impl std::error::Error for ControlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ControlError::Io(e) => Some(e),
            ControlError::Protocol(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ControlError {
    fn from(e: io::Error) -> Self {
        ControlError::Io(e)
    }
}

impl From<serde_json::Error> for ControlError {
    fn from(e: serde_json::Error) -> Self {
        ControlError::Protocol(e)
    }
}

// ---------------------------------------------------------------------------
// Paths & perms
// ---------------------------------------------------------------------------

/// `0700` — owner-only directory.
const DIR_MODE: u32 = 0o700;
/// `0600` — owner-only socket file.
const SOCK_MODE: u32 = 0o600;

/// Name of the per-user control directory.
const DIR_NAME: &str = "md-preview";
/// Name of the control socket within [`DIR_NAME`].
const SOCK_NAME: &str = "sock";

/// Resolve the directory that should hold the control socket.
///
/// Prefers `$XDG_RUNTIME_DIR` (the correct, per-user, tmpfs-backed,
/// auto-cleaned location). Falls back to `$TMPDIR/md-preview-<uid>` and then
/// `/tmp/md-preview-<uid>` when it is unset (e.g. non-systemd logins, cron),
/// keeping the path uid-scoped so two users never collide. The returned dir is
/// **not** created here; [`control_dir`] does that with the right mode.
fn runtime_base() -> Result<PathBuf, ControlError> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir).join(DIR_NAME));
    }
    // Fallback: no runtime dir. Scope by uid so the path is unambiguous.
    let uid = our_uid();
    let leaf = format!("{DIR_NAME}-{uid}");
    if let Some(tmp) = std::env::var_os("TMPDIR")
        && !tmp.is_empty()
    {
        return Ok(PathBuf::from(tmp).join(leaf));
    }
    Ok(PathBuf::from("/tmp").join(leaf))
}

/// Create (if needed) and return the `0700` control directory.
///
/// The mode is applied via [`std::fs::DirBuilder::mode`] so the directory is
/// created owner-only from the start (no `0700` race window). If it already
/// exists we re-assert the mode so a previously-loose dir is tightened.
fn control_dir() -> Result<PathBuf, ControlError> {
    let dir = runtime_base()?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(DIR_MODE);
    builder.create(&dir)?;
    // Re-assert in case the dir pre-existed with a looser mode.
    std::fs::set_permissions(
        &dir,
        std::os::unix::fs::PermissionsExt::from_mode(DIR_MODE),
    )?;
    Ok(dir)
}

/// The control socket path inside the `0700` directory.
///
/// Creates the directory as a side effect (with the right mode).
pub fn socket_path() -> Result<PathBuf, ControlError> {
    Ok(control_dir()?.join(SOCK_NAME))
}

// ---------------------------------------------------------------------------
// SO_PEERCRED — the only production `unsafe`
// ---------------------------------------------------------------------------

/// Our own effective uid via `geteuid`.
///
/// # Safety
/// `libc::geteuid` is an FFI call to a POSIX function that is always safe to
/// call: it takes no arguments, never fails, touches no memory we own, and
/// simply returns the calling process's effective uid. This is one of the two
/// documented production `unsafe` sites in this module.
fn our_uid() -> u32 {
    // SAFETY: see the doc comment above — `geteuid()` is argument-free and
    // infallible, returning a scalar; no invariants to uphold.
    unsafe { libc::geteuid() }
}

/// Read the peer's uid from a connected stream via `SO_PEERCRED`.
///
/// Returns the uid of the process on the other end of `stream`. This is the
/// kernel-attested identity of the connecting process (Linux `SO_PEERCRED`),
/// not anything the peer can spoof.
///
/// # Safety
/// This is the second (and last) production `unsafe` site. We call
/// `libc::getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &mut ucred, &mut len)`:
/// - `fd` is borrowed from a live, owned [`UnixStream`] for the duration of the
///   call, so it is valid.
/// - `cred` is a stack [`libc::ucred`] we fully own; the kernel writes at most
///   `len` bytes into it, and `len` is initialized to `size_of::<ucred>()`, so
///   there is no out-of-bounds write.
/// - We check the return value and the written length before trusting `cred`,
///   so an unexpected short write is treated as an error rather than read as
///   uninitialized memory.
///
/// No memory we hand to the kernel outlives the call; no aliasing.
fn peer_uid(stream: &UnixStream) -> Result<u32, ControlError> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: see the doc comment above — fd is valid for the call, `cred` is a
    // fully-owned, correctly-sized out-param, and we validate rc + len after.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast::<libc::c_void>(),
            std::ptr::addr_of_mut!(len),
        )
    };
    if rc != 0 {
        return Err(ControlError::Io(io::Error::last_os_error()));
    }
    if (len as usize) < std::mem::size_of::<libc::ucred>() {
        return Err(ControlError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned a short ucred",
        )));
    }
    Ok(cred.uid)
}

/// Reject a connection whose peer uid is not our own uid.
///
/// Defense-in-depth behind the `0600`/`0700` filesystem perms: it asserts the
/// same-uid invariant in code, so a future perms misconfiguration still fails
/// closed.
pub fn assert_same_uid(stream: &UnixStream) -> Result<(), ControlError> {
    let ours = our_uid();
    let peer = peer_uid(stream)?;
    if peer != ours {
        return Err(ControlError::PeerUidMismatch { peer, ours });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Frame I/O
// ---------------------------------------------------------------------------

/// Write a value as a single newline-delimited JSON frame.
fn write_frame<T: Serialize, W: Write>(w: &mut W, value: &T) -> Result<(), ControlError> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()?;
    Ok(())
}

/// Read one newline-delimited JSON frame and decode it.
fn read_frame<T: for<'de> Deserialize<'de>, R: BufRead>(r: &mut R) -> Result<T, ControlError> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 {
        return Err(ControlError::UnexpectedEof);
    }
    Ok(serde_json::from_str(line.trim_end_matches('\n'))?)
}

// ---------------------------------------------------------------------------
// Single-instance election
// ---------------------------------------------------------------------------

/// Outcome of [`bind_or_detect`]: either we became the daemon (and own the
/// listener) or we detected a live daemon (and should act as a client).
pub enum Election {
    /// We bound the socket and are the daemon. Serve on this listener.
    Daemon(UnixListener),
    /// A live daemon already owns the socket; act as a thin client.
    Client(ClientHandle),
}

/// A thin-client handle: knows the socket path so it can open short-lived
/// connections for [`ClientHandle::send_request_blocking`].
pub struct ClientHandle {
    path: PathBuf,
}

impl ClientHandle {
    /// Send one request to the running daemon and read its response. Opens a
    /// fresh connection per call (the protocol is one round-trip per frame).
    pub fn send_request_blocking(&self, req: &Request) -> Result<Response, ControlError> {
        send_request_to(&self.path, req)
    }
}

/// Try to become the single daemon; otherwise detect the running one.
///
/// Election algorithm (the stale-socket race is handled, per the audit):
/// 1. Try to `bind` the socket. On success we are the daemon: tighten perms to
///    `0600` and return [`Election::Daemon`].
/// 2. If `bind` fails with `AddrInUse`, a path already exists. Probe it with a
///    [`Request::Ping`]:
///    - A reply means a **live** daemon owns it → become a [`Election::Client`].
///    - A connect that fails with `ConnectionRefused` (`ECONNREFUSED`) means the
///      file is a **stale** socket nobody is listening on → unlink it and retry
///      the bind once.
pub fn bind_or_detect() -> Result<Election, ControlError> {
    let path = socket_path()?;
    elect(&path)
}

/// Detect a live daemon **without ever binding** the socket.
///
/// This is the thin-client entry point used by the `md-preview <file>` launch
/// path (which must *never* itself become the long-lived daemon). It probes the
/// control socket and:
/// - returns `Ok(Some(handle))` when a live daemon is listening (the caller can
///   immediately send an `Open`),
/// - returns `Ok(None)` when there is no daemon yet (no socket, or only a stale
///   one) so the caller can spawn a detached `--daemon` and poll back here,
/// - propagates other I/O errors.
///
/// Unlike [`bind_or_detect`], this never unlinks a stale socket and never binds:
/// reclaiming a stale socket is the spawned daemon's job (its own
/// `bind_or_detect` does it), so the launch path stays side-effect free.
pub fn connect_client() -> Result<Option<ClientHandle>, ControlError> {
    let path = socket_path()?;
    match probe_live(&path) {
        Ok(true) => Ok(Some(ClientHandle { path })),
        Ok(false) => Ok(None),
        Err(e) => Err(e),
    }
}

/// [`bind_or_detect`] against an explicit path (testable seam).
fn elect(path: &Path) -> Result<Election, ControlError> {
    match UnixListener::bind(path) {
        Ok(listener) => {
            set_socket_perms(path)?;
            Ok(Election::Daemon(listener))
        }
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // Something is at the path. Is it a live daemon or a stale file?
            match probe_live(path) {
                Ok(true) => Ok(Election::Client(ClientHandle {
                    path: path.to_path_buf(),
                })),
                Ok(false) => {
                    // Stale socket: nobody listening. Unlink and rebind once.
                    std::fs::remove_file(path)?;
                    let listener = UnixListener::bind(path)?;
                    set_socket_perms(path)?;
                    Ok(Election::Daemon(listener))
                }
                Err(err) => Err(err),
            }
        }
        Err(e) => Err(ControlError::Io(e)),
    }
}

/// Tighten the bound socket to `0600`.
fn set_socket_perms(path: &Path) -> Result<(), ControlError> {
    std::fs::set_permissions(
        path,
        std::os::unix::fs::PermissionsExt::from_mode(SOCK_MODE),
    )?;
    Ok(())
}

/// Probe whether a live daemon is listening at `path`.
///
/// Returns `Ok(true)` if we connected (and the path is owned by a listener),
/// `Ok(false)` if the connect was refused / the path is gone (stale socket),
/// and propagates other I/O errors.
fn probe_live(path: &Path) -> Result<bool, ControlError> {
    match UnixStream::connect(path) {
        Ok(stream) => {
            // Connected — round-trip a Ping to confirm it speaks the protocol.
            // Either way it accepted the connection, so it is live: do not
            // unlink a socket someone owns even if the exchange hiccups.
            let _ = ping_over(stream);
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(ControlError::Io(e)),
    }
}

/// Send a `Ping` and read a frame back, confirming the protocol.
fn ping_over(stream: UnixStream) -> Result<(), ControlError> {
    let mut writer = stream.try_clone()?;
    write_frame(&mut writer, &Request::Ping)?;
    let mut reader = BufReader::new(stream);
    let _: Response = read_frame(&mut reader)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Connect to the daemon at `path`, send `req`, and return its `Response`.
///
/// Blocking, one round-trip; the thin client (a later wave's `main.rs`) uses
/// this after [`bind_or_detect`] returns [`Election::Client`].
fn send_request_to(path: &Path, req: &Request) -> Result<Response, ControlError> {
    let stream = UnixStream::connect(path)?;
    let mut writer = stream.try_clone()?;
    write_frame(&mut writer, req)?;
    let mut reader = BufReader::new(stream);
    read_frame(&mut reader)
}

// ---------------------------------------------------------------------------
// Daemon-side connection handling
// ---------------------------------------------------------------------------

/// Serve one accepted connection: enforce the peer-uid check, read one
/// request, and hand it to `handler` to produce a response.
///
/// The daemon's accept loop (a later wave) calls this per connection. The
/// peer-uid check runs **before** any request is read, so a wrong-uid peer
/// never reaches application logic.
pub fn serve_connection<F>(stream: UnixStream, handler: F) -> Result<(), ControlError>
where
    F: FnOnce(Request) -> Response,
{
    assert_same_uid(&stream)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let req: Request = read_frame(&mut reader)?;
    let resp = handler(req);
    write_frame(&mut writer, &resp)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    fn temp_sock() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Persist the dir for the duration of the test (cleaned by the OS at
        // exit); we need a stable path that outlives the `TempDir` guard.
        dir.keep().join("sock")
    }

    // ---- protocol serde round-trips ----

    #[test]
    fn request_open_round_trip() {
        let req = Request::Open {
            path: "/proj/README.md".into(),
            root: "/proj".into(),
            want_edit: false,
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"type\":\"Open\""));
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn request_ping_round_trip() {
        let req = Request::Ping;
        let line = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn response_round_trips() {
        for resp in [
            Response::Opened {
                url: "file:///x.html".into(),
                nonce: "abc".into(),
            },
            Response::Pong {
                version: "0.1.0".into(),
                edit_mode: true,
            },
            Response::Error {
                message: "boom".into(),
            },
        ] {
            let line = serde_json::to_string(&resp).unwrap();
            let back: Response = serde_json::from_str(&line).unwrap();
            assert_eq!(resp, back);
        }
    }

    #[test]
    fn pong_default_fields_deserialize_from_bare_pong() {
        // Old daemons emit {"type":"Pong"} with no extra fields.
        // New clients must tolerate this (treat missing version as "").
        let bare = r#"{"type":"Pong"}"#;
        let resp: Response = serde_json::from_str(bare).unwrap();
        assert_eq!(
            resp,
            Response::Pong {
                version: String::new(),
                edit_mode: false,
            }
        );
    }

    #[test]
    fn shutdown_request_round_trips() {
        let req = Request::Shutdown;
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"type\":\"Shutdown\""));
        let back: Request = serde_json::from_str(&line).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // Forward-compat: a newer peer adds a field; we must still decode.
        let req: Request = serde_json::from_str(r#"{"type":"Ping","future_field":42}"#).unwrap();
        assert_eq!(req, Request::Ping);

        let resp: Response = serde_json::from_str(
            r#"{"type":"Opened","url":"file:///x","nonce":"n","extra":{"k":1}}"#,
        )
        .unwrap();
        assert_eq!(
            resp,
            Response::Opened {
                url: "file:///x".into(),
                nonce: "n".into()
            }
        );
    }

    // ---- frame I/O ----

    #[test]
    fn frame_round_trip_over_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Request::Ping).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'));
        let mut reader = BufReader::new(&buf[..]);
        let req: Request = read_frame(&mut reader).unwrap();
        assert_eq!(req, Request::Ping);
    }

    #[test]
    fn read_frame_eof_is_error() {
        let mut reader = BufReader::new(&b""[..]);
        let err = read_frame::<Request, _>(&mut reader).unwrap_err();
        assert!(matches!(err, ControlError::UnexpectedEof));
    }

    // ---- single-instance election ----

    /// Spawn a minimal daemon accept loop on `listener` that answers Ping with
    /// Pong and Open with a fixed Opened, until `stop` fires.
    fn spawn_daemon(listener: UnixListener) -> (thread::JoinHandle<()>, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            for conn in listener.incoming() {
                if rx.try_recv().is_ok() {
                    break;
                }
                match conn {
                    Ok(stream) => {
                        let _ = serve_connection(stream, |req| match req {
                            Request::Ping | Request::Shutdown => Response::Pong {
                                version: build_version().to_string(),
                                edit_mode: false,
                            },
                            Request::Open { .. } => Response::Opened {
                                url: "file:///served.html".into(),
                                nonce: "n0".into(),
                            },
                        });
                    }
                    Err(_) => break,
                }
            }
        });
        (handle, tx)
    }

    #[test]
    fn bind_then_client_detects_live_daemon() {
        let path = temp_sock();

        // First election binds → Daemon.
        let listener = match elect(&path).unwrap() {
            Election::Daemon(l) => l,
            Election::Client(_) => panic!("first election should be daemon"),
        };
        let (handle, stop) = spawn_daemon(listener);

        // Second election against the same path detects the live daemon → Client.
        let client = match elect(&path).unwrap() {
            Election::Client(c) => c,
            Election::Daemon(_) => panic!("second election should be a client"),
        };

        // The client can round-trip a real request.
        let resp = client
            .send_request_blocking(&Request::Open {
                path: "/p/a.md".into(),
                root: "/p".into(),
                want_edit: false,
            })
            .unwrap();
        assert_eq!(
            resp,
            Response::Opened {
                url: "file:///served.html".into(),
                nonce: "n0".into()
            }
        );

        let pong = client.send_request_blocking(&Request::Ping).unwrap();
        assert!(matches!(pong, Response::Pong { .. }));

        let _ = stop.send(());
        // Nudge the accept loop so it observes the stop signal, then join.
        let _ = UnixStream::connect(&path);
        let _ = handle.join();
    }

    #[test]
    fn stale_socket_is_unlinked_and_rebound() {
        let path = temp_sock();

        // Create a stale socket file: bind, then drop the listener. On Unix the
        // path persists but nothing listens → connect yields ECONNREFUSED.
        {
            let l = UnixListener::bind(&path).unwrap();
            drop(l);
        }
        if !path.exists() {
            let l = UnixListener::bind(&path).unwrap();
            drop(l);
        }
        assert!(path.exists(), "stale socket file should exist");

        // Election must detect the stale file, unlink it, and rebind as daemon.
        match elect(&path).unwrap() {
            Election::Daemon(l) => {
                let mode = std::os::unix::fs::PermissionsExt::mode(
                    &std::fs::metadata(&path).unwrap().permissions(),
                );
                assert_eq!(mode & 0o777, SOCK_MODE);
                drop(l);
            }
            Election::Client(_) => panic!("stale socket should be reclaimed as daemon"),
        }
    }

    // ---- SO_PEERCRED ----

    #[test]
    fn peer_uid_check_accepts_same_uid() {
        // A same-uid connection (us → us) must pass `assert_same_uid`. This
        // covers the accept path; the cross-uid *reject* path cannot be unit
        // tested in-process (we cannot become another uid without privileges)
        // and needs a privileged integration harness — see module note.
        let path = temp_sock();
        let listener = UnixListener::bind(&path).unwrap();

        let (tx, rx) = mpsc::channel::<Result<u32, String>>();
        let h = thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let same = assert_same_uid(&stream)
                    .map(|()| 0u32)
                    .map_err(|e| e.to_string());
                let pu = peer_uid(&stream).map_err(|e| e.to_string());
                let _ = tx.send(same.and(pu));
            }
        });

        let _client = UnixStream::connect(&path).unwrap();
        let got = rx.recv().unwrap();
        h.join().unwrap();
        assert_eq!(got, Ok(our_uid()));
    }

    #[test]
    fn assert_same_uid_passes_for_local_pair() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_same_uid(&a).unwrap();
    }

    // ---- perms / paths ----

    #[test]
    fn control_dir_is_0700() {
        // Point the resolver at a temp XDG_RUNTIME_DIR for hermeticity.
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: test-only env mutation; this is the only test that touches
        // XDG_RUNTIME_DIR, so no concurrent reader races it within the module.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", tmp.path());
        }
        let dir = control_dir().unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&dir).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, DIR_MODE);
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }
}
