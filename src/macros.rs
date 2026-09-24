//! PRD FR-CS-5: command-sequence macros — named, user-configured sequences
//! of existing commands/actions (`[command.<name>] run = [...]`), explicitly
//! **not** raw-keystroke recording (the PRD's own reasoning: "fragile under
//! async UI" — a recorded keystroke can land in the wrong mode/view once
//! timing or a background fetch shifts what's on screen; a sequence of
//! already-named commands has no such ambiguity, since each step names
//! *what* to do, not *which key* was pressed).
//!
//! A macro step is either a bare [`crate::registry::Action`] name (`"toc"`,
//! `"random-article"`) or a `:`-command string, with or without its leading
//! colon (`"open Alan Turing"` or `":open Alan Turing"`) — dispatched
//! through the exact same paths a keypress or a typed `:` command already
//! uses (`main::dispatch_action`/`main::execute_command`), so a macro can
//! never do anything a reader couldn't already do by hand, one command at a
//! time.
//!
//! Two special step names exist only inside a macro's `run` list — each
//! names a small *behavior*, not a single existing command, because no
//! single action/command already spells it (see each constant's doc
//! comment). They are the exact two names PRD FR-CS-5's own
//! `[command.morning] run = ["open-feed", "tab-open-random-good"]` example
//! uses, so a reader copying that example verbatim gets a working macro.
//!
//! ## Recursion guard
//!
//! A macro's `run` list can itself name another macro — including, by
//! mistake, itself, or two macros calling each other. [`MAX_DEPTH`] caps
//! nesting depth and `main::run_macro`'s seen-name stack refuses to
//! re-enter a macro already on the current call chain. Both exist because
//! they catch different shapes of runaway recursion: a seen-set alone would
//! still let a long *acyclic* chain (`a` calls `b` calls `c` calls `d` …)
//! run arbitrarily deep since no name ever repeats; a depth cap alone would
//! still let a *cyclic* one (`a` calls `b` calls `a` calls `b` …) burn every
//! one of `MAX_DEPTH` steps doing real, repeated work before stopping. The
//! seen-set catches the cyclic case on its very first repeat (cheaper and
//! more honest than waiting for the cap), and the cap independently bounds
//! the acyclic case the seen-set can't see coming.
//!
//! ## Error policy
//!
//! A step that fails (an unknown command, a bad/missing argument, a network
//! call that errors) is reported — via the same `app.notice` a standalone
//! failed command already sets — and **the rest of the sequence still
//! runs**. A macro is a convenience for chaining commands a reader would
//! otherwise type one at a time; one bad step aborting every step after it
//! would make a single typo in a five-step macro silently skip the other
//! four, which is a worse failure mode than "four ran, one didn't" (and the
//! reader already gets a notice naming exactly which one).

/// PRD FR-CS-5 example step `"open-feed"`: opens the start page/feed — the
/// same action as `:start`/`gh` (`App::go_home`). Spelled as its own
/// macro-only name because "feed" (PRD FR-DL-1's Wikifeeds-backed start
/// page) reads clearer in a `[command.morning]` context than the existing
/// `start`/`home` naming, which predates FR-CS-5 and is about ordinary tab
/// navigation, not "begin my reading session."
pub const OPEN_FEED: &str = "open-feed";

/// PRD FR-CS-5 example step `"tab-open-random-good"`: opens a fresh
/// foreground tab, then loads a random ≥GA-assessed article into it
/// (`:tab new` + `:random good`, composed) — no existing single command
/// already does "new tab" and "random good article" together.
pub const TAB_OPEN_RANDOM_GOOD: &str = "tab-open-random-good";

/// How many macro-calls-macro levels `main::run_macro` follows before
/// giving up and reporting a notice instead of recursing further. Generous
/// enough for any real chain (nobody nests five macros deep on purpose)
/// while still bounding a long acyclic chain to a handful of wasted steps
/// rather than an unbounded one — see the module doc comment's "Recursion
/// guard" section for why this exists alongside the seen-set, not instead
/// of it.
pub const MAX_DEPTH: usize = 8;

/// Strips a macro step's optional leading `:` and surrounding whitespace —
/// `"toc"` and `":toc"` are the same step, since a reader copying a working
/// ex-command line into a `run = [...]` array shouldn't have to remember to
/// delete the colon.
pub fn strip_colon(step: &str) -> &str {
    let trimmed = step.trim();
    trimmed.strip_prefix(':').unwrap_or(trimmed).trim()
}

/// Whether `step` names something wikitui can actually run: one of the two
/// macro-only aliases above, a bare `registry::Action` name, or a
/// `:`-command whose *name* (ignoring whether its own arguments are
/// well-formed — a macro step usually supplies its own, e.g. `"open Alan
/// Turing"`, which this cannot pre-validate without inventing a fake title)
/// is recognized by `command::parse_with_user_themes`'s grammar.
///
/// Used both at config-load time (`config::resolve_macros`, PRD FR-CS-5's
/// "validate at load, warn on unknown") and defensively again at run time
/// (`main::run_macro`) since config can be hand-edited or reloaded
/// (`:config reload`) after the load-time check already ran.
pub fn step_names_a_known_command(step: &str) -> bool {
    let step = strip_colon(step);
    if step.is_empty() {
        return false;
    }
    if step == OPEN_FEED || step == TAB_OPEN_RANDOM_GOOD {
        return true;
    }
    if crate::registry::Action::by_name(step).is_some() {
        return true;
    }
    // `command::parse_with_user_themes`'s only two "the command name itself
    // is unrecognized" outcomes are the bare-input case (already excluded
    // above by the `is_empty` check) and its final catch-all arm, which
    // always formats its message starting with this exact prefix — every
    // other error path names a *known* command with a bad/missing
    // argument (`"usage: :open <title>"`, `"unknown :random argument
    // ..."`), which counts as "known" for this pre-flight purpose.
    match crate::command::parse_with_user_themes(step, &[]) {
        Ok(_) => true,
        Err(message) => !message.starts_with("unknown command "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_colon_removes_at_most_one_leading_colon_and_trims() {
        assert_eq!(strip_colon(":toc"), "toc");
        assert_eq!(strip_colon("toc"), "toc");
        assert_eq!(strip_colon("  :open Alan Turing  "), "open Alan Turing");
        assert_eq!(
            strip_colon("::toc"),
            ":toc",
            "only one leading colon is stripped"
        );
    }

    #[test]
    fn recognizes_the_two_macro_only_aliases_with_or_without_a_colon() {
        assert!(step_names_a_known_command(OPEN_FEED));
        assert!(step_names_a_known_command(TAB_OPEN_RANDOM_GOOD));
        assert!(step_names_a_known_command(":open-feed"));
    }

    #[test]
    fn recognizes_bare_registry_action_names() {
        assert!(step_names_a_known_command("toc"));
        assert!(step_names_a_known_command("scroll-down"));
        assert!(step_names_a_known_command(":random-article"));
    }

    #[test]
    fn recognizes_colon_commands_with_or_without_arguments() {
        assert!(step_names_a_known_command("open Alan Turing"));
        assert!(step_names_a_known_command(":open Alan Turing"));
        assert!(step_names_a_known_command("random good"));
        // "open" alone is a known command name even though it is missing
        // its required argument — the macro step normally supplies one;
        // load-time validation checks the *name*, not full argument shape.
        assert!(step_names_a_known_command("open"));
    }

    #[test]
    fn rejects_genuinely_unknown_words() {
        assert!(!step_names_a_known_command("frobnicate"));
        assert!(!step_names_a_known_command(""));
        assert!(!step_names_a_known_command("   "));
        assert!(!step_names_a_known_command(":"));
    }
}
