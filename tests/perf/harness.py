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
import atexit
import codecs
import contextlib
import fcntl
import json
import os
import pathlib
import random
import re
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
# Child processes: every one is tracked and reaped, even on an exception.
# (A wikitui whose pty hangs up without a clean quit can keep running — see
# `App.quit` — so nothing this harness starts may outlive it: stray busy
# processes would skew every later measurement on the machine.)
# ---------------------------------------------------------------------------

CHILDREN = []


def set_binary(path):
    """Drive a different build (e.g. a baseline for a before/after run)."""
    global BIN
    BIN = pathlib.Path(path).resolve()


def reap_all():
    for proc in CHILDREN:
        if proc.poll() is None:
            with contextlib.suppress(OSError):
                os.killpg(proc.pid, signal.SIGKILL)
            with contextlib.suppress(OSError):
                proc.kill()
            with contextlib.suppress(subprocess.TimeoutExpired):
                proc.wait(timeout=5)
    CHILDREN.clear()


atexit.register(reap_all)


def surviving_children():
    """PIDs of processes still running this harness's binary or mock."""
    out = []
    for proc_dir in pathlib.Path("/proc").iterdir():
        if not proc_dir.name.isdigit():
            continue
        try:
            cmd = (proc_dir / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if cmd and (cmd[0] == str(BIN).encode() or
                    (len(cmd) > 1 and cmd[1] == str(MOCK).encode())):
            out.append(int(proc_dir.name))
    return out


def assert_no_survivors(where):
    reap_all()
    left = surviving_children()
    if left:
        for pid in left:
            with contextlib.suppress(OSError):
                os.kill(pid, signal.SIGKILL)
        raise RuntimeError("%s: %d harness process(es) outlived their scenario: %s" % (where, len(left), left))


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
        self.last_send = self.t_spawn
        self.proc = subprocess.Popen([str(BIN), *args], stdin=slave, stdout=slave, stderr=slave,
                                     env=env, start_new_session=True, close_fds=True)
        CHILDREN.append(self.proc)
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
        """Wait until neither output nor our own input has happened for
        `quiet` seconds. Counting from the last key sent too matters: a
        check made right after a keypress must give the app time to answer
        it, not return at once because the screen was already quiet."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                last = max(getattr(self, "last_output", self.t_spawn), self.last_send)
            if time.monotonic() - last >= quiet:
                return
            time.sleep(quiet / 4)
        raise TimeoutError("output never went quiet")

    def send(self, data):
        if isinstance(data, str):
            data = data.encode()
        t = time.monotonic()
        os.write(self.master, data)
        with self.lock:
            self.last_send = t
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
        """Quit cleanly (`:q`), then make sure: SIGKILL the process group
        if it's still there after 3 s, and always reap it. Never just close
        the pty — a hung-up wikitui can keep running."""
        if self.proc.poll() is None:
            with contextlib.suppress(OSError):
                self.send("\x1b")
                time.sleep(0.05)
                self.send(":q\r")
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(OSError):
                    os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait(timeout=5)
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
        self.proc = subprocess.Popen([sys.executable, str(MOCK)], env=env, start_new_session=True,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        CHILDREN.append(self.proc)
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
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
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


READ_TIME = re.compile(r"^\s*\d+ min read\s*$")


def article_header(text):
    """The title of the article at the top of the screen, if one is there:
    the layout's first two rows are the title alone, then "N min read" —
    ratatui paints rows top to bottom, so this is the first thing an opened
    article puts on screen, at any terminal size (at 80 columns the lead
    paragraph itself sits below an inline infobox card)."""
    lines = text.splitlines()
    for i in range(min(len(lines) - 1, 4)):
        if READ_TIME.match(lines[i + 1]) and lines[i].strip():
            return lines[i].strip()
    return None


def showing(title):
    """Predicate: `title`'s article is painted at the top of the screen."""
    display = title.replace("_", " ")
    return lambda text: article_header(text) == display


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
            app.wait_for(showing(title), what="seed open " + title, timeout=60)
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
    """`:open <title>`, Enter, wait for the article's first paint (its
    header rows at the top of the screen); returns ms from the Enter
    keypress to that paint."""
    open_command(app, title)
    t_key = app.send("\r")
    t_seen = app.wait_for(showing(title), what="first paint of " + title, timeout=timeout)
    return ms(t_key, t_seen)


def scenario_open_cached(args):
    """§6.8 "Article open, L1 cache hit < 50 ms" and "L2 hit (re-layout)
    < 150 ms", plus tab switching (default and low_memory — in low_memory a
    background tab keeps only compressed HTML, so switching to it re-parses
    and relayouts: judged against the L2 target, the same work).

    L1: `H` (back) to an article laid out at this width in the same process.
    Tab switch: `gt` between two tabs. L2: a fresh process whose disk cache
    already holds the article, `:open` Enter. Each against a zero-latency
    mock and the broadband emulation — a cached open must not wait on the
    network, so the two should match."""
    print("open_cached", flush=True)
    for net_label, latency, kbit in (("local", 0, 0), ("broadband", BROADBAND_RTT_MS, BROADBAND_KBIT)):
        mock = Mock(latency_ms=latency, kbit=kbit, gzip=bool(kbit))
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
                            t_key = app.send("H")
                            t_seen = app.wait_for(showing(fixture), what="back to fixture")
                            if i >= args.warmup:
                                l1.append(ms(t_key, t_seen))
                            app.wait_quiet(0.2)
                            app.send("L")
                            app.wait_for(showing(other), what="forward to other")
                            app.wait_quiet(0.2)
                    finally:
                        app.quit()
                    record("open.%s.l1_hit.%s_ms" % (net_label, fixture), l1, "ms", "< 50",
                           "H (back) to an article already laid out at this width")
                    # Tab switch, default and low_memory.
                    for mode in ("default", "low_memory"):
                        switch = []
                        extra = ["--low-memory"] if mode == "low_memory" else []
                        app = App(profile, args=extra)
                        try:
                            app.wait_for(start_page_marker, what="start page")
                            open_and_wait(app, fixture)
                            app.send(":")
                            app.wait_for(lambda t: t.splitlines()[-1].lstrip().startswith(":"),
                                         what="command line")
                            app.send("tab new " + other + "\r")
                            app.wait_for(showing(other), what="second tab")
                            app.wait_quiet(0.5)
                            for i in range(args.n + args.warmup):
                                t_key = app.send("gt")
                                t_seen = app.wait_for(showing(fixture), what="switch to fixture tab")
                                if i >= args.warmup:
                                    switch.append(ms(t_key, t_seen))
                                app.wait_quiet(0.2)
                                app.send("gt")
                                app.wait_for(showing(other), what="switch back")
                                app.wait_quiet(0.2)
                        finally:
                            app.quit()
                        target = "< 150" if mode == "low_memory" else "< 50"
                        record("open.%s.tab_switch_%s.%s_ms" % (net_label, mode, fixture), switch, "ms", target,
                               "gt to a background tab" + (" (re-parse from compressed source)"
                                                           if mode == "low_memory" else " (L1 layout hit)"))
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
                    record("open.%s.l2_hit.%s_ms" % (net_label, fixture), l2, "ms", "< 150",
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
               "first paint of the article's top rows (the whole article is fetched and laid "
               "out first; no lead-first paint); corpus size mix, %d ms RTT, %d kbit/s, gzip"
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
        # Two-key chords too: `gt` (next tab) bursts at 60 Hz across three
        # tabs, default and low_memory (where each switch re-parses). Every
        # key must produce exactly one frame and the burst must land on the
        # tab its count predicts.
        tabs = ["Perf_Typical", "Perf_Corpus_1", "Perf_Pathological"]
        for mode in ("default", "low_memory"):
            profile = Profile("chords")
            profile.write_config(mock.port)
            app = App(profile, args=["--low-memory"] if mode == "low_memory" else [])
            rounds, dropped, wrong = 0, 0, 0
            try:
                app.wait_for(start_page_marker, what="start page")
                open_and_wait(app, tabs[0])
                for t in tabs[1:]:
                    app.send(":")
                    app.wait_for(lambda s: s.splitlines()[-1].lstrip().startswith(":"), what="command line")
                    app.send("tab new " + t + "\r")
                    app.wait_for(showing(t), what="tab " + t)
                app.wait_quiet(0.5)
                display = [t.replace("_", " ") for t in tabs]
                for _ in range(max(10, args.n // 2)):
                    idx = display.index(article_header(app.text()))
                    n0 = len(app.perf_events("frame"))
                    k = 7
                    t0 = time.monotonic()
                    for i in range(k):
                        delay = t0 + i / 60.0 - time.monotonic()
                        if delay > 0:
                            time.sleep(delay)
                        app.send("gt")
                    deadline = time.monotonic() + 10
                    while len(app.perf_events("frame")) < n0 + 2 * k and time.monotonic() < deadline:
                        time.sleep(0.02)
                    app.wait_quiet(0.3)
                    rounds += 1
                    if len(app.perf_events("frame")) - n0 != 2 * k:
                        dropped += 1
                    if article_header(app.text()) != display[(idx + k) % len(tabs)]:
                        wrong += 1
            finally:
                app.quit()
                profile.cleanup()
            RESULTS["keys.gt_burst_60hz.%s" % mode] = {
                "rounds": rounds, "keys_per_round": 14, "rounds_with_missing_frames": dropped,
                "rounds_on_wrong_tab": wrong, "target": "no dropped input",
            }
            print("  keys.gt_burst_60hz.%-26s %d rounds x 7 gt: %d with a missing frame, %d on the wrong tab"
                  % (mode, rounds, dropped, wrong), flush=True)
    finally:
        mock.stop()


MEMORY_SETS = {
    # The pathological fixture plus the corpus's first nine long/very-long
    # articles: 0.43–1.55 MB each, ~8 MB of HTML across the ten tabs.
    "mixed": ["Perf_Pathological"] + [
        generate.corpus_title(n) for n in range(generate.CORPUS_SIZE)
        if generate.corpus_size_class(n)[0] in ("very-long", "long")][:9],
    # The worst case: ten distinct ~1.55 MB, 520-reference articles.
    "ten_1.5MB": [generate.large_title(n) for n in range(generate.LARGE_COUNT)],
}


def scenario_memory(args, low_memory=False):
    """§6.8 "Memory < 150 MB RSS with 10 tabs; low_memory mode < 50 MB".
    For each tab set: open the first article, then each of the others in a
    new tab (`:tab new`), then visit every tab once more (`gt` around the
    ring — in low_memory mode each visit re-parses), and read VmRSS/VmHWM
    from /proc/<pid>/status once output settles. Cold cache, fresh process
    and profile per run."""
    label = "low_memory" if low_memory else "default"
    print("memory (%s)" % label, flush=True)
    mock = Mock()
    try:
        for set_name, titles in MEMORY_SETS.items():
            rss, hwm = [], []
            for _ in range(args.n_memory):
                profile = Profile("memory")
                profile.write_config(mock.port, extra="low_memory = true\n" if low_memory else "")
                app = App(profile)
                try:
                    app.wait_for(start_page_marker, what="start page")
                    open_and_wait(app, titles[0])
                    for title in titles[1:]:
                        app.send(":")
                        app.wait_for(lambda t: t.splitlines()[-1].lstrip().startswith(":"),
                                     what="command line")
                        app.send("tab new " + title + "\r")
                        app.wait_for(showing(title), what="tab " + title, timeout=60)
                    # Every tab once more, ending back on the last one.
                    for title in titles:
                        app.send("gt")
                        app.wait_for(showing(title), what="revisit " + title, timeout=60)
                    app.wait_quiet(2.0)
                    r, h = app.rss_kb()
                    rss.append(r / 1024.0)
                    hwm.append(h / 1024.0)
                finally:
                    app.quit()
                    profile.cleanup()
            target = "< 50" if low_memory else "< 150"
            key = "memory.%s.%s" % (label, set_name)
            record(key + ".rss_mb", rss, "MB", target, "10 tabs: " + ", ".join(titles))
            record(key + ".hwm_mb", hwm, "MB", target, "peak RSS (VmHWM)")
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
                rendered = len(app.perf_events("typeahead_render"))
                t_last = None
                for ch in query:
                    t_last = app.send(ch)
                    time.sleep(0.06)
                # The app's own log says when the suggestions were drawn. (Not
                # "wait until the screen is quiet": Search mode redraws on its
                # 30 ms debounce tick, so it never is.)
                deadline = time.monotonic() + 10
                while (len(app.perf_events("typeahead_render")) <= rendered
                       and time.monotonic() < deadline):
                    time.sleep(0.02)
                time.sleep(0.3)
                reqs = [r for r in mock.requests() if "/search/title" in r["path"]]
                app.send("\x1b")
                time.sleep(0.3)
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


def read_open_log(profile):
    """The app's own cache-hit log (`src/hitrate.rs`): all-time counts."""
    path = profile.dirs["state"] / "wikitui" / "cache_hits.json"
    try:
        return json.loads(path.read_text())["all_time"]
    except (OSError, ValueError, KeyError):
        return {"l1": 0, "disk": 0, "saved_offline": 0, "network": 0}


def opens_counted(profile):
    return sum(read_open_log(profile).values())


# The browsing model behind the cache-hit-rate simulation (README "Cache-hit
# rate simulation" has the reasoning). Per step, after the first open of a
# session: go back, revisit something read earlier, jump to a new topic, or
# (the rest) follow a link.
SIM_P_BACK = 0.20
SIM_P_REVISIT = 0.10
SIM_P_JUMP = 0.05
SIM_DWELL_S = 4.0
# The simulation compresses reading time: a SIM_DWELL_S dwell stands in for a
# ~2-minute real one, so the prefetch budgets (per wall-clock hour/day) are
# scaled by the same factor in the "scaled budgets" variants.
SIM_TIME_COMPRESSION = 30


def sim_link_index(rng, model):
    """Which link (0-based, document order) the simulated reader follows."""
    if model == "lead_weighted":
        # Clicks concentrate on early (lead/infobox-adjacent) links; an
        # exponential with mean 6 puts ~57% of follows in the first 5.
        return min(int(rng.expovariate(1 / 6.0)), 39)
    return rng.randrange(40)  # "uniform40": any of the first 40 links


def scenario_hit_rate(args):
    """§6.8 "Steady-state cache-hit rate > 60% of article opens" — a
    SIMULATION, not a field measurement: a seeded random walk over the mock's
    cross-linked perf corpus (link following, back navigation, revisits and
    topic jumps, with prefetch on, over broadband emulation and several
    sessions), scored by the app's own cache-hit log — the figure `:stats`
    shows. Variants vary the two assumptions the result is most sensitive to:
    which links get clicked, and whether prefetch budgets are scaled for the
    simulation's time compression."""
    print("hit_rate (simulation)", flush=True)
    variants = [
        ("lead_weighted.scaled_budgets", "lead_weighted", True),
        ("lead_weighted.default_budgets", "lead_weighted", False),
        ("uniform40.scaled_budgets", "uniform40", True),
    ]
    mock = Mock(latency_ms=BROADBAND_RTT_MS, kbit=BROADBAND_KBIT, gzip=True)
    try:
        for label, model, scaled in variants:
            rng = random.Random(0x60_0608)
            profile = Profile("hitrate")
            extra = ""
            if scaled:
                extra = "[prefetch]\ndaily_mb = %d\nhourly_requests = %d\n" % (
                    20 * SIM_TIME_COMPRESSION, 100 * SIM_TIME_COMPRESSION)
            # [prefetch] must follow the top-level keys write_config adds.
            profile.write_config(mock.port)
            if extra:
                with open(profile.config_path, "a") as f:
                    f.write("\n" + extra)
            read = []
            steps = failed = 0
            try:
                for session in range(args.sim_sessions):
                    app = App(profile)
                    try:
                        app.wait_for(start_page_marker, what="start page")
                        app.wait_quiet(0.5)
                        depth = 0
                        current = generate.corpus_title(rng.randrange(generate.CORPUS_SIZE))
                        before = opens_counted(profile)
                        open_and_wait(app, current)
                        read.append(current)
                        time.sleep(SIM_DWELL_S)
                        for _ in range(args.sim_steps):
                            steps += 1
                            before = opens_counted(profile)
                            r = rng.random()
                            if r < SIM_P_BACK and depth > 0:
                                app.send("H")
                                depth -= 1
                            elif r < SIM_P_BACK + SIM_P_REVISIT and len(read) > 1:
                                title = rng.choice([t for t in read if t != current] or read)
                                open_command(app, title)
                                app.send("\r")
                                depth += 1
                            elif r < SIM_P_BACK + SIM_P_REVISIT + SIM_P_JUMP:
                                title = generate.corpus_title(rng.randrange(generate.CORPUS_SIZE))
                                open_command(app, title)
                                app.send("\r")
                                depth += 1
                            else:
                                app.send("\t" * (sim_link_index(rng, model) + 1))
                                app.wait_quiet(0.2)
                                app.send("\r")
                                depth += 1
                            deadline = time.monotonic() + 10
                            while opens_counted(profile) == before and time.monotonic() < deadline:
                                time.sleep(0.05)
                            if opens_counted(profile) == before:
                                # No open landed (a redlink, say): recover
                                # and don't count the step.
                                failed += 1
                                app.send("\x1b")
                                app.wait_quiet(0.3)
                                continue
                            app.wait_quiet(0.3)
                            header = article_header(app.text())
                            if header:
                                current = header.replace(" ", "_")
                                read.append(current)
                            time.sleep(SIM_DWELL_S)
                    finally:
                        app.quit()
                counts = read_open_log(profile)
            finally:
                profile.cleanup()
            total = sum(counts.values())
            hits = total - counts["network"]
            rate = 100.0 * hits / total if total else float("nan")
            print("  hit_rate.%-40s %.1f%% of %d opens (L1 %d, disk %d, saved/offline %d, network %d; "
                  "%d steps, %d without an open)"
                  % (label, rate, total, counts["l1"], counts["disk"], counts["saved_offline"],
                     counts["network"], steps, failed), flush=True)
            RESULTS["hit_rate." + label] = {
                "rate_percent": round(rate, 1), "opens": total, "counts": counts,
                "steps": steps, "steps_without_open": failed, "target": "> 60",
                "note": "SIMULATION (seeded random walk over the mock corpus), not a field measurement",
            }
    finally:
        mock.stop()


SCENARIOS = {
    "cold_start": scenario_cold_start,
    "open_cached": scenario_open_cached,
    "open_network": scenario_open_network,
    "scroll": scenario_scroll,
    "memory": scenario_memory,
    "search": scenario_search,
    "hit_rate": scenario_hit_rate,
}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("scenarios", nargs="*", help="subset to run (default: all): " + ", ".join(SCENARIOS))
    ap.add_argument("--n", type=int, default=20, help="samples per row after warm-up (default 20)")
    ap.add_argument("--n-network", type=int, default=40, help="network-open samples (default 40)")
    ap.add_argument("--n-memory", type=int, default=3, help="memory runs (default 3)")
    ap.add_argument("--warmup", type=int, default=2, help="discarded warm-up samples per row")
    ap.add_argument("--low-memory", action="store_true", help="also run memory with low_memory = true")
    ap.add_argument("--sim-sessions", type=int, default=3, help="hit_rate: sessions per variant")
    ap.add_argument("--sim-steps", type=int, default=60, help="hit_rate: browsing steps per session")
    ap.add_argument("--out", type=pathlib.Path, help="write results JSON here")
    ap.add_argument("--bin", type=pathlib.Path, default=BIN,
                    help="the wikitui binary to drive (default: target/release/wikitui)")
    args = ap.parse_args()
    set_binary(args.bin)
    if not BIN.exists():
        sys.exit("build the release binary first: cargo build --release")
    chosen = args.scenarios or list(SCENARIOS)
    assert_no_survivors("before starting")

    def save():
        # After every scenario, so a later failure never loses finished rows.
        if args.out:
            args.out.write_text(json.dumps({
                "results": RESULTS,
                "binary": str(BIN),
                "when": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            }, indent=1))

    for name in chosen:
        if name == "memory":
            scenario_memory(args)
            if args.low_memory:
                scenario_memory(args, low_memory=True)
        else:
            SCENARIOS[name](args)
        save()
        assert_no_survivors(name)


if __name__ == "__main__":
    main()
