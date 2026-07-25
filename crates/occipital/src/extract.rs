//! Reader-mode extraction — HTML → clean Markdown + a resolved link list + the
//! page's interactive surface (forms).
//!
//! The trick that makes one pipeline serve both the agent and the human: the
//! page comes back as signal (Markdown), not chrome. A single DOM walk strips
//! boilerplate (nav/header/footer/aside/script/…), picks the main content
//! container, converts the common elements to Markdown, and collects links
//! resolved to absolute URLs in the *same* pass (the follow-along UI consumes
//! `links` directly).
//!
//! Phase 12 (agent browsing) adds the **element registry**: forms are extracted
//! document-wide with stable 1-based ordinals in document order — a header
//! search box counts — and rendered into the reader view as one-line annotated
//! blocks (`[form#1 → GET /search — text "q" · submit "Go"]`) instead of being
//! skipped. Interaction verbs (Phase 13) address forms by these ordinals and
//! links by their position in `links`. Raw form internals are never rendered as
//! prose; the structure lives in `Page::forms`.
//!
//! Deliberately pragmatic, not a full Readability port: the content heuristic
//! (`select_main`) is isolated so a density-scoring algorithm can drop in later
//! without touching the converter. UTF-8 throughout — emoji/CJK are safe by
//! construction (no byte-index slicing).

use std::collections::{HashMap, HashSet};

use ego_tree::{NodeId, NodeRef};
use scraper::{ElementRef, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

/// A link found in the main content, with its anchor text and absolute URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub text: String,
    pub url:  String,
}

/// One fillable field of a form. Hidden inputs are preserved verbatim — they
/// are how sites thread state through a submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FormField {
    /// The submitted parameter name (may be empty for unnamed fields).
    pub name:     String,
    /// `text` / `hidden` / `search` / `select` / `textarea` / `checkbox` / …
    pub kind:     String,
    /// Best-effort human label: `aria-label`, a `<label for=…>`, or placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label:    Option<String>,
    /// The current/default value (a `<select>` reports its selected option).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value:    Option<String>,
    /// `<select>` options (empty for other kinds).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options:  Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

/// An interactive form, addressed by its stable document-order ordinal (`idx`,
/// 1-based). Extracted document-wide — a header search box counts even though
/// header chrome is stripped from the prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Form {
    pub idx:    usize,
    /// Absolute submission URL (the page's own URL when `action` is absent).
    pub action: String,
    /// `get` or `post` (anything else coerces to `get`, per the HTML spec).
    pub method: String,
    pub fields: Vec<FormField>,
    /// The submit control's label, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit: Option<String>,
}

/// The reader-mode result for one page.
#[derive(Debug, Clone, Serialize)]
pub struct Page {
    pub url:          String,
    pub title:        Option<String>,
    pub byline:       Option<String>,
    pub markdown:     String,
    pub links:        Vec<Link>,
    /// The page's interactive surface (document-wide, ordinal-addressed).
    pub forms:        Vec<Form>,
    /// Stable content fingerprint (FNV-1a of the markdown) for dedup + change
    /// detection. Deterministic across runs, unlike `DefaultHasher`.
    pub content_hash: String,
}

/// Tags whose subtrees are boilerplate — skipped entirely. Form *internals*
/// (input/select/option/button/textarea) stay listed: a stray control outside a
/// `<form>` is noise, and controls inside one are structured via `Page::forms`
/// (the `form` arm in `render` never recurses).
const SKIP: &[&str] = &[
    "script", "style", "noscript", "template", "nav", "header", "footer", "aside",
    "svg", "iframe", "button", "input", "select", "option", "textarea",
    "figure", "figcaption",
];

const MAX_DEPTH: usize = 64;

/// Immutable per-page context for the render walk.
struct Ctx<'a> {
    base:     Option<&'a Url>,
    forms:    &'a [Form],
    /// `<form>` node → its 1-based ordinal in `forms`.
    form_ids: &'a HashMap<NodeId, usize>,
}

/// Accumulators shared across the whole walk (unlike `out`, which nests for
/// inline elements).
#[derive(Default)]
struct Collected {
    links:          Vec<Link>,
    seen:           HashSet<String>,
    /// Ordinals annotated inline — the rest land in the trailer block.
    forms_rendered: HashSet<usize>,
}

/// Extract reader-mode Markdown + links + forms from `html`, resolving relative
/// URLs against `base_url`.
pub fn extract(html: &str, base_url: &str) -> Page {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).ok();

    let title = extract_title(&doc);
    let byline = extract_byline(&doc);
    let (forms, form_nodes) = extract_forms(&doc, base.as_ref(), base_url);
    let form_ids: HashMap<NodeId, usize> =
        form_nodes.iter().enumerate().map(|(i, &n)| (n, i + 1)).collect();
    let ctx = Ctx { base: base.as_ref(), forms: &forms, form_ids: &form_ids };

    let mut out = String::new();
    let mut acc = Collected::default();

    let root = select_main(&doc);
    for child in root.children() {
        render(child, &ctx, 0, &mut out, &mut acc);
    }

    // Forms that never rendered inline (outside the main container, or inside
    // stripped chrome like a header search box) still belong in the reader
    // view — a compact trailer keeps agent and human seeing the same surface.
    let missing: Vec<&Form> =
        forms.iter().filter(|f| !acc.forms_rendered.contains(&f.idx)).collect();
    if !missing.is_empty() {
        ensure_blank(&mut out);
        for f in missing {
            out.push_str(&form_annotation(f, base.as_ref()));
            out.push('\n');
        }
    }

    let markdown = normalize(&out);
    let content_hash = fnv1a_hex(markdown.as_bytes());
    Page {
        url: base_url.to_string(),
        title,
        byline,
        markdown,
        links: acc.links,
        forms,
        content_hash,
    }
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

// ---- forms (the element registry) ------------------------------------------

/// Document-wide form extraction, in document order. Returns the forms plus
/// each form's DOM node id (parallel, `idx = position + 1`).
fn extract_forms(doc: &Html, base: Option<&Url>, page_url: &str) -> (Vec<Form>, Vec<NodeId>) {
    let mut forms = Vec::new();
    let mut nodes = Vec::new();
    let Ok(form_sel) = Selector::parse("form") else { return (forms, nodes) };

    // `<label for=…>` → text, document-wide (labels may sit outside the form).
    let labels: HashMap<String, String> = Selector::parse("label[for]")
        .map(|s| {
            doc.select(&s)
                .filter_map(|l| {
                    let id = l.value().attr("for")?.trim().to_string();
                    let text = l.text().collect::<String>().trim().to_string();
                    (!id.is_empty() && !text.is_empty()).then_some((id, text))
                })
                .collect()
        })
        .unwrap_or_default();

    for el in doc.select(&form_sel) {
        let idx = forms.len() + 1;
        // Per the HTML spec a missing/empty action submits to the page itself.
        let action = el
            .value()
            .attr("action")
            .and_then(|a| resolve(base, a))
            .unwrap_or_else(|| page_url.to_string());
        let method = match el.value().attr("method").map(|m| m.trim().to_ascii_lowercase()) {
            Some(m) if m == "post" => "post".to_string(),
            _ => "get".to_string(),
        };
        let (fields, submit) = extract_fields(el, &labels);
        nodes.push(el.id());
        forms.push(Form { idx, action, method, fields, submit });
    }
    (forms, nodes)
}

/// The fillable fields + submit label of one form.
fn extract_fields(
    form: ElementRef,
    labels: &HashMap<String, String>,
) -> (Vec<FormField>, Option<String>) {
    let mut fields = Vec::new();
    let mut submit: Option<String> = None;
    let Ok(sel) = Selector::parse("input, select, textarea, button") else {
        return (fields, submit);
    };
    let opt_sel = Selector::parse("option").ok();

    for el in form.select(&sel) {
        let v = el.value();
        let name = v.attr("name").unwrap_or("").trim().to_string();
        let required = v.attr("required").is_some();
        match v.name() {
            "input" => {
                let kind = v.attr("type").unwrap_or("text").trim().to_ascii_lowercase();
                match kind.as_str() {
                    "submit" | "image" => {
                        if submit.is_none() {
                            submit = attr_nonempty(v, "value");
                        }
                    }
                    "button" | "reset" => {}
                    _ => fields.push(FormField {
                        name,
                        kind,
                        label: field_label(el, labels),
                        value: attr_nonempty(v, "value"),
                        options: Vec::new(),
                        required,
                    }),
                }
            }
            "textarea" => {
                let text = el.text().collect::<String>().trim().to_string();
                fields.push(FormField {
                    name,
                    kind: "textarea".into(),
                    label: field_label(el, labels),
                    value: (!text.is_empty()).then_some(text),
                    options: Vec::new(),
                    required,
                });
            }
            "select" => {
                let mut options = Vec::new();
                let mut value = None;
                if let Some(os) = &opt_sel {
                    for opt in el.select(os) {
                        let text = opt.text().collect::<String>().trim().to_string();
                        let val = attr_nonempty(opt.value(), "value").unwrap_or_else(|| text.clone());
                        // First option is the browser default; an explicit
                        // `selected` overrides it.
                        if opt.value().attr("selected").is_some() || options.is_empty() {
                            value = Some(val.clone());
                        }
                        options.push(val);
                    }
                }
                fields.push(FormField {
                    name,
                    kind: "select".into(),
                    label: field_label(el, labels),
                    value,
                    options,
                    required,
                });
            }
            "button" => {
                let t = v.attr("type").unwrap_or("submit").trim().to_ascii_lowercase();
                if t == "submit" && submit.is_none() {
                    let text = el.text().collect::<String>().trim().to_string();
                    submit = (!text.is_empty()).then_some(text);
                }
            }
            _ => {}
        }
    }
    (fields, submit)
}

/// Best-effort human label for a field: `aria-label` > `<label for=…>` > placeholder.
fn field_label(el: ElementRef, labels: &HashMap<String, String>) -> Option<String> {
    if let Some(a) = attr_nonempty(el.value(), "aria-label") {
        return Some(a);
    }
    if let Some(id) = el.value().attr("id") {
        if let Some(l) = labels.get(id.trim()) {
            return Some(l.clone());
        }
    }
    attr_nonempty(el.value(), "placeholder")
}

fn attr_nonempty(el: &scraper::node::Element, name: &str) -> Option<String> {
    el.attr(name).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// The one-line reader-view block for a form:
/// `[form#1 → GET /search — text "q" · hidden×2 · submit "Go"]`.
/// Same-origin actions display as a path; foreign ones keep the full URL.
fn form_annotation(form: &Form, base: Option<&Url>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for f in form.fields.iter().filter(|f| f.kind != "hidden") {
        let shown = if !f.name.is_empty() {
            &f.name
        } else if let Some(l) = &f.label {
            l
        } else {
            "?"
        };
        parts.push(format!("{} \"{}\"", f.kind, shown));
    }
    let hidden = form.fields.iter().filter(|f| f.kind == "hidden").count();
    if hidden > 0 {
        parts.push(format!("hidden×{hidden}"));
    }
    parts.push(format!("submit \"{}\"", form.submit.as_deref().unwrap_or("Submit")));
    format!(
        "[form#{} → {} {} — {}]",
        form.idx,
        form.method.to_uppercase(),
        display_action(&form.action, base),
        parts.join(" · "),
    )
}

fn display_action(action: &str, base: Option<&Url>) -> String {
    if let (Some(b), Ok(u)) = (base, Url::parse(action)) {
        if u.scheme() == b.scheme() && u.host_str() == b.host_str() {
            let mut p = u.path().to_string();
            if let Some(q) = u.query() {
                p.push('?');
                p.push_str(q);
            }
            return p;
        }
    }
    action.to_string()
}

// ---- markdown rendering -----------------------------------------------------

/// Recursive DOM → Markdown. Block elements ensure surrounding blank lines;
/// inline elements wrap their rendered children.
fn render(node: NodeRef<Node>, ctx: &Ctx, depth: usize, out: &mut String, acc: &mut Collected) {
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
                    render_children(node, ctx, depth, out, acc);
                    ensure_blank(out);
                }
                "p" | "div" | "section" | "article" | "main" | "ul" | "ol" => {
                    ensure_blank(out);
                    render_children(node, ctx, depth, out, acc);
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
                    render_children(node, ctx, depth, out, acc);
                }
                "a" => {
                    let href = el.attr("href").unwrap_or("");
                    let mut text = String::new();
                    render_children(node, ctx, depth, &mut text, acc);
                    let text = text.trim();
                    match resolve(ctx.base, href) {
                        Some(u) => {
                            out.push('[');
                            out.push_str(text);
                            out.push_str("](");
                            out.push_str(&u);
                            out.push(')');
                            if acc.seen.insert(u.clone()) {
                                acc.links.push(Link { text: text.to_string(), url: u });
                            }
                        }
                        None => out.push_str(text),
                    }
                }
                "form" => {
                    // The interactive surface renders as a one-line annotated
                    // block; internals are structured in `Page::forms`, never
                    // prose. No recursion.
                    if let Some(&idx) = ctx.form_ids.get(&node.id()) {
                        if acc.forms_rendered.insert(idx) {
                            ensure_blank(out);
                            out.push_str(&form_annotation(&ctx.forms[idx - 1], ctx.base));
                            ensure_blank(out);
                        }
                    }
                }
                "strong" | "b" => wrap(node, ctx, depth, out, acc, "**", "**"),
                "em" | "i" => wrap(node, ctx, depth, out, acc, "*", "*"),
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
                    render_children(node, ctx, depth, out, acc);
                    ensure_blank(out);
                }
                "img" => {
                    if let Some(u) = el.attr("src").and_then(|s| resolve(ctx.base, s)) {
                        let alt = el.attr("alt").unwrap_or("").trim();
                        out.push_str(&format!("![{alt}]({u})"));
                    }
                }
                _ => render_children(node, ctx, depth, out, acc),
            }
        }
        _ => {}
    }
}

fn render_children(
    node: NodeRef<Node>,
    ctx: &Ctx,
    depth: usize,
    out: &mut String,
    acc: &mut Collected,
) {
    for child in node.children() {
        render(child, ctx, depth + 1, out, acc);
    }
}

fn wrap(
    node: NodeRef<Node>,
    ctx: &Ctx,
    depth: usize,
    out: &mut String,
    acc: &mut Collected,
    open: &str,
    close: &str,
) {
    let mut inner = String::new();
    render_children(node, ctx, depth, &mut inner, acc);
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

    // ---- forms (Phase 12: the element registry) ---------------------------

    const FORM_PAGE: &str = r#"
        <html><head><title>Shop</title></head><body>
          <header>
            <form action="/search"><input type="search" name="q" placeholder="Search products">
              <button>Go</button></form>
          </header>
          <main>
            <h1>Contact</h1>
            <p>Write to us.</p>
            <form action="/contact" method="POST">
              <input type="hidden" name="csrf" value="tok123">
              <label for="em">Email address</label>
              <input type="email" id="em" name="email" required>
              <select name="topic">
                <option value="sales">Sales</option>
                <option value="support" selected>Support</option>
              </select>
              <textarea name="message">hello</textarea>
              <input type="submit" value="Send">
            </form>
          </main>
        </body></html>
    "#;

    #[test]
    fn forms_are_extracted_document_wide_in_order() {
        let p = extract(FORM_PAGE, "https://shop.test/contact");
        assert_eq!(p.forms.len(), 2, "header search box counts too");
        assert_eq!(p.forms[0].idx, 1);
        assert_eq!(p.forms[0].action, "https://shop.test/search", "action resolved absolute");
        assert_eq!(p.forms[0].method, "get", "method defaults to get");
        assert_eq!(p.forms[0].submit.as_deref(), Some("Go"), "button text is the submit label");
        assert_eq!(p.forms[1].idx, 2);
        assert_eq!(p.forms[1].method, "post");
        assert_eq!(p.forms[1].submit.as_deref(), Some("Send"));
    }

    #[test]
    fn form_fields_keep_hidden_labels_options_and_required() {
        let p = extract(FORM_PAGE, "https://shop.test/contact");
        let f = &p.forms[1];
        let by_name = |n: &str| f.fields.iter().find(|x| x.name == n).unwrap();

        let csrf = by_name("csrf");
        assert_eq!(csrf.kind, "hidden");
        assert_eq!(csrf.value.as_deref(), Some("tok123"), "hidden state preserved verbatim");

        let email = by_name("email");
        assert_eq!(email.kind, "email");
        assert!(email.required);
        assert_eq!(email.label.as_deref(), Some("Email address"), "<label for=> resolved");

        let topic = by_name("topic");
        assert_eq!(topic.kind, "select");
        assert_eq!(topic.options, vec!["sales", "support"]);
        assert_eq!(topic.value.as_deref(), Some("support"), "selected option wins");

        let msg = by_name("message");
        assert_eq!(msg.kind, "textarea");
        assert_eq!(msg.value.as_deref(), Some("hello"));

        let q = &p.forms[0].fields[0];
        assert_eq!(q.label.as_deref(), Some("Search products"), "placeholder as label fallback");
    }

    #[test]
    fn reader_view_shows_annotated_form_blocks_not_raw_controls() {
        let p = extract(FORM_PAGE, "https://shop.test/contact");
        // The main-content form renders inline as a one-line annotation…
        assert!(
            p.markdown.contains("[form#2 → POST /contact —"),
            "inline block: {}",
            p.markdown
        );
        assert!(p.markdown.contains("hidden×1"), "hidden fields aggregate: {}", p.markdown);
        assert!(p.markdown.contains("submit \"Send\""));
        // …the header form (stripped chrome) still appears, in the trailer.
        assert!(
            p.markdown.contains("[form#1 → GET /search — search \"q\" · submit \"Go\"]"),
            "trailer block: {}",
            p.markdown
        );
        // Raw control internals never render as prose.
        assert!(!p.markdown.contains("tok123"), "hidden value not in prose");
        assert!(!p.markdown.contains("Sales"), "options not in prose");
    }

    #[test]
    fn pages_without_forms_are_unchanged() {
        let p = extract(PAGE, "https://example.com/article");
        assert!(p.forms.is_empty());
        assert!(!p.markdown.contains("[form#"));
    }

    #[test]
    fn form_annotation_keeps_foreign_actions_full() {
        let html = r#"<main><form action="https://other.test/subscribe" method="post">
            <input name="e"><button>Sub</button></form></main>"#;
        let p = extract(html, "https://shop.test/");
        assert!(
            p.markdown.contains("[form#1 → POST https://other.test/subscribe —"),
            "foreign action stays absolute: {}",
            p.markdown
        );
    }
}
