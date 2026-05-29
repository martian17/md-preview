# md-preview

A dead simple Markdown preview command-line tool. Converts a `.md` file to GitHub-flavored HTML, serves it on a local port, and opens it in your browser — then exits.

## Usage

```bash
md-preview yourfile.md
# or via the short alias:
md yourfile.md
```

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

1. Reads the Markdown file from the argument
2. Converts it to HTML using `pulldown-cmark` with GFM extensions (tables, strikethrough, task lists, footnotes)
3. Wraps the output in a GitHub dark-mode styled template (via `github-markdown-css`)
4. Starts an HTTP server on an ephemeral port
5. Opens the URL in your default browser
6. Serves the page, then exits

## Dependencies

| Crate | Purpose |
|---|---|
| `pulldown-cmark` | Fast GFM-compatible Markdown parser |
| `tiny_http` | Minimal HTTP server |
| `webbrowser` | Cross-platform browser launcher |
