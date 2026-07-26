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
    /// Whether submitting this form can do anything at all. `false` for a GET
    /// form with zero NAMED fields — it cannot carry data by construction, so
    /// its submission is structurally a re-fetch of the action URL
    /// (react.dev's Algolia modal-trigger forms, apex1 field report
    /// 2026-07-26: the no-op returned the homepage as a "search result").
    /// `web_submit` refuses such forms; this flag makes it visible in the
    /// registry before anyone tries.
    #[serde(default = "default_true")]
    pub submittable: bool,
}

fn default_true() -> bool {
    true
}

/// The one rule for whether submitting a form can do anything: a GET form
/// with zero NAMED fields collapses to a bare re-fetch of its action. Shared
/// by extraction (stamping the registry), the cache read path (recomputing
/// over stale rows — a pre-flag row serde-defaults `true`, which inverts the
/// flag's pre-flight purpose; apex1 seam report 2026-07-26), and the engine's
/// submit refusal (which never trusts a stored flag).
pub(crate) fn form_is_submittable(method: &str, fields: &[FormField]) -> bool {
    method == "post" || fields.iter().any(|f| !f.name.is_empty())
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
    /// The body was thin and part of `markdown` was mined from data embedded
    /// in the static HTML (state blobs / ld+json / noscript) — see `salvage`.
    pub salvaged:     bool,
    /// The page renders client-side and nothing was recoverable — the honest
    /// "this isn't blank, it needs JavaScript" signal.
    pub js_required:  bool,
    /// Stable content fingerprint (FNV-1a of the markdown) for dedup + change
    /// detection. Deterministic across runs, unlike `DefaultHasher`.
    pub content_hash: String,
    /// An authored-markdown alternate the page itself advertises
    /// (`<link rel="alternate" type="text/markdown">`) — 10 KB of curated
    /// markdown beats reader-mode extraction of a 150 KB SPA (apex1 field
    /// measurement, vite.dev, 2026-07-26). Reported, never auto-followed:
    /// the caller chooses. Markup-only discovery — probing same-path `.md` /
    /// `/llms.txt` conventions is the parked follow-on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown_alternate: Option<String>,
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

    // A script-driven search the interaction verbs cannot submit — say so,
    // or its absence from the form registry reads as a missing feature
    // rather than a site reality (apex1 field reports, 2026-07-26). Two
    // shapes: a search-looking <input> outside any <form>, and the SPA
    // idiom apex1's verification round caught on openlibrary.org — no
    // <input> at all, just a div/button trigger hydrated by JS, detectable
    // only by its markup affordance. The affordance path is suppressed only
    // by a form that could PLAUSIBLY BE the search (a named text/search
    // field with a searchy name or label) — "the page has forms" is not the
    // same claim: a newsletter form (docs.pydantic.dev) or a dead modal
    // trigger (react.dev) doesn't make a JS-only search submittable.
    if has_formless_search(&doc) || (!has_search_form(&forms) && has_search_affordance(&doc)) {
        ensure_blank(&mut out);
        out.push_str(
            "*[a search affordance exists outside any form — script-driven; not submittable via the interaction verbs]*\n",
        );
    }

    let mut markdown = normalize(&out);
    let mut links = acc.links;
    let mut salvaged = false;
    let mut js_required = false;

    // A thin body on a scripts-heavy page usually means client-side rendering.
    // Mine what the static HTML *does* carry (Phase 14) — and when even that
    // fails, say so instead of returning near-silence.
    if crate::salvage::is_thin(&markdown) {
        if let Some(s) = crate::salvage::salvage(&doc, base.as_ref()) {
            let mut md = String::new();
            if !markdown.is_empty() {
                md.push_str(&markdown);
                md.push_str("\n\n");
            }
            md.push_str("*[salvaged: this page renders client-side — the content below was mined from data embedded in the static HTML]*\n\n");
            md.push_str(&s.markdown);
            markdown = normalize(&md);
            for l in s.links {
                if !links.iter().any(|x| x.url == l.url) {
                    links.push(l);
                }
            }
            salvaged = true;
        } else if crate::salvage::scripts_heavy(&doc) && forms.is_empty() && links.len() < 5 {
            js_required = true;
            let note = "*[this page requires JavaScript to render — no static content or embedded page data was recoverable]*";
            markdown = if markdown.is_empty() {
                note.to_string()
            } else {
                format!("{markdown}\n\n{note}")
            };
        }
    }

    // Unwrap known redirector links (tracking hops) AFTER the salvage merge,
    // so mined links get the same hygiene as rendered ones — otherwise the
    // same site yields clean or wrapped hrefs depending on which path found
    // them (apex1 field report, 2026-07-26).
    for l in &mut links {
        if let Some(real) = unwrap_redirect(&l.url) {
            l.url = real;
        }
    }

    let content_hash = fnv1a_hex(markdown.as_bytes());
    Page {
        url: base_url.to_string(),
        title,
        byline,
        markdown,
        links,
        forms,
        salvaged,
        js_required,
        content_hash,
        markdown_alternate: markdown_alternate(&doc, base.as_ref()),
    }
}

/// A page-advertised authored-markdown alternate, when the markup declares
/// one. Markup-only (zero extra requests) — convention probing (`/llms.txt`,
/// same-path `.md`) is deliberately not done here (apex1's 5-site field
/// tally: markup is the majority discovery path — the prober stays unbuilt).
///
/// A reported affordance must be a REAL one: an alternate that resolves to a
/// host that cannot exist is the phantom-Submit bug from a different door
/// (docs.deno.com advertises `//runtime/index.md` — RFC 3986 reads that as a
/// network-path reference, host `runtime`, which is DNS-dead; they meant one
/// slash). A single-label host can't be a public site, so re-read such an
/// href as root-relative against the page's origin — the URL that actually
/// serves — and if it still doesn't resolve plausibly, report nothing rather
/// than a dead URL.
fn markdown_alternate(doc: &Html, base: Option<&Url>) -> Option<String> {
    let sel = Selector::parse(r#"link[rel="alternate"][type="text/markdown"]"#).ok()?;
    let plausible = |u: &str| -> Option<String> {
        let parsed = Url::parse(u).ok()?;
        let host = parsed.host_str()?;
        let page_host = base.and_then(|b| b.host_str());
        // Dotted host = public-plausible; host == the page's own (covers
        // localhost/intranet pages, where single-label is the norm).
        (host.contains('.') || Some(host) == page_host).then(|| parsed.to_string())
    };
    doc.select(&sel)
        .filter_map(|l| l.value().attr("href"))
        .find_map(|href| {
            let resolved = resolve(base, href)?;
            if let Some(good) = plausible(&resolved) {
                return Some(good);
            }
            // The `//single-label/...` authoring slip: re-read as
            // root-relative and re-validate.
            let rest = href.trim().strip_prefix("//")?;
            plausible(&resolve(base, &format!("/{rest}"))?)
        })
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

pub(crate) fn meta_content(doc: &Html, selector: &str) -> Option<String> {
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
        let submittable = form_is_submittable(&method, &fields);
        forms.push(Form { idx, action, method, fields, submit, submittable });
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
    // The prose must carry what the registry knows: a dead form annotated
    // with a synthesized `submit "Submit"` actively invites the call that
    // will be refused (apex1 seam report, 2026-07-26 — mkdocs.org, where the
    // prose is the only thing a reading model sees).
    if form.submittable {
        parts.push(format!("submit \"{}\"", form.submit.as_deref().unwrap_or("Submit")));
    } else {
        parts.push("not submittable (no named fields)".into());
    }
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
pub(crate) fn resolve(base: Option<&Url>, href: &str) -> Option<String> {
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

/// Known redirectors wrap a result's real destination in a query parameter (a
/// tracking hop) — a `web_click` on such a link chains into the tracker, and
/// the same site yields different link hygiene depending on whether the
/// search-provider path (which unwraps) or the page-extraction path (which
/// didn't) found it. Deliberately conservative: only hosts we know, only
/// parameters whose value is itself a full http(s) URL — anything else is
/// left exactly as the page wrote it. (Bing's `aclick` stays out: its `u`
/// parameter is base64-wrapped, not a URL.)
fn unwrap_redirect(href: &str) -> Option<String> {
    let u = Url::parse(href).ok()?;
    let host = u.host_str()?;
    let param = if host.ends_with("duckduckgo.com") && u.path().contains("/l/") {
        "uddg"
    } else if (host == "google.com" || host.ends_with(".google.com")) && u.path() == "/url" {
        "q"
    } else if host == "out.reddit.com" {
        "url"
    } else {
        return None;
    };
    let dest = u
        .query_pairs()
        .find(|(k, _)| k == param)
        .or_else(|| {
            // Google uses `q` or `url` depending on the surface.
            (param == "q").then(|| u.query_pairs().find(|(k, _)| k == "url")).flatten()
        })
        .map(|(_, v)| v.into_owned())?;
    let parsed = Url::parse(&dest).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.to_string())
}

/// Whether the document carries a search-looking input that is NOT inside any
/// `<form>` — a script-driven search box the interaction verbs cannot submit.
fn has_formless_search(doc: &Html) -> bool {
    let Ok(sel) = Selector::parse(
        r#"input[type="search"], input[name="q"], input[name="query"], input[name="search"]"#,
    ) else {
        return false;
    };
    doc.select(&sel).any(|el| {
        !el.ancestors()
            .filter_map(ElementRef::wrap)
            .any(|a| a.value().name() == "form")
    })
}

/// Whether `s` contains "search" as a standalone word — attribute-scoped
/// matching, so "Research papers" can never trip it.
fn has_search_word(s: &str) -> bool {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w.eq_ignore_ascii_case("search"))
}

/// Whether any form could PLAUSIBLY BE the page's search: at least one NAMED
/// text/search field whose name or label reads searchy. This is what gates
/// the affordance note off — "the page has forms" is not it (apex1's 6-site
/// survey: a newsletter form or a dead modal trigger must not silence the
/// note).
fn has_search_form(forms: &[Form]) -> bool {
    const SEARCHY_NAMES: [&str; 6] = ["q", "s", "query", "search", "keyword", "keywords"];
    forms.iter().any(|f| {
        f.fields.iter().any(|fd| {
            !fd.name.is_empty()
                && matches!(fd.kind.as_str(), "text" | "search")
                && (SEARCHY_NAMES.contains(&fd.name.to_ascii_lowercase().as_str())
                    || fd.label.as_deref().is_some_and(has_search_word))
        })
    })
}

/// Whether the markup carries a search AFFORDANCE without any `<input>` being
/// involved — the modern-SPA shape (`<div class="search-bar-trigger">` and
/// kin, hydrated by JS). Tells, per apex1's 6-site survey (2026-07-26 — the
/// original compound-class hints matched 1/6 in the wild):
/// - `role="search"` — the ARIA landmark;
/// - `aria-label` containing the WORD "search" (6/6 of the survey — an
///   accessibility requirement, not a styling choice; word-matched so
///   "Research papers" can't trip it);
/// - a custom-element tag ending `-search` (`<site-search>`, `<sl-doc-search>`);
/// - compound class/id/data hints (`search-bar`, `docsearch` = Algolia
///   DocSearch, …) — deliberately compound so prose can't false-positive.
fn has_search_affordance(doc: &Html) -> bool {
    if let Ok(sel) = Selector::parse(r#"[role="search"]"#) {
        if doc.select(&sel).next().is_some() {
            return true;
        }
    }
    const HINTS: [&str; 7] = [
        "search-bar", "searchbox", "search-box", "search-trigger", "search-form", "search-field",
        "docsearch",
    ];
    doc.root_element().descendants().filter_map(ElementRef::wrap).any(|el| {
        if el.value().name().ends_with("-search") {
            return true;
        }
        if el.value().attr("aria-label").is_some_and(has_search_word) {
            return true;
        }
        ["class", "id", "data-testid"].iter().any(|a| {
            el.value().attr(a).is_some_and(|v| {
                let v = v.to_ascii_lowercase();
                HINTS.iter().any(|h| v.contains(h))
            })
        })
    })
}

/// FNV-1a (64-bit) as lowercase hex — a small, stable content fingerprint.
/// (Also mints the engine's `result:<hash>` interaction handles.)
pub(crate) fn fnv1a_hex(bytes: &[u8]) -> String {
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

    // ---- SPA salvage + honest JS signaling (Phase 14) ---------------------

    #[test]
    fn next_js_empty_shell_yields_salvaged_markdown() {
        let html = r#"<html><head><title>Post</title></head><body><div id="__next"></div>
            <script id="__NEXT_DATA__" type="application/json">
            {"props":{"pageProps":{"post":{"title":"The reading cortex","slug":"reading-cortex",
             "body":"Occipital gives an agent polite eyes on the live web and a decaying memory of what it read."}}}}
            </script></body></html>"#;
        let p = extract(html, "https://blog.test/p/1");
        assert!(p.salvaged, "state blob recovered");
        assert!(!p.js_required);
        assert!(p.markdown.contains("polite eyes on the live web"), "{}", p.markdown);
        assert!(p.markdown.contains("[salvaged:"), "the note names what happened: {}", p.markdown);
    }

    #[test]
    fn client_only_page_reports_js_required_not_silence() {
        let html = r#"<html><head><title>App</title></head><body><div id="root"></div>
            <script src="/static/js/main.js"></script>
            <script src="/static/js/vendor.js"></script></body></html>"#;
        let p = extract(html, "https://app.test/");
        assert!(p.js_required, "the honest flag");
        assert!(!p.salvaged);
        assert!(p.markdown.contains("requires JavaScript"), "{}", p.markdown);
    }

    #[test]
    fn small_static_or_form_pages_are_never_flagged() {
        let p = extract("<body><p>just a paragraph</p></body>", "https://x.test/");
        assert!(!p.salvaged && !p.js_required, "tiny static page is just tiny");
        let p = extract(FORM_PAGE, "https://shop.test/contact");
        assert!(!p.js_required, "a lean form page is interactive, not a JS shell");
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

    #[test]
    fn known_redirector_links_unwrap_to_their_destination() {
        // Same hygiene regardless of which verb found the link — the field
        // observation was DDG /l/?uddg= wrappers surviving the page path
        // while the provider path unwrapped them.
        let html = r#"<main><p>
            <a href="https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fdocs&rut=abc">wrapped</a>
            <a href="https://www.google.com/url?q=https://example.org/page&sa=U">gwrapped</a>
            <a href="https://example.net/plain">plain</a>
            <a href="https://duckduckgo.com/about">ddg but not a redirect</a>
        </p></main>"#;
        let p = extract(html, "https://serp.test/");
        let urls: Vec<&str> = p.links.iter().map(|l| l.url.as_str()).collect();
        assert!(urls.contains(&"https://example.com/docs"), "{urls:?}");
        assert!(urls.contains(&"https://example.org/page"), "{urls:?}");
        assert!(urls.contains(&"https://example.net/plain"), "{urls:?}");
        assert!(
            urls.contains(&"https://duckduckgo.com/about"),
            "non-redirect paths on a redirector host stay untouched: {urls:?}"
        );
    }

    #[test]
    fn formless_search_input_gets_an_honest_note() {
        // A script-driven search box (input outside any <form>) cannot be
        // submitted by the verbs — say so, or its absence from the registry
        // reads as a missing feature (openlibrary.org field report).
        let html = r#"<body><header><input type="search" placeholder="Search"></header>
            <main><p>content here</p></main></body>"#;
        let p = extract(html, "https://x.test/");
        assert!(
            p.markdown.contains("search affordance exists outside any form"),
            "honest note expected: {}",
            p.markdown
        );
        assert!(p.forms.is_empty(), "nothing submittable was invented");

        // The same input INSIDE a form is submittable — no note.
        let html = r#"<body><main><form action="/s"><input type="search" name="q"></form>
            <p>content</p></main></body>"#;
        let p = extract(html, "https://x.test/");
        assert!(
            !p.markdown.contains("outside any form"),
            "in-form search is registry business, not a note: {}",
            p.markdown
        );
    }

    #[test]
    fn div_trigger_search_on_a_formless_page_gets_the_note_too() {
        // The SPA shape apex1's verification round caught: openlibrary.org
        // ships ZERO <input> and ZERO <form> — the search bar is a div
        // hydrated by JS. The input-based trigger can't fire; the affordance
        // sweep must.
        let html = r#"<body><header>
            <div class="search-bar-component"><div class="search-bar-trigger">
                <span class="search-bar-trigger__label">Search</span>
            </div></div></header>
            <main><p>Failed to fetch carousel. Retry?</p></main></body>"#;
        let p = extract(html, "https://openlibrary.test/");
        assert!(p.forms.is_empty());
        assert!(
            p.markdown.contains("search affordance exists outside any form"),
            "div-trigger search must be flagged: {}",
            p.markdown
        );

        // role="search" is the accessible marker for the same thing.
        let html = r#"<body><div role="search"><button>Search</button></div>
            <main><p>content</p></main></body>"#;
        let p = extract(html, "https://x.test/");
        assert!(p.markdown.contains("search affordance"), "{}", p.markdown);

        // A non-search form does NOT silence the note (the gate is "no
        // submittable SEARCH form", not "no forms" — round-3 widening after
        // the openlibrary waitlist / pydantic newsletter blind spots): the
        // JS-only search still isn't submittable just because an unrelated
        // form is.
        let html = r#"<body><div class="search-bar-trigger">Search</div>
            <main><form action="/w" method="post"><input name="e"><button>Join</button></form>
            <p>content</p></main></body>"#;
        let p = extract(html, "https://x.test/");
        assert!(
            p.markdown.contains("search affordance exists"),
            "an unrelated form must not silence the note: {}",
            p.markdown
        );

        // …and prose-ish attribute values ("research") never trip the hints.
        let html = r#"<body><main><div class="research-notes"><p>content here</p></div></main></body>"#;
        let p = extract(html, "https://x.test/");
        assert!(!p.markdown.contains("search affordance"), "{}", p.markdown);
    }

    #[test]
    fn survey_tells_fire_and_research_still_does_not() {
        // apex1's 6-site survey: aria-label="Search" is the universal tell
        // (an accessibility requirement, not a styling choice).
        let html = r#"<body><div aria-label="Search"><button>⌕</button></div>
            <main><p>content</p></main></body>"#;
        assert!(extract(html, "https://x.test/").markdown.contains("search affordance"));

        // Word-matched: "Research papers" can never trip it.
        let html = r#"<body><div aria-label="Research papers">list</div>
            <main><p>content</p></main></body>"#;
        assert!(!extract(html, "https://x.test/").markdown.contains("search affordance"));

        // Algolia DocSearch — the dominant docs-search widget.
        let html = r#"<body><button class="DocSearch-Button">Search</button>
            <main><p>content</p></main></body>"#;
        assert!(extract(html, "https://x.test/").markdown.contains("search affordance"));

        // Custom-element tags: <site-search> (pydantic), <sl-doc-search> (astro).
        let html = r#"<body><site-search></site-search><main><p>content</p></main></body>"#;
        assert!(extract(html, "https://x.test/").markdown.contains("search affordance"));
    }

    #[test]
    fn dead_form_trailer_is_honest_not_inviting() {
        // mkdocs.org shape: a bare GET form whose only field is an unnamed
        // type=search input, no submit button. The old trailer synthesized
        // `submit "Submit"` — prose inviting the exact call web_submit
        // refuses. Prose and registry must agree.
        let html = r#"<main><form action="/"><input type="search" placeholder="Search..."></form>
            <p>content</p></main>"#;
        let p = extract(html, "https://mkdocs.test/");
        assert!(!p.forms[0].submittable);
        assert!(
            p.markdown.contains("not submittable (no named fields)"),
            "the trailer must carry what the registry knows: {}",
            p.markdown
        );
        assert!(
            !p.markdown.contains("submit \""),
            "no synthesized submit label on a dead form: {}",
            p.markdown
        );

        // A live form keeps its submit label exactly as before.
        let html = r#"<main><form action="/s"><input type="text" name="q"><button>Go</button></form>
            <p>content</p></main>"#;
        let p = extract(html, "https://x.test/");
        assert!(p.markdown.contains("submit \"Go\""), "{}", p.markdown);
    }

    #[test]
    fn markdown_alternate_is_reported_when_the_page_advertises_one() {
        // vite.dev shape (had it used the markup): authored markdown offered
        // via <link rel=alternate> — report it, resolved absolute, and never
        // invent one when it's absent.
        let html = r#"<html><head>
            <link rel="alternate" type="text/markdown" href="/guide.md">
            </head><body><main><p>content</p></main></body></html>"#;
        let p = extract(html, "https://vite.test/guide/");
        assert_eq!(
            p.markdown_alternate.as_deref(),
            Some("https://vite.test/guide.md"),
            "relative href resolves against the page"
        );

        let p = extract("<main><p>content</p></main>", "https://x.test/");
        assert!(p.markdown_alternate.is_none(), "never invented");

        // Other alternate types (rss etc.) don't count.
        let html = r#"<html><head>
            <link rel="alternate" type="application/rss+xml" href="/feed.xml">
            </head><body><main><p>content</p></main></body></html>"#;
        let p = extract(html, "https://x.test/");
        assert!(p.markdown_alternate.is_none(), "only text/markdown qualifies");
    }

    #[test]
    fn dead_alternate_hrefs_are_repaired_or_dropped_never_reported() {
        // docs.deno.com shape: `//runtime/index.md` is a network-path
        // reference per RFC 3986 (host `runtime` — DNS-dead); the site meant
        // one slash. Re-read as root-relative: the URL that actually serves.
        let html = r#"<html><head>
            <link rel="alternate" type="text/markdown" href="//runtime/index.md">
            </head><body><main><p>content</p></main></body></html>"#;
        let p = extract(html, "https://docs.deno.test/runtime/");
        assert_eq!(
            p.markdown_alternate.as_deref(),
            Some("https://docs.deno.test/runtime/index.md"),
            "the authoring slip is repaired to the page's own origin"
        );

        // hono.dev shape: a REAL cross-origin alternate (dotted host) is
        // reported verbatim — the repair must not touch legitimate markup.
        let html = r#"<html><head>
            <link rel="alternate" type="text/markdown" href="https://raw.example.net/docs/index.md">
            </head><body><main><p>content</p></main></body></html>"#;
        let p = extract(html, "https://hono.test/docs/");
        assert_eq!(
            p.markdown_alternate.as_deref(),
            Some("https://raw.example.net/docs/index.md"),
            "cross-origin alternates pass untouched"
        );

        // An absolute single-label host has no root-relative reading to
        // repair to — drop it rather than report a dead URL.
        let html = r#"<html><head>
            <link rel="alternate" type="text/markdown" href="https://runtime/index.md">
            </head><body><main><p>content</p></main></body></html>"#;
        let p = extract(html, "https://x.test/");
        assert!(
            p.markdown_alternate.is_none(),
            "a dead affordance is worse than none: {:?}",
            p.markdown_alternate
        );

        // On a single-label-host page (localhost dev), its own host is
        // plausible by definition.
        let html = r#"<html><head>
            <link rel="alternate" type="text/markdown" href="/x.md">
            </head><body><main><p>content</p></main></body></html>"#;
        let p = extract(html, "http://localhost:8000/");
        assert_eq!(
            p.markdown_alternate.as_deref(),
            Some("http://localhost:8000/x.md"),
            "the page's own host is always plausible"
        );
    }

    #[test]
    fn note_gate_is_search_forms_not_any_forms() {
        // pydantic shape: an unrelated newsletter form must not silence the
        // note about the JS-only <site-search>.
        let html = r#"<body><site-search></site-search><main>
            <form action="https://cio.test/sub" method="post"><input type="email" name="email"><button>Subscribe</button></form>
            <p>content</p></main></body>"#;
        let p = extract(html, "https://pydantic.test/");
        assert_eq!(p.forms.len(), 1);
        assert!(
            p.markdown.contains("search affordance exists outside any form"),
            "a newsletter form is not a search form: {}",
            p.markdown
        );

        // react shape: a DEAD search form (unnamed field) must not silence it
        // either — its own registry entry says submittable:false.
        let html = r#"<body><main>
            <form action="https://react.test/"><input type="text" aria-label="Search"></form>
            <p>content</p></main></body>"#;
        let p = extract(html, "https://react.test/");
        assert!(!p.forms[0].submittable);
        assert!(
            p.markdown.contains("search affordance exists outside any form"),
            "a dead trigger form is not a search form: {}",
            p.markdown
        );

        // mkdocs shape: a REAL search form (named `query` field) — registry
        // speaks, no note, even though search-y markup abounds.
        let html = r#"<body><main>
            <form class="md-search__form" action="/search"><input type="text" name="query" aria-label="Search"><button>Go</button></form>
            <p>content</p></main></body>"#;
        let p = extract(html, "https://mkdocs.test/");
        assert!(p.forms[0].submittable);
        assert!(
            !p.markdown.contains("search affordance exists"),
            "a submittable search form silences the note: {}",
            p.markdown
        );
    }
}
