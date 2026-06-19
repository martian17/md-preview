//! md-preview: the binary/daemon crate.
//!
//! The pure Markdown → HTML rendering surface now lives in the standalone
//! [`md_render`] crate (`crates/md-render`); it is re-exported here so existing
//! `md_preview::render_page`-style call sites (the `md` binary, the daemon,
//! integration tests) keep compiling unchanged. This crate adds the document
//! model (CRDT) and the feature-gated live-preview daemon on top.

// Re-export the pure renderer so `md_preview::render_markdown`,
// `crate::render_page_with`, etc. resolve exactly as before the crate split.
// The renderer carries ZERO web/async/CRDT deps, so `--no-default-features`
// builds still drop the entire daemon stack.
pub use md_render::{
    render_editor_page, render_markdown, render_mermaid_block, render_page, render_page_with,
};

// Real-time collaborative editing (see ADR-0001 "CRDT over OT" and ADR-0003).
// The renderer (in `md-render`) stays pure and reusable; these modules add the
// document model behind a stable contract (`doc::DocSession`).
pub mod doc;
pub mod diff;
pub mod file_peer;
pub mod session;

// The persistent daemon (warp + tokio live preview). Feature-gated so the lib
// stays usable on its own — `--no-default-features` builds the pure renderer and
// document core with ZERO web/server dependencies.
#[cfg(feature = "daemon")]
pub mod server;

// Always-on, multi-root, isolated preview daemon modules (Track-3 rebuild,
// shipped `eb02537`–`ece1ef0`). See HANDOFF.md §3–§4 for the architectural
// decisions; ADR-0005 (registry), ADR-0006 (auth/trust), ADR-0007 (render-
// isolation). All daemon-gated.
#[cfg(feature = "daemon")]
pub mod roots;
#[cfg(feature = "daemon")]
pub mod auth;
#[cfg(feature = "daemon")]
pub mod control;
#[cfg(feature = "daemon")]
pub mod shell;
#[cfg(feature = "daemon")]
pub mod validate;
#[cfg(feature = "daemon")]
pub mod bundle;
#[cfg(feature = "daemon")]
pub mod asset_origin;
#[cfg(feature = "daemon")]
pub mod confine;
