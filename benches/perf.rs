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
//! - `open_l1_hit` — "Article open, L1 cache hit < 50 ms": the real reopen
//!   path (L2 read + zstd decompress + parse + install + an L1 layout hit +
//!   one frame). The document itself isn't cached in memory, so an L1 hit
//!   still re-parses — that's what the reader pays today, so it's what's
//!   measured.
//! - `open_l2_hit` — "Article open, L2 hit (re-layout) < 150 ms": the same
//!   path with the L1 cache emptied first, so it relayouts.
//! - `frame/{80x24,120x40,200x60}`, `scroll_step` — "Scroll: ≤ 16 ms/frame":
//!   one `ui::draw` of the pathological article mid-scroll, and a one-line
//!   scroll plus redraw.
//! - `search_suggestions` — "Search suggestions render < 100 ms after
//!   response": the frame drawn right after a typeahead response is
//!   installed.

use std::hint::black_box;
use std::time::Duration;

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

/// A throwaway L2 directory holding both fixtures, removed on drop.
struct TempCache {
    dir: std::path::PathBuf,
    cache: DiskCache,
}

impl TempCache {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("wikitui-bench-l2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = DiskCache::at(&dir);
        for (i, (name, html)) in FIXTURES.iter().enumerate() {
            cache.put(name, html, 1000 + i as u64);
        }
        TempCache { dir, cache }
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
        let mut reader = Reader::new(120, 40);
        // Warm L1 with this article at this width. Installing a document
        // always drops the current layout (`App::set_document`), so every
        // measured reopen below goes through the L1 lookup — and must hit.
        assert!(reader.open_from_cache(&l2.cache, name));
        reader.draw();
        let before = reader.layout_computations();
        g.bench_function(name, |b| {
            b.iter(|| {
                reader.reopen_from_cache(&l2.cache, name);
                reader.draw();
            })
        });
        assert_eq!(
            reader.layout_computations(),
            before,
            "every measured open must be an L1 hit"
        );
    }
    g.finish();

    let mut g = c.benchmark_group("open_l2_hit");
    g.sample_size(20).measurement_time(Duration::from_secs(10));
    for (name, _) in FIXTURES {
        let mut reader = Reader::new(120, 40);
        assert!(reader.open_from_cache(&l2.cache, name));
        g.bench_function(name, |b| {
            b.iter(|| {
                reader.forget_layouts();
                reader.reopen_from_cache(&l2.cache, name);
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
