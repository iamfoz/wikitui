# wikitui

wikitui is a keyboard-first terminal (TUI) client for reading Wikipedia and
Wikimedia sister projects: search, deep-read articles rendered as styled
text (with optional inline images), across tabs, with bookmarks, reading
history, offline saved pages, and a local page cache with smart prefetch.

**wikitui is an unofficial client, not endorsed by the Wikimedia Foundation.**
"Wikipedia" is a trademark of the Wikimedia Foundation; wikitui makes no
trademark claim on that name and does not use the Wikipedia wordmark or
puzzle-globe logo anywhere in its branding or documentation.

### Not to be confused with `wiki-tui`

wikitui is a distinct, unrelated project from
[`wiki-tui`](https://github.com/Builditluc/wiki-tui) (Rust/ratatui, 728★),
which was archived on 2026-06-25 with no successor. wikitui exists because
that gap opened up: this project picks up the same terminal-Wikipedia-reader
space and adds the features its issue tracker asked for most — tabs,
bookmarks, offline reading, images, caching — plus a best-effort importer
(`wikitui import-wiki-tui <path>`) for `wiki-tui`'s own `config.toml` theme
and keybinding settings, for anyone moving over. Credit to `wiki-tui` and
its contributors for charting this space first.

## Features

- **Article reading**: Parsoid HTML rendered as styled, wrapped, scrollable
  text — theme-aware headings, emphasis, lists, blockquotes, code blocks,
  full tables, infobox cards, inline images (half-block rendering, terminal
  graphics protocol detection), math passthrough, and CJK-correct line
  wrapping (double-width-aware, no mid-glyph splits).
- **Navigation**: link cycling, vimium-style link hints, table of contents
  with fuzzy section jump, in-page search, section folding, footnote peek
  and link-preview popups, quality badges, and redlink handling.
- **Tabs**: buffer-style tabs with per-tab history, background tab opens,
  and session auto-restore across restarts.
- **Splits & bilingual mode**: `:vsplit` for two independently-scrolled
  panes on the same tab set, `:set scrollbind` to lock their scroll
  together, and `:bilingual` to open the current article's other-language
  edition (via langlinks) side by side for comparison reading.
- **Library**: bookmarks with tags and annotations, a read-later queue,
  saved (pinned) offline pages with bulk save, and bibliography export in
  APA/Harvard/MLA/Chicago style.
- **History**: persistent, fuzzy-searchable reading history with visited-
  link styling; incognito mode suppresses it (and prefetch) for the session.
- **Prefetch**: a budgeted, kill-switchable background prefetcher (link-rank
  and trending sources) with a `:prefetch-log` transparency panel, so the
  app feels faster than the website without hiding what it fetched or why.
- **Search**: typeahead and full-text search, CirrusSearch-style operators
  (`intitle:`, `incategory:`, `insource:`, `morelike:`, and more), did-you-
  mean, random article, a related-articles panel, and offline full-text
  search (`:search-offline`) over saved and cached pages (SQLite FTS5) when
  there's no network.
- **Start page**: on-this-day panel and a "today I learned" widget.
- **Themes**: six built-in themes (`terminal`, `full`, `homebrew`, `night`,
  `paper`, `contrast`) plus user themes, with 256-/16-color degradation and
  `NO_COLOR` support; `:theme <name>` or cycle at runtime.
- **Multi-wiki & multi-language**: a language switcher with langlink
  discovery and a fallback chain; the Wikimedia sister projects (Wiktionary,
  Wikivoyage, Wikiquote, Wikinews) alongside Wikipedia; and, via
  `[wiki.<name>]` sections in `config.toml`, **any MediaWiki site**
  (ArchWiki and similar) pointed at its own `api.php`/`rest.php`, with
  graceful feature degradation where a non-Wikimedia wiki lacks Wikifeeds,
  pageviews, or Parsoid REST support; `:wiki <name>` switches at runtime.
- **RTL** *(experimental)*: detects right-to-left article languages (Arabic,
  Hebrew, and similar) and either hands reordering to the terminal's own VTE
  bidi support or falls back to an app-side logical-to-visual reorder. Not a
  full bidi implementation, by design — see `PRD.md` FR-ML-7.
- **Accounts** *(OAuth 2.0 + PKCE login)*: watchlist, Echo notifications,
  contributions, thanks, read-only prefs, gated typo-fix editing with
  conflict detection, and two-way **Reading List sync** (`:sync`,
  `:mirror-watchlist`) against Wikimedia's `Extension:ReadingLists` — the
  same backend the official mobile apps use. Everything here degrades to a
  login prompt, never an error, when logged out.
- **Text-to-speech**: pipes the current article's paragraphs, from the
  reading cursor onward, to a user-configured command (`tts_command`, e.g.
  `espeak-ng` or macOS `say`) — opt-in, off by default.
- **Reading stats** *(local-only)*: articles read, time, streaks, and topic
  distribution, derived from your own reading history and interest model —
  never uploaded anywhere; `wikitui stats --explain` shows top topics.
- **Commands & keys**: a `:` ex-command line, a fuzzy command palette
  (`Ctrl-p`), a fully configurable keymap (`vim`/`emacs` presets or your own
  `keymap.toml`), and a context-sensitive `?` help overlay generated from
  the same command registry — so the palette, help, and keymap can never
  drift out of sync with each other.
- **Talk pages**: read an article's talk page alongside the article.
- **Article attribution**: `i` / `:info` shows the on-screen article's
  title, canonical URL, revision id, license, and a permalink to its
  revision history — see [License & attribution](#license--attribution).
- **Accessibility**: `--dump` (plain-text stdout, no alternate screen), a
  no-motion mode, `ACCESSIBLE` environment support, and mouse support with
  a keyboard equivalent for every mouse action.
- **UI localization architecture**: wikitui's own interface text is
  English-only through v1.x by deliberate product choice, but a string-table
  seam (`t(Key::...)`, see `src/strings.rs`) already exists for a subset of
  UI strings so a real v2 translation pass is a mechanical follow-up, not an
  architecture change; keybindings never assume QWERTY (everything is
  rebindable).
- **Privacy**: no telemetry, ever — see [Privacy](#privacy) below.

### Known limitations / not yet built

Honestly-deferred gaps, so nobody goes looking for them or assumes more
than what's actually there:

- **Inline image protocols**: only the half-block renderer (plain
  fg/bg-colored cells, no terminal cooperation needed) is actually wired
  into the reading view. The kitty-graphics and iTerm2 escape emitters are
  implemented and unit-tested to spec but are not yet called from the
  renderer, and are unverified against a real kitty/iTerm2 terminal. Sixel
  is a documented stub (`graphics::sixel_escape`) — no color-quantization
  encoder exists yet.
- **`wiki://` protocol handler**: wikitui parses `wiki://`/`wiki:` URIs
  passed to it as an argument (so `wikitui wiki://en.wikipedia.org/X`
  works), and a `.desktop` file with the right `MimeType` declaration ships
  in `packaging/` as groundwork — but wikitui does not register itself as
  the OS's default handler for you. Routing real `wiki://` links to wikitui
  is a one-time, manual `xdg-mime`/packager step; see the `.desktop` file's
  own header comment for the exact commands.
- **Image licensing in exports**: wikitui doesn't fetch per-image
  `extmetadata` license data, so it can't *prove* a given thumbnail is
  freely licensed. Saved-page and bookmark exports therefore omit image
  data by default (alt text only) unless you opt in with
  `--include-nonfree`, which is the conservative direction, not a missing
  feature.
- **Packaging breadth**: `cargo install wikitui` works today; Homebrew,
  AUR, nixpkgs packages, and static GitHub-release binaries are planned
  (see [Install](#install)) but not published yet.
- **§6.8 performance targets**: the PRD's parse+layout targets (e.g. < 500 ms
  for a 1.5 MB / 500+-reference article) are asserted in an `#[ignore]`d
  test with a deliberately loose 10-second bound, not the real target — see
  [Performance](#performance) below.

## Install

wikitui's v1.0 packaging targets are crates.io, Homebrew, AUR, and nixpkgs
(plus GitHub release binaries). Only the first is available today:

```sh
cargo install wikitui
```

Homebrew, AUR, and nixpkgs packages, and static-binary GitHub releases, are
planned but not published yet — build from source with `cargo build
--release` in the meantime.

## Quick start

```sh
wikitui                     # start page
wikitui "Alan Turing"       # open an article directly
wikitui --search "turing"   # open straight into search results
wikitui --dump "Alan Turing" > article.txt   # plain-text export, no TUI
```

Press `?` at any time for a context-sensitive keybinding cheat sheet (it's
generated from the live keymap, so it's always accurate), or `Ctrl-p` for
the fuzzy command palette. A few starting points: `/` search, `j`/`k`
scroll, `f` link hints, `t` table of contents, `m` bookmark, `S` save
offline, `i` article info, `:` for ex-commands, `q` quit.

## Configuration

wikitui reads a versioned TOML config file from your platform's standard
config directory (`~/.config/wikitui/config.toml` on Linux), overridable
with `--config <path>` or the `WIKITUI_CONFIG` environment variable.
Precedence is fixed: CLI flags > environment (`WIKITUI_*`) > config file >
built-in defaults. Nothing in the config loader ever aborts on bad input —
an unknown key or an out-of-range value degrades to the default and warns,
it never crashes the app.

Run `wikitui config doctor` for a plain-stdout report: the fully resolved
config with per-value provenance (which layer set it), any config
problems, user theme files and their contrast-ratio lint results, and a
terminal-capability report (color depth, detected graphics protocol, and
so on) — useful before ever launching the TUI itself.

## Performance

`PRD.md` §6.8 sets concrete wall-clock targets (e.g. parse+layout of a
1.5 MB / 500+-reference pathological article in under 500 ms on a
2020-era laptop; an L1 cache-hit article open in under 50 ms). These
targets are **not validated in CI** and should not be read as a proven
claim: CI runners (and the sandboxed environment this project is
developed in) are shared, throttled, and not representative real
hardware, so a strict wall-clock assertion there would fail on noisy-
neighbor load, not a real regression. What exists today is
`corpus_tests::perf_smoke_parse_and_layout_the_pathological_corpus`, an
`#[ignore]`d test with a deliberately generous 10-second bound (20x the
PRD target) that only catches a catastrophic algorithmic regression (an
accidental O(n²) in the parser or wrap loop) — see that test's own doc
comment. Confirming the actual §6.8 targets needs a real-hardware
benchmark run outside CI; nobody has published one yet.

## Privacy

wikitui sends no telemetry of any kind, ever. The only network calls it
makes are:

- Requests to the Wikipedia/Wikimedia wikis you read (article fetches,
  search, feeds, langlinks, and so on).
- Background prefetch requests to those same wikis (link prefetch, trending
  content) — off by default in incognito, and always subject to the
  documented request/byte budgets and kill switch.
- An explicitly opt-in, off-by-default version check against GitHub
  releases (§8) — never enabled unless you turn it on.

Nothing else. There is no analytics SDK, no crash reporter that phones
home, and no first-party or third-party tracking endpoint anywhere in this
codebase — a CI check (`privacy::tests::no_analytics_endpoints_in_the_real_source_tree`)
scans the source tree for known analytics/monitoring hostnames on every
test run and fails the build if one ever appears.

This is not the same thing as "invisible." Being honest about what
wikitui *cannot* protect you from:

- **Wikimedia still sees your IP address and request pattern** for every
  article you read, exactly as it would over a browser — wikitui is a
  client for their API, not an anonymizing proxy. Route it through Tor/a
  VPN yourself if that matters to you.
- **Prefetch reveals predicted interests to Wikimedia** even though it
  never reveals them to us: fetching likely-next articles in the
  background means Wikimedia's servers see requests for pages you haven't
  actually opened yet, alongside the ones you have. Mitigations: `:set
  prefetch=off` (or the config kill switch) turns it off entirely;
  `:prefetch-log` shows exactly what was fetched and why; incognito mode
  (`--incognito` / `zz`) disables it for the session.
- **Personalization is 100% local.** Reading history, bookmarks, saved
  pages, the interest model, and reading stats never leave your machine —
  there is no sync service, no account required, and no server-side profile
  of any kind (the opt-in Reading List sync feature is the one exception:
  it syncs bookmarks *to your own Wikimedia account*, which you explicitly
  logged into and enabled, not to wikitui or any third party). State lives
  in plain SQLite/JSONL files
  under your platform's standard config/data/cache/state directories,
  readable and deletable with ordinary tools (see `wikitui clear-data
  --help` for a one-command wipe, and `[cache] dir` / `$XDG_CACHE_HOME` to
  relocate the cache).

Incognito mode (`--incognito`, or `zz` at runtime — a visible
`[incognito]` marker stays in the status bar the whole time) stops history,
prefetch, and any future stats/interest-model writes outright. It does
**not** stop an explicit save you asked for by name — bookmarking (`m`),
saving a page offline (`S`), enqueuing read-later (`rl`), queuing an
offline fetch, or saving a citation all still work in incognito, because
suppressing something you deliberately asked to keep would be a worse
surprise than warning about it. Each of those prints a warning alongside
its normal confirmation so you always know it persisted.

## License & attribution

wikitui itself is licensed under the **GNU Affero General Public License,
version 3** (see `LICENSE`) — copyleft, network-use clause included: if you
run a modified wikitui as a network service, its source must be made
available to users of that service, same as any other AGPL-3.0 program.

**Article content is not wikitui's to license.** Wikipedia and Wikimedia
sister-project text is licensed **CC BY-SA 4.0** (some older content is
**GFDL**), independently of wikitui's own AGPL-3.0 license on the *client
code*. Reusing article text outside wikitui means following those terms
yourself — attribution and ShareAlike apply to the article, not to
anything wikitui adds around it.

wikitui surfaces that attribution in two places, so you always know what
you're looking at and how to credit it:

- **On screen**: `i` / `:info` opens an overlay for the article currently
  open showing its title, canonical URL, revision id, license, and a
  permalink to its revision history (the authorship record Wikimedia's
  reuse terms point reusers at).
- **On export**: every export this app produces (bookmark exports, saved-
  page exports, bibliography exports) embeds an attribution footer — title,
  revision permalink, license, and retrieval date. A reader's own notes and
  tags in an export are visually separated and labeled as the reader's own
  annotations, since ShareAlike governs the article text they accompany,
  not the reader's commentary on it.

Non-free/fair-use images are shown transiently only and are never written
into saved pages or exports by default.
