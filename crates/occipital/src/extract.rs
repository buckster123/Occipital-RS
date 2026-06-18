//! Reader-mode extraction — HTML → clean Markdown + a resolved link list.
//!
//! The trick that makes one pipeline serve both the agent and the human: the
//! page comes back as signal (Markdown), not chrome. A single DOM walk strips
//! boilerplate (nav/header/footer/aside/script/…), picks the main content
//! container, converts the common elements to Markdown, and collects links
//! resolved to absolute URLs in the *same* pass (the follow-along UI consumes
//! `links` directly).
//!
//! Deliberately pragmatic, not a full Readability port: the content heuristic
//! (`select_main`) is isolated so a density-scoring algorithm can drop in later
//! without touching the converter. UTF-8 throughout — emoji/CJK are safe by
//! construction (no byte-index slicing).

use std::collections::HashSet;

use ego_tree::NodeRef;
use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

/// A link found in the main content, with its anchor text and absolute URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub text: String,
    pub url:  String,
}

/// The reader-mode result for one page.
#[derive(Debug, Clone, Serialize)]
pub struct Page {
    pub url:          String,
    pub title:        Option<String>,
    pub byline:       Option<String>,
    pub markdown:     String,
    pub links:        Vec<Link>,
    /// Stable content fingerprint (FNV-1a of the markdown) for dedup + change
    /// detection. Deterministic across runs, unlike `DefaultHasher`.
    pub content_hash: String,
}

/// Tags whose subtrees are boilerplate — skipped entirely.
const SKIP: &[&str] = &[
    "script", "style", "noscript", "template", "nav", "header", "footer", "aside", "form",
    "svg", "iframe", "button", "input", "select", "option", "figure", "figcaption",
];

const MAX_DEPTH: usize = 64;

/// Extract reader-mode Markdown + links from `html`, resolving relative links
/// against `base_url`.
pub fn extract(html: &str, base_url: &str) -> Page {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).ok();

    let title = extract_title(&doc);
    let byline = extract_byline(&doc);

    let mut out = String::new();
    let mut links: Vec<Link> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let root = select_main(&doc);
    for child in root.children() {
        render(child, base.as_ref(), 0, &mut out, &mut links, &mut seen);
    }

    let markdown = normalize(&out);
    let content_hash = fnv1a_hex(markdown.as_bytes());
    Page { url: base_url.to_string(), title, byline, markdown, links, content_hash }
}

/// Convenience for raw response bytes (lossy UTF-8 decode).
pub fn extract_bytes(bytes: &[u8], base_url: &str) -> Page {
    extract(&String::from_utf8_lossy(bytes), base_url)
}

/// Pick the main content container: the largest `<main>`/`<article>`/`[role=main]`
/// by text length, falling back to `<body>`.
fn select_main(doc: &Html) -> ElementRef<'_> {
    for sel in ["main", "article", "[role=main]"] {
        if let Ok(s) = Selector::parse(sel) {
            if let Some(best) = doc.select(&s).max_by_key(|e| e.text().map(|t| t.len()).sum::<usize>()) {
                // Ignore an empty match (e.g. a stray <article> wrapper).
                if best.text().map(|t| t.len()).sum::<usize>() > 0 {
                    return best;
                }
            }
        }
    }
    Selector::parse("body").ok()
        .and_then(|s| doc.select(&s).next())
        .unwrap_or_else(|| doc.root_element())
}

fn extract_title(doc: &Html) -> Option<String> {
    if let Some(t) = meta_content(doc, "meta[property='og:title']") {
        return Some(t);
    }
    if let Ok(s) = Selector::parse("title") {
        if let Some(el) = doc.select(&s).next() {
            let t = el.text().collect::<String>().trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    if let Ok(s) = Selector::parse("h1") {
        if let Some(el) = doc.select(&s).next() {
            let t = el.text().collect::<String>().trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

fn extract_byline(doc: &Html) -> Option<String> {
    meta_content(doc, "meta[name='author']")
        .or_else(|| meta_content(doc, "meta[property='article:author']"))
}

fn meta_content(doc: &Html, selector: &str) -> Option<String> {
    let s = Selector::parse(selector).ok()?;
    let el = doc.select(&s).next()?;
    let c = el.value().attr("content")?.trim().to_string();
    (!c.is_empty()).then_some(c)
}

/// Recursive DOM → Markdown. Block elements ensure surrounding blank lines;
/// inline elements wrap their rendered children.
fn render(
    node: NodeRef<Node>,
    base: Option<&Url>,
    depth: usize,
    out: &mut String,
    links: &mut Vec<Link>,
    seen: &mut HashSet<String>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    match node.value() {
        Node::Text(t) => push_text(out, &t.text),
        Node::Element(el) => {
            let name = el.name();
            if SKIP.contains(&name) {
                return;
            }
            match name {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level = name[1..].parse::<usize>().unwrap_or(1);
                    ensure_blank(out);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                    render_children(node, base, depth, out, links, seen);
                    ensure_blank(out);
                }
                "p" | "div" | "section" | "article" | "main" | "ul" | "ol" => {
                    ensure_blank(out);
                    render_children(node, base, depth, out, links, seen);
                    ensure_blank(out);
                }
                "br" => out.push('\n'),
                "hr" => {
                    ensure_blank(out);
                    out.push_str("---");
                    ensure_blank(out);
                }
                "li" => {
                    trim_trailing_spaces(out);
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("- ");
                    render_children(node, base, depth, out, links, seen);
                }
                "a" => {
                    let href = el.attr("href").unwrap_or("");
                    let mut text = String::new();
                    render_children(node, base, depth, &mut text, links, seen);
                    let text = text.trim();
                    match resolve(base, href) {
                        Some(u) => {
                            out.push('[');
                            out.push_str(text);
                            out.push_str("](");
                            out.push_str(&u);
                            out.push(')');
                            if seen.insert(u.clone()) {
                                links.push(Link { text: text.to_string(), url: u });
                            }
                        }
                        None => out.push_str(text),
                    }
                }
                "strong" | "b" => wrap(node, base, depth, out, links, seen, "**", "**"),
                "em" | "i" => wrap(node, base, depth, out, links, seen, "*", "*"),
                "code" => {
                    let raw = element_text(node);
                    out.push('`');
                    out.push_str(raw.trim());
                    out.push('`');
                }
                "pre" => {
                    let raw = element_text(node);
                    ensure_blank(out);
                    out.push_str("```\n");
                    out.push_str(raw.trim_end());
                    out.push_str("\n```");
                    ensure_blank(out);
                }
                "blockquote" => {
                    ensure_blank(out);
                    out.push_str("> ");
                    render_children(node, base, depth, out, links, seen);
                    ensure_blank(out);
                }
                "img" => {
                    if let Some(u) = el.attr("src").and_then(|s| resolve(base, s)) {
                        let alt = el.attr("alt").unwrap_or("").trim();
                        out.push_str(&format!("![{alt}]({u})"));
                    }
                }
                _ => render_children(node, base, depth, out, links, seen),
            }
        }
        _ => {}
    }
}

fn render_children(
    node: NodeRef<Node>,
    base: Option<&Url>,
    depth: usize,
    out: &mut String,
    links: &mut Vec<Link>,
    seen: &mut HashSet<String>,
) {
    for child in node.children() {
        render(child, base, depth + 1, out, links, seen);
    }
}

#[allow(clippy::too_many_arguments)]
fn wrap(
    node: NodeRef<Node>,
    base: Option<&Url>,
    depth: usize,
    out: &mut String,
    links: &mut Vec<Link>,
    seen: &mut HashSet<String>,
    open: &str,
    close: &str,
) {
    let mut inner = String::new();
    render_children(node, base, depth, &mut inner, links, seen);
    let inner = inner.trim();
    if inner.is_empty() {
        return;
    }
    out.push_str(open);
    out.push_str(inner);
    out.push_str(close);
}

/// All descendant text of an element node (for `<code>`/`<pre>` raw content).
fn element_text(node: NodeRef<Node>) -> String {
    ElementRef::wrap(node).map(|e| e.text().collect::<String>()).unwrap_or_default()
}

/// Append `text` with whitespace runs collapsed to a single space (inline flow).
fn push_text(out: &mut String, text: &str) {
    let mut last_ws = out.is_empty() || out.ends_with(|c: char| c.is_whitespace());
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            out.push(ch);
            last_ws = false;
        }
    }
}

fn trim_trailing_spaces(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
}

/// Ensure the buffer ends at a paragraph break (≤ two newlines, no trailing space).
fn ensure_blank(out: &mut String) {
    trim_trailing_spaces(out);
    if out.is_empty() {
        return;
    }
    if out.ends_with("\n\n") {
        // already a blank line
    } else if out.ends_with('\n') {
        out.push('\n');
    } else {
        out.push_str("\n\n");
    }
}

/// Resolve `href` against `base`, keeping only http(s). Drops fragments,
/// `mailto:`, `javascript:`, empty.
fn resolve(base: Option<&Url>, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let resolved = match base {
        Some(b) => b.join(href).ok()?,
        None => Url::parse(href).ok()?,
    };
    matches!(resolved.scheme(), "http" | "https").then(|| resolved.to_string())
}

/// Tidy the assembled markdown: strip trailing spaces, cap blank runs at one.
fn normalize(s: &str) -> String {
    let mut joined = String::with_capacity(s.len());
    for line in s.lines() {
        joined.push_str(line.trim_end());
        joined.push('\n');
    }
    let mut result = String::with_capacity(joined.len());
    let mut newlines = 0usize;
    for ch in joined.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                result.push(ch);
            }
        } else {
            newlines = 0;
            result.push(ch);
        }
    }
    result.trim().to_string()
}

/// FNV-1a (64-bit) as lowercase hex — a small, stable content fingerprint.
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"
        <html><head>
          <title>Test Article</title>
          <meta name="author" content="Ada Lovelace">
        </head><body>
          <nav><a href="/home">Home</a><a href="/about">About</a></nav>
          <header>site chrome</header>
          <main>
            <h1>Hello World</h1>
            <p>Some <strong>bold</strong> and <em>italic</em> text with a
               <a href="/docs/intro">link</a> and an
               <a href="https://example.org/ext">absolute link</a>.</p>
            <ul><li>first</li><li>second</li></ul>
            <pre><code>fn main() {}</code></pre>
            <p>Unicode is fine: 日本語 and 🦀 survive.</p>
          </main>
          <footer>footer chrome</footer>
          <script>console.log('nope')</script>
        </body></html>
    "#;

    #[test]
    fn extracts_title_and_byline() {
        let p = extract(PAGE, "https://example.com/article");
        assert_eq!(p.title.as_deref(), Some("Test Article"));
        assert_eq!(p.byline.as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn strips_chrome_and_scripts() {
        let p = extract(PAGE, "https://example.com/article");
        assert!(!p.markdown.contains("site chrome"), "header chrome removed");
        assert!(!p.markdown.contains("footer chrome"), "footer chrome removed");
        assert!(!p.markdown.contains("console.log"), "script removed");
        assert!(!p.markdown.contains("Home"), "nav links removed (outside <main>)");
    }

    #[test]
    fn converts_common_elements_to_markdown() {
        let p = extract(PAGE, "https://example.com/article");
        assert!(p.markdown.contains("# Hello World"), "heading: {}", p.markdown);
        assert!(p.markdown.contains("**bold**"));
        assert!(p.markdown.contains("*italic*"));
        assert!(p.markdown.contains("- first"));
        assert!(p.markdown.contains("```"));
        assert!(p.markdown.contains("fn main() {}"));
    }

    #[test]
    fn resolves_relative_links_to_absolute_and_collects_them() {
        let p = extract(PAGE, "https://example.com/article");
        assert!(p.markdown.contains("[link](https://example.com/docs/intro)"), "relative resolved");
        let urls: Vec<&str> = p.links.iter().map(|l| l.url.as_str()).collect();
        assert!(urls.contains(&"https://example.com/docs/intro"));
        assert!(urls.contains(&"https://example.org/ext"));
        // Nav links live outside <main>, so they are not in the content link list.
        assert!(!urls.iter().any(|u| u.ends_with("/home")), "nav excluded: {urls:?}");
    }

    #[test]
    fn unicode_is_preserved() {
        let p = extract(PAGE, "https://example.com/article");
        assert!(p.markdown.contains("日本語"), "CJK preserved");
        assert!(p.markdown.contains("🦀"), "emoji preserved");
    }

    #[test]
    fn content_hash_is_stable_and_changes_with_content() {
        let a = extract(PAGE, "https://example.com/article");
        let b = extract(PAGE, "https://example.com/article");
        assert_eq!(a.content_hash, b.content_hash, "deterministic");
        let c = extract("<main><p>different</p></main>", "https://example.com/x");
        assert_ne!(a.content_hash, c.content_hash, "different content → different hash");
    }

    #[test]
    fn falls_back_to_body_without_main() {
        let p = extract("<body><p>just a paragraph</p></body>", "https://x.test/");
        assert!(p.markdown.contains("just a paragraph"));
    }

    #[test]
    fn no_blank_line_runs() {
        let p = extract(PAGE, "https://example.com/article");
        assert!(!p.markdown.contains("\n\n\n"), "blank runs collapsed: {:?}", p.markdown);
    }
}
