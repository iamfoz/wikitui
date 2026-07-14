# Mock MediaWiki server

A minimal stand-in for `https://{lang}.wikipedia.org`'s Parsoid HTML
endpoint (`GET /w/rest.php/v1/page/{title}/html`), used for interactive and
manual verification against a real HTTP server instead of live Wikipedia
(which the test/CI network cannot reach, and which politeness rules — PRD
§6.5 — say not to hammer for routine dev-loop checks anyway).

It is a fixed set of fixture pages (`PAGES` in `server.py`), including an
`en`-ish set (Alan Turing / Enigma machine / Computer science, cross-linked)
and a Japanese fixture (`アラン・チューリング`) for CJK-correctness checks
(FR-RD-10 / FR-ML-6).

It also serves search (FR-SR-1/2/4), against separate fixture lists so the
corpus can stay small and readable:

- `GET /w/rest.php/v1/search/title?q=&limit=` — typeahead completions
  (`TITLE_SUGGESTIONS`), title + a Wikidata-style one-line description.
  Every suggested title is also a real `PAGES` key, so opening one always
  resolves. Sleeps 100ms before responding, so the debounce/latency is
  actually visible during manual verification.
- `GET /w/rest.php/v1/search/page?q=&limit=` — full-text search
  (`SEARCH_PAGES`), substring-matched against title/body, returning a
  `<span class="searchmatch">`-highlighted excerpt plus `size`/`wordcount`/
  `timestamp`. A query with zero hits that's in `DID_YOU_MEAN` gets a
  `suggestion` field instead (see `api::SearchOutcome`'s doc comment for why
  that field rides this response rather than a second Action-API request).
  Two CirrusSearch operators (PRD FR-SR-3) get minimal real interpretation,
  enough to prove the client's passthrough actually reaches the server
  unmangled — the rest fall through to the plain substring match, which is
  fine since passthrough itself is asserted at the wire level, not against
  this mock's interpretation of every operator:
  - `intitle:<term>` — matches `SEARCH_PAGES` by title only, never body text.
  - `morelike:<title>` — PRD FR-SR-6's Related panel substrate; looks
    `<title>` up in `RELATED_ARTICLES` (underscores normalised to spaces).

Random article / quality fixtures (PRD FR-SR-5 / FR-DL-3):

- `GET /w/api.php?action=query&list=random&rnnamespace=0&rnlimit=` —
  rotates through `RANDOM_TITLES` (real `PAGES` keys) instead of true
  randomness, so `gr`/`:random` is deterministic-testable: sequential
  requests draw sequential titles, wrapping every 6.
- `GET /w/api.php?action=query&prop=pageassessments&titles=` — one class
  per title from `ASSESSMENTS` (`FA`/`GA`); a title with no entry comes back
  with no `pageassessments` at all, matching a real unassessed page. Two of
  `RANDOM_TITLES`' six entries reach GA+, so any 10-title `:random good`
  batch (`RANDOM_TITLES`' rotation period is 6) is guaranteed to include one.

Two-layer cache / stale-while-revalidate fixtures (PRD FR-OFF-1/2):

- `GET /w/rest.php/v1/page/{title}/html` also sends an `ETag: W/"{revid}/mock-etag"`
  header — one stable fake revid per `PAGES` key (`REVIDS`), which
  `api::parse_revid_from_etag` parses back out.
- `GET /w/rest.php/v1/page/{title}/bare` — the cheap revalidation call
  (Appendix A's "Page metadata / latest revid" row): `{"latest": {"id": revid}}`,
  no article body.
- `WIKITUI_MOCK_UPDATE_TITLE=<title>` (env var, checked at process start)
  simulates an edit to exactly that one fixture: its revid reports one
  higher everywhere (`html`'s ETag and `bare`'s `latest.id`), and its HTML
  gets an extra trailing paragraph — so a stale-while-revalidate test can
  start the mock once unset (seed a cache entry at the base revid), kill
  it, then restart it with this set to the same title and confirm the
  app's background revalidation both notices the new revid and renders
  visibly different content after `r` reloads.

Trending / delight-and-discovery fixtures (PRD FR-PF-2, FR-DL-1/2/7):

- `GET /api/rest_v1/feed/featured/{y}/{m}/{d}` — the one daily Wikifeeds call
  (`FEATURED_FEED`): TFA (with extract), top mostread, a potd thumbnail
  pointing at this same mock's `/media/quadrants.png` (so the half-block
  image pipeline decodes real bytes end to end), an "in the news" story with
  inline `<a>` markup the client must strip, and a couple of bundled
  onthisday entries.
- `GET /api/rest_v1/feed/onthisday/{type}/{m}/{d}` — the `:today` panel's
  per-type call (`ONTHISDAY_BY_TYPE`): events/births/deaths/holidays/selected,
  each a `{"<type>": [...]}` response. Every linked page is a real `PAGES`
  key so following one resolves; an unfixtured type returns an empty list,
  not a 404.

It also serves link-target summaries (`GET /api/rest_v1/page/summary/{title}`,
`SUMMARIES`) for T2 saved-page link-peek. Beyond everything listed above, it
implements only what article rendering, search, the page cache, and the
start-page/on-this-day features need — not a general MediaWiki stand-in.

## Running it

```sh
python3 tests/mock-server/server.py &
```

It listens on `127.0.0.1:8943` and logs nothing (quiet by design — it's
meant to sit in the background during a dev session). Stop it with:

```sh
lsof -ti:8943 | xargs -r kill -9
```

(Prefer that over `pkill -f server.py`, which can also match your shell's
own command line in some setups.)

## Pointing wikitui at it

Set the **supported** base-URL override (PRD §6.7 / §6.2 rule 2) — this is
a real config feature, not a testing hack:

```sh
WIKITUI_BASE_URL=http://127.0.0.1:8943 ./target/debug/wikitui "Alan Turing"
```

`WIKITUI_BASE_URL` outranks everything except a CLI-flag-level override
(there is none for this key yet), and is used verbatim here since it has no
`{lang}` placeholder — every language request lands on the same mock
instance, which only serves what's in `PAGES` regardless of `lang`. A
template containing `{lang}` (e.g. `http://127.0.0.1:8943/{lang}`) is
substituted per-request the same way `https://{lang}.wikipedia.org` is.

The equivalent config-file form, for testing `[wiki.*]` sections
end-to-end, is:

```toml
active_wiki = "mock"

[wiki.mock]
base_url = "http://127.0.0.1:8943"
```

Note the env var is `WIKITUI_BASE_URL`, not `WIKITUI_TEST_BASE_URL` — an
earlier, unsupported name used only while this override didn't exist yet.
`WIKITUI_TEST_BASE_URL` is not read by wikitui and never will be; don't
resurrect it.

## Extending it

Add new titles to the `PAGES` dict (and, for search coverage, matching
entries in `TITLE_SUGGESTIONS`/`SEARCH_PAGES`) in `server.py` and keep
existing ones working — other tests and manual verification runs depend on
them.
