#!/usr/bin/env python3
"""Self-tests for the §6.8 perf tooling (stdlib `unittest`; run with
`python3 -m unittest discover -s tests/perf -p 'test_*.py'`):

- the fixture generator is deterministic and the committed fixtures match it;
- the harness's VT screen model reads crossterm/ratatui-style output the way
  a terminal would (cursor addressing, wide characters, dropped SGR/OSC) and
  answers the queries a terminal answers;
- the mock server's perf-mode options actually emulate what they claim
  (per-request latency, per-connection bandwidth, gzip, the perf corpus) and
  leave /debug/ unshaped.

These pin the measuring instruments, so a number in docs/PERFORMANCE.md
can't silently come from a broken ruler.
"""

import gzip
import json
import pathlib
import subprocess
import sys
import time
import unittest
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / "fixtures"))
sys.dont_write_bytecode = True

import generate  # noqa: E402
import harness  # noqa: E402


class GeneratorTests(unittest.TestCase):
    def test_same_seed_same_bytes(self):
        self.assertEqual(generate.typical(), generate.typical())
        self.assertEqual(generate.corpus_article(7), generate.corpus_article(7))

    def test_committed_fixtures_match_the_generator(self):
        for name, build in generate.FIXTURES.items():
            self.assertEqual((HERE / "fixtures" / name).read_bytes(), build().encode("utf-8"),
                             "%s is stale: re-run tests/perf/fixtures/generate.py" % name)

    def test_pathological_fixture_is_the_prd_shape(self):
        shape = generate.describe(generate.pathological())
        self.assertGreaterEqual(shape["bytes"], 1_500_000)
        self.assertLessEqual(shape["bytes"], 1_650_000)
        self.assertGreaterEqual(shape["references"], 500)

    def test_every_generated_link_stays_in_the_corpus(self):
        html = generate.corpus_article(3)
        import re
        targets = re.findall(r'rel="mw:WikiLink" href="\./([^"#?]+)"', html)
        corpus = {generate.corpus_title(n) for n in range(generate.CORPUS_SIZE)}
        internal = [t for t in targets if t.startswith("Perf_Corpus_")]
        self.assertTrue(internal)
        self.assertTrue(set(internal) <= corpus)

    def test_lead_marker_is_in_the_first_paragraph(self):
        html = generate.corpus_article(11)
        marker = generate.lead_marker(generate.corpus_title(11))
        self.assertIn(marker, html)
        self.assertLess(html.index(marker), html.index("<section data-mw-section-id=\"1\""))


class ScreenTests(unittest.TestCase):
    def test_cursor_addressing_and_sgr(self):
        s = harness.Screen(3, 10)
        s.feed(b"\x1b[?1049h\x1b[2J\x1b[2;3H\x1b[1;31mhi\x1b[0m\x1b[3;1Hbye")
        self.assertEqual(s.row(1), "  hi      ")
        self.assertEqual(s.row(2), "bye       ")

    def test_wide_characters_take_two_cells(self):
        s = harness.Screen(1, 6)
        s.feed("\x1b[1;1H日本x".encode())
        self.assertEqual(s.row(0), "日本x ")
        self.assertEqual(s.c, 5)

    def test_osc_sequences_are_dropped(self):
        s = harness.Screen(1, 12)
        s.feed(b"\x1b]8;;https://example.org\x1b\\link\x1b]8;;\x1b\\!")
        self.assertEqual(s.row(0).rstrip(), "link!")

    def test_utf8_split_across_reads(self):
        s = harness.Screen(1, 4)
        data = "é".encode()
        s.feed(data[:1])
        s.feed(data[1:])
        self.assertEqual(s.row(0).rstrip(), "é")

    def test_erase_in_line_and_display(self):
        s = harness.Screen(2, 5)
        s.feed(b"abcde\x1b[2;1Hfghij\x1b[1;3H\x1b[K")
        self.assertEqual(s.row(0), "ab   ")
        s.feed(b"\x1b[2J")
        self.assertEqual(s.text(), "     \n     ")

    def test_answers_terminal_queries(self):
        s = harness.Screen(5, 5)
        s.feed(b"\x1b[3;2H\x1b[6n\x1b[c\x1b]11;?\x07")
        self.assertEqual(s.replies, [b"\x1b[3;2R", b"\x1b[?62;22c", b"\x1b]11;rgb:0000/0000/0000\x1b\\"])


class MockPerfModeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mock = harness.Mock(latency_ms=150, kbit=4_000, gzip=True, corpus=True)

    @classmethod
    def tearDownClass(cls):
        cls.mock.stop()

    def fetch(self, path, gzip_ok=False):
        req = urllib.request.Request(self.mock.base + path)
        if gzip_ok:
            req.add_header("Accept-Encoding", "gzip")
        t0 = time.monotonic()
        with urllib.request.urlopen(req, timeout=30) as r:
            body = r.read()
            headers = dict(r.headers)
        return time.monotonic() - t0, body, headers

    def test_latency_applies_to_every_response_but_debug(self):
        elapsed, _, _ = self.fetch("/w/rest.php/v1/search/title?q=al&limit=10")
        self.assertGreaterEqual(elapsed, 0.15)
        elapsed, _, _ = self.fetch("/debug/requests")
        self.assertLess(elapsed, 0.1)

    def test_bandwidth_throttles_the_body(self):
        # 155 KB uncompressed at 4 Mbit/s is ~0.31 s on the wire, plus the
        # 0.15 s latency.
        elapsed, body, _ = self.fetch("/w/rest.php/v1/page/Perf_Typical/html")
        self.assertGreater(len(body), 150_000)
        self.assertGreaterEqual(elapsed, 0.15 + 0.9 * len(body) * 8 / 4_000_000)

    def test_gzip_only_when_accepted(self):
        _, plain, headers = self.fetch("/w/rest.php/v1/page/Perf_Typical/html")
        self.assertNotIn("Content-Encoding", headers)
        _, packed, headers = self.fetch("/w/rest.php/v1/page/Perf_Typical/html", gzip_ok=True)
        self.assertEqual(headers.get("Content-Encoding"), "gzip")
        self.assertEqual(gzip.decompress(packed), plain)
        self.assertLess(len(packed), len(plain) / 3)

    def test_perf_corpus_is_served(self):
        _, body, _ = self.fetch("/w/rest.php/v1/page/Perf_Corpus_42/html")
        self.assertEqual(body.decode(), generate.corpus_article(42))

    def test_request_log_carries_monotonic_arrival_times(self):
        before = time.monotonic()
        self.fetch("/w/rest.php/v1/search/title?q=zz&limit=10")
        reqs = [r for r in self.mock.requests() if "q=zz" in r["path"]]
        self.assertTrue(reqs)
        self.assertGreaterEqual(reqs[-1]["t"], before)


class MockDefaultModeTests(unittest.TestCase):
    """With no perf option set, the mock is what it always was: port 8943,
    single-threaded, unshaped. Skipped if something already holds 8943."""

    def test_default_mode_is_unchanged(self):
        import socket
        with socket.socket() as s:
            if s.connect_ex(("127.0.0.1", 8943)) == 0:
                self.skipTest("port 8943 already in use")
        proc = subprocess.Popen([sys.executable, str(harness.MOCK)], stdout=subprocess.DEVNULL,
                                stderr=subprocess.DEVNULL, env={"PYTHONDONTWRITEBYTECODE": "1",
                                                                 "PATH": "/usr/bin:/bin"})
        try:
            deadline = time.monotonic() + 20
            while True:
                try:
                    with urllib.request.urlopen("http://127.0.0.1:8943/debug/requests", timeout=2) as r:
                        json.loads(r.read())
                    break
                except OSError:
                    if time.monotonic() > deadline:
                        raise
                    time.sleep(0.1)
            t0 = time.monotonic()
            with urllib.request.urlopen(
                    "http://127.0.0.1:8943/w/rest.php/v1/page/Alan_Turing/html", timeout=5) as r:
                self.assertNotIn("Content-Encoding", dict(r.headers))
                r.read()
            self.assertLess(time.monotonic() - t0, 0.5)
            with self.assertRaises(OSError):
                urllib.request.urlopen("http://127.0.0.1:8943/w/rest.php/v1/page/Perf_Typical/html",
                                       timeout=5).read()
        finally:
            proc.terminate()
            proc.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
