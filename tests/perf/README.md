# §6.8 performance tooling

Everything used to measure PRD §6.8's targets. The recorded numbers, the
machine they came from, and the verdict per target live in
[`docs/PERFORMANCE.md`](../../docs/PERFORMANCE.md); this file documents the
instruments.

| Piece | What it is |
|---|---|
| `fixtures/generate.py` | Seeded generator for the Parsoid-shaped fixtures and the mock's perf corpus |
| `fixtures/pathological.html`, `fixtures/typical.html` | The committed fixtures (`generate.py --check` proves they match the generator) |
| `../../benches/perf.rs` | criterion benches for the in-process rows (`cargo bench --bench perf`) |
| `../../src/perf_budget.rs` | The same rows as release-mode CI gates (`cargo test --release perf_budget`) |
| `harness.py` | End-to-end pty harness driving the release binary against the mock |
| `test_harness.py` | Self-tests for the generator, the harness's screen model, and the mock's perf mode |

Run the self-tests with `python3 -m unittest discover -s tests/perf -p 'test_*.py'`.

## Fixtures

Both committed fixtures are one article each, built from original filler
prose (a fixed vocabulary) wrapped in markup that mirrors real Parsoid HTML
2.x (`GET /w/rest.php/v1/page/{title}/html`) at realistic attribute density:
an `id="mw…"` on nearly every element; `<section data-mw-section-id>` nesting
with `div.mw-heading` wrappers; `typeof="mw:Transclusion"` + `about="#mwt…"` +
a `data-mw` JSON blob on every template output (infobox, hatnotes,
convert/lang templates, every CS1 citation, navboxes, the reflist);
`sup.mw-ref` markers (`typeof="mw:Extension/ref"`, `span.mw-reflink-text`,
`data-mw` naming the ref); an `ol.mw-references` list with `cite_note-…` ids,
single `↑` and multi-use `a b c` backlinks and COinS `span.Z3988` metadata;
TemplateStyles `<style>` + dedup `<link>` pairs; `rowspan`/`colspan`
wikitables; MathML + fallback-image math nodes (inline and `<dl><dd>`
display); `figure[typeof=mw:File/Thumb]` with `srcset`; navboxes (which the
parser drops as noise, but still has to tokenize); and a tail of category
`<link>`s. `generate.py --describe` prints the counts:

| | pathological.html | typical.html |
|---|---|---|
| Bytes | 1,554,119 | 155,363 |
| References (`ol.mw-references li`) | 520 | 40 |
| Inline ref markers | 643 | 57 |
| `mw:Transclusion` nodes / `data-mw` attributes | 494 / 1,670 | 48 / 139 |
| Sections | 31 | 7 |
| Wikitables (rowspan / colspan cells) | 6 (45 / 46) | 1 (4 / 7) |
| Math nodes / figures / navboxes | 107 / 14 / 4 | 0 / 2 / 1 |
| Wiki links / elements with `id="mw…"` | 1,697 / 10,993 | 312 / 1,336 |
| gzip'd size (level 6) | 208,594 | 26,144 |

References and their markers are ~70% of the pathological fixture's bytes —
the long tail of real "pathological" articles is exactly this: hundreds of
CS1 citations, each carrying its `data-mw` parameters twice over (JSON and
rendered) plus COinS metadata.

Every generated article's first lead sentence carries a unique token
(`catalogue mark LEADxxxxx`, `generate.lead_marker(title)`), handy when
reading a screen dump. The harness itself calls an article painted when its
header rows — the title alone, then `N min read` — are at the top of the
screen: ratatui paints rows top to bottom, so that is the first thing an
open puts on screen at every terminal size (at 80 columns the lead
paragraph sits below an inline infobox card, so the lead token isn't).

## The mock's perf corpus

`WIKITUI_MOCK_PERF_CORPUS=1` makes `tests/mock-server/server.py` serve the
two fixtures (`Perf_Pathological`, `Perf_Typical`) plus 400 generated,
cross-linked articles `Perf_Corpus_0`…`Perf_Corpus_399` (every wiki link in
every one of them targets another corpus title, so a link-following walk
never leaves the corpus), plus `Perf_Large_0`…`Perf_Large_9`: ten distinct
pathological-size articles for the memory scenario's worst case. Each corpus article's size class is drawn from a
fixed seed:

| Class | Weight | Articles | Median size | Median refs |
|---|---|---|---|---|
| short | 30% | 112 | 82 KB | 12 |
| typical | 40% | 159 | 154 KB | 45 |
| long | 20% | 83 | 466 KB | 150 |
| very long | 10% | 46 | 940 KB | 330 |

**Article size mix — basis and caveat.** Live Wikipedia is unreachable from
the measurement environment (its network policy blocks en.wikipedia.org), so
this mix is an assumption, not a measurement. It is deliberately weighted
toward mid-size and long articles rather than the median *article* (most
articles are stubs far smaller than 80 KB of Parsoid HTML), on the reasoning
that article *opens* skew toward the longer, more linked-to, more viewed
articles. Because the weights are a guess, the network scenario reports
every size class separately as well as the mix, so its verdict can be
re-weighted by anyone with real pageview-weighted size data.

## Network emulation

Perf mode's shaping options (see `tests/mock-server/README.md`, "Perf
mode"): a fixed delay before every non-`/debug/` response starts, a
per-connection throughput cap, and gzip on article HTML when the client asks
for it (reqwest does). The network scenario uses **40 ms per request and
25 Mbit/s**:

- 25 Mbit/s is the conservative end of "broadband" (the FCC's 2015–2024
  benchmark download speed; its 2024 benchmark is 100 Mbit/s).
- 40 ms is a typical wired-broadband round trip to a nearby CDN edge;
  Wikipedia serves readers from regional caching data centers.
- The delay is charged once per request — one round trip on an already-open
  connection. TCP/TLS handshakes are not emulated (the mock is plain
  HTTP/1.0 on localhost), and neither is server think time: a cache-hit
  Parsoid response from the CDN is effectively transfer-bound.
- The cap is per connection, so concurrent background prefetch doesn't
  steal foreground bandwidth the way it could on a real shared link (the
  app's foreground gate, NF-NET-1, pauses prefetch during a foreground
  fetch anyway).

## Harness scenarios

`harness.py` spawns `target/release/wikitui` on a pty of a fixed size with
throwaway `HOME`/`XDG_{CONFIG,CACHE,STATE,DATA}_HOME` directories (never the
real user's), points it at a mock on a free port through a `[wiki.mock]`
config section with Wikipedia's capabilities (feeds, pageviews,
pageassessments — so an open makes every request it would make against
Wikipedia), and reads the screen through a small VT model (`Screen`) that
also answers the terminal queries a real emulator would. Timestamps are
`time.monotonic()` at the moment the harness writes a key and at the
moment it reads the output chunk that completes the awaited screen state.
Every row discards `--warmup` samples (default 2) and reports N, p50, p95,
max, min and standard deviation. Every process the harness starts (app or
mock) is tracked and SIGKILLed if it outlives its scenario, and the run
aborts if any harness process survives a scenario — a stray busy process
would skew every later measurement on the machine. "Wait until the screen
is quiet" counts from the later of the last output and the last key sent,
so a check right after a keypress always gives the app time to answer it.

- **cold_start** — spawn → first complete start-page frame (its title row
  and bottom key-hint row both on screen, which ratatui paints first and
  last), and first frame + the time from a keypress (`?`, or any key to
  dismiss onboarding) to its effect on screen. Three users — first run
  (no config: onboarding), returning (config plus a cache/history/index
  seeded by one earlier session), returning and logged in (plus a
  non-expired token file) — each against a normal mock and one that stalls
  every response 5 s.
- **open_cached** — L1: in one process, `H` (back) to an article already
  laid out at this width, keypress → its first paint. Tab switch: `gt`
  between two tabs, in default and `low_memory` mode (where the target tab
  re-parses its compressed source — judged against the L2 target, the same
  work). L2: a fresh process whose disk cache already holds the article,
  `:open` Enter → first paint. Each against a zero-latency mock and the
  broadband emulation: a cached open must not wait on the network, so the
  two should match.
- **open_network** — cold cache, fresh process per sample, broadband
  emulation, `:open` Enter → first paint, over seeded random corpus titles
  (reported per class and as the mix) and the pathological fixture.
- **scroll** — `j` key-repeat at 60 Hz and 120 Hz over the pathological
  article at 80×24, 120×40, 200×60. Pass/fail on dropped input is exact:
  the app's perf log records the scroll offset after every frame, and the
  last one must equal the number of presses. The per-frame draw times in
  that log are the distribution reported. Then two-key chords: bursts of
  seven `gt` at 60 Hz across three tabs (default and `low_memory`), where
  every key must produce exactly one frame and each burst must land on the
  tab its count predicts.
- **memory** — two ten-tab sets, each article in its own tab (`:tab new`):
  "mixed" (the pathological article plus the corpus's first nine long/very-
  long articles, ~8 MB of HTML) and "ten_1.5MB" (`Perf_Large_0`…`9`, ten
  distinct ~1.5 MB, 520-reference articles). Every tab is then visited once
  more (`gt` around the ring), and VmRSS/VmHWM are read from
  `/proc/<pid>/status` once output settles; `--low-memory` repeats both
  sets with `low_memory = true`.
- **search** — type a query at ~60 ms per key: last keystroke → the
  typeahead request's arrival at the mock (both on the shared
  CLOCK_MONOTONIC; the mock's request log records arrival times), and the
  app's own `typeahead_render` event (response decoded → first frame
  showing it).
- **hit_rate** — a *simulation* of §6.8's steady-state cache-hit rate, not
  a field measurement (see below).

## Cache-hit rate simulation

The app counts every article open by where it was served from
(`src/hitrate.rs`: L1, disk, saved/offline, or network — the figure
`:stats` shows). `hit_rate` drives a seeded random walk over the perf
corpus with broadband emulation and prefetch at its defaults, and reads
that same log back. Per step after a session's first open: 20% Back (when
there is somewhere to go back to), 10% reopen an article read earlier, 5%
jump to a random new corpus article, 65% follow a link. A 4 s dwell after
each open gives prefetch its idle window. Three sessions of 60 steps each
(new process per session, same profile, so L2 and history persist and L1
doesn't).

The walk compresses reading time — a 4 s dwell stands in for a ~2 minute
real one — while prefetch budgets are per wall-clock hour and day, so the
main variant scales them by the same factor (30×). The two assumptions the
number is most sensitive to are varied explicitly:

| Variant | Links followed | Prefetch budgets |
|---|---|---|
| `lead_weighted.scaled_budgets` | early links favored (exponential, mean index 6) — readers click lead/infobox links far more than late ones | scaled 30× for time compression |
| `lead_weighted.default_budgets` | same | defaults (20 MB/day, 100 req/h): a lower bound |
| `uniform40.scaled_budgets` | any of the first 40 links equally | scaled 30× |

Every corpus link is equally "popular" to the mock (it has no pageview data
for corpus titles), so prefetch ranks purely by link position; a real
reader's clicks also follow popularity, which the real ranking uses. Treat
the result as what the machinery achieves under these stated assumptions,
not as a measured field KPI.

## The perf log

`WIKITUI_PERF_LOG=<path>` (see `src/perflog.rs`) makes the app append JSON
lines to that local file: `frame` (per-frame draw time, mode, scroll
offset), `typeahead_render`, and `mark` (named startup phases). It is off by
default, writes nowhere else, and records timings and counters only.
