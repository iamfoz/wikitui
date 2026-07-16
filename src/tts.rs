//! PRD FR-PC-2 (TTS piping) / SEC-5 (external command boundaries): pipes the
//! current article's paragraphs, from the reading cursor onward, one at a
//! time to a user-configured `tts_command` (`"espeak-ng -s 160"`, macOS
//! `"say"`). TTS is opt-in and off by default — `tts_command` unset (`None`)
//! means the play action reports a notice rather than silently doing
//! nothing (see `main::start_tts_playback`).
//!
//! ## SEC-5: stdin only, never argv
//!
//! `tts_command` (split into a program + leading arguments by
//! [`parse_command_line`]) is the reader's own trusted config, exactly like
//! `$EDITOR` (`bookmarks::parse_editor_command`) or `bookmark-cmd`-style
//! hooks. Article text is attacker/article-influenceable (any Wikipedia
//! editor can write it) and must never become a `Command::arg` — only ever
//! bytes written to the spawned child's stdin ([`speak_one`]). A paragraph
//! containing shell metacharacters (`$(whoami)`, backticks, `; rm -rf`)
//! reaches the child exactly as those literal bytes; nothing about this
//! module ever invokes a shell to interpret them.
//!
//! ## Threading model — see [`TtsRuntime`]'s doc comment for the full story.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;

use crate::doc::{Block, Document, Span};

/// Splits a `tts_command` value into its program and leading arguments.
/// Deliberately simple whitespace splitting, not full shell parsing — SEC-5
/// only requires that this run a *user-configured* command, never a
/// content-derived one, and `tts_command` is the reader's own trusted
/// setting, so there is no untrusted string here needing quoting/escaping
/// semantics. Mirrors `bookmarks::parse_editor_command`'s identical
/// reasoning for `$EDITOR`; kept as its own small function rather than a
/// shared import since the two configs have no coupling beyond both being
/// "split a user command string" (the same small-per-module-duplication
/// precedent `session::save`'s doc comment sets against `jsonl::
/// atomic_rewrite`). `None` for an empty/whitespace-only spec.
pub fn parse_command_line(spec: &str) -> Option<(String, Vec<String>)> {
    let mut parts = spec.split_whitespace();
    let program = parts.next()?.to_string();
    Some((program, parts.map(str::to_string).collect()))
}

/// Flattens a block's spans to their spoken text only. Unlike
/// `doc::render_plain`'s flattener (which appends `[link: <url>]` for
/// sighted `--dump` piping), a link's href is never appended here — a TTS
/// engine reading a raw URL aloud after every link serves no one; only the
/// visible link text is spoken, exactly as a sighted reader would read it.
fn flatten_for_speech(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

/// PRD FR-PC-2: one "paragraph" per readable document block, from
/// `from_block` (inclusive) onward, in document order. Blocks with no
/// natural spoken form — rules, images, galleries, tables, infoboxes, and
/// standalone math (raw TeX would be actively mispronounced) — are skipped
/// outright rather than mangled into something a TTS engine reads badly.
/// This is not lossy the way skipping would be for `--dump`'s exhaustive
/// plain-text dump (FR-RD-12, `doc::render_plain`) — that is a different,
/// already-served need (a sighted or braille-display reader piping to
/// `less`), whereas this is specifically what gets *spoken*. A block that
/// flattens to only whitespace (an empty heading, a spacer) is dropped too:
/// nothing is ever piped to the command as an empty stdin write.
pub fn paragraphs_from(doc: &Document, from_block: usize) -> Vec<String> {
    doc.blocks
        .iter()
        .skip(from_block)
        .filter_map(|block| {
            let text = match block {
                Block::Heading { spans, .. }
                | Block::Paragraph(spans)
                | Block::ListItem { spans, .. }
                | Block::Blockquote(spans) => flatten_for_speech(spans),
                Block::Code(text) => text.clone(),
                Block::Rule
                | Block::Table(_)
                | Block::Infobox(_)
                | Block::Image { .. }
                | Block::Gallery(_)
                | Block::Math { .. } => return None,
            };
            (!text.trim().is_empty()).then_some(text)
        })
        .collect()
}

/// Which `doc.blocks` index the reading cursor is on: the last block whose
/// `layout::Layout::block_lines` anchor is at or before `scroll` (the
/// active tab's current scroll offset), so "play from cursor" starts with
/// whatever paragraph is on screen right now, not the next one down. `0`
/// when `block_lines` is empty or every anchor is already past `scroll` —
/// an empty/degenerate layout has nothing sensible to resume from, so
/// starting at the top beats reading nothing.
pub fn block_at_scroll(block_lines: &[usize], scroll: u16) -> usize {
    let scroll = scroll as usize;
    block_lines
        .iter()
        .rposition(|&anchor| anchor <= scroll)
        .unwrap_or(0)
}

/// PRD FR-PC-2 / SEC-5: shared playback-control state, cloned onto both
/// `App` and every spawned playback task. A monotonic generation counter,
/// not a plain `bool` "is playing" flag: [`Self::begin`] and [`Self::stop`]
/// both simply bump it, so a spawned task's per-paragraph check
/// ([`Self::is_current`]) can't tell "a fresh play started" apart from
/// "stop was pressed" — it doesn't need to, since both cases mean the same
/// thing to an in-flight loop: stop advancing past the current paragraph.
///
/// ## Threading model
///
/// Playback never blocks the UI thread: `main::start_tts_playback` spawns
/// one `tokio::task` that walks the paragraph list sequentially, spawning
/// one child process per paragraph and `.await`-ing its exit before moving
/// to the next — ordinary async code on tokio's multi-threaded runtime, not
/// a dedicated OS thread. A worker thread parks on the child's exit (it
/// does not busy-loop), so the UI-owning thread (blocked in `event::read()`
/// between keypresses) and every other background task (prefetch,
/// revalidation) keep running unaffected. A long article's full playback
/// can take minutes of wall-clock time without ever stalling a keypress.
///
/// The granularity of interruption is deliberately *between* paragraphs,
/// not mid-utterance: [`Self::stop`] (or a fresh [`Self::begin`]) only
/// takes effect once the currently-speaking child exits on its own — this
/// module does not track or kill an in-flight child process. That is a
/// documented, narrow tradeoff: killing mid-utterance would need an
/// `Arc<Mutex<Child>>` shared between this struct and every spawned task
/// (locked across an `.await`, so it would have to be `tokio::sync::Mutex`,
/// not `std::sync::Mutex`, whose guard isn't `Send`-across-await-safe) for
/// a feature whose only verifiable behavior in a headless/CI environment is
/// a stdin-capturing test script, not real audio — "stops between
/// paragraphs and follows reading position" (the shipped behavior) is what
/// FR-PC-2 actually asks for.
#[derive(Debug, Clone)]
pub struct TtsRuntime {
    generation: Arc<AtomicU64>,
}

impl Default for TtsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl TtsRuntime {
    pub fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Starts a new playback run, invalidating whatever generation (if any)
    /// is currently in flight. Returns the generation the caller's spawned
    /// task must tag every paragraph check with.
    pub fn begin(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Halts playback: bumps the generation so the next [`Self::is_current`]
    /// check inside any in-flight task fails and it stops after its current
    /// paragraph rather than starting another.
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Whether `generation` is still the live one — checked by a spawned
    /// playback task before every paragraph it speaks.
    pub fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }
}

/// Spawns `program args…`, writes `text` to its stdin, then closes stdin and
/// awaits its exit (SEC-5's boundary): `text` — full article prose, entirely
/// attacker/article-influenceable — reaches the child *only* through this
/// stdin write, never as a `Command::arg`. `program`/`args` come only from
/// [`parse_command_line`]'s split of the reader's own `tts_command` config,
/// so the only thing this function ever executes is what the reader
/// configured — never anything derived from the paragraph text. Stdout/
/// stderr are `Stdio::null()`: a TTS engine's own output must never land on
/// top of the alternate-screen TUI. Returns an `Err` (never panics) when the
/// command fails to spawn at all (unset/bogus program) or exits reporting a
/// failure — the caller treats either the same way, as "this paragraph
/// didn't play," and moves on.
pub async fn speak_one(program: &str, args: &[String], text: &str) -> std::io::Result<()> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // Best-effort: a command that closes its own stdin immediately (a
        // broken `tts_command`) fails the write; that surfaces below via
        // the process's own exit status rather than panicking here.
        let _ = stdin.write_all(text.as_bytes()).await;
        let _ = stdin.shutdown().await;
        // `stdin` drops here regardless, closing wikitui's end of the pipe
        // even when `shutdown` itself errored — the child sees EOF either
        // way, so it is never left blocked reading stdin forever.
    }
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "tts_command exited with {status}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{GalleryItem, SpanStyle};

    fn plain_span(text: &str) -> Span {
        Span {
            text: text.to_string(),
            style: SpanStyle::Plain,
        }
    }

    fn link_span(text: &str, href: &str) -> Span {
        Span {
            text: text.to_string(),
            style: SpanStyle::Link(href.to_string()),
        }
    }

    fn doc_with(blocks: Vec<Block>) -> Document {
        Document {
            title: "Test".to_string(),
            blocks,
            citations: Vec::new(),
            truncated: false,
            degraded_parse: false,
            is_disambiguation: false,
        }
    }

    // ---- parse_command_line -------------------------------------------

    #[test]
    fn parse_command_line_splits_program_from_arguments() {
        assert_eq!(
            parse_command_line("espeak-ng -s 160"),
            Some((
                "espeak-ng".to_string(),
                vec!["-s".to_string(), "160".to_string()]
            ))
        );
        assert_eq!(parse_command_line("say"), Some(("say".to_string(), vec![])));
    }

    #[test]
    fn parse_command_line_is_none_for_empty_or_blank() {
        assert_eq!(parse_command_line(""), None);
        assert_eq!(parse_command_line("   "), None);
    }

    // ---- paragraphs_from -------------------------------------------------

    /// PRD FR-PC-2: paragraph extraction pulls every readable block, in
    /// document order, skipping non-prose blocks — the correctness the pty
    /// verification's "per-paragraph, in order" claim rests on.
    #[test]
    fn paragraphs_from_extracts_readable_blocks_in_order_and_skips_the_rest() {
        let doc = doc_with(vec![
            Block::Heading {
                level: 1,
                spans: vec![plain_span("History")],
            },
            Block::Paragraph(vec![
                plain_span("Turing worked at "),
                link_span("Bletchley Park", "./Bletchley_Park"),
                plain_span("."),
            ]),
            Block::Rule,
            Block::Image {
                src: None,
                alt: "a photo".to_string(),
                caption: None,
            },
            Block::ListItem {
                ordered: false,
                index: 0,
                depth: 0,
                spans: vec![plain_span("Enigma")],
            },
            Block::Blockquote(vec![plain_span("A quoted line.")]),
        ]);
        let paragraphs = paragraphs_from(&doc, 0);
        assert_eq!(
            paragraphs,
            vec![
                "History".to_string(),
                "Turing worked at Bletchley Park.".to_string(),
                "Enigma".to_string(),
                "A quoted line.".to_string(),
            ]
        );
    }

    /// A link span's href never leaks into the spoken text — only the
    /// visible link text is read, unlike `doc::render_plain`'s `[link: …]`
    /// annotation.
    #[test]
    fn paragraphs_from_never_speaks_a_links_href() {
        let doc = doc_with(vec![Block::Paragraph(vec![link_span(
            "Bletchley Park",
            "./Bletchley_Park",
        )])]);
        let paragraphs = paragraphs_from(&doc, 0);
        assert_eq!(paragraphs, vec!["Bletchley Park".to_string()]);
    }

    #[test]
    fn paragraphs_from_skips_blocks_with_no_spoken_form() {
        let doc = doc_with(vec![
            Block::Rule,
            Block::Table(crate::doc::Table {
                rows: vec![],
                truncated: false,
            }),
            Block::Infobox(vec![("Born".to_string(), "1912".to_string())]),
            Block::Gallery(vec![GalleryItem {
                src: None,
                caption: "a caption".to_string(),
            }]),
            Block::Math {
                tex: "E=mc^2".to_string(),
                display: true,
            },
        ]);
        assert!(paragraphs_from(&doc, 0).is_empty());
    }

    #[test]
    fn paragraphs_from_drops_blocks_that_flatten_to_only_whitespace() {
        let doc = doc_with(vec![
            Block::Paragraph(vec![plain_span("   ")]),
            Block::Paragraph(vec![plain_span("Real text.")]),
        ]);
        assert_eq!(paragraphs_from(&doc, 0), vec!["Real text.".to_string()]);
    }

    /// "Play from cursor": `from_block` skips everything before it.
    #[test]
    fn paragraphs_from_starts_at_the_given_block_index() {
        let doc = doc_with(vec![
            Block::Paragraph(vec![plain_span("First.")]),
            Block::Paragraph(vec![plain_span("Second.")]),
            Block::Paragraph(vec![plain_span("Third.")]),
        ]);
        assert_eq!(
            paragraphs_from(&doc, 1),
            vec!["Second.".to_string(), "Third.".to_string()]
        );
        assert_eq!(paragraphs_from(&doc, 10), Vec::<String>::new());
    }

    // ---- block_at_scroll --------------------------------------------------

    #[test]
    fn block_at_scroll_finds_the_last_anchor_at_or_before_the_cursor() {
        let block_lines = vec![0, 3, 7, 12];
        assert_eq!(block_at_scroll(&block_lines, 0), 0);
        assert_eq!(block_at_scroll(&block_lines, 2), 0);
        assert_eq!(block_at_scroll(&block_lines, 3), 1);
        assert_eq!(block_at_scroll(&block_lines, 6), 1);
        assert_eq!(block_at_scroll(&block_lines, 7), 2);
        assert_eq!(block_at_scroll(&block_lines, 100), 3);
    }

    #[test]
    fn block_at_scroll_on_empty_anchors_is_zero() {
        assert_eq!(block_at_scroll(&[], 5), 0);
    }

    // ---- TtsRuntime ---------------------------------------------------

    #[test]
    fn begin_returns_a_fresh_generation_each_time_and_invalidates_the_last() {
        let runtime = TtsRuntime::new();
        let gen1 = runtime.begin();
        assert!(runtime.is_current(gen1));
        let gen2 = runtime.begin();
        assert_ne!(gen1, gen2);
        assert!(
            !runtime.is_current(gen1),
            "starting a new play stops the old one"
        );
        assert!(runtime.is_current(gen2));
    }

    #[test]
    fn stop_invalidates_the_current_generation() {
        let runtime = TtsRuntime::new();
        let generation = runtime.begin();
        assert!(runtime.is_current(generation));
        runtime.stop();
        assert!(!runtime.is_current(generation));
    }

    #[test]
    fn a_clone_shares_the_same_generation_state() {
        let runtime = TtsRuntime::new();
        let clone = runtime.clone();
        let generation = runtime.begin();
        assert!(
            clone.is_current(generation),
            "clones observe the same generation"
        );
        clone.stop();
        assert!(
            !runtime.is_current(generation),
            "a stop via a clone is visible everywhere"
        );
    }

    // ---- speak_one: SEC-5's stdin-not-argv guarantee ----------------------

    /// The CRITICAL SEC-5 test: a paragraph containing shell metacharacters
    /// (`$(...)`, backticks, `; rm -rf`) must reach the configured command
    /// as a literal stdin byte string, never interpreted. This spawns a tiny
    /// capture script (never a shell) that appends whatever it reads on
    /// stdin to a file, verbatim — proving both "stdin, not argv" and "never
    /// executed" at once: if the metacharacters were ever passed as an argv
    /// element to a shell, or interpreted by one, the captured file would
    /// contain a command-substitution result or nothing at all, not the
    /// literal source text asserted below.
    #[tokio::test]
    async fn speak_one_delivers_shell_metacharacters_as_literal_stdin_never_executed() {
        let dir = std::env::temp_dir().join(format!(
            "wikitui-tts-sec5-{}-{}",
            std::process::id(),
            SEC5_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let capture_file = dir.join("captured.txt");
        let script_path = dir.join("capture.sh");
        // The script's own argv (the capture file path) is config-derived —
        // fixed at test-setup time, never from the paragraph — exactly the
        // shape a real `tts_command = "/path/to/script.sh /path/to/out"`
        // would take. `cat` never invokes a shell over its stdin; it only
        // copies bytes, so a passing test proves the metacharacters were
        // never handed to anything that *could* interpret them.
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\ncat >> \"{}\"\n", capture_file.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }

        let malicious = "Some prose. $(whoami) `id` ; rm -rf / && echo pwned";
        let result = speak_one(
            script_path.to_str().unwrap(),
            &[capture_file.to_str().unwrap().to_string()],
            malicious,
        )
        .await;
        assert!(
            result.is_ok(),
            "capture script must run cleanly: {result:?}"
        );

        let captured = std::fs::read_to_string(&capture_file).unwrap();
        assert_eq!(
            captured, malicious,
            "the literal paragraph text (metacharacters included) must reach stdin unchanged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    static SEC5_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Per-paragraph delivery: three separate `speak_one` calls append three
    /// separate stdin writes, in order — the shape `main::start_tts_playback`
    /// relies on for "one process per paragraph."
    #[tokio::test]
    async fn speak_one_is_called_once_per_paragraph_appending_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "wikitui-tts-order-{}-{}",
            std::process::id(),
            SEC5_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let capture_file = dir.join("captured.txt");
        let script_path = dir.join("capture.sh");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\ncat >> \"{}\"\nprintf -- '---\\n' >> \"{}\"\n",
                capture_file.display(),
                capture_file.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }

        // The capture path is baked into the script itself (config-derived,
        // fixed at setup time — not the paragraph), so each call takes no
        // arguments; every paragraph's text still only ever reaches the
        // script via its own stdin, one process per paragraph.
        for para in ["First paragraph.", "Second paragraph.", "Third paragraph."] {
            let result = speak_one(script_path.to_str().unwrap(), &[], para).await;
            assert!(result.is_ok(), "{result:?}");
        }

        let captured = std::fs::read_to_string(&capture_file).unwrap();
        assert_eq!(
            captured,
            "First paragraph.---\nSecond paragraph.---\nThird paragraph.---\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A command that doesn't exist must fail to spawn gracefully (an `Err`,
    /// never a panic) — the "spawn-fail handling" the caller (`main::
    /// start_tts_playback`'s spawned task) must survive without taking down
    /// the whole reader.
    #[tokio::test]
    async fn speak_one_reports_an_error_when_the_command_does_not_exist() {
        let result = speak_one("wikitui-this-binary-does-not-exist-anywhere", &[], "hello").await;
        assert!(result.is_err());
    }
}
