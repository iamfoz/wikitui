//! Opt-in, local-only performance log for validating PRD §6.8's targets on
//! a real terminal (`tests/perf/harness.py`): set `WIKITUI_PERF_LOG=<path>`
//! and the event loop appends one JSON object per line to that file —
//! per-frame draw time and scroll offset, the first frame, and how long a
//! typeahead response waited before it was on screen.
//!
//! **Privacy (PRD FR-PR-1)**: this is not telemetry. It is off unless the
//! person running the binary names a file for it, it writes only to that
//! local file (no network path exists in this module — the analytics-host
//! guard in `privacy.rs` scans it like every other source file), and it
//! records timings and counters only — never titles, queries, or any other
//! reading content.
//!
//! **Zero-cost when off**: the env var is read once (`init_from_env`, at
//! startup); after that every call site is one `OnceLock` load and a
//! branch. Events are built only when the log is on, by closures the
//! disabled path never calls.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The env var that turns the log on, naming the file it writes.
pub const ENV_VAR: &str = "WIKITUI_PERF_LOG";

struct Sink {
    out: Mutex<BufWriter<File>>,
    start: Instant,
}

static SINK: OnceLock<Option<Sink>> = OnceLock::new();

/// Read `WIKITUI_PERF_LOG` once and open (append) its file. An unset or
/// empty var, or a file that can't be opened, leaves the log off — never an
/// error the reader sees. Idempotent: only the first call does anything.
pub fn init_from_env() {
    SINK.get_or_init(|| {
        std::env::var_os(ENV_VAR)
            .filter(|v| !v.is_empty())
            .and_then(|path| open_sink(Path::new(&path)))
    });
}

fn open_sink(path: &Path) -> Option<Sink> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    Some(Sink {
        out: Mutex::new(BufWriter::new(file)),
        start: Instant::now(),
    })
}

/// Whether the log is on. The one check every instrumented call site makes
/// before doing any work (taking an `Instant`, formatting an event).
#[inline]
pub fn enabled() -> bool {
    matches!(SINK.get(), Some(Some(_)))
}

/// Append one event. `fields` writes the event's own JSON members (without
/// braces) — e.g. `"ev":"frame","draw_us":812` — after the common
/// `"t_us"` (microseconds since the log opened). Never called when off.
pub fn event(fields: impl FnOnce(&mut String)) {
    if let Some(Some(sink)) = SINK.get() {
        write_event(sink, fields);
    }
}

fn write_event(sink: &Sink, fields: impl FnOnce(&mut String)) {
    let mut line = format!("{{\"t_us\":{},", sink.start.elapsed().as_micros());
    fields(&mut line);
    line.push_str("}\n");
    let mut out = sink.out.lock().unwrap();
    let _ = out.write_all(line.as_bytes());
    // Flushed per event: the harness reads the file while the process is
    // still running (and may SIGKILL it), and the log is only on while
    // measuring, so the syscall per event is the price of never losing one.
    let _ = out.flush();
}

/// One drawn frame: how long `terminal.draw` (layout-if-needed, paint, and
/// the backend write to the tty) took, the mode, and the active tab's
/// scroll offset after it.
pub fn frame(draw: Duration, mode: &str, scroll: u16, frame_index: u64) {
    event(|s| s.push_str(&frame_fields(draw, mode, scroll, frame_index)));
}

fn frame_fields(draw: Duration, mode: &str, scroll: u16, frame_index: u64) -> String {
    format!(
        "\"ev\":\"frame\",\"n\":{frame_index},\"draw_us\":{},\"mode\":\"{mode}\",\"scroll\":{scroll}",
        draw.as_micros()
    )
}

/// A named point on the startup path (§6.8 "cold start → interactive"),
/// so a slow start can be attributed to a phase rather than guessed at.
/// `phase` is a fixed identifier chosen at the call site, never user data.
pub fn mark(phase: &'static str) {
    event(|s| s.push_str(&format!("\"ev\":\"mark\",\"phase\":\"{phase}\"")));
}

/// A typeahead response (PRD FR-SR-1) became visible: `waited` is from the
/// moment its HTTP response was decoded to the end of the first frame that
/// drew it — §6.8's "render < 100 ms after response".
pub fn typeahead_rendered(waited: Duration, suggestions: usize) {
    event(|s| s.push_str(&typeahead_fields(waited, suggestions)));
}

fn typeahead_fields(waited: Duration, suggestions: usize) -> String {
    format!(
        "\"ev\":\"typeahead_render\",\"us\":{},\"suggestions\":{suggestions}",
        waited.as_micros()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sink writes one well-formed JSON object per event, each carrying
    /// the common timestamp, to exactly the file it was opened on.
    #[test]
    fn sink_writes_one_json_object_per_line() {
        let path =
            std::env::temp_dir().join(format!("wikitui-perflog-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let sink = open_sink(&path).expect("temp file opens");
        for n in 0..3u64 {
            write_event(&sink, |s| {
                s.push_str(&format!("\"ev\":\"frame\",\"n\":{n}"))
            });
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let events: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is JSON"))
            .collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2]["n"], 2);
        assert!(events.iter().all(|e| e["t_us"].is_u64()));
        let _ = std::fs::remove_file(&path);
    }

    /// Off (the default: this test process never sets the env var, and
    /// `init_from_env` reads it only once) means `event` never builds or
    /// writes anything — the closure isn't even called.
    #[test]
    fn disabled_log_never_runs_the_event_closure() {
        if std::env::var_os(ENV_VAR).is_some() {
            return; // someone ran the suite with the log on; nothing to assert
        }
        init_from_env();
        assert!(!enabled());
        let mut called = false;
        event(|_| called = true);
        assert!(!called, "a disabled log must not format events");
    }

    /// The two event shapes the pty harness parses carry exactly the fields
    /// it reads, as valid JSON inside the common envelope.
    #[test]
    fn frame_and_typeahead_events_are_the_shape_the_harness_reads() {
        let frame: serde_json::Value = serde_json::from_str(&format!(
            "{{\"t_us\":0,{}}}",
            frame_fields(Duration::from_micros(1234), "Reading", 42, 7)
        ))
        .unwrap();
        assert_eq!(frame["ev"], "frame");
        assert_eq!(frame["draw_us"], 1234);
        assert_eq!(frame["mode"], "Reading");
        assert_eq!(frame["scroll"], 42);
        assert_eq!(frame["n"], 7);
        let typeahead: serde_json::Value = serde_json::from_str(&format!(
            "{{\"t_us\":0,{}}}",
            typeahead_fields(Duration::from_millis(3), 10)
        ))
        .unwrap();
        assert_eq!(typeahead["ev"], "typeahead_render");
        assert_eq!(typeahead["us"], 3000);
        assert_eq!(typeahead["suggestions"], 10);
    }

    /// An unopenable path leaves the log off rather than erroring.
    #[test]
    fn unopenable_path_yields_no_sink() {
        assert!(open_sink(Path::new("/nonexistent-dir/wikitui/perf.jsonl")).is_none());
    }
}
