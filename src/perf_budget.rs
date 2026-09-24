//! PRD §9 "Performance regression: ... wired to CI thresholds (§6.8)": the
//! in-process §6.8 rows as release-mode tests CI runs as a blocking gate
//! (`cargo test --release perf_budget`, one test thread).
//!
//! Ignored in debug builds (`cfg_attr(debug_assertions, ignore)`): an
//! unoptimized build is 10–30× slower and says nothing about what ships, so
//! a plain `cargo test` skips them; `cargo test --release` runs them.
//!
//! Each row times the median of several runs of the same facade calls the
//! criterion benches use (`crate::bench`: the real parse, layout, L2 read,
//! Back/Forward reopen and `ui::draw` paths) and asserts one of two bounds,
//! chosen from the p50 measured on the reference VM (docs/PERFORMANCE.md):
//!
//! - **the PRD target itself**, where that reference p50 is at least 3×
//!   under it — slack enough for a slower shared CI runner not to flake,
//!   while losing the target still fails;
//! - otherwise a **regression bound** of roughly 3× the reference p50, which
//!   catches an algorithmic regression (an accidental O(n²), a lost cache)
//!   without pretending a noisy runner can hold a tight wall-clock budget.
//!
//! This retires `corpus_tests::perf_smoke_parse_and_layout_the_pathological_corpus`
//! (an `#[ignore]`d, non-blocking 10-second bound over two separate
//! synthetic pages, neither Parsoid-shaped).

use std::time::{Duration, Instant};

use crate::bench::{self, DiskCache, Reader};

const PATHOLOGICAL: &str = include_str!("../tests/perf/fixtures/pathological.html");
const TYPICAL: &str = include_str!("../tests/perf/fixtures/typical.html");

/// Runs per row. Odd, so the median is one real sample.
const RUNS: usize = 7;

// Bounds, in ms, with the reference-VM p50 each was set from
// (docs/PERFORMANCE.md, criterion medians).
/// PRD target; reference 90 ms (5.5× headroom).
const PARSE_LAYOUT_PATHOLOGICAL_MS: u64 = 500;
/// No PRD row of its own — regression bound; reference 10 ms.
const PARSE_LAYOUT_TYPICAL_MS: u64 = 30;
/// PRD target (L1 hit < 50 ms); reference 3.3 ms for both fixtures.
const OPEN_L1_MS: u64 = 50;
/// PRD target (L2 hit < 150 ms); reference 13 ms.
const OPEN_L2_TYPICAL_MS: u64 = 150;
/// Regression bound — the pathological L2 hit's reference 99 ms is only
/// 1.5× under the 150 ms target, too close to assert on a shared runner.
const OPEN_L2_PATHOLOGICAL_MS: u64 = 300;
/// PRD target (≤ 16 ms per frame); reference 0.9 ms (scroll step, 200×60)
/// and 3.8 ms (split frame, 200×60).
const FRAME_MS: u64 = 16;
/// PRD target (render < 100 ms after the response); reference 0.5 ms.
const SEARCH_RENDER_MS: u64 = 100;

fn median_of(runs: usize, mut f: impl FnMut() -> Duration) -> Duration {
    f(); // warm-up: first-touch allocation, page cache, lazy statics
    let mut samples: Vec<Duration> = (0..runs).map(|_| f()).collect();
    samples.sort();
    samples[runs / 2]
}

fn timed(f: impl FnOnce()) -> Duration {
    let start = Instant::now();
    f();
    start.elapsed()
}

fn assert_within(row: &str, measured: Duration, bound_ms: u64, kind: &str) {
    println!("perf_budget {row}: median {measured:?} (bound {bound_ms} ms, {kind})");
    assert!(
        measured <= Duration::from_millis(bound_ms),
        "{row}: median {measured:?} exceeds the {bound_ms} ms {kind} — see docs/PERFORMANCE.md"
    );
}

/// A throwaway L2 directory holding both fixtures, keyed on their canonical
/// titles like a network open stores them (the key Back looks up by).
struct TempCache {
    dir: std::path::PathBuf,
    cache: DiskCache,
    pathological: String,
    typical: String,
}

impl TempCache {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("wikitui-perf-budget-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = DiskCache::at(&dir);
        let pathological = cache.put_article(PATHOLOGICAL, 1001);
        let typical = cache.put_article(TYPICAL, 1002);
        TempCache {
            dir,
            cache,
            pathological,
            typical,
        }
    }

    fn title(&self, name: &str) -> &str {
        match name {
            "pathological" => &self.pathological,
            _ => &self.typical,
        }
    }
}

impl Drop for TempCache {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The fixture is what §6.8's row describes: one article, ≥ 1.5 MB of
/// HTML, 500+ references, parsed in full (no SEC-3 cap tripped). Runs in
/// debug builds too — it's a shape check, not a timing.
#[test]
fn perf_budget_pathological_fixture_is_the_prd_shape() {
    assert!(PATHOLOGICAL.len() >= 1_500_000, "{}", PATHOLOGICAL.len());
    let article = bench::parse("pathological", PATHOLOGICAL);
    assert!(
        article.citation_count() >= 500,
        "{}",
        article.citation_count()
    );
    assert!(!article.truncated());
}

/// §6.8: parse + layout of the pathological article < 500 ms.
#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_parse_and_layout_pathological() {
    let t = median_of(RUNS, || {
        timed(|| {
            let article = bench::parse("pathological", PATHOLOGICAL);
            std::hint::black_box(bench::layout(&article, 80));
        })
    });
    assert_within(
        "parse+layout pathological @80",
        t,
        PARSE_LAYOUT_PATHOLOGICAL_MS,
        "PRD target",
    );
}

/// The same pipeline on the typical (~150 KB) article.
#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_parse_and_layout_typical() {
    let t = median_of(RUNS, || {
        timed(|| {
            let article = bench::parse("typical", TYPICAL);
            std::hint::black_box(bench::layout(&article, 80));
        })
    });
    assert_within(
        "parse+layout typical @80",
        t,
        PARSE_LAYOUT_TYPICAL_MS,
        "regression bound",
    );
}

/// §6.8: article open, L1 cache hit < 50 ms — Back to an article just left
/// (parse still in `App::recent_docs`, layout in L1) plus one 120×40 frame.
fn open_l1_back(name: &str, other: &str) -> Duration {
    let l2 = TempCache::new(&format!("l1-{name}"));
    let mut reader = Reader::new(120, 40);
    assert!(reader.open_from_cache(&l2.cache, l2.title(name)));
    reader.draw();
    assert!(reader.open_from_cache(&l2.cache, l2.title(other)));
    reader.draw();
    let layouts = reader.layout_computations();
    let t = median_of(RUNS, || {
        let t = timed(|| {
            assert!(reader.back(&l2.cache));
            reader.draw();
        });
        assert!(reader.forward(&l2.cache));
        reader.draw();
        t
    });
    assert_eq!(reader.layout_computations(), layouts, "must be L1 hits");
    t
}

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_open_l1_hit_typical() {
    let t = open_l1_back("typical", "pathological");
    assert_within("open L1 hit (Back) typical", t, OPEN_L1_MS, "PRD target");
}

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_open_l1_hit_pathological() {
    let t = open_l1_back("pathological", "typical");
    assert_within(
        "open L1 hit (Back) pathological",
        t,
        OPEN_L1_MS,
        "PRD target",
    );
}

/// §6.8: article open, L2 hit (re-layout) < 150 ms — L2 read + zstd +
/// parse + install + layout + one frame, with the L1 cache emptied first.
fn open_l2(name: &str) -> Duration {
    let l2 = TempCache::new(&format!("l2-{name}"));
    let title = l2.title(name).to_string();
    let mut reader = Reader::new(120, 40);
    median_of(RUNS, || {
        reader.forget_layouts();
        timed(|| {
            assert!(reader.reopen_from_cache(&l2.cache, &title));
            reader.draw();
        })
    })
}

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_open_l2_hit_typical() {
    assert_within(
        "open L2 hit typical",
        open_l2("typical"),
        OPEN_L2_TYPICAL_MS,
        "PRD target",
    );
}

#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_open_l2_hit_pathological() {
    assert_within(
        "open L2 hit pathological",
        open_l2("pathological"),
        OPEN_L2_PATHOLOGICAL_MS,
        "regression bound",
    );
}

/// §6.8: scroll within a ≤ 16 ms/frame budget — one line of scroll plus
/// the redraw, on the pathological article mid-document, at the largest
/// terminal size measured (200×60).
#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_scroll_step_pathological_200x60() {
    let mut reader = Reader::new(200, 60);
    reader.open(bench::parse("pathological", PATHOLOGICAL), 1);
    reader.draw();
    reader.scroll_to_fraction(0.5);
    reader.draw();
    let mut down = true;
    let t = median_of(RUNS * 3, || {
        timed(|| {
            reader.scroll_by(if down { 1 } else { -1 });
            down = !down;
            reader.draw();
        })
    });
    assert_within("scroll step pathological 200x60", t, FRAME_MS, "PRD target");
}

/// The same budget for a `:vsplit` of the pathological article (both panes
/// laid out, every frame).
#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_split_frame_pathological_200x60() {
    let mut reader = Reader::new(200, 60);
    reader.open(bench::parse("pathological", PATHOLOGICAL), 1);
    reader.draw();
    assert!(reader.split());
    reader.draw();
    let t = median_of(RUNS * 3, || timed(|| reader.draw()));
    assert_within("split frame pathological 200x60", t, FRAME_MS, "PRD target");
}

/// §6.8: search suggestions render < 100 ms after the response — the frame
/// drawn right after ten typeahead rows are installed.
#[test]
#[cfg_attr(debug_assertions, ignore)]
fn perf_budget_search_suggestions_render() {
    let suggestions: Vec<(String, String)> = (0..10)
        .map(|i| (format!("Perf Corpus {i}"), format!("synthetic article {i}")))
        .collect();
    let mut reader = Reader::new(120, 40);
    reader.open(bench::parse("typical", TYPICAL), 1);
    reader.draw();
    let t = median_of(RUNS, || {
        timed(|| {
            reader.show_suggestions("perf corp", &suggestions);
            reader.draw();
        })
    });
    assert!(
        reader
            .screen_text()
            .iter()
            .any(|row| row.contains("Perf Corpus 3")),
        "the suggestions must actually be on screen"
    );
    assert_within(
        "search suggestions render",
        t,
        SEARCH_RENDER_MS,
        "PRD target",
    );
}
