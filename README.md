# wikitui
TUI Interface to Wikipedia

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
