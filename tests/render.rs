//! Integration tests: drive md-preview purely through its public API, the way
//! another crate (or the future daemon) would. These can only reach `pub`
//! items, so they double as a check that the surface is actually usable.

use md_preview::{render_markdown, render_page};

#[test]
fn render_markdown_handles_a_mixed_document() {
    let md = "\
# Heading

Inline math $a^2 + b^2$ and a display block:

$$
E = mc^2
$$

```rust
fn main() {}
```

    indented code
";
    let html = render_markdown(md);

    // Heading and inline/display math.
    assert!(html.contains("<h1>Heading</h1>"));
    assert!(html.contains(r#"<math-renderer style="display: inline">a^2 + b^2</math-renderer>"#));
    assert!(html.contains("js-display-math"));

    // Both the fenced and indented blocks are real, copyable code blocks.
    assert_eq!(html.matches(r#"<pre class="hl-code">"#).count(), 2);
    assert_eq!(html.matches(r#"class="copy-btn""#).count(), 2);
}

#[test]
fn render_page_wraps_the_body_in_a_full_document() {
    let page = render_page("# Title");
    assert!(page.starts_with("<!DOCTYPE html>"));
    assert!(page.trim_end().ends_with("</html>"));
    assert!(page.contains("<h1>Title</h1>"));
    // Assets needed to actually render math/styles in the browser.
    assert!(page.contains("katex.mjs"));
    assert!(page.contains("github-markdown.min.css"));
}

#[test]
fn mermaid_fence_renders_as_a_diagram_block_others_stay_highlighted() {
    let md = "\
```mermaid
graph TD; A-->B;
```

```rust
fn main() {}
```
";
    let html = render_markdown(md);
    // The mermaid fence is a diagram target: no highlighting, no copy button.
    assert!(html.contains(r#"<pre class="mermaid">"#));
    assert!(html.contains("graph TD; A--&gt;B;"));
    // The normal rust fence is still a highlighted, copyable block (regression).
    assert!(html.contains(r#"<pre class="hl-code">"#));
    assert_eq!(html.matches(r#"class="copy-btn""#).count(), 1);

    // The full page imports and initializes Mermaid.js from the jsdelivr CDN.
    let page = render_page(md);
    assert!(page.contains("mermaid@11/dist/mermaid.esm.min.mjs"));
    assert!(page.contains("mermaid.initialize"));
}
