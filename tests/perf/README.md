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
(`catalogue mark LEADxxxxx`, `generate.lead_marker(title)`): the harness
waits for that token to call the lead section painted.

## The mock's perf corpus

`WIKITUI_MOCK_PERF_CORPUS=1` makes `tests/mock-server/server.py` serve the
two fixtures (`Perf_Pathological`, `Perf_Typical`) plus 400 generated,
cross-linked articles `Perf_Corpus_0`…`Perf_Corpus_399` (every wiki link in
every one of them targets another corpus title, so a link-following walk
never leaves the corpus). Each corpus article's size class is drawn from a
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
max, min and standard deviation.

- **cold_start** — spawn → first complete start-page frame (its title row
  and bottom key-hint row both on screen, which ratatui paints first and
  last), and first frame + the time from a keypress (`?`, or any key to
  dismiss onboarding) to its effect on screen. Three users — first run
  (no config: onboarding), returning (config plus a cache/history/index
  seeded by one earlier session), returning and logged in (plus a
  non-expired token file) — each against a normal mock and one that stalls
  every response 5 s.
- **open_cached** — L1: in one process, `H` (back) to an article already
  laid out at this width, keypress → its lead marker on screen. L2: a fresh
  process whose disk cache already holds the article, `:open` Enter → lead
  marker.
- **open_network** — cold cache, fresh process per sample, broadband
  emulation, `:open` Enter → lead marker, over seeded random corpus titles
  (reported per class and as the mix) and the pathological fixture.
- **scroll** — `j` key-repeat at 60 Hz and 120 Hz over the pathological
  article at 80×24, 120×40, 200×60. Pass/fail on dropped input is exact:
  the app's perf log records the scroll offset after every frame, and the
  last one must equal the number of presses. The per-frame draw times in
  that log are the distribution reported.
- **memory** — the pathological article plus the nine first long/very-long
  corpus articles, each in its own tab (`:tab new`), then VmRSS/VmHWM from
  `/proc/<pid>/status` once output settles.
- **search** — type a query at ~60 ms per key: last keystroke → the
  typeahead request's arrival at the mock (both on the shared
  CLOCK_MONOTONIC; the mock's request log records arrival times), and the
  app's own `typeahead_render` event (response decoded → first frame
  showing it).

## The perf log

`WIKITUI_PERF_LOG=<path>` (see `src/perflog.rs`) makes the app append JSON
lines to that local file: `frame` (per-frame draw time, mode, scroll
offset), `typeahead_render`, and `mark` (named startup phases). It is off by
default, writes nowhere else, and records timings and counters only.
