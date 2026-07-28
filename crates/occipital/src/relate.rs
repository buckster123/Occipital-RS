//! Relate — the connective layer of the knowledge hub.
//!
//! Distillation turns a page into knowledge (summary, entities, tags); relate
//! turns the *pile* of knowledge into a web: pages that mention the same
//! entities or share topic tags are neighbours, and the agent can walk from
//! any curated page to what it already knows around it ("I've read about this
//! before — here's where").
//!
//! Relatedness is **computed live from the `distillations` table, never
//! stored**: the store is small (only curated pages qualify), the overlap
//! query is a scan over in-memory metas, and a link table would just be a
//! cache that can go stale when a page is re-distilled or forgotten. Entities
//! outweigh tags (an entity like "BCM2712" is a far stronger tie than a tag
//! like "hardware"), matching is case-insensitive, and a zero-overlap pair is
//! simply not related — no similarity theatre.
//!
//! The embedding layer is deliberately *not* consulted here: cosine says "the
//! prose is alike", shared entities say "these are about the same things" —
//! the latter is the knowledge-graph signal this layer exists to surface.

use serde::Serialize;

/// Weight of one shared entity vs one shared tag.
const ENTITY_WEIGHT: f32 = 2.0;
const TAG_WEIGHT: f32 = 1.0;
/// Chars of summary a related row carries — recognition, not reading.
const SUMMARY_HEAD_CHARS: usize = 160;

/// A distillation's linkable identity — what `related_pages` scores over.
/// Assembled by [`crate::cache::Cache::all_distill_meta`] (title joined from
/// the pages table).
#[derive(Debug, Clone)]
pub struct DistillMeta {
    pub url: String,
    pub title: Option<String>,
    pub summary: String,
    pub entities: Vec<String>,
    pub tags: Vec<String>,
}

/// One neighbour of a curated page, with *why* it is one — the shared terms
/// are the edge label, not just a score.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RelatedPage {
    pub url: String,
    pub title: Option<String>,
    /// First ~160 chars of the neighbour's summary (recognition, not reading).
    pub summary_head: String,
    /// Weighted overlap: shared entities × 2 + shared tags × 1.
    pub score: f32,
    pub shared_entities: Vec<String>,
    pub shared_tags: Vec<String>,
}

/// Rank every other distilled page by term overlap with `target`. Excludes the
/// target itself and zero-overlap pages; ties break by URL for determinism.
/// Matching is case-insensitive; the reported terms keep the *candidate's*
/// original casing (its distillation named them).
pub fn related_pages(target: &DistillMeta, all: &[DistillMeta], limit: usize) -> Vec<RelatedPage> {
    let norm = |s: &String| s.trim().to_lowercase();
    let t_entities: Vec<String> = target.entities.iter().map(norm).collect();
    let t_tags: Vec<String> = target.tags.iter().map(norm).collect();

    let mut out: Vec<RelatedPage> = all
        .iter()
        .filter(|c| c.url != target.url)
        .filter_map(|c| {
            let shared_entities: Vec<String> = c
                .entities
                .iter()
                .filter(|e| t_entities.contains(&norm(e)))
                .cloned()
                .collect();
            let shared_tags: Vec<String> = c
                .tags
                .iter()
                .filter(|t| t_tags.contains(&norm(t)))
                .cloned()
                .collect();
            let score =
                shared_entities.len() as f32 * ENTITY_WEIGHT + shared_tags.len() as f32 * TAG_WEIGHT;
            if score == 0.0 {
                return None;
            }
            Some(RelatedPage {
                url: c.url.clone(),
                title: c.title.clone(),
                summary_head: c.summary.chars().take(SUMMARY_HEAD_CHARS).collect(),
                score,
                shared_entities,
                shared_tags,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.url.cmp(&b.url))
    });
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(url: &str, entities: &[&str], tags: &[&str]) -> DistillMeta {
        DistillMeta {
            url: url.into(),
            title: Some(url.rsplit('/').next().unwrap_or(url).into()),
            summary: format!("summary of {url}"),
            entities: entities.iter().map(|s| s.to_string()).collect(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn entities_outweigh_tags() {
        let target = meta("https://t", &["BCM2712", "Raspberry Pi 5"], &["hardware"]);
        let all = vec![
            target.clone(),
            meta("https://tags-only", &[], &["hardware"]),
            meta("https://entity", &["BCM2712"], &[]),
        ];
        let r = related_pages(&target, &all, 10);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].url, "https://entity", "one entity beats one tag");
        assert_eq!(r[0].score, 2.0);
        assert_eq!(r[1].score, 1.0);
    }

    #[test]
    fn matching_is_case_insensitive_but_reports_candidate_casing() {
        let target = meta("https://t", &["rust"], &["Web"]);
        let all = vec![target.clone(), meta("https://c", &["Rust"], &["web"])];
        let r = related_pages(&target, &all, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].shared_entities, vec!["Rust"], "candidate's casing survives");
        assert_eq!(r[0].shared_tags, vec!["web"]);
        assert_eq!(r[0].score, 3.0);
    }

    #[test]
    fn self_and_zero_overlap_are_excluded() {
        let target = meta("https://t", &["A"], &["x"]);
        let all = vec![target.clone(), meta("https://unrelated", &["B"], &["y"])];
        assert!(related_pages(&target, &all, 10).is_empty());
    }

    #[test]
    fn ranked_capped_and_deterministic() {
        let target = meta("https://t", &["A", "B"], &["x"]);
        let all = vec![
            target.clone(),
            meta("https://both", &["A", "B"], &[]),   // 4.0
            meta("https://one-b", &["B"], &[]),       // 2.0 — url ties break lexically
            meta("https://one-a", &["A"], &[]),       // 2.0
            meta("https://tag", &[], &["x"]),         // 1.0
        ];
        let r = related_pages(&target, &all, 3);
        assert_eq!(
            r.iter().map(|p| p.url.as_str()).collect::<Vec<_>>(),
            ["https://both", "https://one-a", "https://one-b"],
            "score desc, url asc on ties, capped"
        );
    }

    #[test]
    fn summary_head_is_bounded_and_char_safe() {
        let mut c = meta("https://c", &["A"], &[]);
        c.summary = "é".repeat(500);
        let target = meta("https://t", &["A"], &[]);
        let r = related_pages(&target, &[target.clone(), c], 10);
        assert_eq!(r[0].summary_head.chars().count(), SUMMARY_HEAD_CHARS);
    }
}
