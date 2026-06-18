//! A small, correct-for-the-common-case robots.txt parser.
//!
//! Implements the parts that matter for a polite reader: User-agent grouping
//! (most-specific group wins, else `*`), Allow/Disallow with **longest-match
//! wins** (Allow breaks ties), and `*` / `$` path wildcards. Crawl-delay is
//! parsed and exposed (advisory — the fetcher may raise a domain's interval to
//! honor it). A missing/empty robots.txt means *allow all*.
//!
//! Not (yet) modelled: sitemap directives, host directives, per-rule
//! crawl-delay nuance. Those are refinements, not correctness gaps for fetching.

#[derive(Debug)]
struct Rule {
    allow:   bool,
    pattern: String,
}

#[derive(Debug)]
pub struct Robots {
    rules:           Vec<Rule>,
    pub crawl_delay: Option<f64>,
}

impl Robots {
    /// Allow-all (used for a missing/unreadable robots.txt).
    pub fn allow_all() -> Self {
        Self { rules: Vec::new(), crawl_delay: None }
    }

    /// Parse `txt`, selecting the rule group most specific to `ua_token` (a
    /// lowercased product token like `"occipital"`), falling back to the `*`
    /// group.
    pub fn parse(txt: &str, ua_token: &str) -> Self {
        // (agents, rules, crawl_delay) per group.
        let mut groups: Vec<(Vec<String>, Vec<Rule>, Option<f64>)> = Vec::new();
        let mut agents: Vec<String> = Vec::new();
        let mut rules: Vec<Rule> = Vec::new();
        let mut crawl_delay: Option<f64> = None;
        let mut last_was_rule = false;

        let flush = |agents: &mut Vec<String>, rules: &mut Vec<Rule>, cd: &mut Option<f64>,
                     groups: &mut Vec<(Vec<String>, Vec<Rule>, Option<f64>)>| {
            if !agents.is_empty() || !rules.is_empty() {
                groups.push((std::mem::take(agents), std::mem::take(rules), cd.take()));
            }
        };

        for raw in txt.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((field, value)) = line.split_once(':') else { continue };
            let field = field.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            match field.as_str() {
                "user-agent" => {
                    if last_was_rule {
                        flush(&mut agents, &mut rules, &mut crawl_delay, &mut groups);
                    }
                    agents.push(value.to_ascii_lowercase());
                    last_was_rule = false;
                }
                "disallow" => {
                    if !value.is_empty() {
                        rules.push(Rule { allow: false, pattern: value });
                    }
                    last_was_rule = true;
                }
                "allow" => {
                    if !value.is_empty() {
                        rules.push(Rule { allow: true, pattern: value });
                    }
                    last_was_rule = true;
                }
                "crawl-delay" => {
                    crawl_delay = value.parse().ok();
                    last_was_rule = true;
                }
                _ => {}
            }
        }
        flush(&mut agents, &mut rules, &mut crawl_delay, &mut groups);

        // Pick the most specific group whose agent token applies to us, else `*`.
        let mut best: Option<usize> = None;
        let mut best_len = 0usize;
        let mut star: Option<usize> = None;
        for (i, (group_agents, _, _)) in groups.iter().enumerate() {
            for a in group_agents {
                if a == "*" {
                    star = Some(i);
                } else if ua_token.contains(a.as_str()) && a.len() > best_len {
                    best = Some(i);
                    best_len = a.len();
                }
            }
        }
        let chosen = best.or(star);
        match chosen {
            Some(i) => {
                let (_, rules, cd) = groups.swap_remove(i);
                Self { rules, crawl_delay: cd }
            }
            None => Self::allow_all(),
        }
    }

    /// Is `path` (the URL path, e.g. `/a/b?x=1`) fetchable under these rules?
    pub fn allowed(&self, path: &str) -> bool {
        let mut decision: Option<bool> = None;
        let mut best_len = 0usize;
        for rule in &self.rules {
            if path_matches(&rule.pattern, path) {
                let len = rule.pattern.trim_end_matches('$').len();
                if len > best_len || (len == best_len && rule.allow) {
                    best_len = len;
                    decision = Some(rule.allow);
                }
            }
        }
        decision.unwrap_or(true) // no rule matched → allowed
    }
}

/// Match a robots path `pattern` (anchored at the start of `path`, supporting
/// `*` = any sequence and a trailing `$` = end anchor) against `path`.
fn path_matches(pattern: &str, path: &str) -> bool {
    let (pat, anchored_end) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let ends_with_star = pat.ends_with('*');
    let segments: Vec<&str> = pat.split('*').collect();
    let mut pos = 0usize;
    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        if i == 0 {
            // The first literal segment must match at the start of the path.
            if !path[pos..].starts_with(seg) {
                return false;
            }
            pos += seg.len();
        } else {
            match path[pos..].find(seg) {
                Some(idx) => pos += idx + seg.len(),
                None => return false,
            }
        }
    }
    if anchored_end && !ends_with_star && pos != path.len() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_group_disallow_is_honored() {
        let r = Robots::parse("User-agent: *\nDisallow: /private", "occipital");
        assert!(!r.allowed("/private/secret"));
        assert!(r.allowed("/public/page"));
    }

    #[test]
    fn a_specific_group_overrides_the_wildcard() {
        // Our token gets its own (stricter) group; the `*` group is ignored.
        let txt = "User-agent: occipital\nDisallow: /\n\nUser-agent: *\nAllow: /";
        let r = Robots::parse(txt, "occipital");
        assert!(!r.allowed("/anything"));
        // A different bot falls back to the permissive `*` group.
        let other = Robots::parse(txt, "googlebot");
        assert!(other.allowed("/anything"));
    }

    #[test]
    fn allow_overrides_disallow_by_longest_match() {
        let r = Robots::parse("User-agent: *\nDisallow: /a\nAllow: /a/b", "occipital");
        assert!(r.allowed("/a/b/c"), "more specific Allow wins");
        assert!(!r.allowed("/a/x"), "still blocked where only Disallow matches");
    }

    #[test]
    fn wildcards_and_end_anchor() {
        let r = Robots::parse("User-agent: *\nDisallow: /*.pdf$", "occipital");
        assert!(!r.allowed("/docs/manual.pdf"));
        assert!(r.allowed("/docs/manual.pdfx"), "$ anchors the end");
        assert!(r.allowed("/docs/page.html"));
    }

    #[test]
    fn empty_or_missing_robots_allows_all() {
        assert!(Robots::allow_all().allowed("/anything"));
        assert!(Robots::parse("", "occipital").allowed("/anything"));
    }

    #[test]
    fn crawl_delay_is_parsed_from_the_chosen_group() {
        let r = Robots::parse("User-agent: *\nCrawl-delay: 5\nDisallow: /x", "occipital");
        assert_eq!(r.crawl_delay, Some(5.0));
    }
}
