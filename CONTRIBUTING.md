# Contributing to wikitui

wikitui is a small, solo-to-small-team project (PRD §13 R-10 names
maintainer burnout — the thing that killed both `wiki-tui` and
`wikicurses` — as the risk this file is one of the mitigations for). Boring,
documented, well-tested code and a low-friction contribution path matter
more here than they would on a bigger team.

## Building and testing

```sh
cargo build
cargo test
cargo fmt
cargo clippy --all-targets -- -D warnings
```

All four must be clean before a change lands. There's no `clippy.toml` or
lint-allowlist — the crate is warning-free and stays that way. The
off-by-default `math-layout` feature (see `Cargo.toml`) needs the same two
gates run again with it enabled, since it changes what `layout.rs` compiles:

```sh
cargo test --features math-layout
cargo clippy --all-targets --features math-layout -- -D warnings
```

`.github/workflows/ci.yml` runs this exact battery (both feature configs)
plus `cargo deny check` on every push and pull request, so a change that's
clean locally is clean there too.

### The mock MediaWiki server

Live Wikipedia is not something the test suite or a dev loop should hit
routinely (see [Respecting the API](#respecting-the-api) below) — a stand-in
server lives in-repo at `tests/mock-server/server.py` (its own `README.md`
documents the fixture pages and endpoints it serves). Point a build at it
with an environment variable the app treats as a fully supported override:

```sh
python3 tests/mock-server/server.py &
WIKITUI_BASE_URL=http://127.0.0.1:8943 cargo run -- "Alan Turing"
# when done:
lsof -ti:8943 | xargs -r kill -9
```

Extending the mock server with new fixtures for a feature you're adding is
expected — it's a maintained test asset, not a throwaway script. Keep its
existing fixtures working; other tests depend on them.

Most tests are plain unit tests colocated with the code they exercise and
need neither the mock server nor network access at all.

## Code conventions

- **Module docs** (`//!` at the top of the file) explain the design
  decision and link the PRD requirement ID(s) it implements, not just
  restate the file's contents.
- **Public-item docs** (`///`) explain *why* a choice was made — an
  invariant, a tradeoff, a PRD constraint — not what the next line of code
  obviously does. A comment that just narrates the following statement is a
  comment that shouldn't be there.
- **Tests are colocated**: `#[cfg(test)] mod tests` at the bottom of the
  same file, not a separate integration-test tree, except where a test
  genuinely needs the built binary (pty/CLI-level checks) or the mock
  server. Test names are descriptive snake_case sentences that state the
  behavior being locked (e.g. `cycle_link_wraps_in_both_directions`,
  `permalink_url_uses_the_oldid_form`), not `test_1` or the function name
  with `_works` appended.
- **`.unwrap()`/`.expect()`**: fine in tests, and fine on a `Mutex::lock()`
  in production code (lock poisoning means a prior panic already corrupted
  shared state — there is no sane recovery, so unwrapping is the honest
  choice). Anywhere else in a production code path — parsing, I/O, a
  network response, a config value — return `Result`/`Option` and let the
  caller decide, matching what the rest of the codebase already does:
  a config file with a bad key, a malformed cache entry, a network hiccup,
  or an unparseable response all degrade gracefully (default value, warn,
  or a status-bar error) rather than panicking. If you find yourself
  reaching for `.unwrap()` outside a test or a mutex lock, that's usually a
  sign the error path needs handling, not silencing.
- **Cross-module invariants**: a few things are guarded by tests precisely
  because nothing in the type system enforces them — notably link ordering
  between `doc::collect_links` and `ui::spans_to_rspans`, and line-position
  mappings in the layout engine. If a change touches rendering or the
  document model, run the whole suite, not just the file you edited.
- **Every feature is a named command first, keybinding second** (PRD §5.13):
  new interactive behavior belongs in `registry::COMMANDS` (name, display,
  help, contexts) before it gets a keybinding, so the command palette, the
  `?` help overlay, and `keymap.toml` rebinding all pick it up for free
  instead of drifting out of sync with a hardcoded key handler.

## Commit style

Imperative summary line referencing the PRD requirement ID(s) the commit
implements (e.g. `Add width-aware layout engine (PRD FR-RD-10, FR-RD-9)`),
with a body explaining what changed and why — `git log` is the reference
for the established style. Keep quality gates (above) green in every
commit, not just the final one on a branch.

## Respecting the API

wikitui is an unofficial client (see `README.md`), which makes API
etiquette a matter of not getting the whole project's access revoked, not
just good manners. The PRD's `NF-NET-*` requirements operationalize this:

- One global HTTP client; foreground requests outrank revalidation, which
  outranks prefetch; background traffic is strictly serial.
- A real `User-Agent` on every request identifying the client, version, a
  contact URL, and the underlying HTTP library — never a generic or spoofed
  UA.
- `maxlag` on non-interactive requests; `Retry-After` and lag errors are
  honored, not retried through.
- Exponential backoff with jitter on network errors; a circuit breaker
  suspends background traffic after repeated 429s/5xxs instead of hammering
  a struggling endpoint.
- Single-flight dedup (two tabs open on the same article make one request,
  not two) and batched metadata lookups, up to the API's own batch caps.
- `Cache-Control`/`ETag` are honored for conditional revalidation; the
  cache is never deliberately busted.
- Prefetch has hard request/byte budgets enforced at the queue, not left to
  each call site to self-limit.

Wikipedia content is not wikitui's to scrape — every fetch goes through
documented MediaWiki/RESTBase endpoints (see the mock server's own
comments and `PRD.md` Appendix A), which is also why the mock server exists
instead of screen-scraping live Wikipedia in tests.

## Dependency license audit

wikitui is AGPL-3.0-only (see `LICENSE`); every dependency it links needs a
license compatible with that. `deny.toml` at the repo root is the automated
version of this check (`cargo deny check`, run in CI on every push/PR —
see `.github/workflows/ci.yml`); its own comments explain every allowed
license and every documented advisory exception. What follows here is the
original manual pass over `Cargo.toml`'s *direct* dependencies only, read
straight from each crate's own `Cargo.toml` `license` field — kept because
it's a faster human-readable check of the top-level list than reading the
full resolved graph:

| Crate | License |
|---|---|
| anyhow | MIT OR Apache-2.0 |
| base64 | MIT OR Apache-2.0 |
| chrono | MIT OR Apache-2.0 |
| clap | MIT OR Apache-2.0 |
| crossterm | MIT |
| directories | MIT OR Apache-2.0 |
| ego-tree | ISC |
| image | MIT OR Apache-2.0 |
| ratatui | MIT |
| reqwest | MIT OR Apache-2.0 |
| rusqlite | MIT |
| scraper | ISC |
| serde / serde_json | MIT OR Apache-2.0 |
| sha2 | MIT OR Apache-2.0 |
| textwrap | MIT |
| tokio | MIT |
| toml | MIT OR Apache-2.0 |
| unicode-segmentation / unicode-width | MIT OR Apache-2.0 |
| urlencoding | MIT |
| zstd | MIT |

Every direct dependency is MIT, Apache-2.0, or ISC — all permissive and
all AGPL-compatible; none imposes terms that conflict with or additionally
restrict AGPL-3.0 distribution. Two crates bundle third-party C sources
under the `bundled` build path rather than linking a system library, worth
naming explicitly since the *bundled* code has its own license separate
from the Rust wrapper crate's:

- `rusqlite`'s `bundled` feature (used here so the app doesn't depend on a
  system SQLite) compiles the SQLite amalgamation, which SQLite's authors
  place in the **public domain** — no restriction at all.
- `zstd`'s C library, pulled in via `zstd-sys`, is dual-licensed
  **BSD-3-Clause OR GPL-2.0**; the Rust wrapper crates themselves are
  MIT/Apache-2.0. The BSD-3-Clause option is the one that applies here
  (permissive, no conflict) — nothing requires taking the GPL-2.0 leg.

None of the above conflicts with shipping wikitui under AGPL-3.0.
