//! Cross-module integration tests for the real-time pipeline:
//! file on disk → `FilePeer` ingest → `diff` → `YrsSession` (CRDT) → renderer.
//!
//! The per-module unit tests live next to each module; these exercise the
//! modules together through their public APIs, the way the daemon will.

use md_preview::doc::DocSession;
use md_preview::file_peer::FilePeer;
use md_preview::render_markdown;
use md_preview::session::YrsSession;
use std::fs;
use std::path::PathBuf;

/// Create a unique temp dir containing `doc.md` with `contents`, returning the
/// file path. Caller removes the parent dir when done.
fn temp_doc(tag: &str, contents: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("md-preview-it-{}-{}", std::process::id(), tag));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("doc.md");
    fs::write(&path, contents).unwrap();
    path
}

#[test]
fn external_file_edit_flows_through_to_rendered_html() {
    let path = temp_doc("flow", "# Title\n\nhello\n");
    let session = YrsSession::from_text(&fs::read_to_string(&path).unwrap());
    let mut peer = FilePeer::new(&path, session).unwrap();

    // Initial render reflects the file.
    let html = render_markdown(&peer.session().text());
    assert!(html.contains("Title"));
    assert!(html.contains("hello"));

    // An external editor saves a change: it should ingest as minimal CRDT ops
    // and flow all the way through to the rendered HTML.
    fs::write(&path, "# Title\n\nhello **world**\n").unwrap();
    assert!(peer.sync_from_disk().unwrap(), "external edit should change the doc");

    let html = render_markdown(&peer.session().text());
    assert!(
        html.contains("<strong>world</strong>"),
        "rendered HTML should reflect the external edit, got: {html}"
    );

    // Re-syncing with no on-disk change is a no-op (idempotent guard).
    assert!(!peer.sync_from_disk().unwrap());

    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn session_write_back_matches_file_and_does_not_loop() {
    let path = temp_doc("writeback", "alpha\n");
    let session = YrsSession::from_text("alpha\n");
    let mut peer = FilePeer::new(&path, session).unwrap();

    // Edit through the session (as a browser peer would), then write back.
    let current = peer.session().text();
    let edits = md_preview::diff::diff(&current, "alpha beta\n");
    peer.session_mut().apply(&edits);
    peer.write_to_disk().unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), "alpha beta\n");
    // The write-back's own fs event must not re-ingest (feedback-loop guard).
    assert!(!peer.sync_from_disk().unwrap());

    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn non_ascii_external_edit_round_trips() {
    let path = temp_doc("unicode", "café 🦀\n");
    let session = YrsSession::from_text("café 🦀\n");
    let mut peer = FilePeer::new(&path, session).unwrap();

    fs::write(&path, "café 🦀 日本語\n").unwrap();
    assert!(peer.sync_from_disk().unwrap());
    assert_eq!(peer.session().text(), "café 🦀 日本語\n");

    let _ = fs::remove_dir_all(path.parent().unwrap());
}
