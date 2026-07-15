//! PRD FR-DL-6: the wiki-walk game — a start→goal navigation challenge where
//! the only legal move is following an in-article link. `GameState` is the
//! pure state machine (start, goal, the path actually walked, the clock);
//! everything about *enforcing* "links only" (blocking `:open`/search/
//! random/related while a game is active) lives in `main.rs`, which is the
//! one place that already knows every way a reader can navigate — see
//! `main::execute_command`'s game guard and the `/`/`gr`/`gR` key guards.
//!
//! **Explicitly out of v1 scope (PRD FR-DL-6):** an optimal-path oracle. A
//! client-side BFS over the link graph would mean fetching every article
//! reachable from `start` — rate-hostile against a real wiki (§6.2's whole
//! "respectful client" posture) — so this module never attempts to verify
//! that `goal` is reachable from `start` at all, in any number of clicks,
//! for a `:game <start> <goal>` pair the reader typed themselves. The daily
//! puzzle only softens this by picking from a small, hand-curated,
//! plausible set (`DAILY_PUZZLES`), not by proving reachability either.
//!
//! **Daily seed, "thread the date, don't read the clock in logic"**: matches
//! the convention `prefetch::FeedCache`/`research::today` already use — the
//! caller reads the wall clock once (`chrono::Utc::now().format("%Y-%m-%d")`,
//! same call `main::schedule_trending_prefetch` makes) and passes the string
//! in here. [`daily_puzzle`] is then a pure function of that string: no
//! randomness, so the same calendar day always resolves to the same pair,
//! and a test can pin any date without freezing real time.

/// A small, hand-curated set of daily wiki-walk pairs (PRD FR-DL-6), titles
/// chosen from the mock/real fixture set this project already exercises
/// elsewhere (`Alan Turing` → `Enigma machine` → `Computer science` is the
/// same three-article chain `trail.rs`'s own pty fixture builds a two-hop
/// session from) — plausible, thematically connected pairs, not a
/// reachability-verified set (see the module doc's "explicitly out of v1
/// scope").
const DAILY_PUZZLES: &[(&str, &str)] = &[
    ("Alan Turing", "Computer science"),
    ("Enigma machine", "Computer science"),
    ("Alan Turing", "Enigma machine"),
];

/// FNV-1a: a small, dependency-free, stable string hash — deterministic
/// across runs/platforms (unlike `std`'s `RandomState`-seeded `Hash`, which
/// is deliberately randomized per-process and would make the daily puzzle
/// change on every restart).
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// PRD FR-DL-6's `:game daily`: a deterministic start/goal pair for
/// `date` (a `yyyy-mm-dd` string — see the module doc's "thread the date"
/// convention). Bare `:game` (no arguments) resolves to this too
/// (`main::execute_command`'s `command::GameSpec::Daily` arm) — a documented
/// simplification: rather than inventing a client-side RNG purely to pick a
/// puzzle (this codebase's `:random`/`gr` always draws from the *wiki's own*
/// `list=random`, never a local RNG — see `random.rs`'s module doc), bare
/// `:game` is simply "today's daily," and an interactive picker is future
/// scope.
pub fn daily_puzzle(date: &str) -> (&'static str, &'static str) {
    let hash = fnv1a(date.as_bytes());
    DAILY_PUZZLES[(hash as usize) % DAILY_PUZZLES.len()]
}

/// One in-progress or finished wiki-walk (PRD FR-DL-6). `path` is the trail
/// of articles actually visited during *this* game — `path[0]` is always the
/// start, in whatever form the article's own title resolved to once opened
/// (matching `goal`'s comparison basis) — distinct from `history`/`trail`'s
/// own session-wide tracking, which this module neither reads nor writes:
/// a wiki-walk's path is scoped to just this one game and is reset by every
/// fresh `:game`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameState {
    pub goal: String,
    pub path: Vec<String>,
    /// Unix seconds the game started (`history::now_unix()`), for the HUD's
    /// elapsed timer (`format_elapsed(now - started_at)`).
    pub started_at: i64,
    /// Set the moment `path`'s last entry matches `goal` — see `follow`.
    /// Once `true`, `main.rs`'s navigation guard lifts (PRD's restriction is
    /// "while the challenge is on," not permanent), though `game` itself
    /// stays `Some` so `:game share` keeps working afterward.
    pub won: bool,
}

/// Case-insensitive title comparison: a link's resolved title and the goal
/// string typed at `:game` time may differ only in case (a reader typing
/// `computer science` for `Computer science`), and Wikipedia titles are
/// only ever case-sensitive past the first letter in practice.
fn titles_match(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl GameState {
    /// Starts a new game. `won` is set immediately if `start` already equals
    /// `goal` (a degenerate but honest edge case, not an error).
    pub fn new(start: impl Into<String>, goal: impl Into<String>, started_at: i64) -> Self {
        let start = start.into();
        let goal = goal.into();
        let won = titles_match(&start, &goal);
        Self {
            goal,
            path: vec![start],
            started_at,
            won,
        }
    }

    /// The article the game began on.
    pub fn start(&self) -> &str {
        &self.path[0]
    }

    /// The article currently on screen, in this game's own path (always
    /// present — `path` is never empty).
    pub fn current(&self) -> &str {
        self.path
            .last()
            .expect("path always has at least the start")
    }

    /// Clicks so far: one per link followed, i.e. `path.len() - 1`.
    pub fn clicks(&self) -> u32 {
        (self.path.len() - 1) as u32
    }

    /// Records following a link to `title` (PRD FR-DL-6's click counter):
    /// pushes it onto `path` and flips `won` when it matches the goal. A
    /// no-op on the win check once already won — the path can still grow
    /// (nothing stops a reader from continuing to click after winning), but
    /// `won` only ever transitions `false` → `true`, never back.
    pub fn follow(&mut self, title: impl Into<String>) {
        let title = title.into();
        if !self.won {
            self.won = titles_match(&title, &self.goal);
        }
        self.path.push(title);
    }
}

/// PRD FR-DL-6's HUD elapsed timer: `secs` (already `now - started_at`,
/// computed by the caller — this stays a pure formatter, no clock read of
/// its own, testable without freezing time) as `m:ss`. Negative input
/// (a clock skew) clamps to zero rather than printing a sign.
pub fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// PRD FR-DL-6's shareable, Wordle-style ASCII result card: the start and
/// current/reached article, click count, a filled-block row (one per
/// click), and — while the game isn't won yet — an honest "(in progress)"
/// tag rather than pretending every share is a finished walk. Full titles,
/// not abbreviated: the PRD's own prose example ("Turing→Philosophy") is a
/// stylistic shorthand, not a spec for algorithmic title truncation, which
/// has no well-defined rule for an arbitrary Wikipedia title.
pub fn share_card(state: &GameState) -> String {
    let clicks = state.clicks();
    format!(
        "wikitui wiki-walk: {}\u{2192}{} in {clicks} click{}{} {}",
        state.start(),
        state.current(),
        if clicks == 1 { "" } else { "s" },
        if state.won { "" } else { " (in progress)" },
        "\u{2b1b}".repeat(clicks as usize),
    )
}

/// Which ordinary-navigation commands PRD FR-DL-6 restricts while a game is
/// active and unwon (`main::execute_command`'s guard) — `:open` (direct
/// open) and `:random`/`:related` (both open an article the reader didn't
/// reach by clicking a link in the one on screen). `/` (search) has no
/// `Command` variant of its own (it's a bare mode switch) and is guarded
/// separately in `main::handle_key`; `gr`/`gR` resolve to `Command::Random`/
/// `Command::Related`'s same restriction via `app::GPrefixAction`, checked
/// at their own resolution site rather than here, since they never become a
/// parsed `Command` at all.
pub fn blocks_command(cmd: &crate::command::Command) -> bool {
    matches!(
        cmd,
        crate::command::Command::Open(_)
            | crate::command::Command::Random(_)
            | crate::command::Command::Related
    )
}

/// The notice shown when a guarded action is blocked mid-game (PRD FR-DL-6:
/// "disable search/open while in a game").
pub const BLOCKED_NOTICE: &str =
    "wiki-walk: only link-follows count here — search/open/random/related are disabled mid-game";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ---- GameState: path, clicks, win detection ----------------------------

    #[test]
    fn new_game_starts_at_zero_clicks_with_the_start_as_current() {
        let g = GameState::new("Alan Turing", "Computer science", 1000);
        assert_eq!(g.start(), "Alan Turing");
        assert_eq!(g.current(), "Alan Turing");
        assert_eq!(g.clicks(), 0);
        assert!(!g.won);
    }

    #[test]
    fn a_degenerate_start_equal_to_goal_wins_immediately() {
        let g = GameState::new("Computer science", "computer science", 1000);
        assert!(
            g.won,
            "case-insensitive equality must still win immediately"
        );
    }

    #[test]
    fn following_links_increments_clicks_and_updates_current() {
        let mut g = GameState::new("Alan Turing", "Computer science", 1000);
        g.follow("Enigma machine");
        assert_eq!(g.clicks(), 1);
        assert_eq!(g.current(), "Enigma machine");
        assert!(!g.won);
        g.follow("Computer science");
        assert_eq!(g.clicks(), 2);
        assert!(g.won, "reaching the goal must win");
    }

    #[test]
    fn reaching_the_goal_case_insensitively_still_wins() {
        let mut g = GameState::new("Alan Turing", "Computer science", 1000);
        g.follow("COMPUTER SCIENCE");
        assert!(g.won);
    }

    #[test]
    fn winning_does_not_unwin_on_further_clicks() {
        let mut g = GameState::new("Alan Turing", "Computer science", 1000);
        g.follow("Computer science");
        assert!(g.won);
        g.follow("Something else entirely");
        assert!(g.won, "once won, further clicks must not clear it");
        assert_eq!(g.clicks(), 2);
    }

    // ---- daily_puzzle: deterministic, no randomness -------------------------

    #[test]
    fn daily_puzzle_is_deterministic_for_the_same_date() {
        let a = daily_puzzle("2026-07-15");
        let b = daily_puzzle("2026-07-15");
        assert_eq!(a, b, "the same date must always resolve to the same pair");
    }

    #[test]
    fn daily_puzzle_can_differ_across_dates() {
        // Not a strict requirement (a hash collision is allowed), but with
        // only 3 puzzles and real dates this should very reliably vary —
        // guards against an accidental "always returns index 0" bug.
        let dates = [
            "2026-01-01",
            "2026-02-14",
            "2026-03-03",
            "2026-04-20",
            "2026-05-05",
            "2026-06-18",
            "2026-07-15",
            "2026-08-09",
        ];
        let picks: HashSet<_> = dates.iter().map(|d| daily_puzzle(d)).collect();
        assert!(
            picks.len() > 1,
            "8 different dates all picking the same puzzle looks like a hashing bug"
        );
    }

    #[test]
    fn daily_puzzle_always_returns_one_of_the_curated_set() {
        for date in ["2026-01-01", "2099-12-31", "1970-01-01", ""] {
            let pick = daily_puzzle(date);
            assert!(
                DAILY_PUZZLES.contains(&pick),
                "{date:?} resolved to {pick:?}, not in DAILY_PUZZLES"
            );
        }
    }

    // ---- format_elapsed ------------------------------------------------------

    #[test]
    fn format_elapsed_renders_mm_ss() {
        assert_eq!(format_elapsed(0), "0:00");
        assert_eq!(format_elapsed(5), "0:05");
        assert_eq!(format_elapsed(65), "1:05");
        assert_eq!(format_elapsed(3661), "61:01");
    }

    #[test]
    fn format_elapsed_clamps_negative_to_zero() {
        assert_eq!(format_elapsed(-5), "0:00");
    }

    // ---- share_card -----------------------------------------------------------

    #[test]
    fn share_card_format_matches_the_wordle_style_shape() {
        let mut g = GameState::new("Alan Turing", "Computer science", 1000);
        g.follow("Enigma machine");
        g.follow("Computer science");
        let card = share_card(&g);
        assert_eq!(
            card,
            "wikitui wiki-walk: Alan Turing\u{2192}Computer science in 2 clicks \u{2b1b}\u{2b1b}"
        );
    }

    #[test]
    fn share_card_singular_click_and_in_progress_tag() {
        let mut g = GameState::new("A", "B", 1000);
        g.follow("A middle stop");
        let card = share_card(&g);
        assert!(
            card.contains("1 click "),
            "singular, no trailing s: {card:?}"
        );
        assert!(
            card.contains("(in progress)"),
            "unwon game must say so: {card:?}"
        );
    }

    // ---- blocks_command -------------------------------------------------------

    #[test]
    fn blocks_command_flags_open_random_and_related_only() {
        use crate::command::{Command, RandomSpec};
        assert!(blocks_command(&Command::Open("X".to_string())));
        assert!(blocks_command(&Command::Random(RandomSpec::Any)));
        assert!(blocks_command(&Command::Related));
        assert!(!blocks_command(&Command::Help));
        assert!(!blocks_command(&Command::Talk));
    }
}
