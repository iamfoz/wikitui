# wikitui — Product Requirements Document

| | |
|---|---|
| **Product** | wikitui — a full-featured terminal (TUI) client for Wikipedia and Wikimedia sister projects |
| **Status** | Draft for review |
| **Version** | 1.0 |
| **Date** | 2026-07-09 |
| **Repository** | github.com/iamfoz/wikitui (AGPL-3.0) |

---

## Table of contents

1. [Executive summary](#1-executive-summary)
2. [Background & market opportunity](#2-background--market-opportunity)
3. [Vision, goals & non-goals](#3-vision-goals--non-goals)
4. [Target users & personas](#4-target-users--personas)
5. [Feature requirements](#5-feature-requirements)
6. [Technical architecture](#6-technical-architecture)
7. [Error & empty states catalog](#7-error--empty-states-catalog)
8. [Distribution, packaging & updates](#8-distribution-packaging--updates)
9. [Testing strategy](#9-testing-strategy)
10. [Licensing, attribution & trademark](#10-licensing-attribution--trademark)
11. [Success metrics](#11-success-metrics)
12. [Release plan & roadmap](#12-release-plan--roadmap)
13. [Risks & mitigations](#13-risks--mitigations)
14. [Open questions & required spikes](#14-open-questions--required-spikes)
- [Appendix A: API endpoint reference](#appendix-a-api-endpoint-reference)
- [Appendix B: Default keybinding sketch](#appendix-b-default-keybinding-sketch)
- [Appendix C: Built-in theme specifications](#appendix-c-built-in-theme-specifications)

---

## 1. Executive summary

wikitui is a keyboard-first terminal application for reading Wikipedia: search, browse, and deep-read articles rendered beautifully as styled text (with optional inline images on capable terminals), across tabs, with bookmarks, reading history, offline saved pages, an aggressive-but-polite page cache, and smart prefetching that makes the app feel faster than the website. Logged-in users get their watchlist, notifications, and cross-device Reading List sync via Wikipedia's own APIs.

**Why now.** The category leader, `wiki-tui` (Rust, 728★), was archived on 2026-06-25 with no successor, orphaning its user base across nixpkgs, AUR, and Terminal Trove. Its ceiling — search, TOC, links, themes — left the most-requested features unshipped: no tabs, no bookmarks, no offline mode, no images, no login, no caching. Every one of those ships in this PRD's v1 line: tabs, bookmarks, offline saved pages, images, and caching in v1.0; login in the v1.x series. No terminal Wikipedia client has ever shipped prefetching, reading history, or account integration; those are wikitui's defensible differentiators.

**Product thesis.** Terminal users are Wikipedia's heaviest lookup users, but the terminal has only ever offered them a degraded Wikipedia. wikitui inverts that: local-first storage, instant cache hits, link-hint navigation, and a reading experience tuned for monospace typography make the terminal the *best* place to read Wikipedia, not the fallback.

---

## 2. Background & market opportunity

### 2.1 The incumbent is gone

- **wiki-tui** (Rust/ratatui, 728★) — archived 2026-06-25, read-only; final release v0.9.2 (Dec 2025). Feature ceiling: search with previews, layered article views (not tabs), TOC jump, in-article links, vim keys, TOML themes. Its issue tracker is a ranked list of unmet demand: keybinding discoverability (#177, top-reacted), image support (#240), page caching (#259), alternative wikis (#264), plus a 403 breakage when Wikimedia began enforcing its User-Agent policy (#267) and early breakage from web scraping (#143).
- **wikicurses** (Python/urwid, 84★) — dead since 2018, but proved bookmarks, `:` commands, and arbitrary MediaWiki sites in a tiny app.
- **One-shot CLI tools** (`wikit` 291★, `wik` 679★, `Termipedia`) — summary printers, not reading environments.
- Nothing above ~150★ is currently maintained as an interactive Wikipedia TUI.

### 2.2 Lessons from adjacent TUIs

The UX grammar wikitui adopts is proven elsewhere: tabs-as-buffers (aerc, weechat), read/unread state and saved filters (newsboat), per-panel `?` cheatsheets (lazygit), fuzzy-finding everywhere (fzf, telescope), async-first architecture that never blocks the UI on network (yazi), and hypertext reading with per-tab history, bookmarks, themes, and caching (amfora — the closest analog, now in maintenance mode).

### 2.3 Differentiation summary

| Capability | Any prior Wikipedia TUI? | wikitui |
|---|---|---|
| Real tabs + session restore | No | v1.0 (splits, named sessions v1.x) |
| Bookmarks / read-later / annotations | wikicurses (dead) had basic bookmarks | v1 |
| Page cache + offline saved pages | No interactive client | v1 |
| Prefetching (any kind) | No | v1–v1.x |
| Inline images | No (wiki-tui #240 never shipped) | v1 |
| Reading history + trail graph | No | v1 / v1.x |
| Wikipedia login (watchlist, notifications, Reading List sync) | No | v1.x |
| Quality table/infobox/math rendering | Chronically weak everywhere | v1 |
| Curated theme presets (homebrew/night/paper) | Raw color config only | v1 |

---

## 3. Vision, goals & non-goals

### 3.1 Vision

*The best way to read Wikipedia is in your terminal.* A reader-first, local-first, respectful-by-design client that treats Wikipedia as a library you inhabit, not a website you visit.

### 3.2 Goals

| # | Goal | Measure (local/proxy — see §11) |
|---|---|---|
| G1 | Instant-feeling reading | L1 cache-hit article opens < 50 ms (L2 re-render < 150 ms, §6.8); steady-state cache-hit rate > 60% |
| G2 | Beautiful terminal rendering | Tables, infoboxes, references, math, and images render acceptably on the supported terminal matrix (§9) |
| G3 | Become the reference client for the orphaned wiki-tui niche | Packaged in Homebrew, crates.io, AUR, nixpkgs at v1.0; best-effort wiki-tui theme/keymap import (FR-TH-8) |
| G4 | Respectful API citizenship | 100% of background requests carry `maxlag`, serial background queue, descriptive User-Agent, budget-capped prefetch |
| G5 | Privacy as a feature | Zero telemetry; all personalization local; incognito mode; one-command data wipe |
| G6 | Genuinely accessible | Linear `--dump` pager mode; screen-reader guidance; high-contrast theme; keyboard-only guarantee |

### 3.3 Non-goals

- **Not a browser.** No general web browsing; external links open via the system browser (or are yanked to clipboard over SSH).
- **Not an editing suite.** No reverts, rollbacks, page moves, uploads, or talk-page *writing* — ever. (Minimal typo-fix editing is a v2 candidate; see FR-ACC-8.)
- **Not a sync service.** No bespoke sync server; sync rides on plain files (git/syncthing) and Wikipedia's own Reading List/watchlist APIs.
- **Not a scraper.** All content comes from documented APIs. Scraping broke wiki-tui early (#143), and ignoring API etiquette broke it again when User-Agent enforcement landed (#267); the first is banned by architecture rule, the second by NF-NET-2.
- **Not a whole-corpus mirror.** Kiwix/ZIM solves "all of Wikipedia offline"; wikitui saves *pages you chose* (a ZIM read-backend is a v2+ candidate, FR-OFF-8).

---

## 4. Target users & personas

1. **The terminal dweller** — developer in tmux who wants `wikitui "raft consensus"` mid-task without touching the mouse or leaving the terminal. Cares about: startup speed, typeahead search, sane defaults, clipboard yank.
2. **The rabbit-holer** — recreational deep reader who opens 15 links in background tabs at midnight. Cares about: tabs, link hints, history, the trail view, prefetch making every hop instant.
3. **The researcher/writer** — keeps a reading queue, annotates bookmarks, exports Markdown with attribution into notes. Cares about: sessions, tags, notes, export, search operators.
4. **The Wikipedian** — logged-in editor triaging their watchlist over SSH. Cares about: watchlist feed, notifications, contributions, thanks, watch/unwatch keybind.
5. **The offline/low-bandwidth reader** — commuter, field worker, or metered-connection user. Cares about: saved pages, read-later auto-download, byte budgets, honest offline indicators.
6. **The accessibility-first user** — screen-reader or low-vision user. Cares about: `--dump` linear mode, TTS piping, high-contrast theme, no flashing.

---

## 5. Feature requirements

Feature IDs are stable and referenced throughout. **Priority:** P0 = required for the milestone listed; P1 = should-have; P2 = opportunistic. **Phase:** MVP (v0.x alpha) → v1.0 → v1.x → v2. Complexity: S/M/L. The roadmap in §12 gives the cross-feature cut.

### 5.1 Reading & rendering (RD)

The core promise: MediaWiki API in, beautiful text out.

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-RD-1 | **Article view**: fetch Parsoid HTML (§6.2), convert to internal document model, render as styled, wrapped, scrollable text with theme-aware headings, emphasis, lists, blockquotes (gutter-bar style), and code blocks (syntax-highlighted). MVP interim behavior for content whose full rendering lands at v1.0: tables render via the collapse-to-list path (FR-RD-4), infoboxes as a plain key-value block (FR-RD-5), images as `[image: alt text]` placeholders — a decision, not an accident | P0 | MVP | L |
| FR-RD-2 | **Links**: internal links visually distinct from external; OSC 8 hyperlinks emitted for terminals that support them, plain styled text elsewhere. Redlink styling arrives with redlink detection (FR-DL-5, v1.0; earlier only if SP-1 confirms Parsoid pre-marks redlinks) | P0 | MVP | M |
| FR-RD-3 | **Sections**: heading hierarchy preserved; sections derived from Parsoid `<section>` markup; drives TOC, folding, and position memory | P0 | MVP | S |
| FR-RD-4 | **Tables**: Unicode box-drawing rendering with column sizing, horizontal scroll for wide tables; automatic **collapse-to-list** ("Label: value") when the table exceeds viewport width or in accessible mode; flattening pass for rowspan/colspan and images-in-cells. MVP ships collapse-to-list only | P0 | v1.0 (MVP: collapse-to-list) | L |
| FR-RD-5 | **Infoboxes**: detected via Parsoid `data-mw` template metadata (fallback: `class="infobox"` heuristic), rendered as a boxed key-value card — right sidebar on wide terminals, top block on narrow; images suppressed outside full-color theme. MVP ships plain key-value block | P0 | v1.0 (MVP: plain block) | M |
| FR-RD-6 | **References**: `[n]` markers styled as links; References section rendered as numbered list; see footnote peek FR-NV-4 | P0 | MVP | M |
| FR-RD-7 | **Math**: extract TeX source from MathML `alttext`/annotation in Parsoid HTML. v1: inline `⟨TeX⟩` passthrough with distinct styling + Unicode superscript/subscript normalization for simple cases. v1.x: optional Unicode layout rendering (utftex/libtexprintf behind a feature flag). Full-color mode may image-render formula SVGs — contingent on Mathoid endpoint status (SP-4); degrades to TeX passthrough | P1 | v1.0 → v1.x | M–L |
| FR-RD-8 | **Images**: inline images in full-color theme via terminal graphics (§6.3): protocol auto-detection kitty → iTerm2 → sixel → Unicode half-blocks → alt-text placeholder; captions in dim italics below; alt text always available; galleries as captioned strips or lists | P0 | v1.0 | L |
| FR-RD-9 | **Typography controls**: configurable max line measure (default 88 cells, centered column), ragged-right default with optional justification (river-capped), optional soft hyphenation (Knuth–Liang patterns) | P1 | v1.0 (measure) / v1.x (justify, hyphenate) | M |
| FR-RD-10 | **Unicode correctness**: grapheme-cluster-aware width measurement; East-Asian-Ambiguous width configurable (`ambiguous_width = 1|2`); CJK per-character line breaking; IPA/combining-char safe. Tested against CJK Wikipedias from MVP onward | P0 | MVP | M |
| FR-RD-11 | **Reading-time estimate** in article header and search results (word count ÷ configurable WPM, default 230) | P2 | v1.0 | S |
| FR-RD-12 | **Plain dump mode**: `wikitui --dump <title>` renders the article as linear plain text to stdout (pipe to `less`, scripts, screen readers). No alternate screen, no cursor addressing | P0 | MVP | S |

### 5.2 Navigation (NV)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-NV-1 | **Link navigation, two models**: Tab/Shift-Tab cycling through links (muscle memory) AND vimium-style **link hints** — `f` overlays 2-char home-row labels on visible links, typed label follows; `F` opens in background tab. Hints survive reflow | P0 | MVP (cycling) / v1.0 (hints) | M |
| FR-NV-2 | **TOC sidebar**: toggleable pane (`t`) with the section tree; fuzzy section jump (`gs`) | P0 | MVP | S |
| FR-NV-3 | **Section folding**: `za` fold/unfold, `zM`/`zR` all; folded sections render as `▸ History (12 ¶, 4 subsections)`; in-page search auto-unfolds hits | P1 | v1.0 | M |
| FR-NV-4 | **Footnote peek**: on a `[n]` marker, `K` opens a floating popup with the reference content (resolved locally from the Parsoid DOM — no network); `gK` jumps to References; `Ctrl-o` returns | P1 | v1.0 | M |
| FR-NV-5 | **Link preview popup**: on a focused internal link, `K` shows title, Wikidata description, thumbnail (full-color mode), and lead extract from the page-summary source (§6.2); visually distinct from footnote peek; shares the prefetch cache | P1 | v1.0 | S |
| FR-NV-6 | **In-page search**: `/` incremental highlight-all, `n`/`N` cycle, smart-case, match counter ("3/17") | P0 | MVP | M |
| FR-NV-7 | **Breadcrumbs**: status-bar trail of the current tab's path ("Turing → Enigma → Bletchley Park"), middle-truncated; `gb` opens the full stack as a picker | P2 | v1.0 | S |
| FR-NV-8 | **Reading position memory**: per (wiki, title) store scroll offset, fold state, focused link keyed to revid; on revisit offer "resume at §Legacy? (r)" toast; anchor-based fallback when the article changed | P1 | v1.0 | S |
| FR-NV-9 | **Mouse support (optional, never required)**: scroll wheel, click-to-follow-link, click TOC entries, click tab bar. All functions keyboard-reachable (FR-ACS-3) | P1 | v1.0 | M |
| FR-NV-10 | **Clipboard**: `y` yanks canonical article URL, `Y` yanks a Markdown link, visual-selection yank of text; OSC 52 so yanking works over SSH | P0 | MVP | S |

### 5.3 Search (SR)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-SR-1 | **Typeahead search**: debounced (150–250 ms, in-flight cancellation) title completion with Wikidata one-line descriptions, via REST `search/title` or `prefixsearch` | P0 | MVP | M |
| FR-SR-2 | **Full-text search**: ranked results with highlighted snippets, size, last-edit date; pagination via API continuation; Tab toggles "go to page" vs "search in pages" modes | P0 | MVP | M |
| FR-SR-3 | **Search operators, first-class**: pass through CirrusSearch syntax (`intitle:`, `incategory:`, `insource:/regex/`, `hastemplate:`, `deepcat:`, `articletopic:`, `morelike:`, `prefix:`); operator cheat-sheet on `?` inside the search prompt; operator-name completion | P1 | v1.0 | S |
| FR-SR-4 | **"Did you mean"**: surface the Action API search `suggestion`/`rewrittenquery` fields on zero/poor results | P1 | v1.0 | S |
| FR-SR-5 | **Random article**: `gr`/`:random` via `list=random` (ns 0); "random good article" variant filters by page assessment ≥ GA (batch 10 random titles + one assessments query) | P1 | v1.0 | S |
| FR-SR-6 | **Related articles**: `morelike:{title}` powers a "Related" panel on every article (stable substrate; the legacy `page/related` endpoint is deprecated) | P1 | v1.0 | S |
| FR-SR-7 | **Offline search**: saved pages and cache are full-text indexed locally (SQLite FTS5); when offline, the search box searches local content and labels results "(offline)" | P1 | v1.x | M |

### 5.4 Tabs, splits & sessions (TB)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-TB-1 | **Tabs, buffer-style**: cheap and unlimited; `gt`/`gT` cycle; fuzzy tab picker with titles; tab bar renders `[3/17] Alan Turing` on overflow; close-undo (`u`) | P0 | v1.0 | M |
| FR-TB-2 | **Back/forward history**: MVP ships a single-view back/forward stack (`H`/`L`) preserving scroll and fold state; at v1.0 each tab gets its own independent stack; branching back-then-click forks a trail edge rather than destroying the forward stack | P0 | MVP (single view) / v1.0 (per-tab) | S |
| FR-TB-3 | **Open in background tab**: `F`-hint or `Ctrl-Enter` queues the article into an unfocused tab; loads lazily or via the prefetch budget; loading indicator in tab bar | P0 | v1.0 | S |
| FR-TB-4 | **Splits**: `:vsplit`/`Ctrl-w v` for side-by-side articles with independent scroll; hint-open into "other pane"; `:set scrollbind` sync-scroll for comparing articles (pairs with bilingual mode FR-ML-3) | P1 | v1.x | L |
| FR-TB-5 | **Session save/restore**: crash-safe continuous persistence of tabs, stacks, scroll positions, layout; auto-restore on start (config); named sessions `:mksession ww2-research` / `wikitui --session ww2-research` | P1 | v1.0 (auto-restore) / v1.x (named) | M |

### 5.5 History & trail (HS)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-HS-1 | **Reading history**: every visit logged locally (title, wiki, timestamp, dwell time, referrer article); `Ctrl-h` fuzzy history picker, recency-weighted, with descriptions | P0 | v1.0 | M |
| FR-HS-2 | **Visited-link styling**: previously-read articles render in "visited" theme color (newsboat-style read state) | P1 | v1.0 | S |
| FR-HS-3 | **Trail view**: `:trail` renders the wander graph — nodes = articles (sized by dwell), edges = link-follows — as a navigable tree (git-log-graph aesthetics); Enter reopens a node; export as Markdown/DOT/Mermaid. v1.x ships tree layout; true DAG layout is v2 | P1 | v1.x | L |
| FR-HS-4 | **History hygiene**: `:history clear [range]`, incognito never writes (FR-PR-3), retention window configurable | P0 | v1.0 | S |

### 5.6 Bookmarks, read-later & annotations (BM)

One coherent model, resolving local-vs-server state explicitly:

- **Bookmarks** = durable local references. Tag-first organization (folders simulated as tag views). Stored fields: title, wiki, revid-at-bookmark, optional section anchor, timestamps, tags, note.
- **Read-later queue** = ephemeral intent, FIFO/priority, distinct from bookmarks; enqueueing auto-triggers offline save so commute reading is pre-downloaded.
- **Server-side lists** (logged-in, opt-in): Wikipedia **Reading Lists** (the official mobile apps' sync backend) can mirror bookmarks for cross-device/app sync; the **watchlist** can mirror exactly one designated tag ("watched") — watchlist semantics (edit-monitoring) are surfaced honestly, not conflated with bookmarking.

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-BM-1 | Bookmark CRUD with tags; `m` bookmarks current article (or focused section); fuzzy picker filtered by tag expressions (`#crypto #ww2`) | P0 | v1.0 | M |
| FR-BM-2 | **Annotations**: `ba` opens `$EDITOR` (or inline editor) for a Markdown note on a bookmark; notes are FTS-searchable and shown in picker preview | P1 | v1.0 | S |
| FR-BM-3 | **Read-later queue**: `rl` enqueues from link or article; queue view shows reading-time estimate + offline status; opening auto-dequeues (config); enqueue triggers T0 offline save (FR-OFF-4) | P1 | v1.0 | M |
| FR-BM-4 | **Export**: `:bookmarks export --format md|html|json` — grouped by tag; each entry = title, canonical URL, note, saved date, attribution footer (§10); Netscape-HTML for browser import | P1 | v1.0 | S |
| FR-BM-5 | **Reading List sync** (logged-in, opt-in): two-way sync of bookmarks with Extension:ReadingLists (`action=readinglists`), the same backend as the official iOS/Android apps; conflict policy: server wins on order, local wins on tags/notes (which the server can't store — kept local-only) | P1 | v1.x | M–L |
| FR-BM-6 | **Watchlist mirror** (logged-in, opt-in): one designated tag mirrors to the real watchlist via `action=watch` (batch, watch token); clearly labeled as "watching edits", separate from bookmarks | P1 | v1.x | M |
| FR-BM-7 | **Plain-file storage**: bookmarks/queue as line-oriented JSONL in the data dir, stable ordering, designed for git/syncthing sync (§6.4); documented merge behavior | P0 | v1.0 | S |

### 5.7 Offline: page cache & saved pages (OFF)

Two distinct contracts: the **cache** is best-effort and evictable; **saved pages** are pinned, integrity-checked, and quota-visible.

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-OFF-1 | **Two-layer cache**: L2 = raw Parsoid HTML (compressed, zstd) + metadata keyed by `(wiki, page_id, revid)`; L1 = laid-out render cache keyed additionally by `(renderer schema, width bucket, render flags)`. Content at a revid is immutable → never invalidated, only evicted | P0 | MVP | M |
| FR-OFF-2 | **Stale-while-revalidate**: on open, render cached copy immediately, revalidate `latest.id` in background (cheap bare-metadata call, batched where possible; conditional requests with ETag/`If-None-Match` where the server supports them — spike SP-5); on change show non-blocking "updated — r to reload" toast. TTL backstops: metadata revalidate ≥ 24 h old, force refetch ≥ 30 d old (config) | P0 | MVP | M |
| FR-OFF-3 | **Eviction & caps**: default cache cap 500 MB (config); MVP ships the hard cap with crude LRU (no unbounded alpha cache); v1.0 refines to SLRU/TinyLFU (binge sessions don't flush favorites); L1 evicted aggressively; saved pages excluded from accounting | P0 | MVP (cap+LRU) / v1.0 (SLRU) | M |
| FR-OFF-4 | **Saved pages, tiered depth**: T0 article only (default); T1 + thumbnails at terminal resolution; T2 + link-target summaries (batched extracts) so link-peek works offline, optionally full text of first N lead links. Bulk cost preview before confirming ("Category:Physics → 412 articles, est. 14 MB. Proceed?") | P0 | v1.0 | M |
| FR-OFF-5 | **Bulk save**: save a bookmark tag, a category (`list=categorymembers`, depth 1), or the current tab set; runs on the background queue with budgets + `maxlag` | P1 | v1.0 | M |
| FR-OFF-6 | **Offline indicator**: status glyph ● live / ◐ cached ("cached 3 h ago") / ○ offline (shows revision date); navigating to an uncached link offline offers "queue for fetch when online" / "search saved pages" | P0 | v1.0 | S |
| FR-OFF-7 | **Saved-page export**: Markdown, plain text, HTML (print stylesheet), each with attribution footer (§10) | P1 | v1.0 | S |
| FR-OFF-8 | **ZIM read-backend** (open a Kiwix ZIM as an offline source, bridging online + Kiwix worlds) | P2 | v2 | L |
| FR-OFF-9 | **Storage budgets stated**: ~30 KB/article text-only, ~150–500 KB with T1 thumbnails; 1,000 saved articles ≈ 30–500 MB (validate in SP-6) | — | — | — |

### 5.8 Prefetching (PF)

All prefetching shares one substrate: a **single, strictly serial, lowest-priority background queue** that runs only when idle, pauses instantly for foreground requests, sets `maxlag=5` on every request, honors `Retry-After`, and trips a circuit breaker (suspend prefetch after k consecutive 429/5xx). Prefetch fills the L2 cache only (no L1 render, no images unless specified).

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-PF-1 | **Link prefetch (current article)**: candidate links ranked by `w1·lead-position + w2·log(pageviews) + w3·interest-affinity`; pageviews fetched in **one batched call** (`generator=links` + `prop=pageviews`, 50 titles), never per-article fanout; top-N (default 5) prefetched; the link under the cursor is always a candidate. Link-target summaries batch-fetched to power instant previews | P0 | v1.0 | M |
| FR-PF-2 | **Trending prefetch**: one Wikifeeds featured-content call per day (`tfa`, `mostread`, `onthisday`, picture of the day) seeds both the start page (FR-DL-1) and prefetch of TFA + top-10 most-read bodies on idle | P0 | v1.0 | S |
| FR-PF-3 | **Interest-learning prefetch (local, private, inspectable)**: topic-affinity vector over article categories (maintenance categories filtered), exponentially time-decayed (half-life 30 d, config). Signals: open = 1.0, normalized dwell ≤ +2.0, scroll ≥ 70% = +1.0, bookmark = +3.0, save = +5.0, explicit "not interested" strongly negative. Candidates via `morelike:` on top-affinity reads ∩ trending. No neural nets; the whole model is KBs of human-readable state | P1 | v1.x | L |
| FR-PF-4 | **Transparency**: every prefetched entry stores a reason string ("linked from *Fourier transform* (lead, 12k views/day)", "trending #3", "morelike your Cryptography reading, affinity 0.82"); `:prefetch-log` panel exposes them. Doubles as the debug tool | P0 | v1.0 | S |
| FR-PF-5 | **Budgets**: per-day prefetch byte budget (default 20 MB), per-hour background request budget (default 100, sized to fit the strictest documented anonymous API limit — see §6.5); metered-network detection via NetworkManager D-Bus where available, else `prefetch.metered = never|reduced|always` config; prefetch off in incognito | P0 | v1.0 | M |
| FR-PF-6 | **Kill switch**: `:set prefetch=off` and config equivalent; the app is fully functional without prefetch | P0 | v1.0 | S |

### 5.9 Accounts & login (ACC)

Login is additive: the client is fully functional logged out.

**Auth decision** (resolving the research disagreement): primary = **OAuth 2.0 authorization-code + PKCE** as a public client against Meta-Wiki's central OAuth (`/w/rest.php/oauth2/...`), flow: open system browser → approve → loopback redirect (`http://127.0.0.1:<port>/callback`) captured by a short-lived local listener; if loopback URIs are rejected, fall back to a manual code-paste flow — itself unverified for OAuth 2.0 (only 1.0a "oob" is documented), so SP-2 tests both; if neither works, owner-only consumers become the only path. Access tokens live 4 h, refresh tokens 365 d → transparent refresh. Fallbacks: **owner-only consumer** token (power users, works pre-approval, good for SSH/headless since no device flow exists) and **bot passwords** (third-party MediaWiki wikis without OAuth). **Raw-password `clientlogin` is out of v1**: it requires handling CAPTCHAs/2FA UI and tempts password storage; revisit only if the OAuth spike fails. Passwords are never stored; tokens go to the OS keychain, with a 0600 file fallback (warned) for headless boxes.

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-ACC-1 | OAuth 2.0 PKCE login + token refresh + keychain storage; grants requested: `basic`, `viewmywatchlist`, `editmywatchlist`, `viewmyprivateinfo` (read Reading Lists), `editmyprivateinfo` (write Reading Lists — exact grant sufficiency verified in SP-7); never `editmyoptions` | P0 | v1.x | M |
| FR-ACC-2 | **Watchlist**: view raw list (`list=watchlistraw`) and a "what changed in my topics" activity feed (`list=watchlist`, changes since last seen); `w` toggles watch on the current article (`action=watch`, optional expiry) | P0 | v1.x | M |
| FR-ACC-3 | **Notifications (Echo)**: status-bar unread badge (`meta=notifications&notprop=count`), notifications pane split alerts/messages, mark-read | P1 | v1.x | M |
| FR-ACC-4 | **Contributions**: "my contributions" view (`list=usercontribs`); works for any username | P2 | v1.x | S |
| FR-ACC-5 | **Talk-page reading**: `T` flips article ↔ talk page (talk pages are ordinary pages) | P1 | v1.0 | S |
| FR-ACC-6 | **Thank**: `action=thank` from history/contribs views — a purely positive community gesture | P2 | v1.x | S |
| FR-ACC-7 | **Preferences**: read-only surface of relevant user prefs (`meta=userinfo&uiprop=options`); no preference writing in v1 | P2 | v1.x | S |
| FR-ACC-8 | **Typo-fix editing** (v2 decision gate): select sentence → `$EDITOR` → diff preview → save with summary + minor flag, base-revid conflict detection, `editpage` grant. Never: reverts, moves, uploads, talk-page writing, template editing | P2 | v2 | L |
| FR-ACC-9 | **Session security**: `:logout` revokes locally and links to `Special:OAuthManageMyGrants` for server-side revocation; tokens included in `clear-data --auth` | P0 | v1.x | S |

### 5.10 Multi-language & sister projects (ML)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-ML-1 | **Language switcher**: `:lang` fuzzy picker of the article's langlinks displaying autonyms ("Deutsch", "日本語"); preferred languages pinned to top | P0 | v1.0 | S |
| FR-ML-2 | **Language config**: `languages = ["de", "en"]` fallback chain for search/open; langlinks power "available in your preferred language" hints | P1 | v1.0 | S |
| FR-ML-3 | **Bilingual side-by-side**: split panes showing the same article in two languages (via langlinks) with optional heuristic section-sync scrolling; UX copy sets expectations (interwiki articles are not translations) | P2 | v1.x | M |
| FR-ML-4 | **Sister projects**: Wiktionary, Wikivoyage, Wikiquote, Wikinews via the same APIs on their domains; wiki picker; per-project rendering tweaks (Wiktionary's template-heavy entries) are best-effort | P1 | v1.x | M |
| FR-ML-5 | **Any MediaWiki site**: config `[wiki]` sections pointing at arbitrary `api.php`/`rest.php` (ArchWiki etc. — wiki-tui #264); feature degradation matrix (no Wikifeeds, no pageviews, maybe no Parsoid REST → `action=parse` fallback) | P1 | v1.x | M |
| FR-ML-6 | **CJK correctness**: see FR-RD-10; tested from MVP | P0 | MVP | M |
| FR-ML-7 | **RTL (experimental, v2)**: detect RTL page language; emit VTE bidi escape sequences where supported; optional app-side fribidi-style logical→visual reordering (off by default — double-reordering hazard); document supported emulators (mlterm, VTE family). Full bidi is explicitly not promised | P2 | v2 | L |
| FR-ML-8 | **UI localization of wikitui itself**: English-only through v1.x by explicit choice; string table architecture from MVP so v2 localization is tractable; keybindings never assume QWERTY (all rebindable, FR-CS-3) | P2 | v2 | M |

### 5.11 Theming & appearance (TH)

Ship **six built-in themes** (full specs in Appendix C):

1. **`terminal`** (default) — inherits the user's palette: no background set, 16 ANSI colors only. Respects the user's carefully-tuned terminal.
2. **`full`** — truecolor, Wikipedia-adjacent accents, inline images ON (image pipeline arrives at v1.0; the theme ships at MVP rendering alt-text placeholders until then), syntax highlighting, subtle UI chrome.
3. **`homebrew`** — phosphor green (#33ff33) on black; monochrome luminance ramp; images off (halfblock-mono optional).
4. **`night`** — dark-adapted red on black; red kept at/near full brightness (#ff2b2b) because pure red on black is ~5.25:1 (passes WCAG AA, fails AAA — hierarchy via weight/size, never dimming); optional **amber** variant for higher luminance.
5. **`paper`** — dark grey (#3a3a3a) on cream (#f5f0e1); truecolor with defined 256-color approximations.
6. **`contrast`** — high-contrast accessibility theme meeting ≥ 7:1 on the 16-color palette.

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-TH-1 | Theme files: TOML with **semantic slots** (fg, bg, surface, accent, link, visited-link, heading, emphasis, quote, code, match, warning, error…) auto-expanded to shades; user themes in config dir; base16 scheme import | P0 | MVP | M |
| FR-TH-2 | Runtime switching: `:theme paper` with instant repaint; command-palette entries | P0 | MVP | S |
| FR-TH-3 | **Capability degradation**: truecolor → 256 → 16 → mono, auto-detected (`$COLORTERM`, queries); every built-in theme defines its 256/16-color approximations | P0 | MVP | M |
| FR-TH-4 | **Auto light/dark**: query OSC 11 (guarded by DA1 so unsupporting terminals don't hang), luminance-classify, pick configured light/dark theme; honor DEC mode 2031 change notifications for live switching where available | P1 | v1.0 | M |
| FR-TH-5 | Honor `NO_COLOR` (strip color), `CLICOLOR_FORCE`; non-TTY output is plain text | P0 | MVP | S |
| FR-TH-6 | Theme contrast linting: warn at load when any fg/bg pair < 4.5:1 (except user override flag) | P1 | v1.0 | S |
| FR-TH-7 | Images are a per-theme property (`images = true|false`) overridable at runtime (`:set images=off`) | P0 | v1.0 | S |
| FR-TH-8 | wiki-tui migration: best-effort importer for its `theme.toml` (keys mapped onto wikitui slots) and keybinding config (backs goal G3) | P1 | v1.0 | S |

### 5.12 Personalization & reading comfort (PC)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-PC-1 | Line measure, margins, paragraph spacing, interline spacing (blank-row 1.5/2.0), inter-word spacing +1 — honestly marketed as "spacing options" (cell grids can't letter-space) | P1 | v1.0 | S–M |
| FR-PC-2 | **TTS piping**: rendered plain text piped per-paragraph to a user command (`tts_command = "espeak-ng -s 160"`; macOS `say`); play-from-cursor/stop keybinds | P1 | v1.x | S |
| FR-PC-3 | **Reading stats (local-only)**: articles read, time, streaks, topic distribution; feeds the interest model; suppressed by incognito; `wikitui stats --explain` shows top topics | P2 | v1.x | M |
| FR-PC-4 | Per-article render overrides (`:set` width/justify/images for this tab) | P2 | v1.x | S |

### 5.13 Command system & keybindings (CS)

Architectural rule: **every feature is a named command first, keybinding second.** This yields the palette, ex-commands, macros, and deep links from one substrate.

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-CS-1 | **Command palette** (`Ctrl-p`): fuzzy over all commands, showing current keybinding + one-line help | P0 | v1.0 | M |
| FR-CS-2 | **`:` ex-command line**: `:open Alan Turing`, `:lang de`, `:tab close!`, `:set theme=paper`; completion for command names and args (titles complete via typeahead endpoint) | P0 | MVP | M |
| FR-CS-3 | **Configurable keybindings**: TOML keymap, modes (normal/hint/search/palette), chords, leader key; vim-flavored defaults + an emacs preset; `:map` runtime | P0 | v1.0 | M |
| FR-CS-4 | **`?` help overlay**: context-sensitive per-view cheatsheet (lazygit-style) — the direct answer to wiki-tui's most-upvoted issue | P0 | MVP | S |
| FR-CS-5 | **Command sequences** (macros): named user commands in config: `[command.morning] run = ["open-feed", "tab-open-random-good"]`. No raw-keystroke recording (fragile under async UI) | P2 | v1.x | S |
| FR-CS-6 | **CLI & deep links**: at MVP — `wikitui "Alan Turing"`, any wikipedia.org URL (incl. `#section`, `?oldid=`), `wikitui de:Alan_Turing`, `wikitui --search "insource:foo"`, `--dump`. Flags arrive with their features: `--incognito` (v1.0, FR-PR-3), `--session <name>` (v1.x, FR-TB-5) | P0 | MVP (core) | S |
| FR-CS-7 | **Protocol handler**: `wiki://` x-scheme-handler via .desktop on Linux; macOS requires an app-bundle shim (v2, best-effort) | P2 | v2 | M |
| FR-CS-8 | **First-run onboarding**: one-screen tour (search key, help key, theme picker); creates config with commented defaults | P1 | v1.0 | S |

### 5.14 Accessibility (ACS)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-ACS-1 | **Linear mode**: `--dump`/pager mode (FR-RD-12) is the honest screen-reader path — plain scrolling stdout, tables collapsed to lists, images as alt text, `[link: target]` URLs printed. Documented with recommended reader setups (Orca + VTE terminals, Speakup) | P0 | MVP | S |
| FR-ACS-2 | **Redraw discipline**: minimize gratuitous repaints (screen readers re-announce changed regions); no decorative animation by default | P0 | MVP | arch |
| FR-ACS-3 | **Keyboard-only guarantee**: every action key-reachable; mouse strictly optional | P0 | MVP | policy |
| FR-ACS-4 | **No-motion mode**: config + env (`WIKITUI_ANIMATIONS=none`) disables smooth scroll, spinners (static "loading…"), blink | P1 | v1.0 | S |
| FR-ACS-5 | `contrast` theme (§5.11); respects terminal palette; ≥ 7:1 targets. Ships with the other five built-in themes | P1 | MVP | S |
| FR-ACS-6 | `ACCESSIBLE=1` env implies: linear-leaning behavior, no-motion, plain link URLs, collapse-to-list tables | P1 | v1.0 | S |

### 5.15 Privacy (PR)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-PR-1 | **No telemetry, ever.** Only network calls are to the wikis the user reads (plus prefetch, and the explicitly opt-in, off-by-default version check of §8). CI check asserts no analytics endpoints exist in the binary. Honest docs: Wikimedia still sees your IP/requests; prefetch additionally reveals *predicted* interests (mitigations: toggle, budgets, incognito) | P0 | MVP | policy |
| FR-PR-2 | **Local-only personalization**: interest model & stats never leave the machine; human-readable state files | P0 | v1.x | — |
| FR-PR-3 | **Incognito**: `--incognito` / keybind: no history, no stats, no interest updates, no prefetch; cache entries tagged for wipe at session end; visible status glyph. All persistence routes through one gate (architectural requirement, do early) | P0 | v1.0 | M |
| FR-PR-4 | **`wikitui clear-data [--history|--cache|--stats|--auth|--all]`**; `--auth` deletes tokens locally + links to server-side grant revocation | P0 | v1.0 | S |
| FR-PR-5 | Cache dir relocatable (`$XDG_CACHE_HOME`) so users can point it at tmpfs | P1 | v1.0 | S |

### 5.16 Delight & discovery (DL)

| ID | Requirement | Pri | Phase | Cx |
|---|---|---|---|---|
| FR-DL-1 | **Start page**: today's featured article (badge + extract), top-5 most-read with view counts, "in the news", on-this-day strip, picture of the day (image pipeline; skipped in text themes) — all from the one daily Wikifeeds call (FR-PF-2). Configurable (`startpage = feed|blank|resume`) | P0 | v1.0 | M |
| FR-DL-2 | **On this day**: `:today` panel with tabs events/births/deaths/holidays/selected | P1 | v1.0 | S |
| FR-DL-3 | **Article quality badges**: ★FA/+GA/B/C/Start/Stub from `prop=pageassessments` (batched) in status bar, search results, link previews; enables "random good article" and quality-weighted prefetch. Lift Wing ML score as non-enwiki fallback (v2, cached, sparing) — Lift Wing is gateway-hosted (`api.wikimedia.org/service/lw/...`), a stated exception to §6.2 rule 1 whose post-deprecation status is verified in SP-4; if it dies, non-PageAssessments wikis simply show no badge | P1 | v1.0 | S |
| FR-DL-4 | **Citation-needed highlighting**: `:set show-cn` renders `{{citation needed}}` (matched by template name in `data-mw`) as dim `[citation needed]`; `]c`/`[c` jump; status-bar count ("12 uncited claims") | P2 | v1.x | S–M |
| FR-DL-5 | **Redlink handling**: batched `generator=links&prop=info` missing-flag check (cached aggressively, one extra request per article, skippable on budget); redlinks render dim/struck; following one shows "doesn't exist yet" card with similar-search | P1 | v1.0 | M |
| FR-DL-6 | **Wiki-walk game**: `:game` start→goal navigation by links only; HUD with clicks + timer; daily seeded puzzle; shareable ASCII result card (Wordle-style). Optimal-path oracle explicitly out of v1 (client-side BFS is rate-hostile) | P2 | v1.x | M |
| FR-DL-7 | **TIL widget**: rotating fact on start page from on-this-day "selected" or a random Good Article extract; Enter dives in | P2 | v1.x | S |
| FR-DL-8 | Easter eggs: `:xyzzy` → "Nothing happens."; achievement toasts from trail stats ("Rabbit Hole: 15 articles in one session"); tasteful, off with `pro = true` | P2 | v1.x | S |

---

## 6. Technical architecture

### 6.1 Stack decision

**Rust + ratatui + crossterm**, tokio async runtime, reqwest HTTP, rusqlite (SQLite), `keyring` crate, `ratatui-image` for graphics, custom Parsoid-HTML→document-model→layout renderer.

Rationale:
- Inherits the archived incumbent's niche and its users' expectations (wiki-tui was ratatui); the ratatui ecosystem is the most active TUI ecosystem today.
- Single static binary (musl) — packaging breadth was a recorded wiki-tui failure mode (#18, #230).
- Performance targets (§6.8) — cold start < 100 ms and 1.5 MB-HTML parses — favor native.
- `ratatui-image` solves graphics-protocol negotiation (kitty/iTerm2/sixel/halfblocks) off the shelf.

Accepted costs (tracked as risks/spikes): no native OSC 8 in ratatui (issue #1028 — use span-level escape injection or the hyperrat approach); utftex/libtexprintf is a C library (optional feature flag, v1.x); Rust ZIM bindings are immature (ZIM is v2 anyway); the rich-text layout engine is bespoke (it is also the moat — SP-1 sizes it).

### 6.2 API strategy

**Ground rules** (reflecting 2025–26 Wikimedia API churn):

1. **Per-wiki endpoints only.** Build against `https://{lang}.wikipedia.org/w/api.php` (Action API), `/w/rest.php/v1/` (core REST), and `/api/rest_v1/` (legacy REST where still needed). Do **not** build on `api.wikimedia.org` gateway paths: the API Portal shut down June 2026 and gateway deprecation began July 2026.
2. **Endpoint templates are config-overridable** (per-wiki `[endpoints]` section with sane defaults) so WMF endpoint moves are absorbable by a config update, not only a release.
3. **Primary content source: Parsoid HTML** — `GET /w/rest.php/v1/page/{title}/html` (forward-looking) with `/api/rest_v1/page/html/{title}` as compatible alternate while it lasts. Parsoid HTML is semantically annotated (RDFa/`data-mw`): `rel=mw:WikiLink|mw:ExtLink` classifies links; `typeof=mw:Transclusion` + `data-mw` exposes template names (infobox/citation-needed detection); `<section>` wrappers give structure; math nodes carry TeX in `alttext`. Legacy-parser HTML (`action=parse&prop=text`) is the fallback for third-party wikis.
4. **Link previews / prefetch metadata**: `GET /api/rest_v1/page/summary/{title}` (extract, description, thumbnail, one call) **with a stated fallback** — a single batched Action API call (`prop=extracts|pageimages|description`, ≤ 20 titles for extracts) — because RESTBase is sunsetting and no core-REST summary successor is confirmed (SP-4).
5. **Search**: typeahead via `/w/rest.php/v1/search/title` (limit ≤ 100) or `list=prefixsearch`; full-text via `list=search` (snippets, `suggestion` did-you-mean, CirrusSearch operators).
6. **Feeds**: per-wiki Wikifeeds `GET /api/rest_v1/feed/featured/{yyyy}/{mm}/{dd}` and `/feed/onthisday/{type}/{mm}/{dd}`; availability varies per language (`/feed/availability`).
7. **Popularity**: in-band `prop=pageviews` / `list=mostviewed` (PageViewInfo extension, batched 50 titles — the link-ranking primitive); AQS `wikimedia.org/api/rest_v1/metrics/pageviews/top/...` for raw charts. There is no API ranking a page's outgoing links — the client joins links × pageviews itself, in one batched call.
8. **Auth**: §5.9. **Writes** (watch, thank, reading lists, options): Action API with CSRF tokens (`meta=tokens`).
9. **Multi-wiki addressing**: language editions and sister projects are hosts; langlinks (`prop=langlinks&llprop=autonym|langname|url`) and Wikidata (`prop=pageprops&ppprop=wikibase_item`) for cross-wiki mapping.
10. **Batching discipline**: 50 titles per multi-value param (500 with `apihighlimits`); `formatversion=2` always; gzip always.

### 6.3 Rendering pipeline

```
Parsoid HTML ──parse──▶ Document Model (typed AST) ──layout──▶ Laid-out lines (L1 cache)
                          │                                        │
                          ├─ sections, links, refs,                ├─ width-bucketed, theme-independent
                          │  tables, infobox, math,                │  spans + semantic styles
                          │  images, templates                     ▼
                          ▼                                     paint (theme applied at paint time)
                       L2 cache (persisted)
```

- The **document model** is the product's central artifact: it feeds terminal layout, `--dump` plain text, Markdown/HTML export, TTS text, and the FTS index. It is versioned (schema version participates in L1 cache keys).
- Layout is incremental and async: lead section paints first on cache miss (skeleton + cached summary if available); long articles layout progressively.
- **Images**: media list from Parsoid DOM; thumbnails at cell-grid-appropriate resolution via `prop=pageimages`/`imageinfo&iiurlwidth`; protocol negotiation and placement via ratatui-image; tmux passthrough cases documented (sixel in tmux ≥ 3.4; kitty via Unicode placeholders) with known scroll-artifact caveats.
- **Terminal-size degradation**: full layout ≥ 100 cols (sidebars); compact 80×24 (infobox inline, TOC as popup); minimal < 80 (single column, no chrome); hard floor 60×16 with an honest "terminal too small" screen.

### 6.4 Storage & data layout

XDG-compliant on Linux; platform-native equivalents on macOS (`~/Library/Caches`, `~/Library/Application Support`) and Windows (`%LOCALAPPDATA%`) via the standard directories library.

| Location | Contents | Sync posture |
|---|---|---|
| `$XDG_CONFIG_HOME/wikitui/` | `config.toml`, `keymap.toml`, `themes/*.toml` | dotfiles (git) |
| `$XDG_DATA_HOME/wikitui/` | bookmarks.jsonl, readlater.jsonl, saved pages store + index, exports | git/syncthing-friendly: line-oriented, stable ordering, no read-churn |
| `$XDG_STATE_HOME/wikitui/` | history.sqlite, sessions, interest model (JSON), reading stats | local; optionally synced |
| `$XDG_CACHE_HOME/wikitui/` | L2 content cache (SQLite + zstd blobs), L1 render cache, thumbnails | never synced; safe to delete |

- SQLite for indexes/history/FTS5; content blobs zstd-compressed. Single-writer advisory lock per store; second instances open read-only + queue writes (multi-instance beyond that is explicitly best-effort in v1 — R-13).
- Sync = documentation + file design, not an engine: JSONL append-mostly files merge acceptably under syncthing; conflicts surface as `.conflict` copies the app offers to merge (union by record id).
- Blobs are not synced; the second machine re-fetches from the synced saved-pages *list*.

### 6.5 Networking & respectful-client requirements (testable NFRs)

The 2025–26 Wikimedia rate-limit changes make politeness a survival requirement. The official global limits (documented at mediawiki.org/wiki/Wikimedia_APIs/Rate_limits, rolled out ~March 2026) are **500 req/h anonymous per IP and 5,000 req/h authenticated, enforced with per-minute smoothing** — per the primary research these apply to the Action API and REST APIs on Wikimedia wikis, though a competing reading scopes them to the (now-deprecated) api.wikimedia.org gateway, and older rest_v1 guidance says ≤ 200 req/s. **Design to the strictest plausible reading (500/h anon on per-wiki endpoints); verify live in SP-3.** FR-PF-5's default background budget (100 req/h) fits either reading.

- **NF-NET-1**: One global HTTP client; priority queues foreground > revalidation > prefetch; background strictly serial; foreground max 2 in flight.
- **NF-NET-2**: `User-Agent: wikitui/{ver} (https://github.com/iamfoz/wikitui; {contact}) {lib}/{ver}` on 100% of requests.
- **NF-NET-3**: `maxlag=5` on 100% of non-interactive requests; on lag error honor `Retry-After`, pause ≥ 5 s.
- **NF-NET-4**: On 429 honor `Retry-After`; exponential backoff + jitter on network errors; circuit breaker suspends all background traffic after k consecutive 429/5xx.
- **NF-NET-5**: Single-flight dedup (two tabs, one fetch); batch every metadata lookup (revids, pageviews, extracts, assessments, redlinks) to API batch caps.
- **NF-NET-6**: Honor `Cache-Control`/`ETag`; conditional revalidation where supported; never cache-bust.
- **NF-NET-7**: Prefetch budgets (FR-PF-5) enforced at the queue, not at call sites.
- **NF-NET-8**: Short timeouts (3–5 s) with immediate cache fallback (`stale-if-error`); failed fetches queue for reconnect.
- **NF-NET-9**: Logged-in/OAuth requests identify the client for elevated limits where applicable; the app encourages (never requires) login when prefetch budgets saturate.

### 6.6 Security

- **SEC-1 — Terminal injection**: article text, titles, and snippets are attacker-influenceable. All remote-derived text is sanitized before terminal emission: control characters and escape sequences stripped/escaped; the renderer emits only its own styling codes. Fuzz-tested (§9).
- **SEC-2 — OSC 8 hygiene**: emitted hyperlink URIs restricted to `https://` targets derived from validated wiki URLs; no arbitrary schemes from content.
- **SEC-3 — Parser hardening**: HTML parse and zstd/gzip decompression have size/depth limits (pathological-payload budget: 10× the largest real article); parser fuzzing in CI.
- **SEC-4 — Token storage**: OS keychain primary; 0600 file fallback with a visible warning; no passwords ever stored (§5.9).
- **SEC-5 — External command boundaries**: `$EDITOR`, `tts_command`, `bookmark-cmd`-style hooks run user-configured commands only — never content-derived strings; article text passed via stdin, not argv.
- **SEC-6 — Supply chain**: `cargo audit`/`cargo deny` in CI; lockfile committed; release binaries built in CI with provenance attestation.

### 6.7 Configuration system

- **Format**: TOML. `config.toml` (general + `[wiki.*]` sections + `[endpoints]` overrides), `keymap.toml`, `themes/*.toml`.
- **Precedence**: CLI flags > environment (`WIKITUI_*`, plus honored standards `NO_COLOR`, `CLICOLOR_FORCE`, `ACCESSIBLE`) > config file > built-in defaults.
- **Versioned schema**: `config_version` key; automatic migration with a printed diff summary; unknown keys warn, never crash.
- **Live reload**: SIGHUP / `:config reload` for theme + keymap; network/storage settings apply on restart.
- **`wikitui config doctor`**: validates config, theme contrast (FR-TH-6), keymap conflicts, terminal capabilities — prints a capability report (truecolor? images? OSC 8? clipboard?).

### 6.8 Performance targets (product requirements, validated by SP-1/SP-6)

| Metric | Target |
|---|---|
| Cold start → interactive | < 100 ms (nothing network-blocking on the startup path) |
| Article open, L1 cache hit | < 50 ms |
| Article open, L2 hit (re-layout) | < 150 ms |
| Article open, network (broadband p50) | < 800 ms to first paint (lead section first) |
| Parse+layout of a pathological article (1.5 MB HTML, 500+ refs) | < 500 ms on 2020-era laptop |
| Search suggestions | render < 100 ms after response; debounce 150–250 ms |
| Scroll | no dropped input at 60 Hz redraw budget (≤ 16 ms/frame) |
| Memory | < 150 MB RSS with 10 tabs; `low_memory` mode < 50 MB |
| Steady-state cache-hit rate (the KPI that justifies §5.7–5.8) | > 60% of article opens |

---

## 7. Error & empty states catalog

Every state below has designed UX, not a raw error string:

| State | Behavior |
|---|---|
| **Disambiguation page** | Detected (pageprops `disambiguation` / summary `type=disambiguation` — verify SP-4); rendered as a first-class chooser list with descriptions, not prose |
| **Redirect** | Follow + notice line "Redirected from *X*" with `:noredirect` to view the redirect page |
| **Search: zero results** | Show "did you mean" suggestion (FR-SR-4), spelling-rewritten query, and an offline-results section if applicable |
| **Redlink followed** | "Article doesn't exist yet" card: similar-title search + yankable create-URL |
| **Bookmark/saved page whose target moved or was deleted** | Title→pageid aliasing repairs moves silently (notice on next open); deletions marked in pickers with last-cached copy offered |
| **Offline, uncached link** | Inline card: "queue for fetch when online" / "search saved pages" (FR-OFF-6) |
| **429 / maxlag on interactive request** | Status-bar notice "Wikipedia is busy — retrying in Ns"; automatic; never a modal |
| **Article HTML fails to parse** | Degrade to plaintext extract with a "degraded rendering" banner + `:report-page` helper that captures a repro bundle |
| **Terminal too small** | Graceful layout tiers (§6.3); hard floor screen with required size |
| **First run** | Onboarding screen (FR-CS-8); no network until first user action beyond the feed fetch (which is skippable) |
| **Crash** | Panic handler restores terminal state (no broken terminals — a recorded wiki-tui complaint class), writes a local crash report, prints its path (never auto-submitted — FR-PR-1) |
| **Login: OAuth failure/expiry** | Clear re-auth prompt; app continues logged-out; background queue degrades to anonymous budgets |

---

## 8. Distribution, packaging & updates

- **Platforms**: Tier 1 — Linux (x86_64, aarch64, glibc + musl static), macOS (arm64, x86_64). Tier 2 — Windows (Windows Terminal ≥ 1.22 recommended; sixel there, no kitty protocol), *BSD.
- **Channels at v1.0**: crates.io, Homebrew, AUR, nixpkgs; GitHub release binaries (static musl + checksums + provenance). Post-v1: Debian/Fedora, winget/scoop. Packaging breadth is a launch requirement, not a follow-up (wiki-tui lesson).
- **Terminal floor**: any VT100-ish terminal ≥ 60×16; feature detection upgrades experience (§6.7 doctor prints the matrix).
- **Updates**: package-manager-first; **no self-update**; optional version check against the GitHub releases endpoint is **off by default** (tension with FR-PR-1 resolved by opt-in), surfaced as a status-bar hint only.
- **Toolchain policy**: MSRV declared via `rust-version` in Cargo.toml, tracking stable minus two releases; MSRV bumps are minor-version events (matters for nixpkgs/Debian/Fedora toolchain pinning).
- **API-churn response plan**: endpoint templates in config (§6.2); a pinned "known endpoint status" doc in the repo; CI contract tests (§9) detect Wikimedia moves early; point releases + config-only mitigation path.

---

## 9. Testing strategy

- **Renderer snapshot tests**: recorded Parsoid-HTML fixtures → document model → laid-out text snapshots. Corpus must include: a Featured Article, a 1.5 MB+ page, 500+ references, deeply nested rowspan/colspan tables, math-heavy (Maxwell's equations), CJK (ja/zh), RTL (ar/he), IPA/diacritics, galleries, disambiguation, redirects, stubs.
- **API contract tests**: nightly (not per-PR) live checks against real endpoints with the project UA, serial + maxlag — asserting response shapes, not content; failures open issues automatically. Per-PR tests run against recorded fixtures (VCR-style).
- **Terminal matrix**: automated where possible (PTY harness asserting emitted sequences per capability profile), manual smoke checklist per release: kitty, WezTerm, Ghostty, foot, Alacritty, iTerm2, GNOME Terminal, Windows Terminal, xterm; each inside and outside tmux.
- **Unicode/width property tests**: grapheme clusters, EAW-ambiguous, zero-width joiners — wrapping never overflows a cell row.
- **Fuzzing**: HTML parser, wikitext-adjacent edge cases, decompression, and the SEC-1 sanitizer (CI, continuous).
- **Accessibility protocol**: a written manual test script executed per release with Orca (VTE terminal) and Speakup against `--dump` mode and the main UI; regressions are release blockers for `--dump`.
- **Performance regression**: criterion benches for parse/layout on the pathological corpus wired to CI thresholds (§6.8).

---

## 10. Licensing, attribution & trademark

- **App license**: AGPL-3.0 (already in repo). Dependency licenses audited via `cargo deny` (AGPL-compatible only).
- **Content attribution (display)**: article footer + `:info` show license (CC BY-SA 4.0; some content GFDL), revision id, and a permalink to the article's history (author attribution per Wikimedia reuse terms).
- **Content attribution (export)**: every export (bookmarks, saved pages, trail) embeds an attribution footer: title, revision permalink, license, retrieval date. User annotations are visually separated and marked as the user's own (ShareAlike applies to the article text they accompany — noted in export docs).
- **Images**: `pilicense=free` default for cached/saved thumbnails; **non-free/fair-use images are displayed transiently only and never written into saved pages/exports** (policy default; `--include-nonfree` exists with a warning, off by default). License/credit line from `extmetadata` shown with images where available.
- **Trademark**: "Wikipedia" and the puzzle-globe are WMF trademarks. wikitui must: state "unofficial client, not endorsed by the Wikimedia Foundation" in README/about; never use the puzzle-globe or Wikipedia wordmark logo in branding; the name "wikitui" makes no trademark claim. Name-collision note: distinct from the archived `wiki-tui` project — README disambiguates and credits it.
- **API terms**: comply with WMF API usage policies (UA policy, rate limits, robot policy as applicable) — operationalized as NF-NET-* requirements.

---

## 11. Success metrics

No telemetry (FR-PR-1) means measuring without surveillance:

| Signal | Source | Target (12 mo post-v1.0) |
|---|---|---|
| Adoption | Package download counts (crates.io, Homebrew analytics, AUR votes), GitHub stars | Exceed wiki-tui's peak (728★); top-3 "wikipedia" result on Terminal Trove |
| Retention proxy | Opt-in annual user survey linked from README/release notes | ≥ 60% of respondents use weekly |
| Product quality | Issue-tracker themes; median time-to-first-response | No recurring "broken terminal"/"garbled render" class ≥ 2 releases |
| Performance promise | **Local-only** stats screen shows the user their own cache-hit rate | > 60% steady-state (users can verify our headline claim themselves) |
| API citizenship | Zero UA-policy/rate-limit blocks from WMF; contract tests green | 0 incidents |
| Accessibility | `--dump` protocol pass per release; community feedback | 0 open release-blocking a11y regressions |

---

## 12. Release plan & roadmap

The milestone lists below are the authoritative cross-feature cut and are kept in sync with the Phase column of every §5 table. **Sequencing & resourcing assumption:** 1–2 maintainers; milestones are scope-gated, not date-gated. Rough relative sizing: v0.5 ≈ 3–4 months from SP-1 completion (the renderer dominates); v1.0 ≈ +4–6 months; v1.x is a series of minors, each cut when its feature cluster is done. If the v1.0 cut proves too large under real velocity, the pre-agreed shed order is: mouse → folding → footnote peek → quality badges → redlink handling (never: bookmarks, saved pages, prefetch, tabs).

### v0.5 "Reader" (MVP alpha)

Goal: better than wiki-tui ever was, for pure reading.

> Search (typeahead + full-text) · article rendering (text, links, sections, lists, quotes, code, basic refs; tables collapse-to-list, infoboxes as plain key-value blocks) · TOC + fuzzy section jump · link cycling · in-page search · single view with back/forward stack · two-layer cache (L1+L2) + stale-while-revalidate + hard size cap (crude LRU) · 6 themes (incl. `contrast`; `full` without images until v1.0) + degradation + NO_COLOR · `:` commands · `?` help overlay · `--dump` · CLI deep links · clipboard yank · CJK-correct wrapping · config file + doctor · crash-safe terminal restore

### v1.0 "Librarian"

Goal: the librarian features — everything local.

> Tabs + background tabs + per-tab history + session auto-restore · link hints · full tables + infobox cards · inline images (full theme) · bookmarks + tags + annotations + export · read-later queue · saved pages (T0–T2) + bulk save + offline indicator · history + fuzzy recall + visited styling · prefetch: link-rank + trending, budgets, `:prefetch-log`, kill switch · start page + on-this-day · search operators + did-you-mean + random + related panel · language switcher + language config · talk-page reading · quality badges · redlink handling · line measure + spacing options · keybinding config + palette + first-run · wiki-tui theme/keymap import · incognito + clear-data · math passthrough · reading-position memory · folding · footnote peek + link previews · mouse · light/dark auto · a11y: no-motion, `ACCESSIBLE` · SLRU cache eviction · packaging: crates.io/Homebrew/AUR/nixpkgs

### v1.x "Member" (series of minors)

> OAuth login + watchlist + notifications + Reading List sync + watchlist mirror + thanks · interest-learning prefetch · splits + scrollbind · bilingual mode · sister projects + arbitrary MediaWiki wikis · offline FTS search · trail view (tree) · TTS · reading stats · citation-needed · wiki-walk game · named sessions · macros · utftex math (feature flag)

### v2 "Scholar" (directional)

> Typo-fix editing (decision gate FR-ACC-8) · RTL experimental · ZIM read-backend · trail DAG layout + optimal-path oracle · `wiki://` protocol handler · UI localization · Lift Wing quality fallback

---

## 13. Risks & mitigations

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R-1 | **Wikimedia API churn** (RESTBase sunset, gateway deprecation, endpoint moves) breaks content or previews | High | High | Per-wiki endpoints only; config-overridable endpoint templates; Action-API fallbacks specified for every rest_v1 dependency (summary, feeds noted); nightly contract tests; SP-4 |
| R-2 | **Rate limits stricter than assumed** — prefetch becomes hostile traffic | Medium | High | Design to strictest documented reading; budgets + circuit breaker are P0; SP-3 live verification; login path for elevated limits |
| R-3 | **Custom renderer under-scoped** — the single largest engineering item | Medium | High | SP-1 spike with pathological corpus before v0.5 commit; document model kept small; progressive layout |
| R-4 | **OAuth consumer approval friction** (Meta review lead time; loopback URI acceptance unverified) | Medium | Medium | SP-2 registration spike early; owner-only consumer + bot-password fallbacks ship regardless; login is v1.x, not v1.0 |
| R-5 | Terminal-graphics fragmentation (tmux artifacts, Alacritty no-sixel) creates "images are broken" perception | High | Medium | Progressive enhancement with honest capability report (`config doctor`); halfblock fallback always works; docs matrix |
| R-6 | Screen-reader users poorly served by grid TUI despite effort | High | Medium | `--dump` is the honest committed path (P0, release-gated); don't over-promise |
| R-7 | RTL expectations unmeetable in most emulators | High | Low (scoped) | Explicitly experimental/v2; emulator-support docs |
| R-8 | Name/trademark friction ("wikitui" vs "wiki-tui"; WMF marks) | Low | Medium | §10 posture; disambiguation + credit in README; no WMF branding |
| R-9 | Scope creep — this PRD is large | High | High | Hard milestone cuts in §12; every feature has an ID and a phase; MVP ships before any account feature is started |
| R-10 | Solo-maintainer burnout (killed wiki-tui and wikicurses) | Medium | High | Small core + plugin-friendly command architecture; CONTRIBUTING + good-first-issues from day one; boring, documented code |
| R-11 | SQLite/JSONL sync-conflict edge cases corrupt user data | Low | High | Append-mostly formats; `.conflict` surfacing; `config doctor` integrity check; backups before migration |
| R-12 | Hostile content (terminal injection) | Medium | High | SEC-1 sanitizer + fuzzing are P0, not hardening-later |
| R-13 | Multi-instance write collisions | Medium | Low | Advisory lock + read-only secondary (documented limitation in v1) |

---

## 14. Open questions & required spikes

Ordered. SP-1 and SP-4 gate v0.5 architecture decisions; SP-3 gates v1.0 prefetch defaults; SP-2 gates the v1.x login schedule.

| # | Spike / question | Gates |
|---|---|---|
| SP-1 | **Renderer sizing**: build a throwaway Parsoid-HTML→text renderer over the pathological corpus; measure parse/layout times vs §6.8 targets; validate document-model shape (incl. `data-mw-section-id`/section markup details, infobox `class` heuristic validity across wikis, Parsoid redlink pre-marking) | Stack commitment, v0.5 scope |
| SP-2 | **OAuth registration spike**: register a consumer on Meta; test loopback redirect URI acceptance AND whether an oob/manual-code-paste flow exists for OAuth 2.0 (documented only for 1.0a); measure approval lead time; verify owner-only token works against per-wiki endpoints and elevated rate limits | FR-ACC-1 design, v1.x schedule |
| SP-3 | **Rate-limit ground truth**: live-probe effective limits on `{lang}.wikipedia.org` `/w/api.php`, `/w/rest.php`, `/api/rest_v1` (anon vs authed); reconcile the contradictory documentation; set final default budgets for FR-PF-5 | Prefetch defaults |
| SP-4 | **rest_v1 / gateway successor watch**: confirm current status of `page/summary`, `feed/*`, `media-list`, `page/random`, Mathoid math-render endpoints (RESTBase sunset), and Lift Wing inference endpoints (gateway deprecation); implement + test the batched Action-API summary fallback; verify summary `type=disambiguation` field | FR-NV-5, FR-DL-1, FR-DL-3, FR-RD-7, §7 |
| SP-5 | **Conditional requests**: empirically test ETag/`If-None-Match` behavior on `/w/rest.php` and `/api/rest_v1` page endpoints | FR-OFF-2 revalidation math |
| SP-6 | **Storage multipliers**: measure real Parsoid-HTML sizes + zstd ratios across article-size deciles; validate FR-OFF-9 numbers | Cache/save quotas, docs claims |
| SP-7 | **ReadingLists API depth**: verify third-party OAuth consumers can use `action=readinglists`; which grants the write modules require (FR-ACC-1's list assumes `editmyprivateinfo`); list-size caps; conflict semantics | FR-BM-5, FR-ACC-1 |
| SP-8 | Echo cross-wiki notification param (`notcrosswikisummary`) existence | FR-ACC-3 |
| SP-9 | PageViewInfo `pvipdays` defaults/max; `list=mostviewed` availability across languages | FR-PF-1 |
| SP-10 | Keybinding ergonomics for non-QWERTY/IME users (community input) | FR-CS-3 defaults |
| SP-11 | macOS metered-network signal (likely none — confirm) and Windows equivalents | FR-PF-5 |
| SP-12 | Whether `morelike:` quality is acceptable as the only "related articles" source across non-English wikis | FR-SR-6 |

---

## Appendix A: API endpoint reference

Primary endpoints per capability (per-wiki hosts; `{w}` = e.g. `en.wikipedia.org`). Fallbacks noted where the primary is deprecation-exposed.

| Capability | Primary | Fallback / notes |
|---|---|---|
| Article HTML | `GET https://{w}/w/rest.php/v1/page/{title}/html` (Parsoid) | `GET https://{w}/api/rest_v1/page/html/{title}` (while it lasts); `action=parse&prop=text&parser=parsoid`; legacy parser for 3rd-party wikis |
| Page metadata / latest revid | `GET /w/rest.php/v1/page/{title}/bare` | `action=query&prop=info|revisions` (batched) |
| Sections/TOC (cheap) | `action=parse&prop=sections|tocdata` | Parsoid `<section>` parse |
| Summary (previews/prefetch) | `GET /api/rest_v1/page/summary/{title}` | **batched** `action=query&prop=extracts|pageimages|description` (`exintro&explaintext`, `exlimit≤20`) |
| Typeahead | `GET /w/rest.php/v1/search/title?q=&limit=` | `list=prefixsearch`; `action=opensearch` |
| Full-text search | `action=query&list=search&srsearch=` (snippets, `srinfo=suggestion`) | `GET /w/rest.php/v1/search/page` |
| Feeds (TFA/mostread/OTD/POTD) | `GET /api/rest_v1/feed/featured/{y}/{m}/{d}`; `/feed/onthisday/{type}/{m}/{d}` | availability per wiki via `/feed/availability`; no confirmed core-REST successor (SP-4) |
| Pageviews (batch, link ranking) | `action=query&prop=pageviews` (+`generator=links`); `list=mostviewed` | AQS `wikimedia.org/api/rest_v1/metrics/pageviews/{per-article|top}/...` |
| Images | `prop=pageimages` (`pithumbsize`), `prop=imageinfo` (`iiurlwidth`, `extmetadata` for license) | REST `/w/rest.php/v1/file/{title}` |
| Langlinks | `prop=langlinks&llprop=autonym|langname|url` | `GET /w/rest.php/v1/page/{title}/links/language` |
| Wikidata mapping | `prop=pageprops&ppprop=wikibase_item`; `wikidata.org` `wbgetentities` sitelinks | |
| Random | `action=query&list=random&rnnamespace=0` | rest_v1 `page/random/*` (status unverified) |
| Quality | `prop=pageassessments` (enwiki + PageAssessments wikis) | Lift Wing article-quality models (v2; gateway-hosted — survival unverified, SP-4) |
| Auth (Wikimedia) | OAuth 2.0: `https://meta.wikimedia.org/w/rest.php/oauth2/{authorize,access_token}` (PKCE) | owner-only consumer; bot passwords via `action=login` (3rd-party wikis) |
| Watchlist | `list=watchlistraw`, `list=watchlist`; write `action=watch` (+`expiry`) | tokens via `meta=tokens&type=watch` |
| Notifications | `meta=notifications`; `action=echomarkread` | |
| Reading Lists | `action=readinglists` (setup/create/update; cross-wiki) | SP-7 |
| Contributions | `list=usercontribs` | |
| Thanks | `action=thank&rev=` | |
| Math (optional) | TeX from Parsoid MathML `alttext` | Mathoid render endpoints (rest_v1, status unverified) |

Operational constants: batch ≤ 50 titles (500 with `apihighlimits`); `formatversion=2`; extracts `exlimit≤20` (whole-article: 1); REST search limit ≤ 100.

## Appendix B: Default keybinding sketch

Illustrative, not final (SP-10; all rebindable):

```
Global        ?  help overlay          Ctrl-p  command palette      :  ex command
              q  close view/tab        Q  quit (confirm)            Esc  cancel/back layer
Search        s  or /  (from start) typeahead   Enter open   Tab toggle title/full-text mode
Reading       j/k ↓/↑ scroll     Ctrl-d/Ctrl-u half-page   gg/G top/bottom   Space page down
              t  TOC pane        gs  fuzzy section  za fold          /  in-page search, n/N
              f  link hints      F  hint → background tab            Tab/S-Tab  cycle links
              K  peek (link preview / footnote)     gK  go to references
              y  yank URL        Y  yank Markdown link               o  open in browser
Tabs          gt/gT  next/prev   bb  tab picker     u  reopen closed  Ctrl-Enter  bg-tab open
History       H/L  back/forward  Ctrl-h  history picker              :trail  wander graph
Library       m  bookmark        ba  annotate       rl  read-later    B  bookmark picker
              S  save offline    w  watch/unwatch (logged in)
Article       T  talk page       :lang  language    gr  random        i  article info/attribution
Modes         zz  incognito toggle    :theme <name>    :set images=on|off
```

## Appendix C: Built-in theme specifications

All themes define truecolor values plus explicit 256- and 16-color approximations (FR-TH-3). Contrast ratios computed against WCAG relative luminance; the linter (FR-TH-6) enforces ≥ 4.5:1 body text.

| Theme | bg | fg (body) | Headings | Links | Accent / notes |
|---|---|---|---|---|---|
| `terminal` | none (inherit) | default fg | bold | ANSI blue/underline | 16-color only; never sets bg; the safe default everywhere |
| `full` | `#101418` | `#d8dee9` | white/bold, accent underline rules | `#6fb3ff` (visited `#9d8cff`) | images ON; syntax highlighting; infobox card borders `#2e3440` |
| `homebrew` | `#000000` | `#33ff33` | `#66ff66` bold | `#99ff99` underline | monochrome green luminance ramp; images off; scanline-free honest phosphor cosplay |
| `night` | `#000000` | `#ff2b2b` | `#ff5555` bold | `#ff8080` underline | red held near full brightness (pure red/black ≈ 5.25:1 = AA, not AAA); hierarchy via weight, never dimming; `night-amber` variant `#ffb000` for higher luminance; both flagged unsuitable for protanopia in docs |
| `paper` | `#f5f0e1` | `#3a3a3a` | `#1a1a1a` bold | `#1a5276` underline | light theme; needs truecolor (256 approx defined); images optional sepia-toned |
| `contrast` | ANSI black | ANSI bright-white | bright-white bold | bright-cyan underline | ≥ 7:1 targets on the 16-color palette; pairs with `ACCESSIBLE` |

Theme file schema (excerpt):

```toml
# ~/.config/wikitui/themes/mytheme.toml
[meta]      name = "mytheme"  dark = true  images = false
[colors]    bg = "#101418"  fg = "#d8dee9"  accent = "#6fb3ff"
            link = "#6fb3ff"  link_visited = "#9d8cff"  heading = "#eceff4"
            quote = "#a3be8c"  code_bg = "#161b22"  match = "#ebcb8b"
            warning = "#d08770"  error = "#bf616a"  dim = "#4c566a"
[fallback]  palette256 = { bg = 234, fg = 253, accent = 111 }   # per-slot
            palette16  = { bg = "black", fg = "white", accent = "blue" }
```

---

*End of document.*
