# Performance: PRD §6.8, measured

This page records every PRD §6.8 performance target as measured on real
hardware: how it was measured, the numbers (p50 / p95 / max), a verdict,
what had to change to meet it, and what the numbers don't establish. The
instruments themselves (fixtures, benches, the pty harness, the mock's
network emulation) are documented in
[`tests/perf/README.md`](../tests/perf/README.md).

Reproduce everything below with one command, on a quiet machine:

```sh
tests/perf/reproduce.sh            # about an hour; harness results in perf-results.json
```

It builds the release binary, then runs the criterion benches
(`cargo bench --bench perf`), the release-mode CI budget rows
(`cargo test --release perf_budget`), and every harness scenario
(`python3 tests/perf/harness.py --low-memory`). One scenario at a time:
`python3 tests/perf/harness.py scroll`, `… memory --low-memory`, and so on
(`--help` lists them).

## Verdicts

All times in milliseconds, p50 / p95 / max, from one run on the machine
described below. "Met" means met on that machine; see
[What these numbers don't establish](#what-these-numbers-dont-establish).

| §6.8 target | Measured | Verdict |
|---|---|---|
| Cold start → interactive < 100 ms, nothing network-blocking | First frame 15.5–23.2 / ≤ 32.1 / ≤ 32.9 for first-run, returning and logged-in readers, with a normal network and with every response stalled 5 s | **Met** |
| Article open, L1 hit < 50 ms | Back to an article: 8.3–10.8 / ≤ 14.2 / ≤ 15.2 end to end; 3.1–6.8 in-process | **Met** |
| Article open, L2 hit < 150 ms | Typical article 18.7–19.6 / ≤ 25.4 / ≤ 27.5. Pathological 1.55 MB article 111.8–113.5 / 124.6–140.5 / 146.9–158.8 | **Met** at p50 and p95; one pathological sample in 40 went over (158.8) |
| Tab switch in `low_memory` mode (judged against the L2 target) | Typical 16.5–17.2 / ≤ 24.2 / ≤ 32.7. Pathological 108.7–109.0 / 124.3–141.7 / 140.8–143.1 | **Met**, tight for the pathological article |
| Article open, network, broadband p50 < 800 ms to first paint | Size mix 70.4 / 167.2 / 180.1 (N = 40); pathological 227.5 p50 | **Met**. Not painted lead-section-first (not implemented; see below) |
| Parse + layout of a 1.5 MB / 500+-reference article < 500 ms | 90.1 (criterion); 90.9 in the CI gate | **Met**, 5.5× headroom |
| Search suggestions render < 100 ms after the response | 20.3 / 20.6 / 20.9 | **Met** |
| Search debounce 150–250 ms | 214.0 / 214.6 / 214.7 (min 213.8) | **Met** |
| Scroll: no dropped input at 60 Hz, ≤ 16 ms per frame | 0 dropped of 300 key presses at 60 Hz and at 120 Hz, at 80×24, 120×40 and 200×60; 0 dropped in 60 Hz `gt` chord bursts. Frame draw ≤ 1.42 / ≤ 2.05 / ≤ 3.60 | **Met** |
| Memory < 150 MB RSS with 10 tabs | 61.6 MB (peak 65.8) for a mixed set; 84.4 MB for ten 1.5 MB articles | **Met** |
| `low_memory` mode < 50 MB | 33.2–42.4 MB settled; peak (VmHWM) 42.8 MB mixed, 47.0 MB for ten 1.5 MB articles | **Met**, peak 3 MB under the limit in the worst case |
| Steady-state cache-hit rate > 60% of article opens | 65.0–78.3% in a simulated reader under three browsing assumptions | **Not measurable here**: a simulation, not a field measurement. It clears 60% under all three assumptions |

## Machine, build and method

**Machine.** A cloud VM, not the PRD's "2020-era laptop": 4 vCPUs of an
Intel Xeon (family 6, model 207) at 2.10 GHz under KVM (Firecracker), 15.7
GiB RAM, no swap. Ubuntu 24.04.4 LTS, Linux 6.18.44, glibc 2.39.

**Build.** rustc 1.94.1, `cargo build --release` with Cargo's default
release profile (the crate sets no `[profile.release]`: opt-level 3, no
LTO), default features. Python 3.11.15 for the harness and the mock.
Measured commit: 7665c7a, 2026-09-24.

**Hygiene.** Every number on this page comes from one uninterrupted run of
`tests/perf/reproduce.sh` on an otherwise idle machine (1-minute load
average 0.8 on 4 vCPUs when it started, no compiler running while anything
was measured). The harness tracks every process it starts, SIGKILLs any
that outlive their scenario, and aborts if one survives: an orphaned busy
process from an earlier run would skew every later number. Each harness
row discards 2 warm-up samples and then takes N samples (N = 20 unless a
table says otherwise). Development runs on the same machine agreed with
these within a few percent at p50. Tails (p95, max) move more between
runs; where a tail decides a verdict, the verdict says so.

**Three instruments**, each covering what the others can't:

| Instrument | Measures | Where |
|---|---|---|
| criterion benches | In-process cost of parse, layout, the L1/L2 open paths and a frame draw, on the committed fixtures | `benches/perf.rs`, `cargo bench --bench perf` |
| Budget rows (blocking CI gate) | The same paths as release-mode tests with pass/fail bounds (median of 7 runs) | `src/perf_budget.rs`, `cargo test --release perf_budget` |
| pty harness | The real release binary end to end, on a pseudo-terminal of a fixed size with throwaway XDG directories, against the mock wiki server. Timed from the moment the harness writes a key to the moment it reads the output that completes the expected screen | `tests/perf/harness.py` |

In the harness, an article counts as painted when its header rows (the
title, then "N min read") are at the top of the screen. ratatui paints
rows top to bottom, so that is the first thing any open puts on screen.
The harness times bytes arriving on the pty. A real terminal emulator
then takes its own time to draw them, which no number here includes.

**Fixtures.** `tests/perf/fixtures/pathological.html` is a seeded article
shaped like Parsoid HTML 2.x: 1.55 MB with 520 references, 643 inline
reference markers, 1,670 `data-mw` attributes, 107 math nodes, 6 tables
with rowspan/colspan, an infobox, navboxes and figures. `typical.html` is
155 KB with 40 references. The mock also serves a 400-article
cross-linked corpus with a stated size mix (30% short ~80 KB, 40% typical
~155 KB, 20% long ~470 KB, 10% very long ~940 KB) and ten distinct 1.5 MB
articles for the memory worst case.

**Network emulation.** Used for the network opens, the broadband variants
of the cached opens, and the cache-hit simulation: 40 ms per request and
25 Mbit/s per connection, gzip on article HTML. The basis and limits are in
[`tests/perf/README.md`](../tests/perf/README.md#network-emulation).

## Results

### Cold start → interactive (< 100 ms)

From spawning the process to the first complete start-page frame; then
"interactive" adds the time the first keypress takes to show its effect
(`?` opens help; on first run, any key dismisses onboarding). First run
has no config. Returning has a config, cache, history and offline index
left by an earlier session. Logged in adds a valid token file. "Stalled"
makes the mock hold every response for 5 s, so anything on the startup
path that waits on the network would show up as ≥ 5,000 ms.

| Reader | Network | First frame | Interactive |
|---|---|---|---|
| First run | normal | 23.2 / 31.9 / 31.9 | 23.8 / 32.4 / 32.7 |
| First run | stalled 5 s | 21.5 / 22.8 / 23.1 | 22.0 / 23.3 / 23.5 |
| Returning | normal | 17.2 / 23.7 / 24.4 | 17.8 / 24.6 / 24.8 |
| Returning | stalled 5 s | 15.5 / 21.5 / 22.8 | 16.3 / 22.2 / 23.9 |
| Returning, logged in | normal | 20.2 / 26.8 / 28.5 | 20.9 / 27.4 / 29.0 |
| Returning, logged in | stalled 5 s | 20.9 / 32.1 / 32.9 | 21.3 / 32.7 / 34.0 |

**Met**, with ≥ 2.9× headroom at the worst max.

### Article opens from cache (L1 < 50 ms, L2 < 150 ms)

- **L1 hit:** in one process, `H` (Back) to an article already laid out at
  this width, from keypress to first paint.
- **Tab switch:** `gt` to another tab. In `low_memory` mode the target tab
  re-parses its compressed HTML (the same work as an L2 hit), so it's
  judged against the L2 target.
- **L2 hit:** a fresh process whose disk cache already holds the article,
  `:open` Enter to first paint.

Each was measured against a zero-latency mock ("local") and the broadband
emulation, because a cached open must not wait on the network. The two
agree.

| Open | Article | Local | Broadband | Target |
|---|---|---|---|---|
| L1 hit (Back) | typical | 8.3 / 12.3 / 13.8 | 9.2 / 11.5 / 11.8 | < 50 |
| L1 hit (Back) | pathological | 10.6 / 12.7 / 13.8 | 10.8 / 14.2 / 15.2 | < 50 |
| Tab switch | typical | 5.0 / 6.9 / 10.0 | 5.0 / 6.4 / 8.1 | < 50 |
| Tab switch | pathological | 6.0 / 7.0 / 7.0 | 5.7 / 6.4 / 6.6 | < 50 |
| Tab switch, `low_memory` | typical | 17.2 / 24.2 / 32.7 | 16.5 / 19.0 / 24.2 | < 150 |
| Tab switch, `low_memory` | pathological | 108.7 / 124.3 / 140.8 | 109.0 / 141.7 / 143.1 | < 150 |
| L2 hit | typical | 19.6 / 25.4 / 27.5 | 18.7 / 23.9 / 25.3 | < 150 |
| L2 hit | pathological | 111.8 / 124.6 / 146.9 | 113.5 / 140.5 / 158.8 | < 150 |

In-process (criterion, 120×40), the same paths cost:

| Path | pathological | typical |
|---|---|---|
| L1 hit: Back + frame | 6.3 (median of 7 in the CI gate: 3.1) | 6.8 (3.1) |
| Reopen with the layout cached but the article re-read from L2 and re-parsed | 51.2 | 8.1 |
| L2 hit: read + zstd + parse + layout + frame | 103.1 (gate: 101.1) | 13.0 (gate: 12.2) |

**Met.** L1 has ≥ 3× headroom. The typical L2 hit has ≥ 5× headroom. The
pathological L2 hit and `low_memory` tab switch are the tightest rows on
this page. Their p50 is about 110 ms, nearly all of it parsing and laying
out 1.55 MB of HTML (90 ms). Their p95 stays under 150 ms, but one L2 sample
in 40 reached 158.8 ms. The CI gate therefore holds the pathological L2
hit to a regression bound (300 ms) rather than the 150 ms target.

### Article open, network (broadband p50 < 800 ms)

Cold cache, a fresh process for every sample, broadband emulation,
`:open` Enter to first paint. Titles are drawn at random (seeded) from
the corpus. Results are reported by size class and as the mix.

| Articles | N | p50 / p95 / max |
|---|---|---|
| Size mix | 40 | 70.4 / 167.2 / 180.1 |
| short (~80 KB) | 13 | 61.3 / 69.5 / 71.1 |
| typical (~155 KB) | 17 | 70.7 / 75.4 / 77.8 |
| long (~470 KB) | 7 | 113.7 / 131.0 / 133.0 |
| very long (~940 KB) | 3 | 168.6 / 178.9 / 180.1 |
| pathological (1.55 MB) | 5 | 227.5 / 229.6 / 229.8 |

**Met**, with 11× headroom for the mix and 3.5× for the pathological
article. The "lead section first" in §6.8 is not implemented. The whole
article is fetched, parsed and laid out before the first paint. See
[Considered and not done](#considered-and-not-done).

### Parse + layout of the pathological article (< 500 ms)

criterion, in-process, 1.55 MB / 520 references:

| Step | pathological | typical |
|---|---|---|
| parse | 44.2 | 4.6 |
| layout at 80 / 120 / 200 columns | 46.8 / 45.1 / 46.5 | 5.1 / 5.2 / 5.4 |
| parse + layout (80 columns) | 90.1 | 10.1 |

**Met**, with 5.5× headroom. CI gates it at the PRD's 500 ms (measured
there as a median of 7: 90.9 ms). The typical article, which has no
§6.8 row, is gated at a 30 ms regression bound.

### Search suggestions (render < 100 ms after response; debounce 150–250 ms)

Typing a query at ~60 ms per key. Debounce is measured from the last
keystroke to the typeahead request reaching the mock; both sides use the
same monotonic clock. Render is measured from the response being decoded
to the first frame that shows it, using the app's own perf log.

| Row | p50 / p95 / max |
|---|---|
| Debounce | 214.0 / 214.6 / 214.7 |
| Render after response | 20.3 / 20.6 / 20.9 |
| Suggestion list draw, in-process (80×24 / 120×40) | 0.16 / 0.46 |

**Met.** The debounce is the configured 200 ms plus up to one 30 ms tick of
the search-mode poll. That tick is also most of the 20 ms render latency.

### Scroll (no dropped input at 60 Hz, ≤ 16 ms per frame)

`j` held down (key repeat) at 60 Hz and 120 Hz over the pathological
article, 300 presses per row. The app's perf log records the scroll offset
after every frame. "No dropped input" means the final offset equals the
number of presses. The frame times are the app's own draw times for each
frame.

| Size | Rate | Presses → final offset | Frame draw p50 / p95 / max |
|---|---|---|---|
| 80×24 | 60 Hz | 300 → 300 | 0.37 / 0.56 / 1.14 |
| 80×24 | 120 Hz | 300 → 300 | 0.37 / 0.56 / 0.68 |
| 120×40 | 60 Hz | 300 → 300 | 0.71 / 1.25 / 1.35 |
| 120×40 | 120 Hz | 300 → 300 | 0.75 / 1.16 / 3.60 |
| 200×60 | 60 Hz | 300 → 300 | 1.42 / 2.05 / 2.49 |
| 200×60 | 120 Hz | 300 → 300 | 1.34 / 1.98 / 2.68 |

Two-key chords: 10 bursts of seven `gt` at 60 Hz across three tabs, in
default and `low_memory` mode. Every key produced its frame and every
burst landed on the predicted tab (0 of 10 failed in either mode). An
earlier observation of `gt` losing ~1 key in 10, made by driving the app
through tmux, did not reproduce on a direct pty. The harness's own early
version of it was a race in the harness, since fixed.

In-process (criterion, pathological article): one frame is 0.19 / 0.44 /
0.92 ms at 80×24 / 120×40 / 200×60, and one scroll step plus its frame is
0.20 / 0.45 / 0.97 ms. A split-screen frame, which draws two layouts, is
2.8 ms at 120×40 and 3.5 ms at 200×60.

**Met**, with ≥ 4× headroom on the slowest frame seen.

### Memory (< 150 MB with 10 tabs; `low_memory` < 50 MB)

Ten tabs, each article in its own tab, with every tab visited once more
afterwards. VmRSS and VmHWM are read from `/proc` once output settles.
There are two sets. "Mixed" is the pathological article plus the corpus's
first nine long and very long articles (~8 MB of HTML). "Ten 1.5 MB" is
ten distinct 1.55 MB, 520-reference articles. N = 3 per row.

| Mode | Set | RSS p50 (min–max) | Peak (VmHWM) p50 (max) | Target |
|---|---|---|---|---|
| default | mixed | 61.6 (56.6–65.8) | 63.3 (65.8) | < 150 |
| default | ten 1.5 MB | 84.4 (83.4–84.5) | 84.4 (84.5) | < 150 |
| `low_memory` | mixed | 42.4 (30.1–42.8) | 42.7 (42.8) | < 50 |
| `low_memory` | ten 1.5 MB | 33.2 (32.8–33.5) | 47.0 (47.0) | < 50 |

**Met.** Default mode has 1.8× headroom in the worst case. `low_memory`
peaks 3 MB under its limit with ten 1.5 MB tabs. About 14 MB of every
figure is the binary's code and shared libraries, which no mode can shed.

### Steady-state cache-hit rate (> 60% of article opens): simulation

This target is a field KPI: the share of a real reader's article opens
served without waiting on the network. It can't be measured without real
readers. What was measured is a **simulated** reader: a seeded random walk
over the mock corpus with broadband emulation and prefetch on. Each step
is 20% Back, 10% reopen something read earlier, 5% jump to a new topic,
and 65% follow a link. There is a 4 s dwell after each open. The run is 3
sessions of 60 steps with a new process each session. The score comes
from the app's own open log, the same numbers `:stats` shows. The model
and its reasoning are in
[`tests/perf/README.md`](../tests/perf/README.md#cache-hit-rate-simulation).

| Assumption varied | Hit rate | Opens (L1 · disk · network) |
|---|---|---|
| Lead links clicked most; prefetch budgets scaled for the simulation's 30× time compression | 78.3% | 180 (46 · 95 · 39) |
| Lead links clicked most; default prefetch budgets (a lower bound) | 65.0% | 180 (46 · 71 · 63) |
| Any of the first 40 links clicked equally; scaled budgets | 68.3% | 180 (37 · 86 · 57) |

**Not validated** as a field number. The machinery clears 60% under each
stated assumption. The corpus has no pageview data, so prefetch ranks
links by position alone. Real readers also click by popularity, which the
real prefetch ranking uses.

## What changed to meet the targets

Each change was measured before and after on the same machine (quiet,
release build). The harness figures in this section come from development
runs with N = 10 unless stated. The tables above are from the final run.

1. **Paint only the visible rows** (f4f2e94). Every frame used to paint
   the whole laid-out article and let the widget skip to the visible rows,
   so a frame's cost grew with the article. On the pathological fixture a
   frame took 19.5 / 28.6 / 36.1 ms at 80×24 / 120×40 / 200×60
   (criterion), over the 16 ms scroll budget at every size. Now the layout
   is sliced to the visible window first. At the time that gave 0.58 /
   1.32 / 2.84 ms per frame, and a one-line scroll step went from 21.0 /
   26.7 / 34.9 ms to 0.63 / 1.43 / 2.91 ms.
2. **Nothing network-bound between a cached open and its first frame**
   (16caca3). Article enrichment (quality badge, redlink check, interest
   categories) was awaited inline after every open, L1 and L2 hits
   included. Against the broadband emulation that alone added 40–120 ms.
   It now runs on its own task and lands through a channel. Broadband,
   p50: typical Back 57.1 → 9.8 ms, typical L2 hit 144.9 → 19.2 ms,
   pathological Back 102.9 → 10.6 ms, pathological L2 hit 239.6 →
   111.7 ms.
3. **Back and Forward reuse the parsed document** (16caca3). An "L1 hit"
   still re-read the disk cache and re-parsed the article. The pathological
   fixture's Back took 61.2 ms locally, over the 50 ms target. The last
   four documents a tab navigated away from are now kept (none in
   `low_memory` mode) and served again while the disk cache still holds
   that revision. Back, local, p50: pathological 61.2 → 9.9 ms, typical
   15.3 → 8.3 ms. In-process: 52 → 3.3 ms.
4. **A network open parses its HTML once** (16caca3), not three times
   (for the cache key, the offline index and display). Offline-search
   indexing moved to the blocking pool. Together with (2), broadband
   network opens went from 206 → 68 ms p50 for the size mix (N = 20) and
   435 → 224 ms for the pathological article (N = 5).
5. **The startup notification poll no longer blocks startup** (16caca3).
   A returning logged-in reader's first frame waited on the notification
   poll. With every response stalled 5 s, the first frame went from
   5,072 ms to 27 ms.
6. **Reference markers are no longer followable links** (65eb134). This is
   a correctness fix found by the cache-hit simulation. Real Parsoid
   writes a footnote marker as `./Page_title#cite_note-1`, but only a bare
   `#cite_note-1` was recognized as same-page. So on real articles every
   `[n]` marker was a Tab-followable link whose Enter reopened the same
   article (counted as an L1 hit), and `K`'s footnote peek found no
   markers. The simulation's rates above were taken after the fix.

## Considered and not done

- **Lead-section-first painting of network opens.** §6.8's network row
  says "< 800 ms to first paint (lead section first)". wikitui doesn't
  paint the lead section before the rest of the article has arrived. It
  fetches the whole article's HTML, parses and lays it out, then paints.
  Against the broadband emulation the first paint already comes at ~70 ms
  p50 for the size mix and ~230 ms for the 1.55 MB article, far inside
  800 ms, so the extra fetch-and-stream machinery wasn't added. It would
  matter on much slower links. The per-size-class rows show how the time
  scales with article size.
- **Shared ownership of cached layouts** (`Arc<Layout>`). The L1 layout
  cache hands out a deep copy on every hit, and the reading view keeps a
  second copy. Measured with that copy included: Back takes 3.1 ms
  (median) to 6.3 ms (criterion mean) in-process for the 1.55 MB article,
  and a split-screen frame (two layouts) takes 3.5 ms at 200×60. That
  isn't worth an ownership change.
- **The pathological L2 hit's tail.** Its p50 (~110 ms) is parse and
  layout of 1.55 MB of HTML (~90 ms in-process) plus decompression and
  the frame. The occasional slower sample is allocator and scheduler noise
  on a shared VM. Making it reliably faster means a faster parser or
  incremental layout. That's a bigger change than this target (met at p50
  and p95) justifies today.

## What these numbers don't establish

- **Hardware.** This is one cloud VM, not a 2020-era laptop. A 2020 laptop
  CPU at full turbo is probably comparable per core, but throttled or on
  battery it can be slower. Most rows have 3× or more headroom here. The
  tight ones are the pathological L2 hit and the pathological `low_memory`
  tab switch (~110 ms p50 against 150 ms), which a slower machine could
  push over.
- **The network.** The server was a local mock with emulated latency and
  bandwidth, not live Wikipedia (unreachable from the measurement
  machine). There was no DNS, no TCP or TLS handshake and no server think
  time, and the article-size mix is a guess. Real first opens will be
  slower by at least a connection setup. The per-size-class rows let
  anyone re-weight the mix.
- **The terminal.** Times end when the bytes reach the pty. A terminal
  emulator's own rendering comes on top.
- **Memory** was measured only on Linux with glibc. Other allocators and
  platforms return freed memory differently.
- **The cache-hit rate** comes from a simulated reader, not a field
  measurement. Your own figure is in `:stats`.

## Reading your own numbers

`WIKITUI_PERF_LOG=<path>` makes wikitui append per-frame draw times,
typeahead render latency and startup phase marks as JSON lines to that
local file. It is off unless set, writes nowhere else and never touches
the network. The harness uses it for the frame-time and search rows.
`:stats` or `wikitui stats` shows your cache-hit rate over your last 500
article opens.
