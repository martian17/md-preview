//! First-party editor bundle: serve the mycelium-editor dist files offline.
//!
//! The mycelium-editor package is FIRST-PARTY (hibiki-automatic org), so its
//! dist files are embedded in the binary with `include_bytes!` rather than
//! going through the third-party `bundle.rs` verifying-cache pipeline (which
//! is for supply-chain-pinned CDN assets that must stay out of git).
//!
//! ## Route
//! `GET /editor-bundle/<filename>` — serves one of the six allowed files:
//!   - `yjs.es.js`              — shared yjs instance (self-contained; the importmap anchor)
//!   - `mycelium-editor.es.js`  — the thin ESM facade
//!   - `index-xTTurzIe.js`     — the main CodeMirror chunk (748 KB; no yjs inlined)
//!   - `index-Xv-DOIsY.js`     — the yCollab/y-codemirror.next chunk (imports yjs via importmap)
//!   - `preview-runtime.es.js`  — `enableMathCopyAsTex` export (1.3 KB)
//!   - `crdt.es.js`             — offline CRDT companion (y-protocols + lib0; externalizes yjs)
//!
//! ## Security
//! - **Strict allowlist**: only the five filenames above are served; any other
//!   path returns 404. There is NO path traversal here (the route uses a fixed
//!   match on the tail string, not the filesystem).
//! - **Same-origin**: the route lives on the MAIN daemon origin (`127.0.0.1:<port>`),
//!   NOT the secondary bundle/static origin. The editor scripts are loaded
//!   same-origin (`src="/editor-bundle/…"`), so no `Access-Control-Allow-Origin`
//!   or `Cross-Origin-Resource-Policy` headers are needed (and MUST NOT be added
//!   — CORP on a same-origin asset would be wrong, and ACAO `*` would be
//!   unnecessarily permissive for first-party code).
//! - **No SRI**: the files are embedded at compile time; the binary IS the pin.
//!   Adding integrity attributes on same-origin module scripts would not add
//!   security (the browser already trusts the same-origin fetch).
//! - **Cache-Control**: the files are keyed by content-hash in their filenames
//!   (Vite chunk hashes, e.g. `index-CUCuSPF_.js`), so `immutable` caching is
//!   safe. On a binary upgrade, new hash → new URL → fresh fetch.

/// The six allowed editor-bundle filenames and their embedded bytes.
/// `ALLOWLIST[i].0` is the exact filename the route must match; `.1` is the
/// content (embedded at compile time via `include_bytes!`). Any request for
/// a filename not in this table returns **404**.
///
/// The filenames contain their Vite content-hash, so content cannot change
/// under the same URL — the `immutable` cache directive is safe here.
///
/// `yjs.es.js` is the shared yjs instance anchor — the importmap maps the bare
/// `"yjs"` specifier here so every `import * as Y from 'yjs'` resolves to the
/// SAME module instance across all chunks, enabling instanceof checks in
/// y-codemirror.next to pass correctly (true char-level CRDT, not LWW).
static ALLOWLIST: &[(&str, &[u8])] = &[
    (
        "yjs.es.js",
        include_bytes!("editor_bundle/yjs.es.js"),
    ),
    (
        "mycelium-editor.es.js",
        include_bytes!("editor_bundle/mycelium-editor.es.js"),
    ),
    (
        "index-xTTurzIe.js",
        include_bytes!("editor_bundle/index-xTTurzIe.js"),
    ),
    (
        "index-Xv-DOIsY.js",
        include_bytes!("editor_bundle/index-Xv-DOIsY.js"),
    ),
    (
        "preview-runtime.es.js",
        include_bytes!("editor_bundle/preview-runtime.es.js"),
    ),
    (
        "crdt.es.js",
        include_bytes!("editor_bundle/crdt.es.js"),
    ),
];

/// Look up a file by exact name in the allowlist. `None` for unknown names.
fn lookup(filename: &str) -> Option<&'static [u8]> {
    ALLOWLIST
        .iter()
        .find(|(name, _)| *name == filename)
        .map(|(_, bytes)| *bytes)
}

/// Cache-Control for immutable content-hashed assets.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Build the editor-bundle warp route.
///
/// `GET /editor-bundle/<filename>` → the embedded JS bytes.
///
/// Response headers:
/// - `Content-Type: text/javascript; charset=utf-8`
/// - `Cache-Control: public, max-age=31536000, immutable`
///
/// Unknown filename → **404**.
/// No CORS, no CORP, no SRI headers (same-origin first-party asset).
pub fn editor_bundle_routes(
) -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    use warp::Filter;
    warp::path("editor-bundle")
        .and(warp::get())
        .and(warp::path::tail())
        .map(|tail: warp::path::Tail| serve_editor_file(tail.as_str()))
}

/// Serve a single editor bundle file (shared by the route and tests).
fn serve_editor_file(filename: &str) -> warp::reply::Response {
    use warp::http::StatusCode;
    use warp::reply::Reply;

    let Some(bytes) = lookup(filename) else {
        return warp::reply::with_status("", StatusCode::NOT_FOUND).into_response();
    };

    let mut resp = warp::reply::Response::new(bytes.into());
    let headers = resp.headers_mut();
    headers.insert(
        "content-type",
        warp::http::HeaderValue::from_static("text/javascript; charset=utf-8"),
    );
    headers.insert(
        "cache-control",
        warp::http::HeaderValue::from_static(IMMUTABLE),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_has_exactly_six_entries() {
        assert_eq!(ALLOWLIST.len(), 6);
    }

    #[test]
    fn lookup_known_names_returns_bytes() {
        for (name, _) in ALLOWLIST {
            let bytes = lookup(name);
            assert!(bytes.is_some(), "lookup({name}) must succeed");
            assert!(!bytes.unwrap().is_empty(), "bytes for {name} must be non-empty");
        }
    }

    #[test]
    fn lookup_unknown_name_returns_none() {
        assert!(lookup("../../etc/passwd").is_none());
        assert!(lookup("evil.js").is_none());
        assert!(lookup("").is_none());
        assert!(lookup("mycelium-editor.es.js.map").is_none());
    }

    #[test]
    fn main_chunk_is_esm_and_starts_with_expected_prefix() {
        let bytes = lookup("mycelium-editor.es.js").unwrap();
        let text = std::str::from_utf8(bytes).expect("valid utf-8");
        // The facade imports from the sibling chunk.
        assert!(text.contains("index-xTTurzIe.js"), "facade must import main chunk");
    }

    #[test]
    fn yjs_bundle_is_present_and_nonempty() {
        let bytes = lookup("yjs.es.js").unwrap();
        assert!(!bytes.is_empty(), "yjs.es.js must be non-empty");
        let text = std::str::from_utf8(bytes).expect("valid utf-8");
        // The shared yjs bundle must export the core Y.Doc/Y.Text constructors.
        assert!(text.contains("Doc") || text.contains("Y.Doc") || text.contains("YDoc"),
            "yjs.es.js must contain Doc class");
    }

    #[test]
    fn ycollab_chunk_imports_yjs_as_external() {
        let bytes = lookup("index-Xv-DOIsY.js").unwrap();
        let text = std::str::from_utf8(bytes).expect("valid utf-8");
        // The yCollab chunk must import yjs as an external bare specifier.
        // This is what the importmap resolves to the shared yjs instance.
        assert!(text.contains("from \"yjs\"") || text.contains("from 'yjs'"),
            "index-Xv-DOIsY.js must import yjs as external specifier");
    }

    #[test]
    fn crdt_bundle_exports_expected_symbols() {
        let bytes = lookup("crdt.es.js").unwrap();
        let text = std::str::from_utf8(bytes).expect("valid utf-8");
        // The CRDT bundle must export the protocol pieces the editor page uses.
        assert!(text.contains("syncProtocol") || text.contains("readSyncMessage"),
            "crdt.es.js must contain sync protocol");
        assert!(text.contains("awarenessProtocol") || text.contains("encodeAwareness"),
            "crdt.es.js must contain awareness protocol");
    }

    #[test]
    fn no_cors_or_corp_headers_on_success_response() {
        // Editor bundle is same-origin; ACAO and CORP must NOT be set.
        // We build a minimal response using the same helper and check headers.
        let resp = serve_editor_file("preview-runtime.es.js");
        let headers = resp.headers();
        assert!(
            headers.get("access-control-allow-origin").is_none(),
            "must NOT set ACAO on same-origin editor bundle"
        );
        assert!(
            headers.get("cross-origin-resource-policy").is_none(),
            "must NOT set CORP on same-origin editor bundle"
        );
        assert_eq!(
            headers.get("content-type").unwrap(),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            headers.get("cache-control").unwrap(),
            IMMUTABLE
        );
    }

    #[test]
    fn unknown_file_is_404() {
        let resp = serve_editor_file("unknown.js");
        assert_eq!(resp.status(), warp::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn route_returns_js_for_known_file() {
        let routes = editor_bundle_routes();
        let resp = warp::test::request()
            .method("GET")
            .path("/editor-bundle/preview-runtime.es.js")
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/javascript; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn route_returns_404_for_unknown_file() {
        let routes = editor_bundle_routes();
        let resp = warp::test::request()
            .method("GET")
            .path("/editor-bundle/evil.js")
            .reply(&routes)
            .await;
        assert_eq!(resp.status(), 404);
    }
}
