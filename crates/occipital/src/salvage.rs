//! SPA salvage — recovering content from pages that render client-side,
//! **without executing any JavaScript** (Phase 14, docs/agent-browsing.md).
//!
//! The insight: content that wants to be found either server-renders or embeds
//! its state as JSON in the HTML. When reader-mode extraction comes back
//! suspiciously thin, this module mines what the static document *does* carry:
//!
//! - `application/ld+json` blocks — articles/products structured for crawlers
//! - framework state blobs — `__NEXT_DATA__`, `__NUXT_DATA__`, and
//!   `window.__INITIAL_STATE__ = {…}`-style assignments whose right-hand side
//!   is valid JSON (JS object literals that aren't JSON are skipped, honestly)
//! - `<noscript>` fallbacks, meta descriptions, RSS/Atom feed links
//!
//! State blobs have arbitrary schemas, so harvesting is heuristic: walk the
//! JSON and collect strings that *look like prose* (plus values under
//! content-ish keys), bounded so one giant blob can't flood the page. When even
//! salvage finds nothing on a scripts-heavy page, the caller flags
//! `js_required` — the agent learns *why* the page is thin instead of
//! concluding it's blank.

use std::collections::HashSet;

use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use crate::extract::Link;

/// Markdown below this many content characters counts as a thin extraction
/// (form-annotation lines excluded — a bare login page is thin but honest).
const THIN_BODY_CHARS: usize = 200;

/// Harvest bounds: one page state blob can be megabytes of JSON.
const MAX_HARVEST_STRINGS: usize = 30;
const MAX_HARVEST_CHARS: usize = 6_000;
const MAX_SECTION_CHARS: usize = 4_000;

/// What a salvage pass recovered.
pub(crate) struct Salvage {
    pub markdown: String,
    /// Discovered RSS/Atom feeds (appended to the page's link list).
    pub links:    Vec<Link>,
}

/// Whether extracted markdown is thin enough to try salvage.
pub(crate) fn is_thin(markdown: &str) -> bool {
    let content_chars: usize = markdown
        .lines()
        .filter(|l| !l.trim_start().starts_with("[form#"))
        .map(|l| l.chars().count())
        .sum();
    content_chars < THIN_BODY_CHARS
}

/// Whether the document leans on scripts enough that a thin body plausibly
/// means client-side rendering (≥ 2 external scripts, or a big inline one).
pub(crate) fn scripts_heavy(doc: &Html) -> bool {
    let Ok(sel) = Selector::parse("script") else { return false };
    let mut external = 0usize;
    for s in doc.select(&sel) {
        if s.value().attr("src").is_some() {
            external += 1;
            if external >= 2 {
                return true;
            }
        } else if s.text().collect::<String>().len() > 1_000 {
            return true;
        }
    }
    false
}

/// Mine the document for recoverable content. `None` when nothing meaningful
/// was found (the honest outcome — never fabricate a page).
pub(crate) fn salvage(doc: &Html, base: Option<&Url>) -> Option<Salvage> {
    let mut sections: Vec<String> = Vec::new();

    let ld = ld_json_sections(doc);
    sections.extend(ld);

    let harvested = state_blob_prose(doc);
    if !harvested.is_empty() {
        sections.push(harvested.join("\n\n"));
    }

    let noscript = noscript_text(doc);
    if !noscript.is_empty() {
        sections.push(noscript);
    }

    // A meta description alone is a poor page, but better than silence — and a
    // good lead paragraph when real sections were found.
    let desc = crate::extract::meta_content(doc, "meta[property='og:description']")
        .or_else(|| crate::extract::meta_content(doc, "meta[name='description']"));

    let links = feed_links(doc, base);

    let mut md = String::new();
    if let Some(d) = &desc {
        md.push_str(d);
    }
    for s in &sections {
        if !md.is_empty() {
            md.push_str("\n\n");
        }
        md.push_str(s);
    }
    let md = md.trim().to_string();

    // A description-only salvage still counts (it names what the page is);
    // an empty one does not.
    if md.chars().count() < 40 && links.is_empty() {
        return None;
    }
    if md.is_empty() {
        return None;
    }
    Some(Salvage { markdown: md, links })
}

// ---- ld+json ---------------------------------------------------------------

/// Sections from `application/ld+json`: any entity carrying a headline/name +
/// description/articleBody renders as `## title` + paragraphs.
fn ld_json_sections(doc: &Html) -> Vec<String> {
    let Ok(sel) = Selector::parse("script[type='application/ld+json']") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for script in doc.select(&sel) {
        let text = script.text().collect::<String>();
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        for entity in flatten_entities(&v) {
            if let Some(section) = entity_section(entity) {
                out.push(section);
            }
        }
    }
    out
}

/// ld+json can be a single object, an array, or an `@graph` wrapper.
fn flatten_entities(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) => a.iter().flat_map(flatten_entities).collect(),
        Value::Object(o) => match o.get("@graph") {
            Some(g) => flatten_entities(g),
            None => vec![v],
        },
        _ => Vec::new(),
    }
}

fn entity_section(v: &Value) -> Option<String> {
    let get = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let title = get("headline").or_else(|| get("name"));
    let desc = get("description");
    let body = get("articleBody").or_else(|| get("text"));
    if title.is_none() && desc.is_none() && body.is_none() {
        return None;
    }
    let mut s = String::new();
    if let Some(t) = title {
        s.push_str("## ");
        s.push_str(t);
    }
    if let Some(d) = desc {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(d);
    }
    if let Some(b) = body {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        let b = strip_markup(b);
        s.push_str(truncate_chars(&b, MAX_SECTION_CHARS));
    }
    Some(s)
}

// ---- framework state blobs -------------------------------------------------

/// Prose harvested from every parseable state blob in the document.
fn state_blob_prose(doc: &Html) -> Vec<String> {
    let Ok(sel) = Selector::parse("script") else { return Vec::new() };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut budget = MAX_HARVEST_CHARS;

    for script in doc.select(&sel) {
        if out.len() >= MAX_HARVEST_STRINGS || budget == 0 {
            break;
        }
        let el = script.value();
        let text = script.text().collect::<String>();
        let ty = el.attr("type").unwrap_or("").trim();
        let id = el.attr("id").unwrap_or("");

        let json: Option<Value> = if id == "__NEXT_DATA__"
            || (ty.eq_ignore_ascii_case("application/json") && !id.is_empty())
        {
            // Next.js, Nuxt 3 (`__NUXT_DATA__`), and friends: the whole script
            // body is JSON.
            serde_json::from_str(&text).ok()
        } else if ty.is_empty() || ty.contains("javascript") {
            // `window.__INITIAL_STATE__ = {…};` — try the RHS as JSON.
            assignment_json(&text)
        } else {
            None
        };

        if let Some(v) = json {
            harvest(&v, &mut seen, &mut out, &mut budget, 0);
        }
    }
    out
}

/// The state-blob assignment pattern: take everything after the first `=` of a
/// `window.__…__ =` line, trim a trailing `;`, and require valid JSON. JS
/// object literals that aren't JSON (functions, unquoted keys) fail the parse
/// and are skipped — no script is ever *executed*.
fn assignment_json(script: &str) -> Option<Value> {
    const MARKERS: &[&str] = &["window.__", "__INITIAL_STATE__", "__PRELOADED_STATE__", "__NUXT__"];
    if !MARKERS.iter().any(|m| script.contains(m)) {
        return None;
    }
    let rhs = script.split_once('=')?.1.trim();
    let rhs = rhs.strip_suffix(';').unwrap_or(rhs).trim();
    serde_json::from_str(rhs).ok()
}

const CONTENT_KEYS: &[&str] = &[
    "title", "headline", "name", "description", "summary", "articlebody", "body", "text",
    "content", "excerpt", "caption", "subtitle",
];

const MAX_HARVEST_DEPTH: usize = 24;

/// Walk a JSON value collecting prose: values under content-ish keys, plus any
/// string that reads like a sentence. Bounded and deduped.
fn harvest(v: &Value, seen: &mut HashSet<String>, out: &mut Vec<String>, budget: &mut usize, depth: usize) {
    if out.len() >= MAX_HARVEST_STRINGS || *budget == 0 || depth > MAX_HARVEST_DEPTH {
        return;
    }
    match v {
        Value::Object(o) => {
            for (k, val) in o {
                if let Value::String(s) = val {
                    let key_hit = CONTENT_KEYS.contains(&k.to_ascii_lowercase().as_str());
                    take_if_prose(s, key_hit, seen, out, budget);
                } else {
                    harvest(val, seen, out, budget, depth + 1);
                }
            }
        }
        Value::Array(a) => {
            for val in a {
                if let Value::String(s) = val {
                    take_if_prose(s, false, seen, out, budget);
                } else {
                    harvest(val, seen, out, budget, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn take_if_prose(s: &str, key_hit: bool, seen: &mut HashSet<String>, out: &mut Vec<String>, budget: &mut usize) {
    if out.len() >= MAX_HARVEST_STRINGS || *budget == 0 {
        return;
    }
    let t = s.trim();
    let min_len = if key_hit { 15 } else { 60 };
    if !looks_like_prose(t, min_len) {
        return;
    }
    let clean = strip_markup(t);
    let clean = truncate_chars(&clean, *budget).to_string();
    if clean.is_empty() || !seen.insert(clean.clone()) {
        return;
    }
    *budget = budget.saturating_sub(clean.chars().count());
    out.push(clean);
}

/// A cheap "is this human text" filter: long enough, has real words, isn't a
/// URL/slug/serialized fragment.
fn looks_like_prose(s: &str, min_len: usize) -> bool {
    if s.chars().count() < min_len {
        return false;
    }
    if s.starts_with("http://") || s.starts_with("https://") || s.starts_with('/') {
        return false;
    }
    if s.starts_with('{') || s.starts_with('[') || s.contains("\":") {
        return false;
    }
    let words = s.split_whitespace().count();
    if words < min_len / 10 {
        return false; // slugs and identifiers have few spaces
    }
    let alpha = s.chars().filter(|c| c.is_alphabetic() || c.is_whitespace()).count();
    alpha * 10 >= s.chars().count() * 6 // ≥60% letters+spaces
}

// ---- noscript / feeds ------------------------------------------------------

/// Text of `<noscript>` fallbacks. html5ever parses noscript children as raw
/// text (scripting on), so the markup arrives as a string — strip it.
fn noscript_text(doc: &Html) -> String {
    let Ok(sel) = Selector::parse("noscript") else { return String::new() };
    let mut out = String::new();
    for n in doc.select(&sel) {
        let t = collapse_ws(&strip_markup(&n.text().collect::<String>()));
        // Skip boilerplate like "You need to enable JavaScript to run this app."
        if t.chars().count() >= 40 && !t.to_ascii_lowercase().contains("enable javascript") {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&t);
        }
    }
    out
}

/// RSS/Atom feeds advertised via `<link rel=alternate>` — a JS-only page often
/// still has a perfectly readable feed.
fn feed_links(doc: &Html, base: Option<&Url>) -> Vec<Link> {
    let Ok(sel) = Selector::parse("link[rel='alternate']") else { return Vec::new() };
    let mut out = Vec::new();
    for l in doc.select(&sel) {
        let ty = l.value().attr("type").unwrap_or("").to_ascii_lowercase();
        if !(ty.contains("rss") || ty.contains("atom")) {
            continue;
        }
        let Some(url) = l.value().attr("href").and_then(|h| crate::extract::resolve(base, h)) else {
            continue;
        };
        let text = l.value().attr("title").unwrap_or("feed").trim().to_string();
        out.push(Link { text, url });
    }
    out
}

// ---- small helpers ---------------------------------------------------------

/// Strip HTML tags out of a string (state blobs often carry HTML fragments) —
/// parse as a fragment and keep the text.
fn strip_markup(s: &str) -> String {
    if !s.contains('<') {
        return s.to_string();
    }
    let frag = Html::parse_fragment(s);
    collapse_ws(&frag.root_element().text().collect::<String>())
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> Html {
        Html::parse_document(html)
    }

    #[test]
    fn thin_ignores_form_annotations() {
        assert!(is_thin("short"));
        assert!(is_thin("[form#1 → GET /search — text \"q\" · submit \"Go\"]\nshort"));
        let long = "long enough content ".repeat(20);
        assert!(!is_thin(&long));
    }

    #[test]
    fn scripts_heavy_needs_real_script_weight() {
        assert!(!scripts_heavy(&doc("<body><p>static</p></body>")));
        assert!(!scripts_heavy(&doc("<body><script>var a=1;</script></body>")));
        assert!(scripts_heavy(&doc(
            "<body><script src='/a.js'></script><script src='/b.js'></script></body>"
        )));
        let big = format!("<body><script>{}</script></body>", "x".repeat(1100));
        assert!(scripts_heavy(&doc(&big)));
    }

    #[test]
    fn ld_json_article_becomes_a_section() {
        let html = r#"<head><script type="application/ld+json">
            {"@context":"https://schema.org","@type":"NewsArticle",
             "headline":"Rust ships new release",
             "description":"The release brings faster builds.",
             "articleBody":"<p>The Rust team announced the release today.</p>"}
        </script></head><body></body>"#;
        let s = salvage(&doc(html), None).expect("salvaged");
        assert!(s.markdown.contains("## Rust ships new release"));
        assert!(s.markdown.contains("faster builds"));
        assert!(s.markdown.contains("announced the release today"), "HTML stripped: {}", s.markdown);
        assert!(!s.markdown.contains("<p>"), "no raw tags: {}", s.markdown);
    }

    #[test]
    fn next_data_prose_is_harvested_and_ids_are_not() {
        let html = r#"<body><div id="__next"></div>
            <script id="__NEXT_DATA__" type="application/json">
            {"props":{"pageProps":{"post":{
                "title":"Understanding polite crawlers",
                "slug":"understanding-polite-crawlers",
                "body":"A polite crawler identifies itself honestly and paces its requests so the origin never feels it.",
                "authorId":"a1b2c3"}}},"page":"/p/[slug]"}
            </script></body>"#;
        let s = salvage(&doc(html), None).expect("salvaged");
        assert!(s.markdown.contains("Understanding polite crawlers"), "{}", s.markdown);
        assert!(s.markdown.contains("identifies itself honestly"));
        assert!(!s.markdown.contains("a1b2c3"), "ids are not prose");
        assert!(!s.markdown.contains("/p/[slug]"), "route templates are not prose");
    }

    #[test]
    fn window_state_assignment_parses_only_strict_json() {
        let ok = r#"<body><script>window.__INITIAL_STATE__ = {"article":{"title":"Salvage works fine","content":"State blobs embedded as JSON are readable without running any script at all."}};</script></body>"#;
        let s = salvage(&doc(ok), None).expect("salvaged");
        assert!(s.markdown.contains("readable without running any script"));

        let js_only = r#"<body><script>window.__NUXT__=(function(a){return {data:a}}("x"));</script></body>"#;
        assert!(salvage(&doc(js_only), None).is_none(), "JS literals are skipped, not executed");
    }

    #[test]
    fn noscript_and_feeds_are_recovered() {
        let html = r#"<head>
            <link rel="alternate" type="application/rss+xml" title="Site feed" href="/feed.xml">
          </head><body>
            <noscript>You need to enable JavaScript to run this app.</noscript>
            <noscript><p>Our full archive remains <b>available</b> through the monthly index pages and the RSS feed.</p></noscript>
          </body>"#;
        let base = Url::parse("https://site.test/").unwrap();
        let s = salvage(&doc(html), Some(&base)).expect("salvaged");
        assert!(s.markdown.contains("full archive remains available"));
        assert!(!s.markdown.contains("enable JavaScript"), "boilerplate skipped");
        assert_eq!(s.links[0].url, "https://site.test/feed.xml");
        assert_eq!(s.links[0].text, "Site feed");
    }

    #[test]
    fn empty_shell_with_nothing_recoverable_salvages_none() {
        let html = r#"<body><div id="root"></div><script src="/app.js"></script></body>"#;
        assert!(salvage(&doc(html), None).is_none());
    }

    #[test]
    fn harvest_is_bounded() {
        // 500 long prose strings → capped at MAX_HARVEST_STRINGS / chars.
        let items: Vec<String> = (0..500)
            .map(|i| format!("\"This is a perfectly reasonable sentence number {i} with enough words to count as prose.\""))
            .collect();
        let html = format!(
            r#"<body><script id="__NEXT_DATA__" type="application/json">{{"props":[{}]}}</script></body>"#,
            items.join(",")
        );
        let s = salvage(&doc(&html), None).expect("salvaged");
        assert!(s.markdown.chars().count() <= MAX_HARVEST_CHARS + MAX_HARVEST_STRINGS * 2);
    }
}
