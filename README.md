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

This opens your browser at the live preview and keeps serving until you stop it with Ctrl-C.

### Flags and environment

```bash
md-preview <file.md> [--port N] [--no-open]
```

- `--port N` — port to serve on (default `7878`; also settable via `MD_PREVIEW_PORT`). The port is fixed, not ephemeral.
- `--no-open` — start the server but don't open a browser (also `MD_PREVIEW_NO_OPEN=1`). The URL is printed to stdout.
- `BROWSER` — if set, used as the opener command instead of the system default (e.g. `BROWSER=/bin/true` opens nothing). Useful in CI/headless runs.

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

1. Takes the Markdown file from the argument; its parent directory becomes the confinement root, so any `.md` (and local assets) under that directory can be previewed
2. Starts a persistent HTTP server (`warp` + `tokio`) on a fixed port (`7878` by default), and opens your browser at `/view?path=<file>`
3. Converts Markdown to HTML using `pulldown-cmark` with GFM extensions (tables, strikethrough, task lists, footnotes) plus math (`$...$`, `$$...$$`)
4. Highlights code blocks server-side with `syntect`, emitting class-based markup styled by bundled GitHub Light/Dark themes
5. Wraps the output in a `github-markdown-css` template that auto-switches light/dark via `prefers-color-scheme`; math is rendered in the browser by KaTeX
6. Watches the file with `notify`; on every change — an external editor *or* the built-in editor's save — it re-renders and pushes the new HTML to the page over a WebSocket, so the preview updates live without a reload
7. Keeps running until you stop it (Ctrl-C)

> Markdown styling (`github-markdown-css`) and math (KaTeX) load from a CDN, so rendering those needs an internet connection. Syntax-highlight themes are bundled into the binary and work offline.

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
| `futures-util` | Stream helpers for the WebSocket forwarding |
| `serde` | Query-string deserialization for routes |
| `webbrowser` | Cross-platform browser launcher |

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Copyright 2026 Yutaro Yoshii.
