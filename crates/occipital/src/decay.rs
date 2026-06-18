//! Decay & forgetting — pure math. Web content perishes, so a cached page's
//! standing falls with *disuse* (time since last access). Effective salience
//! drives both recall ranking (stale pages sink) and GC (stale, unread, unpinned
//! pages are pruned). Kept primitive-only (no DB/clock) so it's trivially tested;
//! the `engine` supplies ages + decides what to prune.

/// Decay multiplier in `(0, 1]` that **halves every `half_life`** of disuse:
/// `2^(-disuse/half_life)`. `1.0` for fresh (disuse 0) or a non-positive
/// half-life (decay disabled).
pub fn decay_factor(disuse_secs: f64, half_life_secs: f64) -> f64 {
    if half_life_secs <= 0.0 {
        return 1.0;
    }
    2f64.powf(-disuse_secs.max(0.0) / half_life_secs)
}

/// Effective salience = stored salience × decay(disuse). This is the value GC
/// thresholds on and recall multiplies relevance by.
pub fn effective_salience(stored: f32, disuse_secs: f64, half_life_secs: f64) -> f32 {
    (stored as f64 * decay_factor(disuse_secs, half_life_secs)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_does_not_decay() {
        assert!((decay_factor(0.0, 100.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn halves_every_half_life() {
        assert!((decay_factor(100.0, 100.0) - 0.5).abs() < 1e-9, "one half-life → 0.5");
        assert!((decay_factor(200.0, 100.0) - 0.25).abs() < 1e-9, "two → 0.25");
    }

    #[test]
    fn older_disuse_decays_more() {
        assert!(decay_factor(10.0, 100.0) > decay_factor(90.0, 100.0));
    }

    #[test]
    fn zero_half_life_disables_decay() {
        assert_eq!(decay_factor(9999.0, 0.0), 1.0);
    }

    #[test]
    fn effective_salience_scales_with_decay() {
        // A high stored salience, long unused, drops below a typical GC floor.
        assert!(effective_salience(1.0, 0.0, 100.0) > 0.99, "fresh keeps its salience");
        assert!(effective_salience(1.0, 400.0, 100.0) < 0.1, "long disuse → near zero");
    }
}
