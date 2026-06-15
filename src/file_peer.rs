//! `FilePeer` — bridges an on-disk `.md` file and a [`DocSession`].
//!
//! Watches the file (via `notify`); on an external save, diffs the new contents
//! against the session text ([`crate::diff`]) and applies the minimal edits, so
//! the file behaves as just another peer on the document. Write-back (session →
//! file) is guarded against re-ingesting our own writes.
//!
//! ## Feedback-loop guard
//! Our own [`FilePeer::write_to_disk`] also fires a filesystem event. Rather
//! than carry an "ignore the next event" flag (racy: coalesced/duplicate fs
//! events make the count unreliable), the guard is *stateless*: ingestion
//! ([`FilePeer::sync_from_disk`]) compares the file contents against
//! `session.text()` and does nothing when they are equal. The event from our
//! own write therefore diffs to nothing and is a no-op. This is robust to
//! duplicate, dropped, or reordered events.
//!
//! ## Atomic-rename saves
//! Editors like neovim save by writing a temp file and `rename(2)`-ing it over
//! the target, which changes the file's inode. Watching the inode directly
//! would stop delivering events after the first such save. Instead we watch the
//! **parent directory** ([`RecursiveMode::NonRecursive`]) and filter events
//! down to the canonical target path, so a fresh inode at the same path is still
//! seen.
//!
//! ## Threading (Phase 2 note)
//! The session is held as a plain generic `S: DocSession` and is *not* required
//! to be `Send` — it stays single-threaded. Only a lightweight `()` wake-up
//! signal crosses the `notify` watcher thread, over an `mpsc` channel this
//! `FilePeer` owns. The deterministic core ([`sync_from_disk`]) is independent
//! of the watcher and of timing, so the future daemon can ignore [`run`] and
//! instead wrap the session in `Arc<Mutex<…>>` and drive `sync_from_disk` from
//! its own event loop; that refactor touches only how the session is stored, not
//! the ingest logic.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::diff::diff;
use crate::doc::DocSession;

/// Bridges a single on-disk file with a [`DocSession`]: ingests external saves
/// into the session and writes the session's text back to the file.
///
/// Generic over the session type so it works with the real
/// [`crate::session::YrsSession`] and with test doubles alike.
pub struct FilePeer<S: DocSession> {
    /// The live document this file is a peer of.
    session: S,
    /// Canonical path of the watched file (events are filtered to this path).
    target: PathBuf,
    /// Canonical parent directory — what we actually hand to `notify`, so that
    /// atomic-rename saves (new inode, same path) keep being delivered.
    parent: PathBuf,
    /// The watcher handle. Kept alive for as long as the peer lives; dropping it
    /// stops the watch. `Some` once [`FilePeer::watch`] has been called.
    watcher: Option<RecommendedWatcher>,
    /// Wake-up signals from the watcher thread. Each `()` means "something
    /// touched the directory; consider re-syncing". The payload is intentionally
    /// trivial — all truth comes from re-reading the file in `sync_from_disk`.
    rx: Receiver<()>,
    /// Sender handed to the watcher closure. Stored so [`FilePeer::watch`] can
    /// clone it into the closure; held here to keep the channel open.
    tx: mpsc::Sender<()>,
}

impl<S: DocSession> FilePeer<S> {
    /// Create a peer for `path`, taking ownership of `session`.
    ///
    /// The file need not exist yet (a watch on the parent directory will pick up
    /// its later creation); only the parent directory must exist so the path can
    /// be canonicalized. No watcher is started until [`FilePeer::watch`] is
    /// called — tests can drive [`FilePeer::sync_from_disk`] /
    /// [`FilePeer::write_to_disk`] without any watcher.
    pub fn new(path: impl AsRef<Path>, session: S) -> std::io::Result<Self> {
        let path = path.as_ref();

        // Canonicalize the *parent* directory (it must exist), then re-attach the
        // file name. We avoid canonicalizing the file itself because it may not
        // exist yet, and because during an atomic-rename save it can momentarily
        // be absent. The file name component is preserved verbatim.
        let parent_raw = path.parent().filter(|p| !p.as_os_str().is_empty());
        let parent = match parent_raw {
            Some(p) => p.canonicalize()?,
            // No parent (e.g. a bare file name): treat the cwd as the directory.
            None => std::env::current_dir()?,
        };
        let file_name = path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no file name component",
            )
        })?;
        let target = parent.join(file_name);

        let (tx, rx) = mpsc::channel();

        Ok(Self {
            session,
            target,
            parent,
            watcher: None,
            rx,
            tx,
        })
    }

    /// The canonical path of the watched file.
    pub fn path(&self) -> &Path {
        &self.target
    }

    /// Borrow the underlying session (read-only access to e.g. `text()`).
    pub fn session(&self) -> &S {
        &self.session
    }

    /// Mutably borrow the underlying session.
    pub fn session_mut(&mut self) -> &mut S {
        &mut self.session
    }

    /// Read the file and merge any external change into the session.
    ///
    /// Computes `diff(session.text(), file_contents)` and applies it via
    /// `session.apply`, so an external edit lands as minimal CRDT ops rather than
    /// a whole-document replacement. Returns `Ok(true)` if the document text
    /// changed, `Ok(false)` if it was already in sync (the idempotent /
    /// feedback-loop guard).
    ///
    /// This is the deterministic core: it depends only on the file and the
    /// session, never on watcher timing, and is what tests call directly.
    ///
    /// Transient states are tolerated: if the file is briefly missing (mid
    /// atomic-rename), this returns `Ok(false)` rather than erroring — a real
    /// change will arrive with the next event once the rename completes. Other
    /// I/O errors are propagated.
    pub fn sync_from_disk(&mut self) -> std::io::Result<bool> {
        let contents = match std::fs::read_to_string(&self.target) {
            Ok(c) => c,
            // File momentarily gone during a rename, or not yet created: not an
            // error to us. The next event re-triggers once it settles.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };

        let current = self.session.text();
        if current == contents {
            // Already in sync — this is the guard that swallows the fs event
            // caused by our own write_to_disk, and makes ingestion idempotent.
            return Ok(false);
        }

        let edits = diff(&current, &contents);
        if edits.is_empty() {
            // Defensive: equal text already returned above, so a non-equal
            // string should yield edits. Treat an empty diff as "no change".
            return Ok(false);
        }
        self.session.apply(&edits);
        Ok(true)
    }

    /// Write the session's current text to the file.
    ///
    /// After this, the file equals `session.text()`, so the filesystem event our
    /// own write generates will diff to nothing in [`FilePeer::sync_from_disk`]
    /// (the feedback-loop guard) — no flag bookkeeping required.
    pub fn write_to_disk(&self) -> std::io::Result<()> {
        std::fs::write(&self.target, self.session.text())
    }

    /// Start watching the file's parent directory and wire events to a wake-up
    /// channel.
    ///
    /// The watcher closure runs on `notify`'s own thread; it must stay cheap and
    /// must not touch the (non-`Send`) session. It only filters events to our
    /// target path and sends a `()` over the channel. The owning thread drains
    /// the channel (via [`FilePeer::try_drain`] or [`FilePeer::run`]) and calls
    /// [`FilePeer::sync_from_disk`].
    ///
    /// Performs an initial [`FilePeer::sync_from_disk`] so any change that
    /// happened before the watch was established is not missed.
    pub fn watch(&mut self) -> notify::Result<()> {
        let tx = self.tx.clone();
        let target = self.target.clone();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            // Ignore watcher errors here; the owning side stays correct by
            // re-reading the file. A failed/garbled event simply isn't relayed.
            if let Ok(event) = res {
                // Filter the directory's events down to our target path. During
                // an atomic-rename save the new file arrives at this same path
                // (fresh inode), so a path match — not an inode match — is what
                // keeps us in sync across editor saves.
                if event.paths.iter().any(|p| p == &target) {
                    // A disconnected receiver just means nobody is listening;
                    // dropping the signal is fine (the truth is on disk).
                    let _ = tx.send(());
                }
            }
        })?;

        // NonRecursive: we only care about this one directory's direct entries.
        watcher.watch(&self.parent, RecursiveMode::NonRecursive)?;
        self.watcher = Some(watcher);

        // Catch up on anything that changed before the watch was live. Map the
        // io::Error into notify's error type so `watch` has one error channel.
        self.sync_from_disk().map_err(notify::Error::io)?;
        Ok(())
    }

    /// Drain all pending wake-up signals and, if any arrived, run a single
    /// [`FilePeer::sync_from_disk`]. Returns whether the document changed.
    ///
    /// Non-blocking. Coalesces a burst of events into one sync (the file is read
    /// once for its final state), which is exactly what we want for a save that
    /// emits several events. Safe to call when no watcher is running (it just
    /// finds an empty channel and returns `Ok(false)`).
    pub fn try_drain(&mut self) -> std::io::Result<bool> {
        let mut signaled = false;
        while self.rx.try_recv().is_ok() {
            signaled = true;
        }
        if signaled {
            self.sync_from_disk()
        } else {
            Ok(false)
        }
    }

    /// Blocking ingest loop, for the future daemon to spawn on its own thread.
    ///
    /// Waits for a wake-up signal, then drains and syncs. Returns `Ok(())` when
    /// the channel closes (all senders dropped — i.e. the `FilePeer`/watcher was
    /// torn down). Tests should prefer [`FilePeer::sync_from_disk`] /
    /// [`FilePeer::try_drain`] to stay off the timing path; this method exists
    /// for the long-running daemon.
    pub fn run(&mut self) -> std::io::Result<()> {
        // `recv` blocks until a signal or channel close. Once woken, drain any
        // coalesced extras and sync the file's final state in one read.
        while self.rx.recv().is_ok() {
            while self.rx.try_recv().is_ok() {}
            self.sync_from_disk()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::YrsSession;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// A unique temp directory for one test, cleaned up on drop. Avoids pulling
    /// in an external tempfile dependency.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            // Process id + a monotonically increasing counter keeps names unique
            // across concurrently-running tests in the same process.
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("md-preview-fp-{}-{}-{}", tag, std::process::id(), n));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn file(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// External edit: writing the file and syncing pulls the change into the
    /// session; a second sync is a no-op (idempotent).
    #[test]
    fn sync_from_disk_ingests_external_edit() {
        let dir = TempDir::new("ext");
        let file = dir.file("doc.md");
        std::fs::write(&file, "hello").unwrap();

        let mut peer = FilePeer::new(&file, YrsSession::from_text("hello")).unwrap();
        // Already in sync (session seeded to match the file): no change.
        assert!(!peer.sync_from_disk().unwrap());

        // Simulate an external editor save.
        std::fs::write(&file, "hello world").unwrap();
        assert!(peer.sync_from_disk().unwrap(), "external change should apply");
        assert_eq!(peer.session().text(), "hello world");
        assert_eq!(peer.session().text(), std::fs::read_to_string(&file).unwrap());

        // Idempotent: nothing new on disk.
        assert!(!peer.sync_from_disk().unwrap());
    }

    /// Write-back: the session's text reaches the file, and the resulting fs
    /// state does not feed back as an external change.
    #[test]
    fn write_to_disk_then_sync_is_no_feedback_loop() {
        let dir = TempDir::new("wb");
        let file = dir.file("doc.md");
        std::fs::write(&file, "start").unwrap();

        let mut peer = FilePeer::new(&file, YrsSession::from_text("start")).unwrap();

        // Mutate the session (as a browser peer would), then write it out.
        peer.session_mut().apply(&[crate::doc::TextEdit::insert(5, "!")]);
        assert_eq!(peer.session().text(), "start!");
        peer.write_to_disk().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "start!");

        // The file now equals session.text(): ingesting our own write is a no-op.
        assert!(
            !peer.sync_from_disk().unwrap(),
            "our own write must not feed back as an external change"
        );
        assert_eq!(peer.session().text(), "start!");
    }

    /// Non-ASCII content round-trips both directions, exercising the UTF-16
    /// position contract end to end.
    #[test]
    fn non_ascii_round_trips_both_directions() {
        let dir = TempDir::new("utf");
        let file = dir.file("doc.md");
        std::fs::write(&file, "café 🦀").unwrap();

        let mut peer = FilePeer::new(&file, YrsSession::from_text("café 🦀")).unwrap();
        assert!(!peer.sync_from_disk().unwrap());

        // External edit inserting more non-ASCII (CJK + another astral emoji).
        std::fs::write(&file, "café 🦀 日本語 🎉").unwrap();
        assert!(peer.sync_from_disk().unwrap());
        assert_eq!(peer.session().text(), "café 🦀 日本語 🎉");

        // Session-side edit, written back, must reproduce exactly on disk.
        let new_text = "Ωmega café 🦀 日本語 🎉";
        let edits = diff(&peer.session().text(), new_text);
        peer.session_mut().apply(&edits);
        peer.write_to_disk().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), new_text);
        assert!(!peer.sync_from_disk().unwrap());
    }

    /// A missing file (mid atomic-rename, or not yet created) is tolerated.
    #[test]
    fn missing_file_is_not_an_error() {
        let dir = TempDir::new("missing");
        let file = dir.file("nope.md");
        // File never created; parent dir exists so `new` can canonicalize it.
        let mut peer = FilePeer::new(&file, YrsSession::from_text("seed")).unwrap();
        assert!(
            !peer.sync_from_disk().unwrap(),
            "absent file: no change, no panic"
        );
    }

    /// Watcher-based integration test (INCLUDED, not ignored).
    ///
    /// We poll `try_drain` up to a generous timeout rather than sleeping a fixed
    /// amount and checking once, so it converges as soon as the event arrives and
    /// tolerates a slow/coalescing backend. If your environment makes inotify
    /// flaky, this is the test to `#[ignore]`; it passes locally here, so it is
    /// left enabled.
    #[test]
    fn watcher_converges_within_timeout() {
        let dir = TempDir::new("watch");
        let file = dir.file("doc.md");
        std::fs::write(&file, "v1").unwrap();

        let mut peer = FilePeer::new(&file, YrsSession::from_text("v1")).unwrap();
        peer.watch().expect("watch should start");

        // External save after the watch is live.
        std::fs::write(&file, "v1 + external").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut converged = false;
        while Instant::now() < deadline {
            // Drain any wake-ups and sync; stop as soon as the text matches.
            let _ = peer.try_drain();
            if peer.session().text() == "v1 + external" {
                converged = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(converged, "watcher should converge within 2s");
    }
}
