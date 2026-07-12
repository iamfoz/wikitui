# Mock MediaWiki server

A minimal stand-in for `https://{lang}.wikipedia.org`'s Parsoid HTML
endpoint (`GET /w/rest.php/v1/page/{title}/html`), used for interactive and
manual verification against a real HTTP server instead of live Wikipedia
(which the test/CI network cannot reach, and which politeness rules — PRD
§6.5 — say not to hammer for routine dev-loop checks anyway).

It is a fixed set of fixture pages (`PAGES` in `server.py`), including an
`en`-ish set (Alan Turing / Enigma machine / Computer science, cross-linked)
and a Japanese fixture (`アラン・チューリング`) for CJK-correctness checks
(FR-RD-10 / FR-ML-6). It does not implement search, summaries, or any other
endpoint — only what article rendering needs.

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

Add new titles to the `PAGES` dict in `server.py` and keep existing ones
working — other tests and manual verification runs depend on them.
