//! Markdown → HTML rendering for md-preview.
//!
//! The crate is split so the rendering is usable on its own (tests, a future
//! daemon, other tools): [`render_markdown`] turns Markdown into an HTML body
//! fragment, and [`render_page`] wraps that fragment in the full standalone
//! HTML document (styles, KaTeX, copy-button script). The binary in `main.rs`
//! only handles argument parsing and serving.

use pulldown_cmark::Options;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::io::Cursor;
use syntect::highlighting::{Color, ThemeSet};
use syntect::html::{css_for_theme_with_class_style, ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

// Real-time collaborative editing (see ~/.claude/plans/inherited-rolling-finch.md).
// The renderer above stays pure and reusable; these modules add the document
// model behind a stable contract (`doc::DocSession`).
pub mod doc;
pub mod diff;
pub mod file_peer;
pub mod session;

// The persistent daemon (warp + tokio live preview). Feature-gated so the lib
// stays usable on its own — `--no-default-features` builds the pure renderer and
// document core with ZERO web/server dependencies.
#[cfg(feature = "daemon")]
pub mod server;

/// Class prefix for syntect's highlight classes. Shared by the HTML generator
/// and the generated CSS so the markup and stylesheet stay in sync.
const HL_PREFIX: &str = "hl-";

/// GitHub's official TextMate themes (from primer/github-textmate-theme),
/// embedded so the binary stays self-contained. We only use them to generate
/// CSS — class-based highlighting itself is theme-independent.
const GITHUB_LIGHT_THEME: &str = include_str!("../themes/github-light.tmTheme");
const GITHUB_DARK_THEME: &str = include_str!("../themes/github-dark.tmTheme");

/// GitHub-style "copy code" button. Holds both the copy and check octicons; the
/// `.copied` class (toggled by JS on click) swaps which one is visible. The
/// click handler reads the sibling <pre><code> text, so no code is duplicated
/// into an attribute.
const COPY_BUTTON: &str = r##"<button class="copy-btn" type="button" aria-label="Copy code to clipboard" title="Copy">
<svg class="octicon octicon-copy" aria-hidden="true" height="16" width="16" viewBox="0 0 16 16"><path d="M0 6.75C0 5.784.784 5 1.75 5h1.5a.75.75 0 0 1 0 1.5h-1.5a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-1.5a.75.75 0 0 1 1.5 0v1.5A1.75 1.75 0 0 1 9.25 16h-7.5A1.75 1.75 0 0 1 0 14.25Z"></path><path d="M5 1.75C5 .784 5.784 0 6.75 0h7.5C15.216 0 16 .784 16 1.75v7.5A1.75 1.75 0 0 1 14.25 11h-7.5A1.75 1.75 0 0 1 5 9.25Zm1.75-.25a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-7.5a.25.25 0 0 0-.25-.25Z"></path></svg>
<svg class="octicon octicon-check" aria-hidden="true" height="16" width="16" viewBox="0 0 16 16"><path d="M13.78 4.22a.75.75 0 0 1 0 1.06l-7.25 7.25a.75.75 0 0 1-1.06 0L2.22 9.28a.751.751 0 0 1 .018-1.042.751.751 0 0 1 1.042-.018L6 10.94l6.72-6.72a.75.75 0 0 1 1.06 0Z"></path></svg>
</button>"##;

/// Format a syntect color as `#rrggbb`, or `inherit` if the theme omits it.
fn hex(c: Option<Color>) -> String {
    match c {
        Some(c) => format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b),
        None => "inherit".to_string(),
    }
}

/// Build the code-highlighting CSS: GitHub Light as the default, GitHub Dark
/// inside a `prefers-color-scheme: dark` media query. Both themes emit the same
/// `hl-` class names, so the dark block simply overrides in dark mode — one
/// server render, theme chosen by the browser (the way GitHub itself does it).
fn syntax_css() -> String {
    let style = ClassStyle::SpacedPrefixed { prefix: HL_PREFIX };
    let light = ThemeSet::load_from_reader(&mut Cursor::new(GITHUB_LIGHT_THEME))
        .expect("bundled light theme is valid");
    let dark = ThemeSet::load_from_reader(&mut Cursor::new(GITHUB_DARK_THEME))
        .expect("bundled dark theme is valid");
    let light_css = css_for_theme_with_class_style(&light, style).expect("generate light CSS");
    let dark_css = css_for_theme_with_class_style(&dark, style).expect("generate dark CSS");
    // Explicit <pre> background/foreground, specific enough to beat
    // github-markdown-css's `.markdown-body pre` rule (syntect's own `.hl-code`
    // rule has lower specificity and would otherwise lose).
    //
    // GitHub backs code blocks with --bgColor-muted (canvas-subtle), NOT the
    // editor canvas baked into the tmTheme: #f6f8fa light / #161b22 dark, per
    // github-markdown-css 5.x. We keep each theme's own text color.
    let l_fg = hex(light.settings.foreground);
    let d_fg = hex(dark.settings.foreground);
    let (l_bg, d_bg) = ("#f6f8fa", "#161b22");
    format!(
        "{light_css}\n\
         .markdown-body pre.hl-code {{ background-color: {l_bg}; color: {l_fg}; }}\n\
         @media (prefers-color-scheme: dark) {{\n{dark_css}\n\
         .markdown-body pre.hl-code {{ background-color: {d_bg}; color: {d_fg}; }}\n}}\n"
    )
}

/// Minimal HTML escaping for text placed inside the <math-renderer> element.
/// The frontend reads `this.textContent`, which decodes these back to raw TeX
/// before handing it to KaTeX, so escaping here is lossless.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a code block as GitHub-style class-based markup: a `.code-wrap`
/// holding the copy button and a `<pre class="hl-code"><code>` of highlighted
/// spans (no inline colors — the `hl-` CSS in <head> handles light/dark).
/// `lang` is the fence token, or "" for an indented block (→ plain text).
fn render_code_block(ps: &SyntaxSet, lang: &str, code: &str) -> String {
    let syntax = ps
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut hl = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        ps,
        ClassStyle::SpacedPrefixed { prefix: HL_PREFIX },
    );
    for line in LinesWithEndings::from(code) {
        let _ = hl.parse_html_for_line_which_includes_newline(line);
    }
    format!(
        "<div class=\"code-wrap\">{button}<pre class=\"hl-code\"><code>{code}</code></pre></div>",
        button = COPY_BUTTON,
        code = hl.finalize()
    )
}

/// Wrap raw TeX in the <math-renderer> custom element consumed by KaTeX.
fn math_renderer(tex: &str, display: bool) -> String {
    let escaped = escape_html(tex.trim());
    if display {
        format!(
            r#"<math-renderer class="js-display-math" style="display: block">{}</math-renderer>"#,
            escaped
        )
    } else {
        format!(
            r#"<math-renderer style="display: inline">{}</math-renderer>"#,
            escaped
        )
    }
}

/// Convert Markdown to an HTML **body fragment** (no surrounding document).
///
/// Beyond GitHub-flavored Markdown this handles two things specially:
/// math (`$...$`, `$$...$$`) becomes `<math-renderer>` elements for KaTeX, and
/// code blocks (fenced *or* indented) become syntax-highlighted `<pre>` blocks
/// with a copy button. Use [`render_page`] for a standalone HTML document.
pub fn render_markdown(markdown_input: &str) -> String {
    let ps = SyntaxSet::load_defaults_newlines();

    // GFM + math options. ENABLE_MATH turns `$...$` into Event::InlineMath
    // and `$$...$$` into Event::DisplayMath.
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_MATH);

    let parser = Parser::new_ext(markdown_input, options);
    let mut html_output = String::new();

    // Code blocks (fenced *or* indented) are intercepted: we suppress
    // pulldown's default <pre><code>, accumulate the body text across all the
    // Text events between Start and End, then emit our own highlighted markup
    // once on End. Accumulating (rather than rendering per Text event) keeps a
    // block whole even if the parser splits its body into several events.
    // `code_block` is Some((lang, buffer)) while inside a block; lang is the
    // fence token, or "" for an indented block.
    let mut code_block: Option<(String, String)> = None;

    let events = parser.filter_map(|event| {
        match event {
            // Inline `$...$` and display `$$...$$` math (from ENABLE_MATH).
            Event::InlineMath(tex) => Some(Event::Html(math_renderer(&tex, false).into())),
            Event::DisplayMath(tex) => Some(Event::Html(math_renderer(&tex, true).into())),
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                    pulldown_cmark::CodeBlockKind::Indented => String::new(),
                };
                code_block = Some((lang, String::new()));
                None // suppress default tag; the whole block is emitted on End
            }
            Event::End(TagEnd::CodeBlock) => {
                let (lang, code) = code_block.take().expect("CodeBlock End without Start");
                Some(Event::Html(render_code_block(&ps, &lang, &code).into()))
            }
            // While inside a code block, route text into the buffer instead of
            // the document. (A ```math fence is just a code block here — only
            // inline $...$ and display $$...$$ get KaTeX-rendered.)
            Event::Text(text) if code_block.is_some() => {
                code_block.as_mut().unwrap().1.push_str(&text);
                None
            }
            other => Some(other),
        }
    });

    pulldown_cmark::html::push_html(&mut html_output, events);
    html_output
}

/// Render Markdown to a complete, standalone HTML document: the
/// [`render_markdown`] body plus the `<head>` styles (github-markdown-css,
/// KaTeX CSS, auto light/dark highlight CSS) and the scripts that drive KaTeX
/// and the copy-code buttons.
///
/// This is a thin wrapper over [`render_page_with`] with empty extras; its
/// output is byte-identical to assembling the document by hand.
pub fn render_page(markdown_input: &str) -> String {
    render_page_with(&render_markdown(markdown_input), "", "")
}

/// Assemble the full standalone HTML document around an already-rendered
/// `body` fragment, injecting `extra_head` just before `</head>` and
/// `extra_body` just before `</body>`.
///
/// This is the **pure** seam the daemon uses to add its live-reload WebSocket
/// client (and wrap the body in `<div id="doc">…</div>`) without duplicating
/// the bundled assets/styles, while keeping the renderer free of any web/server
/// dependency. With empty extras the output is byte-identical to the original
/// standalone page, so [`render_page`] and the existing snapshot tests are
/// unaffected.
pub fn render_page_with(body: &str, extra_head: &str, extra_body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
        <html>
        <head>
            <meta charset="utf-8">
            <meta name="viewport" content="width=device-width, initial-scale=1">
            <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/github-markdown-css/5.5.1/github-markdown.min.css">
            <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/katex@0.16.9/dist/katex.min.css">
            <style>
                .markdown-body {{
                    box-sizing: border-box;
                    min-width: 200px;
                    max-width: 980px;
                    margin: 0 auto;
                    padding: 45px;
                }}
                @media (max-width: 767px) {{ .markdown-body {{ padding: 15px; }} }}
                /* Page background follows the system theme, like the code blocks. */
                body {{ background-color: #ffffff; }}
                @media (prefers-color-scheme: dark) {{ body {{ background-color: #0d1117; }} }}
                /* Copy-code button, GitHub-style: top-right of each block,
                   fades in on hover, swaps to a green check when copied. */
                .code-wrap {{ position: relative; }}
                .copy-btn {{
                    position: absolute; top: 8px; right: 8px;
                    display: inline-flex; align-items: center; justify-content: center;
                    width: 28px; height: 28px; padding: 0;
                    border: 1px solid transparent; border-radius: 6px;
                    background: transparent; color: #656d76; cursor: pointer;
                    opacity: 0; transition: opacity .1s, background-color .1s, border-color .1s;
                }}
                .code-wrap:hover .copy-btn, .copy-btn:focus {{ opacity: 1; }}
                .copy-btn:hover {{ background-color: rgba(175,184,193,.2); border-color: rgba(175,184,193,.4); }}
                .copy-btn .octicon-check {{ display: none; color: #1a7f37; }}
                .copy-btn.copied {{ opacity: 1; }}
                .copy-btn.copied .octicon-copy {{ display: none; }}
                .copy-btn.copied .octicon-check {{ display: inline-block; }}
                @media (prefers-color-scheme: dark) {{
                    .copy-btn {{ color: #7d8590; }}
                    .copy-btn:hover {{ background-color: rgba(99,110,123,.25); border-color: rgba(99,110,123,.4); }}
                    .copy-btn .octicon-check {{ color: #3fb950; }}
                }}
                /* GitHub Light/Dark code highlighting, auto-switching. */
{syntax_css}
            </style>{extra_head}
        </head>
        <body class="markdown-body">
            {body}
        <script type="module">
import katex from 'https://cdn.jsdelivr.net/npm/katex@0.16.9/dist/katex.mjs';

class MathRenderer extends HTMLElement {{
    connectedCallback() {{
        // Get the raw TeX code inside the tag
        const rawTex = this.textContent.trim();
        const isDisplay = this.classList.contains('js-display-math');

        try {{
            // Render the math directly inside this element
            katex.render(rawTex, this, {{
                displayMode: isDisplay,
                throwOnError: false
            }});
        }} catch (err) {{
            console.error("Failed to render LaTeX:", err);
        }}
    }}
}}

// Register the custom element
customElements.define('math-renderer', MathRenderer);

// Copy-code buttons: copy the sibling <pre><code> text, then flash the check.
// navigator.clipboard works here because http://127.0.0.1 is a secure context.
document.addEventListener('click', async (e) => {{
    const btn = e.target.closest('.copy-btn');
    if (!btn) return;
    const code = btn.parentElement.querySelector('pre code');
    try {{
        await navigator.clipboard.writeText(code ? code.textContent : '');
        btn.classList.add('copied');
        btn.setAttribute('title', 'Copied!');
        setTimeout(() => {{
            btn.classList.remove('copied');
            btn.setAttribute('title', 'Copy');
        }}, 2000);
    }} catch (err) {{
        console.error('Copy failed:', err);
    }}
}});
        </script>{extra_body}
        </body>
        </html>"#,
        syntax_css = syntax_css(),
        body = body,
        extra_head = extra_head,
        extra_body = extra_body
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_escapes_markup_chars() {
        assert_eq!(escape_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        // Order matters: an already-escaped entity must not be double-escaped
        // into something lossy. `&` is replaced first, so `<` -> `&lt;` stays.
        assert_eq!(escape_html("<"), "&lt;");
    }

    #[test]
    fn math_renderer_distinguishes_inline_and_display() {
        let inline = math_renderer("x^2", false);
        assert!(inline.contains(r#"style="display: inline""#));
        assert!(!inline.contains("js-display-math"));
        assert!(inline.contains("x^2"));

        let display = math_renderer("x^2", true);
        assert!(display.contains("js-display-math"));
        assert!(display.contains(r#"style="display: block""#));
    }

    #[test]
    fn math_renderer_trims_and_escapes() {
        let out = math_renderer("  a < b  ", false);
        assert!(out.contains("a &lt; b"));
        assert!(!out.contains("  a"));
    }

    #[test]
    fn inline_math_becomes_inline_math_renderer() {
        let html = render_markdown("An equation $a + b$ here.");
        assert!(html.contains(r#"<math-renderer style="display: inline">a + b</math-renderer>"#));
        assert!(!html.contains("js-display-math"));
    }

    #[test]
    fn display_math_becomes_display_math_renderer() {
        let html = render_markdown("$$\nx = y\n$$");
        assert!(html.contains("js-display-math"));
        assert!(html.contains("x = y"));
    }

    #[test]
    fn fenced_code_block_is_highlighted_with_copy_button() {
        let html = render_markdown("```rust\nfn main() {}\n```");
        assert!(html.contains(r#"<pre class="hl-code">"#));
        assert!(html.contains(r#"class="copy-btn""#));
        // Rust keyword picked up by the highlighter.
        assert!(html.contains("hl-storage"));
        // Exactly one block -> exactly one copy button.
        assert_eq!(html.matches(r#"class="copy-btn""#).count(), 1);
    }

    #[test]
    fn indented_code_block_is_also_a_code_block() {
        // Regression: indented blocks once leaked through as raw body text with
        // no <pre>/<code> and no copy button.
        let html = render_markdown("Intro:\n\n    let x = 42;\n");
        assert!(html.contains(r#"<pre class="hl-code">"#));
        assert!(html.contains(r#"class="copy-btn""#));
        assert!(html.contains("let x = 42;"));
    }

    #[test]
    fn math_fence_is_a_code_block_not_katex() {
        // ```math is a *code block* showing raw TeX, not a rendered equation.
        let html = render_markdown("```math\nx = y\n```");
        assert!(html.contains(r#"<pre class="hl-code">"#));
        assert!(!html.contains("math-renderer"));
    }

    #[test]
    fn plain_markdown_passes_through() {
        let html = render_markdown("# Title\n\nSome **bold** text.");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
    }

    #[test]
    fn gfm_tables_are_enabled() {
        let html = render_markdown("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(html.contains("<table>"));
    }

    #[test]
    fn render_page_is_a_full_document_with_assets() {
        let page = render_page("# Hi");
        assert!(page.starts_with("<!DOCTYPE html>"));
        assert!(page.contains("github-markdown.min.css"));
        assert!(page.contains("katex.min.css"));
        assert!(page.contains("customElements.define('math-renderer'"));
        // The highlight CSS is injected.
        assert!(page.contains("prefers-color-scheme: dark"));
        // And the rendered body is embedded.
        assert!(page.contains("<h1>Hi</h1>"));
    }

    #[test]
    fn render_page_with_empty_extras_is_byte_identical_to_render_page() {
        // The pure seam must not perturb the standalone page when no extras are
        // injected — this is what keeps the existing snapshot/asset tests and any
        // byte-for-byte consumers stable.
        for md in ["", "# Hi", "Inline $a^2$ and\n\n```rust\nfn x() {}\n```\n"] {
            let body = render_markdown(md);
            assert_eq!(
                render_page_with(&body, "", ""),
                render_page(md),
                "render_page_with(.., \"\", \"\") must equal render_page for {md:?}"
            );
        }
    }

    #[test]
    fn render_page_with_injects_extras_in_the_right_places() {
        let body = render_markdown("# Hi");
        let page = render_page_with(&body, "<!--HEADMARK-->", "<!--BODYMARK-->");
        // extra_head lands inside <head> (before the closing tag).
        let head_pos = page.find("<!--HEADMARK-->").expect("head extra present");
        let head_close = page.find("</head>").expect("</head> present");
        assert!(head_pos < head_close, "extra_head must be inside <head>");
        // extra_body lands just before </body>.
        let body_pos = page.find("<!--BODYMARK-->").expect("body extra present");
        let body_close = page.find("</body>").expect("</body> present");
        assert!(body_pos < body_close, "extra_body must be before </body>");
        assert!(head_close < body_pos, "head extra precedes body extra");
        // The original body fragment is still embedded.
        assert!(page.contains("<h1>Hi</h1>"));
    }
}
