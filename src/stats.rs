//! PRD FR-PC-3 reading stats (local-only): "articles read, time, streaks,
//! topic distribution; feeds the interest model; suppressed by incognito;
//! `wikitui stats --explain` shows top topics." Everything here is derived, at
//! read time, from data that already exists — the reading history
//! (`history::History`, PRD FR-HS-1) and the interest model (`interest.rs`,
//! FR-PF-3) — so there is no separate "stats file" to keep in sync or wipe:
//! the numbers are a *view* over history + interest, computed on demand.
//!
//! **Incognito suppression is upstream, by construction.** Incognito never
//! writes history or interest (the `privacy::decide` gate denies
//! `Write::History`/`Write::Interest`), so a session read in incognito simply
//! leaves nothing for this module to count — the suppression FR-PC-3 requires
//! is a consequence of the privacy gate, not a second check here.
//!
//! The `compute` step is a pure function of a visit list plus the interest
//! model's top topics, so streak/total/distinct arithmetic is unit-testable
//! without a database or a clock; `run` is the thin `wikitui stats` CLI shell
//! around it, mirroring `cleardata::run`'s plain-stdout, TUI-free posture.

use std::collections::{BTreeSet, HashSet};

use crate::config::ResolvedConfig;
use crate::history::Visit;

/// Seconds per day, for bucketing visit timestamps into calendar days. Days
/// are **UTC-bucketed** (a documented simplification): streaks count distinct
/// UTC days with a read, which can differ from the reader's local calendar day
/// by at most the timezone offset — acceptable for a "how many days running
/// have I read something" figure, and it keeps the arithmetic deterministic
/// and clock-free for testing.
const DAY_SECS: i64 = 86_400;

/// The computed reading stats (PRD FR-PC-3). A plain data bag — `compute`
/// fills it, `run` (or a `:stats` view) renders it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadingStats {
    /// Distinct `(lang, title)` articles ever read.
    pub distinct_articles: usize,
    /// Total number of page loads (a revisit counts again).
    pub total_visits: usize,
    /// Total accumulated dwell time across every visit, in seconds.
    pub total_time_secs: i64,
    /// Consecutive days with a read ending at the most recent reading day
    /// (PRD FR-PC-3 "streaks"). A gap resets it — see [`streaks`].
    pub current_streak_days: u32,
    /// The longest such consecutive run anywhere in the history.
    pub longest_streak_days: u32,
    /// PRD FR-PC-3 "topic distribution": the interest model's top categories
    /// with their affinity scores (the model and stats "share the same
    /// category data," as the brief puts it).
    pub top_topics: Vec<(String, f64)>,
}

/// Compute the stats from a raw (non-deduplicated) visit list and the interest
/// model's top topics. Pure: no I/O, no clock — days come from the visits' own
/// `opened_at`, so the same input always yields the same output.
pub fn compute(visits: &[Visit], top_topics: Vec<(String, f64)>) -> ReadingStats {
    let mut distinct: HashSet<(String, String)> = HashSet::new();
    let mut days: BTreeSet<i64> = BTreeSet::new();
    let mut total_time_secs: i64 = 0;
    for v in visits {
        distinct.insert((v.lang.clone(), v.title.clone()));
        days.insert(v.opened_at.div_euclid(DAY_SECS));
        total_time_secs += v.dwell_secs.max(0);
    }
    let (current, longest) = streaks(&days);
    ReadingStats {
        distinct_articles: distinct.len(),
        total_visits: visits.len(),
        total_time_secs,
        current_streak_days: current,
        longest_streak_days: longest,
        top_topics,
    }
}

/// `(current_streak, longest_streak)` in days over a set of day indices.
/// **Current** = the length of the consecutive run ending at the most recent
/// day present (the streak the reader is "on"); a gap before today's-most-
/// recent day means the current streak is just that final run, not an older
/// longer one. **Longest** = the longest consecutive run anywhere. Both are 0
/// for an empty history.
fn streaks(days: &BTreeSet<i64>) -> (u32, u32) {
    if days.is_empty() {
        return (0, 0);
    }
    let mut longest = 1u32;
    let mut run = 1u32;
    let mut prev: Option<i64> = None;
    // The run length ending at each day, tracked so the final one is "current".
    let mut ending_run = 1u32;
    for &day in days {
        if let Some(p) = prev {
            if day == p + 1 {
                run += 1;
            } else {
                run = 1;
            }
        }
        ending_run = run;
        longest = longest.max(run);
        prev = Some(day);
    }
    (ending_run, longest)
}

/// A human "Nh Nm" (or "Nm", or "Ns") duration for the total-time line —
/// pure, so it renders identically in the CLI and any future `:stats` view.
pub fn human_duration(total_secs: i64) -> String {
    let s = total_secs.max(0);
    let hours = s / 3600;
    let minutes = (s % 3600) / 60;
    let seconds = s % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// `wikitui stats [--explain]` (PRD FR-PC-3): the standalone, TUI-free
/// subcommand — same posture as `cleardata::run`/`doctor::run`, plain stdout,
/// no terminal or network init. Reads the on-disk history and interest model,
/// computes, and prints. `--explain` additionally shows the interest model's
/// top topics with scores and a one-line "why" (which signals move them).
/// Returns the process exit code.
pub fn run(resolved: &ResolvedConfig, explain: bool) -> i32 {
    let history = crate::history::History::open();
    let visits = history.all_visits();

    let half_life = resolved.interest_half_life_days.value;
    let model = match crate::interest::interest_path() {
        Some(path) => crate::interest::InterestModel::load(&path, half_life),
        None => crate::interest::InterestModel::new(half_life),
    };
    let top_topics = model.top_categories(TOP_TOPICS);
    let stats = compute(&visits, top_topics);

    println!("Reading stats (local-only — PRD FR-PC-3)");
    println!("  articles read : {}", stats.distinct_articles);
    println!("  total visits  : {}", stats.total_visits);
    println!(
        "  reading time  : {}",
        human_duration(stats.total_time_secs)
    );
    println!("  current streak: {} day(s)", stats.current_streak_days);
    println!("  longest streak: {} day(s)", stats.longest_streak_days);

    if stats.top_topics.is_empty() {
        println!("  top topics    : (none yet — read a few articles to build the interest model)");
    } else {
        println!("  top topics    :");
        for (cat, sc) in &stats.top_topics {
            println!("      {sc:>6.2}  {cat}");
        }
    }

    if explain {
        println!();
        println!(
            "How top topics are learned (interest model, half-life {:.0}d):",
            model.half_life_days()
        );
        println!("  Each category's score is the sum of reading signals on articles in it,");
        println!(
            "  decayed by half every {:.0} days. Signals: open +{:.0}, dwell up to +{:.0},",
            model.half_life_days(),
            crate::interest::SIGNAL_OPEN,
            crate::interest::DWELL_SIGNAL_MAX
        );
        println!(
            "  scroll +{:.0}, bookmark +{:.0}, save +{:.0}, 'not interested' {:.0}.",
            crate::interest::SIGNAL_SCROLL,
            crate::interest::SIGNAL_BOOKMARK,
            crate::interest::SIGNAL_SAVE,
            crate::interest::SIGNAL_NOT_INTERESTED
        );
        println!("  The model is local only (FR-PR-2) and never leaves this machine.");
        if let Some(path) = crate::interest::interest_path() {
            println!("  Stored, human-readable, at: {}", path.display());
        }
    }
    0
}

/// How many topics the stats surface lists. Small: the point is a distribution
/// summary, not the whole vector (`:interests` shows more).
const TOP_TOPICS: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn visit(lang: &str, title: &str, day: i64, dwell: i64) -> Visit {
        Visit {
            id: 0,
            lang: lang.to_string(),
            title: title.to_string(),
            opened_at: day * DAY + 100, // +100s so it's mid-day, not on the boundary
            dwell_secs: dwell,
            referrer_lang: None,
            referrer_title: None,
        }
    }

    #[test]
    fn compute_counts_distinct_articles_visits_and_time() {
        let visits = vec![
            visit("en", "Alan Turing", 0, 60),
            visit("en", "Enigma machine", 0, 30),
            visit("en", "Alan Turing", 1, 90), // revisit: same article, new visit
        ];
        let stats = compute(&visits, vec![]);
        assert_eq!(stats.distinct_articles, 2, "two distinct articles");
        assert_eq!(stats.total_visits, 3, "three page loads");
        assert_eq!(stats.total_time_secs, 180, "dwell summed across all visits");
    }

    #[test]
    fn a_streak_runs_across_consecutive_days_and_breaks_on_a_gap() {
        // Days 1,2,3 (a 3-day run), then a gap, then day 5 (a fresh run of 1).
        let visits = vec![
            visit("en", "A", 1, 0),
            visit("en", "B", 2, 0),
            visit("en", "C", 3, 0),
            visit("en", "D", 5, 0),
        ];
        let stats = compute(&visits, vec![]);
        assert_eq!(
            stats.longest_streak_days, 3,
            "days 1-2-3 are the longest run"
        );
        assert_eq!(
            stats.current_streak_days, 1,
            "the gap before day 5 resets the current streak to just day 5"
        );
    }

    #[test]
    fn a_single_unbroken_run_is_both_current_and_longest() {
        let visits = vec![
            visit("en", "A", 10, 0),
            visit("en", "B", 11, 0),
            visit("en", "C", 12, 0),
        ];
        let stats = compute(&visits, vec![]);
        assert_eq!(stats.current_streak_days, 3);
        assert_eq!(stats.longest_streak_days, 3);
    }

    #[test]
    fn multiple_reads_on_the_same_day_count_as_one_streak_day() {
        let visits = vec![
            visit("en", "A", 4, 0),
            visit("en", "B", 4, 0),
            visit("en", "C", 5, 0),
        ];
        let stats = compute(&visits, vec![]);
        assert_eq!(
            stats.longest_streak_days, 2,
            "two calendar days, not three visits"
        );
        assert_eq!(stats.current_streak_days, 2);
    }

    #[test]
    fn empty_history_has_zero_everything() {
        let stats = compute(&[], vec![]);
        assert_eq!(stats, ReadingStats::default());
    }

    #[test]
    fn topic_distribution_is_passed_through_from_the_interest_model() {
        let topics = vec![
            ("Cryptography".to_string(), 4.0),
            ("Computing".to_string(), 1.0),
        ];
        let stats = compute(&[visit("en", "A", 0, 0)], topics.clone());
        assert_eq!(stats.top_topics, topics);
    }

    #[test]
    fn human_duration_buckets_sensibly() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(90), "1m 30s");
        assert_eq!(human_duration(3661), "1h 1m");
    }
}
