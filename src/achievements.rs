//! PRD FR-DL-8's achievement toasts: a small, tasteful set of session-trail
//! milestones ("Rabbit Hole: 15 articles in one session") that fire once
//! each, consuming `trail::stats` — the FR-DL-8 seam C8 (commit 0a06843)
//! left unconsumed (`trail::TrailStats`'s own doc comment names this exact
//! module as the future call site). `App::check_achievements` is the only
//! caller: it rebuilds the session trail after every recorded visit,
//! computes `trail::stats`, and calls [`newly_crossed`] with the set of
//! achievement ids already shown this session — everything in this module
//! is pure and network-free, so the threshold logic is testable without a
//! live `App`.
//!
//! Deliberately small (three thresholds, two metrics) rather than a
//! sprawling badge system — "tasteful," per the PRD — and entirely
//! off-switchable: `App::pro = true` skips `check_achievements` outright
//! (see that method's doc comment), the same gate `:xyzzy` checks.

use crate::trail::TrailStats;

/// One achievement: a stable `id` (for the "already shown" set — never
/// reused across achievements, even if a message wording changes later), the
/// toast text, and the stat + threshold that unlocks it.
#[derive(Debug, Clone, Copy)]
pub struct Achievement {
    pub id: &'static str,
    pub message: &'static str,
    threshold: usize,
    metric: fn(&TrailStats) -> usize,
}

/// The full, ordered achievement list. Order matters only for
/// [`newly_crossed`]'s "first newly-crossed wins" tie-break when a single
/// navigation happens to cross more than one threshold at once (rare in
/// practice — thresholds are spaced out and a session normally grows one
/// article at a time).
pub const ACHIEVEMENTS: &[Achievement] = &[
    Achievement {
        id: "curious-5",
        message: "Curious: 5 articles in one session",
        threshold: 5,
        metric: |s| s.article_count,
    },
    Achievement {
        id: "rabbit-hole-15",
        message: "Rabbit Hole: 15 articles in one session",
        threshold: 15,
        metric: |s| s.article_count,
    },
    Achievement {
        id: "deep-diver-8",
        message: "Deep Diver: 8 links deep in one chain",
        threshold: 8,
        metric: |s| s.longest_chain,
    },
];

/// Every achievement `stats` has crossed (`metric(stats) >= threshold`) that
/// isn't already in `already_shown` — in `ACHIEVEMENTS`' own order. The
/// caller (`App::check_achievements`) is expected to toast at most the
/// first entry and insert its id into `already_shown` before the next call,
/// so a threshold only ever toasts once per session no matter how many more
/// navigations keep the stat above it.
pub fn newly_crossed(
    stats: &TrailStats,
    already_shown: &std::collections::HashSet<&'static str>,
) -> Vec<&'static Achievement> {
    ACHIEVEMENTS
        .iter()
        .filter(|a| (a.metric)(stats) >= a.threshold && !already_shown.contains(a.id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn stats(article_count: usize, longest_chain: usize) -> TrailStats {
        TrailStats {
            article_count,
            max_depth: longest_chain.saturating_sub(1),
            longest_chain,
        }
    }

    #[test]
    fn below_every_threshold_crosses_nothing() {
        let shown = HashSet::new();
        assert!(newly_crossed(&stats(1, 1), &shown).is_empty());
    }

    #[test]
    fn crossing_five_articles_unlocks_curious_only() {
        let shown = HashSet::new();
        let crossed = newly_crossed(&stats(5, 1), &shown);
        assert_eq!(crossed.len(), 1);
        assert_eq!(crossed[0].id, "curious-5");
    }

    #[test]
    fn crossing_fifteen_articles_unlocks_both_article_count_thresholds() {
        let shown = HashSet::new();
        let crossed = newly_crossed(&stats(15, 1), &shown);
        let ids: Vec<&str> = crossed.iter().map(|a| a.id).collect();
        assert_eq!(ids, vec!["curious-5", "rabbit-hole-15"]);
    }

    #[test]
    fn an_already_shown_achievement_does_not_cross_again() {
        let mut shown = HashSet::new();
        shown.insert("curious-5");
        let crossed = newly_crossed(&stats(20, 1), &shown);
        let ids: Vec<&str> = crossed.iter().map(|a| a.id).collect();
        assert_eq!(
            ids,
            vec!["rabbit-hole-15"],
            "curious-5 already shown must not fire again even though the stat still qualifies"
        );
    }

    #[test]
    fn deep_diver_uses_longest_chain_not_article_count() {
        let shown = HashSet::new();
        // Many articles, but a shallow chain — deep-diver-8 must not fire.
        let crossed = newly_crossed(&stats(50, 3), &shown);
        assert!(!crossed.iter().any(|a| a.id == "deep-diver-8"));

        // A deep chain with few total articles still fires deep-diver-8.
        let crossed = newly_crossed(&stats(8, 8), &shown);
        assert!(crossed.iter().any(|a| a.id == "deep-diver-8"));
    }

    #[test]
    fn every_achievement_id_is_unique() {
        let mut ids = std::collections::HashSet::new();
        for a in ACHIEVEMENTS {
            assert!(ids.insert(a.id), "duplicate achievement id {:?}", a.id);
        }
    }
}
