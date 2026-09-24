//! PRD §9 "Performance regression: criterion benches for parse/layout on the
//! pathological corpus" — plus every other §6.8 row that can be measured
//! in-process. Run with `cargo bench --bench perf` (see
//! docs/PERFORMANCE.md for the recorded numbers, the hardware they came
//! from, and how the pty-driven end-to-end rows in `tests/perf/` complement
//! these).
//!
//! Fixtures are the committed, generator-checked Parsoid-shaped pages in
//! `tests/perf/fixtures/` (`generate.py --describe` prints their shape):
//! `pathological.html` (~1.55 MB, 520 references — §6.8's pathological row)
//! and `typical.html` (~150 KB).
//!
//! Groups, one per §6.8 row they back:
//! - `parse`, `layout/{80,120,200}`, `parse_layout` — "Parse+layout of a
//!   pathological article < 500 ms".
//! - `open_l1_hit` — "Article open, L1 cache hit < 50 ms": Back to an
//!   article just left — its parse still in memory (`App::recent_docs`),
//!   its layout in L1 — install + one frame. `open_l1_layout_reparse` is the
//!   other reopen (a link back to it): L1 layout hit, but L2 read + zstd +
//!   parse again.
//! - `open_l2_hit` — "Article open, L2 hit (re-layout) < 150 ms": the same
//!   path with the L1 cache emptied first, so it relayouts.
//! - `frame/{80x24,120x40,200x60}`, `frame_split`, `scroll_step` — "Scroll:
//!   ≤ 16 ms/frame": one `ui::draw` of the pathological article mid-scroll
//!   (single pane, and a `:vsplit` of it), and a one-line scroll plus redraw.
//! - `search_suggestions` — "Search suggestions render < 100 ms after
//!   response": the frame drawn right after a typeahead response is
//!   installed.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use wikitui::bench::{self, DiskCache, Reader};

const PATHOLOGICAL: &str = include_str!("../tests/perf/fixtures/pathological.html");
const TYPICAL: &str = include_str!("../tests/perf/fixtures/typical.html");

const FIXTURES: [(&str, &str); 2] = [("pathological", PATHOLOGICAL), ("typical", TYPICAL)];

fn parse_and_layout(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse");
    g.sample_size(20).measurement_time(Duration::from_secs(8));
    for (name, html) in FIXTURES {
        g.bench_function(name, |b| b.iter(|| bench::parse(name, black_box(html))));
    }
    g.finish();

    let mut g = c.benchmark_group("layout");
    g.sample_size(20).measurement_time(Duration::from_secs(8));
    for (name, html) in FIXTURES {
        let article = bench::parse(name, html);
        for width in [80u16, 120, 200] {
            g.bench_with_input(BenchmarkId::new(name, width), &width, |b, &w| {
                b.iter(|| bench::layout(&article, black_box(w)))
            });
        }
    }
    g.finish();

    let mut g = c.benchmark_group("parse_layout");
    g.sample_size(20).measurement_time(Duration::from_secs(10));
    for (name, html) in FIXTURES {
        g.bench_function(name, |b| {
            b.iter(|| bench::layout(&bench::parse(name, black_box(html)), 80))
        });
    }
    g.finish();
}

/// A throwaway L2 directory holding both fixtures, removed on drop, each
/// keyed on its canonical title like a real network open stores it
/// (`titles`, in `FIXTURES` order) — the key history, and so Back, uses.
struct TempCache {
    dir: std::path::PathBuf,
    cache: DiskCache,
    titles: Vec<String>,
}

impl TempCache {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("wikitui-bench-l2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = DiskCache::at(&dir);
        let titles = FIXTURES
            .iter()
            .enumerate()
            .map(|(i, (_, html))| cache.put_article(html, 1000 + i as u64))
            .collect();
        TempCache { dir, cache, titles }
    }

    /// The canonical title of the fixture labeled `name`.
    fn title(&self, name: &str) -> &str {
        let i = FIXTURES.iter().position(|(n, _)| *n == name).unwrap();
        &self.titles[i]
    }
}

impl Drop for TempCache {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn opens(c: &mut Criterion) {
    let l2 = TempCache::new();

    let mut g = c.benchmark_group("open_l1_hit");
    g.sample_size(20).measurement_time(Duration::from_secs(8));
    for (name, _) in FIXTURES {
        let other = FIXTURES.iter().find(|(n, _)| *n != name).unwrap().0;
        let mut reader = Reader::new(120, 40);
        // Visit `name`, then `other`: Back to `name` is then the reader's
        // real L1 hit — its parse still in `App::recent_docs`, its layout in
        // L1 — and Forward restores the setup for the next iteration.
        assert!(reader.open_from_cache(&l2.cache, l2.title(name)));
        reader.draw();
        assert!(reader.open_from_cache(&l2.cache, l2.title(other)));
        reader.draw();
        let before = reader.layout_computations();
        g.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut timed = Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    assert!(reader.back(&l2.cache));
                    reader.draw();
                    timed += start.elapsed();
                    assert!(reader.forward(&l2.cache));
                    reader.draw();
                }
                timed
            })
        });
        assert_eq!(
            reader.layout_computations(),
            before,
            "every measured open must be an L1 hit"
        );
    }
    g.finish();

    // The other reopen: following a link (or `:open`) back to an article
    // still laid out at this width — an L1 layout hit, but its document is
    // read from L2 and re-parsed (`recent_docs` serves Back/Forward only).
    let mut g = c.benchmark_group("open_l1_layout_reparse");
    g.sample_size(20).measurement_time(Duration::from_secs(8));
    for (name, _) in FIXTURES {
        let mut reader = Reader::new(120, 40);
        let title = l2.title(name);
        assert!(reader.open_from_cache(&l2.cache, title));
        reader.draw();
        let before = reader.layout_computations();
        g.bench_function(name, |b| {
            b.iter(|| {
                assert!(reader.reopen_from_cache(&l2.cache, title));
                reader.draw();
            })
        });
        assert_eq!(reader.layout_computations(), before, "L1 layout hits");
    }
    g.finish();

    let mut g = c.benchmark_group("open_l2_hit");
    g.sample_size(20).measurement_time(Duration::from_secs(10));
    for (name, _) in FIXTURES {
        let mut reader = Reader::new(120, 40);
        let title = l2.title(name);
        assert!(reader.open_from_cache(&l2.cache, title));
        g.bench_function(name, |b| {
            b.iter(|| {
                reader.forget_layouts();
                assert!(reader.reopen_from_cache(&l2.cache, title));
                reader.draw();
            })
        });
    }
    g.finish();
}

fn frames(c: &mut Criterion) {
    let mut g = c.benchmark_group("frame");
    g.sample_size(50).measurement_time(Duration::from_secs(5));
    for (w, h) in [(80u16, 24u16), (120, 40), (200, 60)] {
        let mut reader = Reader::new(w, h);
        reader.open(bench::parse("pathological", PATHOLOGICAL), 1);
        reader.draw();
        reader.scroll_to_fraction(0.5);
        reader.draw();
        g.bench_function(format!("{w}x{h}"), |b| b.iter(|| reader.draw()));
    }
    g.finish();

    // A split (PRD FR-TB-4): both panes' layouts come out of L1 on every
    // frame.
    let mut g = c.benchmark_group("frame_split");
    g.sample_size(50).measurement_time(Duration::from_secs(5));
    for (w, h) in [(120u16, 40u16), (200, 60)] {
        let mut reader = Reader::new(w, h);
        reader.open(bench::parse("pathological", PATHOLOGICAL), 1);
        reader.draw();
        assert!(reader.split());
        reader.draw();
        g.bench_function(format!("{w}x{h}"), |b| b.iter(|| reader.draw()));
    }
    g.finish();

    let mut g = c.benchmark_group("scroll_step");
    g.sample_size(50).measurement_time(Duration::from_secs(5));
    for (w, h) in [(80u16, 24u16), (120, 40), (200, 60)] {
        let mut reader = Reader::new(w, h);
        reader.open(bench::parse("pathological", PATHOLOGICAL), 1);
        reader.draw();
        reader.scroll_to_fraction(0.5);
        reader.draw();
        let mut down = true;
        g.bench_function(format!("{w}x{h}"), |b| {
            b.iter(|| {
                reader.scroll_by(if down { 1 } else { -1 });
                down = !down;
                reader.draw();
            })
        });
    }
    g.finish();

    let suggestions: Vec<(String, String)> = (0..10)
        .map(|i| {
            (
                format!("Perf Corpus {i}"),
                format!("synthetic article number {i} in the perf corpus"),
            )
        })
        .collect();
    let mut g = c.benchmark_group("search_suggestions");
    g.sample_size(50).measurement_time(Duration::from_secs(5));
    for (w, h) in [(80u16, 24u16), (120, 40)] {
        let mut reader = Reader::new(w, h);
        reader.open(bench::parse("typical", TYPICAL), 1);
        reader.draw();
        g.bench_function(format!("{w}x{h}"), |b| {
            b.iter(|| {
                reader.show_suggestions("perf corp", &suggestions);
                reader.draw();
            })
        });
    }
    g.finish();
}

criterion_group!(benches, parse_and_layout, opens, frames);
criterion_main!(benches);
