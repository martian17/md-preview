# md-preview

A dead simple Markdown preview command-line tool. Renders a `.md` file to GitHub-flavored HTML and serves a **live preview** in your browser — the page re-renders instantly whenever the file changes on disk (or in the built-in editor). The server keeps running until you stop it (Ctrl-C).

Features:

- **GitHub-flavored Markdown** — tables, strikethrough, task lists, footnotes
- **TeX math** — inline `$...$` and display `$$...$$`, rendered with KaTeX
- **Syntax highlighting** — class-based, with bundled GitHub Light/Dark themes
- **Copy-code buttons** on every code block
- **Auto light/dark** — page, code, and math follow your system `prefers-color-scheme`
- **Live reload** — saving the file (from any editor) re-renders the preview over a WebSocket, no manual refresh
- **Built-in editor** — switch between *preview*, *split* (editor + preview), and *editor* views from an in-page toolbar; edits save straight back to the file
- **Cross-file links & local images** — relative `.md` links open in the same live preview, and local images/assets render without a CDN

## Usage

```bash
md-preview yourfile.md
# or via the short alias:
md yourfile.md
```

This contacts the always-on daemon (spawning it if needed), registers the file's project root, and opens the browser at the preview URL.

### Flags and environment

```bash
md-preview <file.md> [--no-open]
md-preview --warm-cache
md-preview --daemon
```

- `--no-open` — register the file and print the URL without opening a browser (also `MD_PREVIEW_NO_OPEN=1`).
- `--warm-cache` — pre-fetch and verify all pinned bundle assets into the local cache, then exit. Run once after install if you want offline-first operation from the first preview.
- `--daemon` — start the daemon in server mode (no document); used by the systemd user unit. If a daemon is already running this exits cleanly.
- `BROWSER` — if set, used as the opener command instead of the system default. Setting `BROWSER=true` (or `BROWSER=/bin/true`) intentionally suppresses opening any browser — the daemon is still spawned and the preview is served, nothing is launched. Useful in CI/headless runs.

## How to run

The normal flow is a single command — point `md-preview` at a file and it does the rest:

```bash
md-preview path/to/file.md
```

Each `md-preview <file>` is a **thin client**. If a daemon is already running it connects to it and opens the preview. If none is running, it **auto-spawns a detached `md-preview --daemon` background process** (new session via `setsid`, stdio redirected to `~/.cache/md-preview/daemon.log`), waits for it to start listening, then opens the preview — so the command **returns immediately and the daemon keeps running in the background** (it never ties up your terminal). Later `md-preview <file>` invocations reuse that same daemon (no second process, no "Address already in use"). If two run at once, only one daemon wins the election; the other's spawned `--daemon` exits as a no-op and both clients connect. The daemon keeps running until you stop it (kill the background `md-preview --daemon`, or the systemd user unit — installed separately by the operator).

**Warm the bundle cache (optional, recommended once after install):**

```bash
md-preview --warm-cache
```

Pre-fetches and SHA-384-verifies the pinned Mermaid, KaTeX, KaTeX fonts, and `github-markdown-css` assets into `~/.cache/md-preview/` (respects `$XDG_CACHE_HOME`). After this, previews render fully offline. Without it the daemon fetches+verifies each asset lazily on first use.

**Multiple project roots, one daemon:**

```bash
md-preview ~/projectA/docs/intro.md   # registers projectA's root, opens it
md-preview ~/projectB/README.md       # same daemon now also serves projectB
```

The daemon holds a **multi-root registry**. Each `md-preview <file>` registers the file's project root (detected by walking ancestors for `.git`/`Cargo.toml`/etc., stopping before `$HOME`); any file under any registered root is then previewable. Roots persist across restarts with a 30-day sliding TTL.

**Port model — loopback only, two ports:**

- The daemon binds **`127.0.0.1` only** (never a routable address). It is never reachable off-host.
- **Shell origin** — the trusted SPA (`/view`, `/content`, `/ws`, `/claim`, …): port **`7878`** by default, falling back to an OS-assigned ephemeral port if `7878` is busy.
- **Secondary static origin** — bundle assets + short-TTL `/cap/<token>` capability URLs for document images, served cross-origin to the sandboxed render iframe: the shell port **+ 1** (`7879` by default), with the same ephemeral fallback.

A strict `Host` allowlist (`127.0.0.1:<port>` / `localhost:<port>` only) guards both origins against DNS-rebinding; the CLI↔daemon control channel is a `0600` Unix socket under `$XDG_RUNTIME_DIR/md-preview/` (peer-uid checked), so no token ever touches argv or a URL.

**Headless / CI:** add `--no-open` (or set `MD_PREVIEW_NO_OPEN=1`, or `BROWSER=/bin/true`) to register the file and print the preview URL without launching a browser.

## Installation

### Prerequisites

- **Rust toolchain** — install via [rustup](https://rustup.rs):
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```
- **C linker** (required by Rust to link binaries):
  ```bash
  # Debian / Ubuntu / WSL
  sudo apt-get install -y gcc build-essential
  ```

### Build and install

```bash
git clone https://github.com/martian17/md-preview.git
cd md-preview
cargo install --path .
```

This places `md-preview` in `~/.cargo/bin/`. Add it to your PATH if it isn't already:

```bash
echo '. "$HOME/.cargo/env"' >> ~/.bashrc
source ~/.bashrc
```

### Optional short alias

Create an `md` symlink so you can type `md file.md` instead:

```bash
ln -sf ~/.cargo/bin/md-preview ~/.cargo/bin/md
```

## WSL (Windows Subsystem for Linux)

Works out of the box on WSL2 — no extra configuration needed.

The `webbrowser` crate detects WSL by reading `/proc/version` and launches your default Windows browser via `explorer.exe`. WSL2's built-in localhost proxy forwards `http://127.0.0.1:<port>` from Windows into the WSL network, so the browser connects directly to the Rust server running inside WSL.

**Tested on:** Ubuntu 24.04 (aarch64) on WSL2 running on Snapdragon X Windows.

If the browser fails to open for any reason, the URL is printed to stdout so you can open it manually.

## How it works

1. `md <file>` is a thin client: it talks to the always-on daemon over a Unix socket (`$XDG_RUNTIME_DIR/md-preview/`), registers the file's project root, and opens the browser at the preview URL. If no daemon is running it spawns one detached first.
2. The daemon (`warp` + `tokio`) runs as a systemd user service (`Restart=always`) — one long-lived process per login, not one per file. It holds a **multi-root registry**: on `md <file>`, the project root (detected by walking ancestors for `.git`/`Cargo.toml`/etc., stopping before `$HOME`) is registered; all files under it can then be previewed. Roots persist across reboots with a 30-day sliding TTL.
3. All filesystem access goes through a single **confinement funnel** (`confine.rs`): canonicalize → check against the union of registered roots → deny sensitive paths (`~/.ssh`, `$HOME`, `/etc`, …) → open with `O_NOFOLLOW` and hold the fd for the read. No TOCTOU re-resolution.
4. The preview page is a **trusted SPA shell** (intact localhost origin, session-cookie-gated) that loads the rendered content into a **sandboxed null-origin `<iframe>`** (`sandbox="allow-scripts"`, `connect-src 'none'`). Document images/assets are served from a second loopback port as short-TTL capability URLs, cross-origin to the iframe (canvas-tainted, unexfiltratable).
5. Converts Markdown to HTML using `pulldown-cmark` with GFM extensions (tables, strikethrough, task lists, footnotes) plus math (`$...$`, `$$...$$`).
6. Highlights code blocks server-side with `syntect`, emitting class-based markup styled by bundled GitHub Light/Dark themes.
7. Watches the file with `notify`; on every change — an external editor *or* the built-in editor's save — it re-renders and pushes the new HTML to the page over a WebSocket, so the preview updates live without a reload.

> Built with the default `daemon` feature, the daemon fetches `github-markdown-css`, KaTeX, and Mermaid from pinned CDN URLs on first use and caches them locally (verifying SHA-384); subsequent starts and offline use serve from that cache (`~/.cache/md-preview/`). Built **without** the daemon feature (`--no-default-features`), the standalone HTML output references the CDN URLs directly and requires an internet connection to render styles and math. Syntax-highlight themes are always bundled into the binary and work offline.

Built **without** the default `daemon` feature (`cargo build --no-default-features`), the binary drops every web dependency and instead renders the file to stdout once — a zero-dependency fallback that keeps the renderer reusable as a library.

## Dependencies

Always built in:

| Crate | Purpose |
|---|---|
| `pulldown-cmark` | Fast GFM-compatible Markdown parser (with math extension) |
| `syntect` | Syntax highlighting (class-based HTML + theme CSS) |
| `yrs` | CRDT document model (Yjs-compatible) backing the live document |
| `similar` | Text diffing — turns an external save into minimal edits |
| `notify` | Filesystem watching for live reload |

Daemon only (the default `daemon` feature; absent under `--no-default-features`):

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime |
| `warp` | HTTP + WebSocket server |
| `futures-util` | Stream helpers for WebSocket forwarding |
| `serde` / `serde_json` | Query-string deserialization + control-plane protocol |
| `webbrowser` | Cross-platform browser launcher |
| `getrandom` | Entropy for nonce/session/capability tokens |
| `libc` | `SO_PEERCRED`, `O_NOFOLLOW`, `fstat` (confinement funnel) |
| `ureq` | Bundle cache HTTP fetcher |
| `sha2` | SHA-384 SRI verification for the bundle cache |
| `subtle` | Constant-time comparison for secrets |
| `base64` | URL-safe + standard encoding for tokens |

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Copyright 2026 Yutaro Yoshii.
