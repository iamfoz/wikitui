#!/usr/bin/env python3
"""End-to-end PRD §6.8 performance harness for wikitui (Python stdlib only).

Drives the RELEASE binary in a real pty against the in-repo mock MediaWiki
server (`tests/mock-server/server.py`, perf mode) and measures what a reader
actually experiences: bytes on a terminal. Every run uses throwaway
XDG_CONFIG/CACHE/STATE/DATA directories — never the real user's — and the
app's opt-in local perf log (`WIKITUI_PERF_LOG`, see `src/perflog.rs`) for
the numbers only the process itself can see (per-frame draw time, scroll
offset, typeahead render latency).

    cargo build --release
    python3 tests/perf/harness.py            # every scenario
    python3 tests/perf/harness.py cold_start scroll --n 30

Results print as a table and are written to --out (JSON). Methodology,
parameters and their basis: tests/perf/README.md; recorded numbers:
docs/PERFORMANCE.md.
"""

import argparse
import codecs
import contextlib
import fcntl
import json
import os
import pathlib
import random
import select
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unicodedata
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
MOCK = ROOT / "tests" / "mock-server" / "server.py"
BIN = ROOT / "target" / "release" / "wikitui"
sys.path.insert(0, str(ROOT / "tests" / "perf" / "fixtures"))
sys.dont_write_bytecode = True
import generate  # noqa: E402  (the fixture generator: titles, lead markers)

# Broadband emulation for the network-open scenario (see README "Network
# emulation"): one round trip before every response starts, and a per-
# connection throughput cap, with gzip on the wire.
BROADBAND_RTT_MS = 40
BROADBAND_KBIT = 25_000


# ---------------------------------------------------------------------------
# A minimal VT screen: enough of xterm for crossterm/ratatui's output
# ---------------------------------------------------------------------------

class Screen:
    """Tracks what a terminal would show: cursor addressing, erases, and
    printable cells (wide characters take two). SGR/mode/OSC sequences are
    parsed and dropped. Answers the queries a real terminal would (DA1,
    DSR cursor position, OSC 11 background) via `replies`."""

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.r = self.c = 0
        self.state = "text"
        self.seq = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.replies = []

    def text(self):
        return "\n".join("".join(ch for ch in row) for row in self.grid)

    def row(self, r):
        return "".join(self.grid[r])

    def _put(self, ch):
        if unicodedata.combining(ch) and self.c > 0:
            self.grid[self.r][self.c - 1] += ch
            return
        width = 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
        if self.c >= self.cols:
            return
        self.grid[self.r][self.c] = ch
        if width == 2 and self.c + 1 < self.cols:
            self.grid[self.r][self.c + 1] = ""
        self.c += width

    def _csi(self, seq):
        final = seq[-1]
        body = seq[:-1]
        private = body[:1] in ("?", ">", "<", "=")
        params = [p for p in body.lstrip("?><=").split(";")]

        def num(i, default):
            try:
                return int(params[i]) if params[i] != "" else default
            except (IndexError, ValueError):
                return default

        if final in "Hf":
            self.r = min(max(num(0, 1) - 1, 0), self.rows - 1)
            self.c = min(max(num(1, 1) - 1, 0), self.cols - 1)
        elif final == "J":
            mode = num(0, 0)
            if mode in (2, 3):
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
            elif mode == 0:
                self.grid[self.r][self.c:] = [" "] * (self.cols - self.c)
                for rr in range(self.r + 1, self.rows):
                    self.grid[rr] = [" "] * self.cols
        elif final == "K":
            mode = num(0, 0)
            if mode == 0:
                self.grid[self.r][self.c:] = [" "] * (self.cols - self.c)
            elif mode == 2:
                self.grid[self.r] = [" "] * self.cols
        elif final == "A":
            self.r = max(self.r - num(0, 1), 0)
        elif final == "B":
            self.r = min(self.r + num(0, 1), self.rows - 1)
        elif final == "C":
            self.c = min(self.c + num(0, 1), self.cols - 1)
        elif final == "D":
            self.c = max(self.c - num(0, 1), 0)
        elif final == "G":
            self.c = min(max(num(0, 1) - 1, 0), self.cols - 1)
        elif final == "c" and not private:
            self.replies.append(b"\x1b[?62;22c")
        elif final == "n" and num(0, 0) == 6:
            self.replies.append(("\x1b[%d;%dR" % (self.r + 1, self.c + 1)).encode())

    def feed(self, data):
        for ch in self.decoder.decode(data):
            st = self.state
            if st == "text":
                if ch == "\x1b":
                    self.state, self.seq = "esc", ""
                elif ch == "\r":
                    self.c = 0
                elif ch == "\n":
                    if self.r < self.rows - 1:
                        self.r += 1
                    else:
                        self.grid.pop(0)
                        self.grid.append([" "] * self.cols)
                elif ch == "\b":
                    self.c = max(self.c - 1, 0)
                elif ch == "\t":
                    self.c = min((self.c // 8 + 1) * 8, self.cols - 1)
                elif ch >= " " and ch != "\x7f":
                    self._put(ch)
            elif st == "esc":
                if ch == "[":
                    self.state = "csi"
                elif ch == "]":
                    self.state = "osc"
                elif ch == "P":
                    self.state = "dcs"
                elif ch in "()*+":
                    self.state = "charset"
                else:
                    self.state = "text"
            elif st == "charset":
                self.state = "text"
            elif st == "csi":
                self.seq += ch
                if "@" <= ch <= "~":
                    self._csi(self.seq)
                    self.state = "text"
            elif st in ("osc", "dcs"):
                if ch == "\x07":
                    self._end_string(st)
                elif ch == "\x1b":
                    self.state = st + "_esc"
                else:
                    self.seq += ch
            elif st in ("osc_esc", "dcs_esc"):
                # ESC \ (ST) ends the string; anything else is malformed —
                # resync as a fresh escape.
                self._end_string(st[:3])
                if ch != "\\":
                    self.state = "esc"

    def _end_string(self, kind):
        if kind == "osc" and self.seq.startswith("11;?"):
            self.replies.append(b"\x1b]11;rgb:0000/0000/0000\x1b\\")
        self.state, self.seq = "text", ""


# ---------------------------------------------------------------------------
# The app under test, in a pty
# ---------------------------------------------------------------------------

class Profile:
    """Throwaway XDG directories for one simulated user."""

    def __init__(self, label):
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="wikitui-perf-%s-" % label))
        self.home = self.root / "home"
        self.dirs = {k: self.root / k for k in ("config", "cache", "state", "data")}
        for d in [self.home, *self.dirs.values()]:
            d.mkdir(parents=True, exist_ok=True)
        self.perf_log = self.root / "perf.jsonl"

    @property
    def config_path(self):
        return self.dirs["config"] / "wikitui" / "config.toml"

    def write_config(self, port, extra=""):
        self.config_path.parent.mkdir(parents=True, exist_ok=True)
        self.config_path.write_text(
            "config_version = 1\n"
            'active_wiki = "mock"\n'
            + extra
            + "\n[wiki.mock]\n"
            'base_url = "http://127.0.0.1:%d"\n'
            # The capabilities real Wikipedia has, so every request an open
            # makes against Wikipedia is made against the mock too.
            "wikifeeds = true\npageviews = true\npageassessments = true\n" % port
        )

    def env(self):
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.dirs["config"]),
            "XDG_CACHE_HOME": str(self.dirs["cache"]),
            "XDG_STATE_HOME": str(self.dirs["state"]),
            "XDG_DATA_HOME": str(self.dirs["data"]),
            "TERM": "xterm-256color",
            "LANG": "C.UTF-8",
            "WIKITUI_PERF_LOG": str(self.perf_log),
        }
        return env

    def cleanup(self):
        shutil.rmtree(self.root, ignore_errors=True)


class App:
    """One wikitui process on a pty of a fixed size, with a reader thread
    feeding a `Screen` and timestamping every chunk of output."""

    def __init__(self, profile, rows=40, cols=120, args=(), env_extra=None):
        self.rows, self.cols = rows, cols
        self.screen = Screen(rows, cols)
        self.lock = threading.Condition()
        self.bytes_seen = 0
        master, slave = os.openpty()
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        env = profile.env()
        env.update(env_extra or {})
        self.profile = profile
        self.t_spawn = time.monotonic()
        self.proc = subprocess.Popen([str(BIN), *args], stdin=slave, stdout=slave, stderr=slave,
                                     env=env, start_new_session=True, close_fds=True)
        os.close(slave)
        self.master = master
        self.alive = True
        self.thread = threading.Thread(target=self._pump, daemon=True)
        self.thread.start()

    def _pump(self):
        while self.alive:
            try:
                ready, _, _ = select.select([self.master], [], [], 0.05)
            except (OSError, ValueError):
                break
            if not ready:
                continue
            try:
                data = os.read(self.master, 1 << 16)
            except OSError:
                break
            if not data:
                break
            now = time.monotonic()
            with self.lock:
                self.screen.feed(data)
                self.bytes_seen += len(data)
                self.last_output = now
                replies, self.screen.replies = self.screen.replies, []
                self.lock.notify_all()
            for reply in replies:
                with contextlib.suppress(OSError):
                    os.write(self.master, reply)
        self.alive = False
        with self.lock:
            self.lock.notify_all()

    def wait_for(self, pred, timeout=30.0, what="condition"):
        """Block until `pred(screen_text)` holds; returns the monotonic time
        of the output chunk that made it true."""
        deadline = time.monotonic() + timeout
        with self.lock:
            while True:
                if pred(self.screen.text()):
                    return self.last_output
                left = deadline - time.monotonic()
                if left <= 0 or not self.alive:
                    raise TimeoutError("timed out waiting for %s; screen:\n%s" % (what, self.screen.text()))
                self.lock.wait(min(left, 0.25))

    def wait_quiet(self, quiet=0.3, timeout=30.0):
        """Wait until no output has arrived for `quiet` seconds."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                last = getattr(self, "last_output", self.t_spawn)
            if time.monotonic() - last >= quiet:
                return
            time.sleep(quiet / 4)
        raise TimeoutError("output never went quiet")

    def send(self, data):
        if isinstance(data, str):
            data = data.encode()
        t = time.monotonic()
        os.write(self.master, data)
        return t

    def text(self):
        with self.lock:
            return self.screen.text()

    def rss_kb(self):
        status = pathlib.Path("/proc/%d/status" % self.proc.pid).read_text()
        fields = dict(line.split(":", 1) for line in status.splitlines() if ":" in line)
        return int(fields["VmRSS"].split()[0]), int(fields["VmHWM"].split()[0])

    def perf_events(self, kind=None):
        path = self.profile.perf_log
        if not path.exists():
            return []
        out = []
        for line in path.read_text().splitlines():
            with contextlib.suppress(ValueError):
                ev = json.loads(line)
                if kind is None or ev.get("ev") == kind:
                    out.append(ev)
        return out

    def quit(self):
        if self.proc.poll() is None:
            with contextlib.suppress(OSError):
                self.send("\x1b")
                time.sleep(0.05)
                self.send(":q\r")
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait()
        self.alive = False
        self.thread.join(timeout=2)
        with contextlib.suppress(OSError):
            os.close(self.master)


# ---------------------------------------------------------------------------
# The mock server
# ---------------------------------------------------------------------------

def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Mock:
    def __init__(self, latency_ms=0, kbit=0, gzip=False, corpus=True):
        self.port = free_port()
        env = dict(os.environ)
        env.update({
            "WIKITUI_MOCK_PORT": str(self.port),
            "WIKITUI_MOCK_LATENCY_MS": str(latency_ms),
            "WIKITUI_MOCK_BANDWIDTH_KBIT": str(kbit),
            "WIKITUI_MOCK_GZIP": "1" if gzip else "0",
            "WIKITUI_MOCK_PERF_CORPUS": "1" if corpus else "0",
            "PYTHONDONTWRITEBYTECODE": "1",
        })
        self.proc = subprocess.Popen([sys.executable, str(MOCK)], env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 120
        while True:
            try:
                self.debug("requests")
                break
            except OSError:
                if time.monotonic() > deadline or self.proc.poll() is not None:
                    raise RuntimeError("mock server failed to start")
                time.sleep(0.1)

    @property
    def base(self):
        return "http://127.0.0.1:%d" % self.port

    def debug(self, what):
        with urllib.request.urlopen("%s/debug/%s" % (self.base, what), timeout=5) as r:
            return json.loads(r.read() or b"{}")

    def requests(self):
        return self.debug("requests")["requests"]

    def reset(self):
        self.debug("reset")

    def stop(self):
        self.proc.terminate()
        with contextlib.suppress(subprocess.TimeoutExpired):
            self.proc.wait(timeout=5)


# ---------------------------------------------------------------------------
# Stats and reporting
# ---------------------------------------------------------------------------

def pct(values, p):
    s = sorted(values)
    if not s:
        return float("nan")
    k = (len(s) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def summarize(values):
    return {
        "n": len(values),
        "p50": round(pct(values, 50), 2),
        "p95": round(pct(values, 95), 2),
        "max": round(max(values), 2) if values else None,
        "min": round(min(values), 2) if values else None,
        "stdev": round(statistics.stdev(values), 2) if len(values) > 1 else 0.0,
    }


RESULTS = {}


def record(key, values, unit, target=None, note=""):
    summary = summarize(values)
    summary.update({"unit": unit, "target": target, "note": note, "samples": [round(v, 3) for v in values]})
    RESULTS[key] = summary
    tgt = "" if target is None else " (target %s)" % target
    print("  %-46s n=%-3d p50=%8.2f p95=%8.2f max=%8.2f %s%s"
          % (key, summary["n"], summary["p50"], summary["p95"], summary["max"] or 0, unit, tgt),
          flush=True)


def ms(t0, t1):
    return (t1 - t0) * 1000.0


# ---------------------------------------------------------------------------
# Screen predicates
# ---------------------------------------------------------------------------

def has(*needles):
    return lambda text: all(n in text for n in needles)


def lacks(*needles):
    return lambda text: not any(n in text for n in needles)


def open_command(app, title):
    """Type `:open <title>` (not yet submitted); returns once it's echoed."""
    app.send(":")
    app.wait_for(lambda t: t.splitlines()[-1].lstrip().startswith(":"), what="command line")
    app.send("open " + title)
    app.wait_for(has("open " + title), what="typed command")


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

def scenario_cold_start(args):
    """§6.8 "Cold start → interactive < 100 ms (nothing network-blocking on
    the startup path)". Spawn → first complete frame, and → the first
    keypress's effect on screen, for first-run (onboarding) and returning
    users, against a normal mock and one that stalls EVERY response 5 s."""
    print("cold_start", flush=True)
    for mock_label, latency in (("normal", 0), ("stall5s", 5000)):
        mock = Mock(latency_ms=latency)
        try:
            # A returning user: config + a warmed cache/history/index from
            # one earlier session that opened a handful of articles.
            returning = Profile("returning")
            returning.write_config(mock.port)
            if latency == 0:
                seed_returning_profile(returning, mock)
            else:
                seed_returning_profile_offline(returning, args)
            logged_in = Profile("loggedin")
            logged_in.write_config(mock.port)
            shutil.copytree(returning.dirs["state"], logged_in.dirs["state"], dirs_exist_ok=True)
            shutil.copytree(returning.dirs["cache"], logged_in.dirs["cache"], dirs_exist_ok=True)
            write_auth(logged_in)
            variants = [("first_run", None), ("returning", returning), ("returning_logged_in", logged_in)]
            for label, profile in variants:
                frames, acks = [], []
                for i in range(args.n + args.warmup):
                    prof = profile or Profile("firstrun")
                    env_extra = {"WIKITUI_BASE_URL": mock.base} if profile is None else {}
                    app = App(prof, rows=40, cols=120, env_extra=env_extra)
                    try:
                        if profile is None:
                            t_frame = app.wait_for(has("Welcome to wikitui"), what="onboarding",
                                                   timeout=30)
                            t_key = app.send(" ")
                            t_ack = app.wait_for(lacks("Welcome to wikitui"), what="onboarding dismissed",
                                                 timeout=30)
                        else:
                            t_frame = app.wait_for(start_page_marker, what="start page", timeout=30)
                            t_key = app.send("?")
                            t_ack = app.wait_for(help_marker, what="help overlay", timeout=30)
                        if i >= args.warmup:
                            frames.append(ms(app.t_spawn, t_frame))
                            acks.append(ms(app.t_spawn, t_frame) + ms(t_key, t_ack))
                    finally:
                        app.quit()
                        if profile is None:
                            prof.cleanup()
                key = "cold_start.%s.%s" % (mock_label, label)
                record(key + ".first_frame_ms", frames, "ms", "< 100")
                record(key + ".first_key_ack_ms", acks, "ms", "< 100",
                       "first frame + (key sent -> its effect drawn)")
            returning.cleanup()
            logged_in.cleanup()
        finally:
            mock.stop()


def start_page_marker(text):
    """The start page's title row is the first thing a frame paints and the
    bottom key-hint row the last (ratatui writes rows top to bottom), so
    both present means a complete first frame."""
    lines = text.splitlines()
    return bool(lines) and lines[0].startswith("wikitui") and lines[-1].strip() != ""


def help_marker(text):
    # The `?` overlay's title: "<view> — keys".
    return "— keys" in text


def write_auth(profile):
    """A logged-in session's token file: a non-expired access token, so the
    startup path's notification-count poll goes straight to the network
    (no refresh round trip first)."""
    path = profile.dirs["state"] / "wikitui" / "auth.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({
        "access_token": "perf-harness-token",
        "refresh_token": "perf-harness-refresh",
        "expires_at": int(time.time()) + 86_400,
        "username": "PerfReader",
        "editing": False,
    }))
    os.chmod(path, 0o600)


SEED_TITLES = ["Perf_Typical"] + [generate.corpus_title(n) for n in range(1, 9)]


def seed_returning_profile(profile, mock):
    """One earlier session: open a handful of articles (fills the L2 cache,
    history, and the offline search index), then quit."""
    app = App(profile)
    try:
        app.wait_for(start_page_marker, what="start page")
        for title in SEED_TITLES:
            open_command(app, title)
            app.send("\r")
            app.wait_for(has(generate.lead_marker(title)), what="seed open " + title, timeout=60)
        app.wait_quiet(1.0)
    finally:
        app.quit()


def seed_returning_profile_offline(profile, args):
    """The stalled-mock run can't fetch anything to seed with; seed from a
    separate normal mock instead (same corpus, same titles)."""
    mock = Mock()
    try:
        profile.write_config(mock.port)
        seed_returning_profile(profile, mock)
    finally:
        mock.stop()


def open_and_wait(app, title, timeout=60):
    """`:open <title>`, Enter, wait for its lead marker; returns ms from the
    Enter keypress to the lead section on screen."""
    open_command(app, title)
    t_key = app.send("\r")
    t_seen = app.wait_for(has(generate.lead_marker(title)), what="lead of " + title, timeout=timeout)
    return ms(t_key, t_seen)


def scenario_open_cached(args):
    """§6.8 "Article open, L1 cache hit < 50 ms" and "L2 hit (re-layout)
    < 150 ms". L1: reopen via Back after visiting another article (same
    width → L1 layout hit). L2: a fresh process whose disk cache already
    holds the article (no L1 at all → read + decompress + parse + layout)."""
    print("open_cached", flush=True)
    mock = Mock()
    try:
        for fixture, other in (("Perf_Typical", "Perf_Corpus_1"), ("Perf_Pathological", "Perf_Typical")):
            profile = Profile("cached")
            profile.write_config(mock.port)
            try:
                # Warm L2 for both articles in one session.
                app = App(profile)
                try:
                    app.wait_for(start_page_marker, what="start page")
                    open_and_wait(app, fixture)
                    open_and_wait(app, other)
                    app.wait_quiet(1.0)
                finally:
                    app.quit()
                # L1: back/forward between the two, in one process.
                l1 = []
                app = App(profile)
                try:
                    app.wait_for(start_page_marker, what="start page")
                    open_and_wait(app, fixture)
                    open_and_wait(app, other)
                    app.wait_quiet(0.5)
                    for i in range(args.n + args.warmup):
                        t_key = app.send("\x7f")  # Backspace: back (vim preset)
                        t_seen = app.wait_for(has(generate.lead_marker(fixture)), what="back to fixture")
                        if i >= args.warmup:
                            l1.append(ms(t_key, t_seen))
                        app.wait_quiet(0.2)
                        app.send("L")  # forward
                        app.wait_for(has(generate.lead_marker(other)), what="forward to other")
                        app.wait_quiet(0.2)
                finally:
                    app.quit()
                record("open.l1_hit.%s_ms" % fixture, l1, "ms", "< 50",
                       "Back to an article laid out at this width (L1 layout hit; L2 read + parse still happen)")
                # L2: a fresh process per sample, warm disk cache.
                l2 = []
                for i in range(args.n + args.warmup):
                    app = App(profile)
                    try:
                        app.wait_for(start_page_marker, what="start page")
                        app.wait_quiet(0.3)
                        t = open_and_wait(app, fixture)
                        if i >= args.warmup:
                            l2.append(t)
                    finally:
                        app.quit()
                record("open.l2_hit.%s_ms" % fixture, l2, "ms", "< 150",
                       "fresh process, article in the disk cache (read + zstd + parse + layout + paint)")
            finally:
                profile.cleanup()
    finally:
        mock.stop()


def network_titles(count, rng):
    return [generate.corpus_title(n) for n in rng.sample(range(10, generate.CORPUS_SIZE), count)]


def scenario_open_network(args):
    """§6.8 "Article open, network (broadband p50) < 800 ms to first paint".
    Cold cache, fresh process per sample; the mock emulates broadband
    (BROADBAND_RTT_MS per request, BROADBAND_KBIT per connection, gzip) and
    serves the seeded corpus's size mix. Reported for the mix and per size
    class, so no verdict rests on the mix's weights alone."""
    print("open_network", flush=True)
    mock = Mock(latency_ms=BROADBAND_RTT_MS, kbit=BROADBAND_KBIT, gzip=True)
    rng = random.Random(0x6_8_800)
    by_class = {}
    mix = []
    try:
        titles = network_titles(args.n_network + args.warmup, rng)
        for i, title in enumerate(titles):
            profile = Profile("network")
            profile.write_config(mock.port)
            app = App(profile)
            try:
                app.wait_for(start_page_marker, what="start page")
                app.wait_quiet(0.5)
                t = open_and_wait(app, title)
            finally:
                app.quit()
                profile.cleanup()
            if i < args.warmup:
                continue
            label = generate.corpus_size_class(int(title.rsplit("_", 1)[1]))[0]
            by_class.setdefault(label, []).append(t)
            mix.append(t)
        record("open.network.mix_ms", mix, "ms", "p50 < 800",
               "first paint of the lead section; corpus size mix, %d ms RTT, %d kbit/s, gzip"
               % (BROADBAND_RTT_MS, BROADBAND_KBIT))
        for label in ("short", "typical", "long", "very-long"):
            if label in by_class:
                record("open.network.%s_ms" % label, by_class[label], "ms", "p50 < 800")
        # The pathological article over the same link, for the record.
        patho = []
        for i in range(max(5, args.n // 4) + 1):
            profile = Profile("network")
            profile.write_config(mock.port)
            app = App(profile)
            try:
                app.wait_for(start_page_marker, what="start page")
                app.wait_quiet(0.5)
                t = open_and_wait(app, "Perf_Pathological")
            finally:
                app.quit()
                profile.cleanup()
            if i >= 1:
                patho.append(t)
        record("open.network.pathological_ms", patho, "ms", "p50 < 800")
    finally:
        mock.stop()


def scenario_scroll(args):
    """§6.8 "Scroll: no dropped input at 60 Hz redraw budget (≤ 16 ms per
    frame)". Key-repeat `j` at 60 Hz (and a 120 Hz burst) over the
    pathological article; the perf log's per-frame scroll offset must end
    exactly at the number of presses, and its per-frame draw times are the
    distribution reported."""
    print("scroll", flush=True)
    mock = Mock()
    try:
        for rows, cols in ((24, 80), (40, 120), (60, 200)):
            for hz, presses in ((60, 300), (120, 300)):
                profile = Profile("scroll")
                profile.write_config(mock.port)
                app = App(profile, rows=rows, cols=cols)
                try:
                    app.wait_for(start_page_marker, what="start page")
                    open_and_wait(app, "Perf_Pathological")
                    app.wait_quiet(0.5)
                    first_frame = len(app.perf_events("frame"))
                    interval = 1.0 / hz
                    t0 = time.monotonic()
                    for k in range(presses):
                        target = t0 + k * interval
                        delay = target - time.monotonic()
                        if delay > 0:
                            time.sleep(delay)
                        app.send("j")
                    app.wait_quiet(0.5)
                    frames = app.perf_events("frame")[first_frame:]
                finally:
                    app.quit()
                    profile.cleanup()
                final = frames[-1]["scroll"] if frames else None
                draws = [f["draw_us"] / 1000.0 for f in frames]
                key = "scroll.%dx%d.%dhz" % (cols, rows, hz)
                record(key + ".draw_ms", draws, "ms", "<= 16",
                       "final scroll %s after %d presses (%s)" % (final, presses,
                                                                 "no drops" if final == presses else "DROPPED"))
                RESULTS[key + ".draw_ms"]["final_scroll"] = final
                RESULTS[key + ".draw_ms"]["presses"] = presses
                RESULTS[key + ".draw_ms"]["frames"] = len(frames)
    finally:
        mock.stop()


MEMORY_TITLES = ["Perf_Pathological"] + [
    generate.corpus_title(n) for n in range(generate.CORPUS_SIZE)
    if generate.corpus_size_class(n)[0] in ("very-long", "long")][:9]


def scenario_memory(args, low_memory=False):
    """§6.8 "Memory < 150 MB RSS with 10 tabs; low_memory mode < 50 MB".
    Opens the pathological article plus the corpus's nine first long/very-
    long articles, each in its own tab (`:tab new`), then reads VmRSS and
    VmHWM from /proc/<pid>/status once output settles."""
    label = "low_memory" if low_memory else "default"
    print("memory (%s)" % label, flush=True)
    mock = Mock()
    rss, hwm = [], []
    try:
        for i in range(args.n_memory):
            profile = Profile("memory")
            profile.write_config(mock.port, extra="low_memory = true\n" if low_memory else "")
            app = App(profile)
            try:
                app.wait_for(start_page_marker, what="start page")
                open_and_wait(app, MEMORY_TITLES[0])
                for title in MEMORY_TITLES[1:]:
                    app.send(":")
                    app.wait_for(lambda t: t.splitlines()[-1].lstrip().startswith(":"), what="command line")
                    app.send("tab new " + title + "\r")
                    app.wait_for(has(generate.lead_marker(title)), what="tab " + title, timeout=60)
                app.wait_quiet(2.0)
                r, h = app.rss_kb()
                rss.append(r / 1024.0)
                hwm.append(h / 1024.0)
            finally:
                app.quit()
                profile.cleanup()
        target = "< 50" if low_memory else "< 150"
        record("memory.%s.10_tabs.rss_mb" % label, rss, "MB", target,
               "tabs: " + ", ".join(MEMORY_TITLES))
        record("memory.%s.10_tabs.hwm_mb" % label, hwm, "MB", target, "peak RSS (VmHWM)")
    finally:
        mock.stop()


def scenario_search(args):
    """§6.8 "Search suggestions: render < 100 ms after response; debounce
    150–250 ms". Type a query at human speed, then measure (a) last
    keystroke → the typeahead request arriving at the mock (the debounce,
    on the shared CLOCK_MONOTONIC), and (b) the app's own perf-log
    `typeahead_render` (response decoded → first frame showing it)."""
    print("search", flush=True)
    mock = Mock()
    debounce, render = [], []
    try:
        profile = Profile("search")
        profile.write_config(mock.port)
        app = App(profile)
        try:
            app.wait_for(start_page_marker, what="start page")
            app.wait_quiet(0.5)
            for i in range(args.n + args.warmup):
                mock.reset()
                app.send("/")
                app.wait_for(lambda t: "Search" in t or "search" in t, what="search prompt")
                query = "alan"[: 2 + i % 3]
                t_last = None
                for ch in query:
                    t_last = app.send(ch)
                    time.sleep(0.06)
                app.wait_for(has("Alan Turing"), what="suggestions", timeout=10)
                app.wait_quiet(0.4)
                reqs = [r for r in mock.requests() if "/search/title" in r["path"]]
                app.send("\x1b")
                app.wait_quiet(0.3)
                if i < args.warmup or not reqs:
                    continue
                debounce.append(ms(t_last, reqs[-1]["t"]))
            render = [e["us"] / 1000.0 for e in app.perf_events("typeahead_render")][args.warmup:]
        finally:
            app.quit()
            profile.cleanup()
        record("search.debounce_ms", debounce, "ms", "150-250",
               "last keystroke -> typeahead request at the server")
        record("search.render_after_response_ms", render, "ms", "< 100",
               "response decoded -> first frame showing it (app perf log)")
    finally:
        mock.stop()


SCENARIOS = {
    "cold_start": scenario_cold_start,
    "open_cached": scenario_open_cached,
    "open_network": scenario_open_network,
    "scroll": scenario_scroll,
    "memory": scenario_memory,
    "search": scenario_search,
}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("scenarios", nargs="*", help="subset to run (default: all): " + ", ".join(SCENARIOS))
    ap.add_argument("--n", type=int, default=20, help="samples per row after warm-up (default 20)")
    ap.add_argument("--n-network", type=int, default=40, help="network-open samples (default 40)")
    ap.add_argument("--n-memory", type=int, default=3, help="memory runs (default 3)")
    ap.add_argument("--warmup", type=int, default=2, help="discarded warm-up samples per row")
    ap.add_argument("--low-memory", action="store_true", help="also run memory with low_memory = true")
    ap.add_argument("--out", type=pathlib.Path, help="write results JSON here")
    args = ap.parse_args()
    if not BIN.exists():
        sys.exit("build the release binary first: cargo build --release")
    chosen = args.scenarios or list(SCENARIOS)
    for name in chosen:
        if name == "memory":
            scenario_memory(args)
            if args.low_memory:
                scenario_memory(args, low_memory=True)
        else:
            SCENARIOS[name](args)
    if args.out:
        args.out.write_text(json.dumps({
            "results": RESULTS,
            "binary": str(BIN),
            "when": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        }, indent=1))


if __name__ == "__main__":
    main()
