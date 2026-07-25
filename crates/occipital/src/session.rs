//! Sessions & identity (Phase 15) — an **opt-in** persistent cookie jar and
//! per-domain request headers.
//!
//! Why hand-rolled: `reqwest::cookie::Jar` cannot be enumerated or serialized,
//! and a session that evaporates on restart is useless to a long-running agent.
//! This jar implements the same `CookieStore` seam with RFC-6265-shaped
//! matching (domain / path / secure / expiry), so reqwest drives it
//! automatically, plus the CRUD the CLI needs.
//!
//! The boundary that keeps this polite (docs/politeness.md): one jar, one
//! identity. Sessions exist so multi-step flows and *operator-provisioned*
//! logins work — never to farm or rotate identities, and the honest UA is not
//! overridable per-domain. Cookies are **off by default** (`OCCIPITAL_COOKIES`);
//! with the feature off nothing is stored, sent, or written.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, TimeZone, Utc};
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use url::Url;

/// Headers a node operator may never set per-domain: the UA is the honest
/// identity (a locked decision), cookies belong to the jar, and the rest are
/// computed by the HTTP layer itself.
const FORBIDDEN_HEADERS: &[&str] = &["user-agent", "cookie", "host", "content-length"];

/// One stored cookie. `host_only` marks a cookie set without a `Domain`
/// attribute — it must match its origin host exactly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cookie {
    pub name:      String,
    pub value:     String,
    /// Lowercased, no leading dot.
    pub domain:    String,
    pub path:      String,
    #[serde(default)]
    pub secure:    bool,
    #[serde(default)]
    pub host_only: bool,
    /// Unix seconds; `None` = a session cookie (memory only, never persisted).
    #[serde(default)]
    pub expires:   Option<i64>,
}

impl Cookie {
    /// The expiry as RFC3339, or `None` for a session cookie — display helper
    /// so consumers need no time crate of their own.
    pub fn expires_rfc3339(&self) -> Option<String> {
        let ts = self.expires?;
        Some(
            DateTime::from_timestamp(ts, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| ts.to_string()),
        )
    }

    fn expired_at(&self, now: i64) -> bool {
        matches!(self.expires, Some(e) if e <= now)
    }

    /// RFC 6265 domain-match.
    fn domain_matches(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        if self.host_only {
            return host == self.domain;
        }
        host == self.domain || host.ends_with(&format!(".{}", self.domain))
    }

    /// RFC 6265 path-match.
    fn path_matches(&self, path: &str) -> bool {
        if path == self.path {
            return true;
        }
        if !path.starts_with(&self.path) {
            return false;
        }
        self.path.ends_with('/') || path.as_bytes().get(self.path.len()) == Some(&b'/')
    }
}

/// A persistent cookie jar. Session cookies (no expiry) live in memory only —
/// browser semantics; expiring cookies are written `0600` so a login survives
/// a restart.
pub struct CookieJar {
    file:    PathBuf,
    /// Whether changes are written to disk (false for an ephemeral jar).
    persist: bool,
    cookies: Mutex<Vec<Cookie>>,
}

impl CookieJar {
    /// Load the jar from `file` (absent/unreadable → empty, never errors).
    /// Expired entries are dropped on load.
    pub fn load(file: &Path, persist: bool) -> Self {
        let stored: Vec<Cookie> = std::fs::read_to_string(file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let now = Utc::now().timestamp();
        let cookies = stored.into_iter().filter(|c| !c.expired_at(now)).collect();
        Self { file: file.to_path_buf(), persist, cookies: Mutex::new(cookies) }
    }

    /// Stored cookies, newest-domain-first ordering irrelevant — a stable
    /// (domain, name) sort for display.
    pub fn list(&self) -> Vec<Cookie> {
        let mut v = self.cookies.lock().unwrap().clone();
        v.sort_by(|a, b| (&a.domain, &a.name).cmp(&(&b.domain, &b.name)));
        v
    }

    /// Drop cookies for one domain (suffix-aware) or, with `None`, all of them.
    /// Returns how many were removed.
    pub fn clear(&self, domain: Option<&str>) -> usize {
        let mut c = self.cookies.lock().unwrap();
        let before = c.len();
        match domain {
            None => c.clear(),
            Some(d) => {
                let d = d.trim_start_matches('.').to_ascii_lowercase();
                c.retain(|k| !(k.domain == d || k.domain.ends_with(&format!(".{d}"))));
            }
        }
        let removed = before - c.len();
        drop(c);
        if removed > 0 {
            self.flush();
        }
        removed
    }

    /// Insert/replace by the RFC key (name, domain, path).
    fn store(&self, cookie: Cookie) {
        let mut c = self.cookies.lock().unwrap();
        c.retain(|k| !(k.name == cookie.name && k.domain == cookie.domain && k.path == cookie.path));
        // A `Max-Age=0` / past-expiry Set-Cookie is a deletion.
        if !cookie.expired_at(Utc::now().timestamp()) {
            c.push(cookie);
        }
        drop(c);
        self.flush();
    }

    /// Persist the expiring cookies as `0600` JSON (best-effort — a jar that
    /// cannot be written still works for the session).
    fn flush(&self) {
        if !self.persist {
            return;
        }
        let persistable: Vec<Cookie> = self
            .cookies
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.expires.is_some())
            .cloned()
            .collect();
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        match serde_json::to_string_pretty(&persistable) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.file, json) {
                    tracing::warn!("cookie jar not written ({}): {e}", self.file.display());
                    return;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600)).ok();
                }
            }
            Err(e) => tracing::warn!("cookie jar not serialized: {e}"),
        }
    }

    /// The `Cookie` header value for a request URL (`None` when nothing matches).
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let host = url.host_str()?.to_ascii_lowercase();
        let path = if url.path().is_empty() { "/" } else { url.path() };
        let secure = url.scheme() == "https";
        let now = Utc::now().timestamp();

        let c = self.cookies.lock().unwrap();
        let mut matched: Vec<&Cookie> = c
            .iter()
            .filter(|k| !k.expired_at(now))
            .filter(|k| k.domain_matches(&host) && k.path_matches(path))
            .filter(|k| !k.secure || secure)
            .collect();
        if matched.is_empty() {
            return None;
        }
        // Longest path first (RFC 6265 §5.4).
        matched.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        Some(
            matched
                .iter()
                .map(|k| format!("{}={}", k.name, k.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Apply one `Set-Cookie` header from `url`.
    pub fn set_from_header(&self, header: &str, url: &Url) {
        if let Some(c) = parse_set_cookie(header, url) {
            self.store(c);
        }
    }
}

impl reqwest::cookie::CookieStore for CookieJar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        for h in headers {
            if let Ok(s) = h.to_str() {
                self.set_from_header(s, url);
            }
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        HeaderValue::from_str(&self.header_for(url)?).ok()
    }
}

/// Parse a `Set-Cookie` value against the request URL. Returns `None` for a
/// malformed header or a `Domain` the origin may not set (the cross-site
/// guard).
fn parse_set_cookie(header: &str, url: &Url) -> Option<Cookie> {
    let host = url.host_str()?.to_ascii_lowercase();
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let mut cookie = Cookie {
        name:      name.to_string(),
        value:     value.trim().to_string(),
        domain:    host.clone(),
        path:      default_path(url.path()),
        secure:    false,
        host_only: true,
        expires:   None,
    };
    let mut max_age: Option<i64> = None;

    for attr in parts {
        let (k, v) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => (attr.trim().to_ascii_lowercase(), String::new()),
        };
        match k.as_str() {
            "domain" if !v.is_empty() => {
                let d = v.trim_start_matches('.').to_ascii_lowercase();
                // A site may only widen to its own registrable-ish suffix: the
                // request host must be inside the claimed domain.
                if host == d || host.ends_with(&format!(".{d}")) {
                    cookie.domain = d;
                    cookie.host_only = false;
                } else {
                    return None; // cross-site Set-Cookie — refuse
                }
            }
            "path" if v.starts_with('/') => cookie.path = v,
            "secure" => cookie.secure = true,
            "max-age" => max_age = v.parse::<i64>().ok(),
            "expires" if cookie.expires.is_none() => {
                cookie.expires = parse_http_date(&v).map(|d| d.timestamp())
            }
            _ => {}
        }
    }
    // Max-Age wins over Expires (RFC 6265 §5.3).
    if let Some(ma) = max_age {
        cookie.expires = Some(Utc::now().timestamp() + ma);
    }
    Some(cookie)
}

/// RFC 6265 default-path: everything up to the last `/`.
fn default_path(path: &str) -> String {
    if !path.starts_with('/') {
        return "/".into();
    }
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].to_string(),
    }
}

/// `Expires` accepts the IMF-fixdate form plus the RFC-2822/1036 variants seen
/// in the wild.
fn parse_http_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    for fmt in ["%a, %d %b %Y %H:%M:%S GMT", "%A, %d-%b-%y %H:%M:%S GMT", "%a %b %e %H:%M:%S %Y"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Utc.from_local_datetime(&naive).single();
        }
    }
    DateTime::parse_from_rfc2822(s).ok().map(|d| d.with_timezone(&Utc))
}

// ---- per-domain headers ----------------------------------------------------

/// Operator-configured extra request headers, keyed by host. `"*"` applies
/// everywhere; a leading-dot key (`".example.com"`) covers the domain and its
/// subdomains; anything else is an exact host match. More specific wins.
///
/// The honest user-agent is never overridable (see [`FORBIDDEN_HEADERS`]).
#[derive(Debug, Default)]
pub struct HeaderRules {
    rules: BTreeMap<String, BTreeMap<String, String>>,
}

impl HeaderRules {
    /// Load from a JSON file: `{ "example.com": { "accept-language": "en" } }`.
    /// Absent/unreadable → no rules (never errors). Forbidden header names are
    /// dropped with a warning.
    pub fn load(file: Option<&Path>) -> Self {
        let Some(file) = file else { return Self::default() };
        let Some(text) = std::fs::read_to_string(file).ok() else { return Self::default() };
        let parsed: BTreeMap<String, BTreeMap<String, String>> =
            match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("header rules not parsed ({}): {e}", file.display());
                    return Self::default();
                }
            };
        let mut rules = BTreeMap::new();
        for (host, headers) in parsed {
            let kept: BTreeMap<String, String> = headers
                .into_iter()
                .filter(|(k, _)| {
                    let lower = k.to_ascii_lowercase();
                    if FORBIDDEN_HEADERS.contains(&lower.as_str()) {
                        tracing::warn!("ignoring {lower:?} in header rules — not overridable");
                        return false;
                    }
                    true
                })
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect();
            if !kept.is_empty() {
                rules.insert(host.to_ascii_lowercase(), kept);
            }
        }
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Headers to add for `host`, least-specific first (so a caller applying
    /// them in order lets the exact-host rule win).
    pub fn for_host(&self, host: &str) -> Vec<(String, String)> {
        let host = host.to_ascii_lowercase();
        let mut out: Vec<(String, String)> = Vec::new();
        let mut push_all = |m: &BTreeMap<String, String>| {
            for (k, v) in m {
                out.retain(|(ek, _)| ek != k);
                out.push((k.clone(), v.clone()));
            }
        };
        if let Some(m) = self.rules.get("*") {
            push_all(m);
        }
        for (key, m) in &self.rules {
            if let Some(suffix) = key.strip_prefix('.') {
                if host == suffix || host.ends_with(&format!(".{suffix}")) {
                    push_all(m);
                }
            }
        }
        if let Some(m) = self.rules.get(&host) {
            push_all(m);
        }
        out
    }
}

/// Merge rule headers with per-request ones — the explicit request headers win
/// on a name collision (conditional-GET validators, provider API keys).
pub(crate) fn merge_headers(
    rules: Vec<(String, String)>,
    explicit: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut out = rules;
    for (k, v) in explicit {
        let lower = k.to_ascii_lowercase();
        out.retain(|(ek, _)| ek.to_ascii_lowercase() != lower);
        out.push((k, v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn jar() -> CookieJar {
        CookieJar { file: PathBuf::from("/nonexistent"), persist: false, cookies: Mutex::new(Vec::new()) }
    }

    #[test]
    fn default_path_strips_the_last_segment() {
        assert_eq!(default_path("/a/b/c"), "/a/b");
        assert_eq!(default_path("/a"), "/");
        assert_eq!(default_path("/"), "/");
        assert_eq!(default_path(""), "/");
    }

    #[test]
    fn set_and_send_roundtrip_with_path_and_secure_rules() {
        let j = jar();
        j.set_from_header("sid=abc; Path=/; Max-Age=3600", &url("https://site.test/login"));
        j.set_from_header("deep=1; Path=/admin; Max-Age=3600", &url("https://site.test/admin/x"));
        j.set_from_header("tls=1; Path=/; Secure; Max-Age=3600", &url("https://site.test/"));

        let h = j.header_for(&url("https://site.test/admin/page")).unwrap();
        assert!(h.contains("deep=1") && h.contains("sid=abc") && h.contains("tls=1"));
        assert!(h.starts_with("deep=1"), "longest path first: {h}");

        let h = j.header_for(&url("https://site.test/other")).unwrap();
        assert!(!h.contains("deep=1"), "path-scoped cookie withheld: {h}");

        let h = j.header_for(&url("http://site.test/")).unwrap();
        assert!(!h.contains("tls=1"), "Secure cookie never leaves over http: {h}");
    }

    #[test]
    fn host_only_and_domain_cookies_scope_correctly() {
        let j = jar();
        j.set_from_header("a=1; Max-Age=60", &url("https://www.site.test/"));
        j.set_from_header("b=2; Domain=site.test; Max-Age=60", &url("https://www.site.test/"));

        let www = j.header_for(&url("https://www.site.test/")).unwrap();
        assert!(www.contains("a=1") && www.contains("b=2"));

        let api = j.header_for(&url("https://api.site.test/")).unwrap();
        assert!(!api.contains("a=1"), "host-only stays on its host: {api}");
        assert!(api.contains("b=2"), "domain cookie spans subdomains: {api}");
    }

    #[test]
    fn cross_site_domain_attribute_is_refused() {
        let j = jar();
        j.set_from_header("evil=1; Domain=other.test; Max-Age=60", &url("https://site.test/"));
        assert!(j.header_for(&url("https://other.test/")).is_none());
        assert!(j.list().is_empty(), "refused outright");
    }

    #[test]
    fn expiry_deletion_and_session_cookies() {
        let j = jar();
        j.set_from_header("gone=1; Max-Age=3600", &url("https://site.test/"));
        assert_eq!(j.list().len(), 1);
        j.set_from_header("gone=1; Max-Age=0", &url("https://site.test/"));
        assert!(j.list().is_empty(), "Max-Age=0 deletes");

        j.set_from_header("old=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT", &url("https://site.test/"));
        assert!(j.list().is_empty(), "a past Expires is a deletion");

        j.set_from_header("sess=1", &url("https://site.test/"));
        assert_eq!(j.list()[0].expires, None, "no expiry → session cookie");
    }

    #[test]
    fn persistence_keeps_expiring_cookies_and_drops_session_ones() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("cookies.json");
        {
            let j = CookieJar::load(&file, true);
            j.set_from_header("keep=1; Max-Age=3600", &url("https://site.test/"));
            j.set_from_header("temp=1", &url("https://site.test/"));
        }
        let j = CookieJar::load(&file, true);
        let names: Vec<String> = j.list().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["keep"], "expiring cookie survives a restart, session one does not");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "jar is private");
        }

        assert_eq!(j.clear(Some("site.test")), 1);
        assert!(CookieJar::load(&file, true).list().is_empty(), "clear persists");
    }

    #[test]
    fn header_rules_match_wildcard_suffix_and_exact() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("headers.json");
        std::fs::write(
            &file,
            r#"{
                "*": {"Accept-Language": "en"},
                ".example.com": {"X-Tier": "suffix"},
                "api.example.com": {"X-Tier": "exact", "X-Extra": "1"},
                "spoof.test": {"User-Agent": "NotOccipital/9", "X-Ok": "yes"}
            }"#,
        )
        .unwrap();
        let r = HeaderRules::load(Some(&file));

        let other = r.for_host("other.test");
        assert_eq!(other, vec![("accept-language".to_string(), "en".to_string())]);

        let api = r.for_host("api.example.com");
        let tier = api.iter().find(|(k, _)| k == "x-tier").unwrap();
        assert_eq!(tier.1, "exact", "exact host beats the suffix rule");
        assert!(api.iter().any(|(k, _)| k == "x-extra"));
        assert!(api.iter().any(|(k, _)| k == "accept-language"), "wildcard still applies");

        assert_eq!(r.for_host("www.example.com").iter().find(|(k, _)| k == "x-tier").unwrap().1, "suffix");

        let spoof = r.for_host("spoof.test");
        assert!(!spoof.iter().any(|(k, _)| k == "user-agent"), "the honest UA is not overridable");
        assert!(spoof.iter().any(|(k, _)| k == "x-ok"), "the rest of the rule survives");
    }

    #[test]
    fn missing_header_file_is_no_rules() {
        assert!(HeaderRules::load(None).is_empty());
        assert!(HeaderRules::load(Some(Path::new("/nonexistent/headers.json"))).is_empty());
    }

    #[test]
    fn explicit_request_headers_beat_rule_headers() {
        let merged = merge_headers(
            vec![("accept-language".into(), "en".into()), ("if-none-match".into(), "rule".into())],
            vec![("if-none-match".into(), "\"real-etag\"".into())],
        );
        let etag = merged.iter().find(|(k, _)| k.eq_ignore_ascii_case("if-none-match")).unwrap();
        assert_eq!(etag.1, "\"real-etag\"");
        assert_eq!(merged.len(), 2, "no duplicate header names");
    }
}
