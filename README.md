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
  mean, random article, and a related-articles panel.
- **Start page**: on-this-day panel and a "today I learned" widget.
- **Themes**: six built-in themes (`terminal`, `full`, `homebrew`, `night`,
  `paper`, `contrast`) plus user themes, with 256-/16-color degradation and
  `NO_COLOR` support; `:theme <name>` or cycle at runtime.
- **Multi-language**: a language switcher with langlink discovery and a
  fallback chain.
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
- **Privacy**: no telemetry, ever — see [Privacy](#privacy) below.

### Planned, not yet built

The following are on the roadmap (see `PRD.md` §12) but do **not** exist
yet — mentioned here so nobody goes looking for them: OAuth login,
watchlist/notifications/Reading List sync (v1.x); split panes and
scrollbind, bilingual mode, arbitrary MediaWiki wikis, offline full-text
search, text-to-speech, reading stats (v1.x); the `wiki://` protocol
handler, RTL support, and UI localization (v2). A `.desktop` file with the
protocol handler's `MimeType` declaration is shipped in `packaging/` as
groundwork for distro packagers, but wikitui does not register or handle
`wiki://` URIs today — see that file's own header comment.

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
  pages, and (once built) the interest model and reading stats never leave
  your machine — there is no sync service, no account required, and no
  server-side profile of any kind. State lives in plain SQLite/JSONL files
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
