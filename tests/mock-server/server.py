import http.server, urllib.parse, json, time, os, zlib, struct

# PRD FR-OFF-1/2 fixture revids: stable fake MediaWiki revision ids, one per
# PAGES key, exposed via the `ETag` header on `/page/{title}/html`
# (`W/"{revid}/mock-etag"`, the Parsoid ETag shape `api.rs::
# parse_revid_from_etag` parses) and via `/page/{title}/bare`'s
# `{"latest": {"id": revid}}` (Appendix A's cheap revalidation call).
REVIDS = {
    "Alan_Turing": 1001,
    "Enigma_machine": 1002,
    "Computer_science": 1003,
    "Terminal_Injection_Test": 1004,
    "アラン・チューリング": 1005,
    "計算機科学": 1006,
    "Rendering_Showcase": 1007,
    "Image_Showcase": 1008,
}

# PRD FR-RD-8 media fixture: build a real, tiny PNG at import time (stdlib
# zlib, no third-party deps) so the /media endpoint serves genuine image
# bytes the app decodes end to end. A 2x2 image with four distinct solid
# quadrants (TL red, TR green, BL blue, BR yellow) makes the half-block
# colour mapping verifiable: rendered to a 2-col x 1-row cell box, cell 0 is
# fg=red/bg=blue and cell 1 is fg=green/bg=yellow.
def _make_png(width, height, rows):
    def chunk(typ, data):
        body = typ + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xffffffff)
    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)  # 8-bit RGB
    raw = bytearray()
    for row in rows:
        raw.append(0)  # per-scanline filter type 0 (None)
        for (r, g, b) in row:
            raw += bytes((r, g, b))
    idat = zlib.compress(bytes(raw))
    return sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")


QUAD_PNG = _make_png(2, 2, [
    [(255, 0, 0), (0, 255, 0)],
    [(0, 0, 255), (255, 255, 0)],
])

# Counts /media requests so a pty test can assert a text theme fetches NO
# image bytes (FR-TH-7) while the full theme does. Single-threaded HTTPServer,
# so a plain global is race-free. Exposed at /debug/media-hits.
MEDIA_HITS = 0

# PRD §5.8 / NF-NET-2 verification: a log of every non-debug request's path
# and User-Agent header. A pty test asserts that (a) opening an article
# triggers the batched pageviews call + the ranked link-body prefetches, (b)
# a prefetch=off / --incognito run makes NO such background request, and (c)
# every request carries the mandated User-Agent. Single-threaded server, so a
# plain global list is race-free. Exposed at /debug/requests; /debug/reset
# clears it between phases.
REQUEST_LOG = []

# Simulates a wiki edit for exactly one fixture, for stale-while-revalidate
# pty verification: when set to a PAGES key, that title's revid is reported
# one higher than its REVIDS entry (on both the html ETag and the bare
# endpoint) and its HTML gets an extra trailing paragraph, so a test can
# start the mock once with this unset (seed a cache entry at the base
# revid), then restart it with this set to the same title and confirm the
# app's background revalidation notices the new revid and fetches visibly
# different content.
UPDATED_TITLE = os.environ.get("WIKITUI_MOCK_UPDATE_TITLE")


def current_revid(title):
    revid = REVIDS.get(title, 0)
    return revid + 1 if title == UPDATED_TITLE else revid


def current_html(title, html):
    if title == UPDATED_TITLE:
        return html.replace(
            "</body>", "<p>Updated content marker: this revision was bumped.</p></body>"
        )
    return html


PAGES = {
    "Alan_Turing": """<html><head><title>Alan Turing</title></head><body>
      <p>Alan Mathison Turing was an English mathematician, computer scientist, logician,
      cryptanalyst, philosopher and theoretical biologist. He was highly influential in the
      development of theoretical <a href="./Computer_science">computer science</a>, providing a
      formalisation of the concepts of algorithm and computation with the Turing machine, which
      can be considered a model of a general-purpose computer. Turing is widely considered to be
      the father of theoretical computer science and artificial intelligence.</p>
      <h2>Early life</h2>
      <p>Alan Turing was born in London<sup class="reference"><a href="#cite_note-1">[1]</a></sup>
      while his father was on leave from his position with the Indian Civil Service. Turing's
      father was the son of a clergyman, and Turing had an elder brother. His parents enrolled
      him at St Michael's, a primary school, at the age of six, and the headmistress recognised
      his talent early on.</p>
      <h2>Career and research</h2>
      <p>During the Second World War, Turing worked for the Government Code and Cypher School at
      Bletchley Park, Britain's codebreaking centre that produced Ultra intelligence. He led Hut 8,
      the section responsible for German naval cryptanalysis, and devised techniques for speeding
      the breaking of the <a href="./Enigma_machine">Enigma machine</a>, including improvements to
      the pre-war Polish bomba method, an electromechanical machine that could find settings for
      the Enigma machine. Turing played a crucial role in cracking intercepted messages that
      enabled the Allies to defeat the Axis powers in many crucial engagements.</p>
      <h2>Legacy</h2>
      <p>Turing left an extensive legacy in mathematics and computing which today is recognised
      more widely, with statues, prizes, and many things named after him, including the annual
      Turing Award, the highest distinction in computer science. His portrait appears on the
      Bank of England fifty pound note.</p>
      <div class="mw-references-wrap">
        <ol class="references">
          <li id="cite_note-1">
            <span class="mw-cite-backlink"><a href="#cite_ref-1">^</a></span>
            <span class="reference-text">Hodges, Andrew. <cite>Alan Turing: The Enigma</cite>. Princeton University Press, 2012.
              <a href="https://example.com/hodges-turing">https://example.com/hodges-turing</a></span>
          </li>
          <li id="cite_note-2">
            <span class="mw-cite-backlink"><a href="#cite_ref-2">^</a></span>
            <span class="reference-text">Copeland, B. Jack. "The Turing Test." Minds and Machines, 2000.</span>
          </li>
        </ol>
      </div>
    </body></html>""",

    "Enigma_machine": """<html><head><title>Enigma machine</title></head><body>
      <p>The Enigma machine is a cipher device developed and used in the early to mid-20th
      century to protect commercial, diplomatic, and military communication. It was employed
      extensively by Nazi Germany during World War II, in all branches of the German military.</p>
    </body></html>""",

    "Computer_science": """<html><head><title>Computer science</title></head><body>
      <p>Computer science is the study of computation, information, and automation. Computer
      science spans theoretical disciplines to applied disciplines.</p>
    </body></html>""",

    # PRD SEC-1 pty verification fixture: a hostile article exercising every
    # terminal-injection vector the sanitizer (src/sanitize.rs) must strip,
    # plus two categories of content it must NOT strip. Real bytes/code
    # points, not escaped text, so a pty session reading this article is a
    # genuine end-to-end check of doc::parse_article_html's sanitize pass:
    #   - \x1b]0;pwned\x07  -- an OSC window-title-set attempt
    #   - \x1b[31m ... \x1b[0m -- a CSI color-change escape
    #   - ‮ ... ‬ -- a right-to-left-override spoofing attempt
    #   - a ZWJ-joined family emoji, which MUST survive as one glyph
    #   - an IPA transcription built from combining marks, which MUST
    #     survive untouched (FR-RD-10)
    "Terminal_Injection_Test": """<html><head><title>Terminal Injection Test</title></head><body>
      <p>Before the attack. A window-title hijack attempt follows:
      \x1b]0;pwned\x07 -- did the terminal's title change? Next, a color-change
      escape sequence: \x1b[31mred text that must never reach the terminal live\x1b[0m --
      and now a right-to-left override spoofing attempt: ‮evil-looking-reversed-text‬ end.
      A family emoji that must survive intact: \U0001F468‍\U0001F469‍\U0001F467 (zero-width-joiner joined).
      An IPA word built from combining marks that must survive: t͡ʃɔːñ (a nasalized vowel).</p>
    </body></html>""",

}

# PRD FR-RD-4/FR-RD-5 / §6.3 pty-verification fixture: one article that
# exercises every branch of the table + infobox renderer and the size tiers.
#   - a proper person infobox (name/born/died/fields/known for) -> floats
#     right of the lead on wide terminals, top block on narrow ones
#   - a simple 3-column wikitable -> box-drawing grid
#   - a table with colspan + rowspan -> grid expansion, visible in the grid
#   - a very wide (12-column) table -> horizontal scroll when it can't fit,
#     collapse-to-list in accessible mode
PAGES["Rendering_Showcase"] = """<html><head><title>Rendering Showcase</title></head><body>
  <table class="infobox"><tbody>
    <tr><th colspan="2">Ada Lovelace</th></tr>
    <tr><th>Born</th><td>10 December 1815, London, England</td></tr>
    <tr><th>Died</th><td>27 November 1852 (aged 36)</td></tr>
    <tr><th>Fields</th><td>Mathematics, computing</td></tr>
    <tr><th>Known for</th><td>The first published algorithm intended for a machine</td></tr>
  </tbody></table>
  <p>This showcase article exercises the table and infobox renderer described in
  FR-RD-4 and FR-RD-5. On a wide terminal the infobox to the right is a floated
  card and this lead paragraph wraps in the column to its left; on a narrow
  terminal the card becomes a block above the text. The sections below drive the
  box-drawing grid, rowspan and colspan expansion, and horizontal scrolling.</p>
  <h2>Simple table</h2>
  <p>A plain three-column wikitable renders as a box-drawing grid with a header
  rule under the first row.</p>
  <table class="wikitable"><tbody>
    <tr><th>Year</th><th>Event</th><th>Place</th></tr>
    <tr><td>1815</td><td>Born in London</td><td>England</td></tr>
    <tr><td>1843</td><td>Published the translation and notes on the Analytical Engine</td><td>England</td></tr>
    <tr><td>1852</td><td>Died</td><td>England</td></tr>
  </tbody></table>
  <h2>Spanned table</h2>
  <p>This table mixes colspan and rowspan; the grid expansion fills spanned
  positions with blank cells so columns stay aligned.</p>
  <table class="wikitable"><tbody>
    <tr><th colspan="3">Analytical Engine notes</th></tr>
    <tr><th>Section</th><th>Topic</th><th>Length</th></tr>
    <tr><td rowspan="2">Note G</td><td>Bernoulli numbers</td><td>long</td></tr>
    <tr><td>The first algorithm</td><td>long</td></tr>
    <tr><td>Note A</td><td>General remarks</td><td>short</td></tr>
  </tbody></table>
  <h2>Wide table</h2>
  <p>A table with many columns cannot fit at once; use the bracket keys to
  scroll its column window horizontally, or read it as a list in accessible
  mode.</p>
  <table class="wikitable"><tbody>
    <tr><th>Col01</th><th>Col02</th><th>Col03</th><th>Col04</th><th>Col05</th><th>Col06</th><th>Col07</th><th>Col08</th><th>Col09</th><th>Col10</th><th>Col11</th><th>Col12</th></tr>
    <tr><td>alpha</td><td>bravo</td><td>charlie</td><td>delta</td><td>echo</td><td>foxtrot</td><td>golf</td><td>hotel</td><td>india</td><td>juliet</td><td>kilo</td><td>ZEBRA-END</td></tr>
    <tr><td>a1</td><td>b2</td><td>c3</td><td>d4</td><td>e5</td><td>f6</td><td>g7</td><td>h8</td><td>i9</td><td>j10</td><td>k11</td><td>z12</td></tr>
  </tbody></table>
</body></html>"""

# Japanese fixture: long CJK paragraphs that must wrap per-character with
# kinsoku, a heading structure, and internal links.

PAGES["アラン・チューリング"] = """<html><head><title>アラン・チューリング</title></head><body>
  <p>アラン・マティソン・チューリングは、イギリスの数学者、論理学者、暗号解読者、計算機科学者である。電子計算機の黎明期の研究に従事し、計算機械チューリングマシンとして計算を定式化して、その概念は計算機科学および数理論理学の発展に大きく寄与した。チューリングマシンは、コンピュータの理論的な基礎として、今日まで計算可能性の理論の中心にあり続けている。彼はしばしば<a href="./%E8%A8%88%E7%AE%97%E6%A9%9F%E7%A7%91%E5%AD%A6">計算機科学</a>および人工知能の父と呼ばれている。</p>
  <h2>生涯</h2>
  <p>チューリングは1912年6月23日、ロンドンのメイダ・ヴェールで生まれた。幼少期から数学と科学に非凡な才能を示し、ケンブリッジ大学キングス・カレッジで数学を学んだ。1936年に発表した論文「計算可能数について」において、後にチューリングマシンと呼ばれる抽象機械を導入し、決定問題に否定的な解答を与えた。この論文は、計算という概念そのものを数学的に定義した画期的な業績であり、現代のコンピュータの理論的基礎となっている。</p>
  <h2>第二次世界大戦</h2>
  <p>第二次世界大戦中、チューリングは政府暗号学校が置かれたブレッチリー・パークでドイツの暗号機<a href="./Enigma_machine">エニグマ</a>の解読に従事した。彼は海軍のエニグマ暗号を担当する第8棟の責任者となり、ポーランドの暗号学者が開発した手法を改良して、エニグマの設定を発見できる電気機械式の装置ボンブを設計した。この解読作業は連合国の勝利に大きく貢献し、戦争を二年以上短縮したと評価されている（諸説あり）。「暗号解読は、チェスのような知的な競技である。」と彼は述べたと伝えられる。</p>
  <h2>チューリングテスト</h2>
  <p>1950年の論文「計算する機械と知性」において、チューリングは機械が思考できるかという問いを検討し、後にチューリングテストと呼ばれる判定基準を提案した。これは、審査員が相手の姿を見ずに文字だけで対話し、人間と機械を区別できなければ、その機械は知的であるとみなすというものである。この提案は人工知能research分野の出発点のひとつとなった。</p>
  <h2>遺産</h2>
  <p>チューリングの業績は今日ますます広く認められており、計算機科学における最高の栄誉として毎年チューリング賞が授与されている。2021年からはイングランド銀行の50ポンド紙幣に肖像が採用された。</p>
</body></html>"""

# PRD FR-RD-8 pty-verification fixture: a figure with an http media src +
# figcaption, and a two-item gallery. The img src points at this same mock's
# /media endpoint so the app's lazy image fetch resolves against it.
PAGES["Image_Showcase"] = """<html><head><title>Image Showcase</title></head><body>
  <p>This article exercises inline image rendering described in FR-RD-8: a
  figure with a caption, and a small gallery below it.</p>
  <figure typeof="mw:File">
    <img src="http://127.0.0.1:8943/media/quadrants.png" alt="Four-colour test pattern"/>
    <figcaption>A four-colour test pattern with red, green, blue and yellow quadrants.</figcaption>
  </figure>
  <h2>Gallery</h2>
  <ul class="gallery mw-gallery-traditional">
    <li class="gallerybox">
      <div class="thumb"><img src="http://127.0.0.1:8943/media/quadrants.png" alt="thumb one"/></div>
      <div class="gallerytext">Gallery item one</div>
    </li>
    <li class="gallerybox">
      <div class="thumb"><img src="http://127.0.0.1:8943/media/quadrants.png" alt="thumb two"/></div>
      <div class="gallerytext">Gallery item two</div>
    </li>
  </ul>
</body></html>"""

PAGES["計算機科学"] = """<html><head><title>計算機科学</title></head><body>
  <p>計算機科学は、情報と計算の理論的基礎、およびそのコンピュータ上への実装と応用に関する研究分野である。</p>
</body></html>"""

# Typeahead fixtures (FR-SR-1): title + a Wikidata-style one-line
# description, matched by case-insensitive prefix. Every title here is also
# a real key in PAGES, so "Enter opens the suggestion" always resolves to a
# real article instead of a 404 — including the CJK one (FR-ML-6).
TITLE_SUGGESTIONS = [
    ("Alan Turing", "British mathematician, logician, cryptanalyst, and computer scientist (1912-1954)"),
    ("Enigma machine", "German electro-mechanical rotor cipher machine"),
    ("Computer science", "study of computation, automation, and information"),
    ("アラン・チューリング", "イギリスの数学者・計算機科学者・暗号解読者 (1912-1954)"),
    ("計算機科学", "計算と情報に関する学問分野"),
]

# Full-text search fixtures (FR-SR-2): title, body text to excerpt/match
# against, and the size/wordcount/last-edit-date fields the PRD's "size,
# last-edit date" line wants (§5.3 FR-SR-2). Kept as plain prose distinct
# from PAGES' HTML so excerpting doesn't have to strip markup.
SEARCH_PAGES = [
    {
        "title": "Alan Turing",
        "description": "British mathematician, logician, cryptanalyst, and computer scientist (1912-1954)",
        "text": (
            "Alan Mathison Turing was an English mathematician, computer scientist, logician, "
            "cryptanalyst, philosopher and theoretical biologist. He was highly influential in "
            "the development of theoretical computer science, providing a formalisation of the "
            "concepts of algorithm and computation with the Turing machine."
        ),
        "size": 4820,
        "wordcount": 812,
        "timestamp": "2026-06-30T10:15:00Z",
    },
    {
        "title": "Enigma machine",
        "description": "German electro-mechanical rotor cipher machine",
        "text": (
            "The Enigma machine is a cipher device developed and used in the early to "
            "mid-20th century to protect commercial, diplomatic, and military communication."
        ),
        "size": 1024,
        "wordcount": 180,
        "timestamp": "2026-05-14T08:00:00Z",
    },
    {
        "title": "Computer science",
        "description": "Study of computation, automation, and information",
        "text": (
            "Computer science is the study of computation, information, and automation. "
            "Computer science spans theoretical disciplines to applied disciplines."
        ),
        "size": 512,
        "wordcount": 90,
        "timestamp": "2026-04-01T00:00:00Z",
    },
]

# PRD Appendix A "Summary" fixtures (FR-OFF-4 T2 link-peek): a plain-text
# extract per title, served at /api/rest_v1/page/summary/{title}. A title with
# no explicit entry falls back to a synthesized one-liner so any internal link
# still resolves offline.
SUMMARIES = {
    "Computer_science": "Computer science is the study of computation, information, and automation.",
    "Enigma_machine": "The Enigma machine was a cipher device used in the early to mid-20th century.",
    "Alan_Turing": "Alan Turing was an English mathematician and computer scientist.",
}

# PRD FR-OFF-5 bulk-save-by-category fixtures: category name (without the
# Category: prefix, matched case-insensitively) -> member article titles
# (namespace 0). Served via the Action API list=categorymembers shape.
CATEGORIES = {
    "physics": ["Alan Turing", "Computer science", "Enigma machine"],
    "computing": ["Computer science", "Alan Turing"],
}

# PRD FR-PF-1 link-ranking fixture (§6.2 rule 7): the outgoing links of a
# source article joined with pageviews, served as the ONE batched
# generator=links + prop=pageviews response. Keyed by the source's display
# title (underscores normalised to spaces, as the request's titles= arrives).
# Titles here match what the client parses out of the fixture HTML's ./Links,
# so link ranking has real view counts to sort by. Enigma outranks Computer
# science on views, so the top-N order is verifiable.
LINK_PAGEVIEWS = {
    "Alan Turing": [
        {"title": "Enigma machine", "views": 12000},
        {"title": "Computer science", "views": 5000},
    ],
}

# PRD FR-PF-2 trending fixture: the Wikifeeds featured-content payload served
# at /api/rest_v1/feed/featured/{y}/{m}/{d}. TFA + most-read titles are all
# real PAGES keys so their prefetched bodies resolve, not 404. Also exercises
# the FR-DL-1 start page's extra sections: the TFA extract, "in the news"
# (with an <a>-tagged story the client must strip to plain text), a potd
# thumbnail pointing at this same mock's real-PNG /media endpoint (so the
# B6 half-block image pipeline decodes genuine bytes end to end), and
# onthisday entries carrying a year + linked page (the start page's condensed
# strip and the TIL widget both read this bundled list — FR-DL-2's `:today`
# panel instead uses the dedicated ONTHISDAY_BY_TYPE fixture below).
FEATURED_FEED = {
    "tfa": {
        "title": "Alan_Turing",
        "normalizedtitle": "Alan Turing",
        "extract": "Alan Turing was an English mathematician, computer scientist, and logician.",
    },
    "mostread": {
        "articles": [
            {"title": "Enigma_machine", "normalizedtitle": "Enigma machine", "views": 90000, "rank": 1},
            {"title": "Computer_science", "normalizedtitle": "Computer science", "views": 40000, "rank": 2},
        ]
    },
    "image": {
        "title": "File:Sample.jpg",
        "thumbnail": {"source": "http://127.0.0.1:8943/media/quadrants.png"},
    },
    "news": [
        {
            "story": 'Anniversary of the <a href="./Enigma_machine">Enigma machine</a>\'s break marked.',
            "links": [{"title": "Enigma_machine", "normalizedtitle": "Enigma machine"}],
        },
    ],
    "onthisday": [
        {
            "text": "Alan Turing was born.",
            "year": 1912,
            "pages": [{"title": "Alan_Turing", "normalizedtitle": "Alan Turing"}],
        },
        {"text": "A minor, unlinked anniversary.", "year": 1954},
    ],
}

# PRD FR-DL-2 fixture: the dedicated `feed/onthisday/{type}/{m}/{d}` payload
# `:today` fetches once per type — a single top-level key named after the
# type, per the real Wikifeeds shape (`prefetch::parse_onthisday`'s
# contract). Every linked page is a real PAGES key so Enter opens something.
ONTHISDAY_BY_TYPE = {
    "events": [
        {
            "text": "The Enigma machine entered military service.",
            "year": 1932,
            "pages": [{"title": "Enigma_machine", "normalizedtitle": "Enigma machine"}],
        },
    ],
    "births": [
        {
            "text": "Alan Turing was born.",
            "year": 1912,
            "pages": [{"title": "Alan_Turing", "normalizedtitle": "Alan Turing"}],
        },
    ],
    "deaths": [
        {"text": "A computer scientist died.", "year": 1980},
    ],
    "holidays": [
        {"text": "Computer Science Education Week begins."},
    ],
    "selected": [
        {
            "text": "Alan Turing published \"On Computable Numbers\".",
            "year": 1936,
            "pages": [{"title": "Alan_Turing", "normalizedtitle": "Alan Turing"}],
        },
        {
            "text": "Computer science was recognised as its own discipline.",
            "year": 1965,
            "pages": [{"title": "Computer_science", "normalizedtitle": "Computer science"}],
        },
    ],
}

# Did-you-mean corrections (FR-SR-4 / §7's zero-results row) for queries
# that hit no SEARCH_PAGES text at all. Keyed lowercase; see
# `api::SearchOutcome`'s doc comment for why this rides the REST
# `search/page` response as a schema extension rather than a second
# Action-API round trip.
DID_YOU_MEAN = {
    "alan truing": "Alan Turing",
    "enigma mahcine": "Enigma machine",
}


def make_excerpt(text, query):
    """Wraps the first case-insensitive occurrence of `query` in `text` with
    the same `<span class="searchmatch">` markup the real REST search/page
    endpoint emits, with a little surrounding context — exercising the
    client's searchmatch-span parser end to end."""
    idx = text.lower().find(query.lower())
    if idx == -1:
        return text[:120]
    start = max(0, idx - 40)
    end = min(len(text), idx + len(query) + 40)
    prefix = ("…" if start > 0 else "") + text[start:idx]
    matched = text[idx : idx + len(query)]
    suffix = text[idx + len(query) : end] + ("…" if end < len(text) else "")
    return f'{prefix}<span class="searchmatch">{matched}</span>{suffix}'


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        parts = parsed.path.split('/')
        params = urllib.parse.parse_qs(parsed.query)

        # PRD NF-NET-2 verification: record every real request's path + UA.
        if not parsed.path.startswith('/debug/'):
            REQUEST_LOG.append({
                "path": self.path,
                "ua": self.headers.get('User-Agent', ''),
            })

        if parsed.path == '/debug/media-hits':
            self._send_json({"hits": MEDIA_HITS})
        elif parsed.path == '/debug/requests':
            self._send_json({"requests": REQUEST_LOG})
        elif parsed.path == '/debug/reset':
            REQUEST_LOG.clear()
            self._send_json({"ok": True})
        elif '/page/summary/' in parsed.path:
            self._serve_summary(parts)
        elif '/feed/featured/' in parsed.path:
            self._serve_featured_feed()
        elif '/feed/onthisday/' in parsed.path:
            self._serve_onthisday(parts)
        elif '/page/' in parsed.path and parsed.path.endswith('/html'):
            self._serve_article(parts)
        elif '/page/' in parsed.path and parsed.path.endswith('/bare'):
            self._serve_bare(parts)
        elif parsed.path.endswith('/api.php'):
            self._serve_action_api(params)
        elif parsed.path.endswith('/search/title'):
            self._serve_search_title(params)
        elif parsed.path.endswith('/search/page'):
            self._serve_search_page(params)
        elif parsed.path.startswith('/media/'):
            self._serve_media()
        else:
            self.send_response(404)
            self.end_headers()

    def _serve_featured_feed(self):
        # PRD FR-PF-2 / FR-DL-1: the one daily Wikifeeds featured-content call.
        self._send_json(FEATURED_FEED)

    def _serve_onthisday(self, parts):
        # PRD FR-DL-2: `:today`'s dedicated per-type call
        # (`feed/onthisday/{type}/{m}/{d}`) — the response is a single
        # top-level key named after the requested type, per the real
        # Wikifeeds shape. An unknown/unfixtured type still returns 200 with
        # an empty list rather than 404, so a type this fixture doesn't
        # cover degrades to "no entries" instead of an error.
        event_type = parts[-3]
        entries = ONTHISDAY_BY_TYPE.get(event_type, [])
        self._send_json({event_type: entries})

    def _serve_summary(self, parts):
        # PRD Appendix A "Summary" (FR-OFF-4 T2): plain-text extract for a
        # link target. A title without an explicit SUMMARIES entry gets a
        # synthesized one-liner so link-peek still resolves offline.
        title = urllib.parse.unquote(parts[-1])
        extract = SUMMARIES.get(title, f"{title.replace('_', ' ')} is a topic on Wikipedia.")
        self._send_json({"title": title.replace('_', ' '), "extract": extract})

    def _serve_action_api(self, params):
        action = params.get('action', [''])[0]
        listing = params.get('list', [''])[0]
        generator = params.get('generator', [''])[0]
        prop = params.get('prop', [''])[0]
        # PRD FR-OFF-5 bulk-save-by-category: list=categorymembers, depth 1.
        if action == 'query' and listing == 'categorymembers':
            cmtitle = params.get('cmtitle', [''])[0]
            name = cmtitle.split(':', 1)[-1].strip().lower()
            members = CATEGORIES.get(name, [])
            self._send_json({
                "query": {"categorymembers": [{"title": t} for t in members]}
            })
            return
        # PRD FR-PF-1: the ONE batched generator=links + prop=pageviews call.
        # `pvipdays` day keys are synthesised so the client's per-title sum
        # reproduces the fixture view counts.
        if action == 'query' and generator == 'links' and 'pageviews' in prop:
            source = params.get('titles', [''])[0].replace('_', ' ')
            links = LINK_PAGEVIEWS.get(source, [])
            pages = [
                {"title": link["title"], "pageviews": {"2026-07-13": link["views"]}}
                for link in links
            ]
            self._send_json({"query": {"pages": pages}})
            return
        self.send_response(404)
        self.end_headers()

    def _serve_media(self):
        # PRD FR-RD-8: serve the real tiny PNG, counting the hit so tests can
        # assert whether an image request was made at all (text-theme gating).
        global MEDIA_HITS
        MEDIA_HITS += 1
        self.send_response(200)
        self.send_header('Content-Type', 'image/png')
        self.send_header('Content-Length', str(len(QUAD_PNG)))
        self.end_headers()
        self.wfile.write(QUAD_PNG)

    def _serve_article(self, parts):
        title = urllib.parse.unquote(parts[-2])
        html = PAGES.get(title)
        if html is None:
            self.send_response(404)
            self.end_headers()
            return
        body = current_html(title, html).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(body)))
        # PRD FR-OFF-1/2: the Parsoid-shaped ETag api.rs's
        # parse_revid_from_etag parses the revid out of.
        self.send_header('ETag', f'W/"{current_revid(title)}/mock-etag"')
        self.end_headers()
        self.wfile.write(body)

    def _serve_bare(self, parts):
        # PRD Appendix A's cheap "Page metadata / latest revid" call
        # (`GET /w/rest.php/v1/page/{title}/bare`): just the current revid,
        # none of the article body. A small artificial delay (mirroring
        # `_serve_search_title`'s debounce-visibility sleep) so manual/pty
        # verification of stale-while-revalidate can actually observe the
        # cached copy rendering before the background revalidation's
        # "updated — r to reload" notice lands, instead of both happening
        # within the same terminal frame on loopback-fast localhost.
        time.sleep(0.4)
        title = urllib.parse.unquote(parts[-2])
        if title not in PAGES:
            self.send_response(404)
            self.end_headers()
            return
        self._send_json({"latest": {"id": current_revid(title)}})

    def _serve_search_title(self, params):
        # Artificial latency (PRD FR-SR-1 / §6.8: "debounce 150-250ms") so
        # manual pty verification can actually see the typeahead dropdown
        # arrive after the debounce, instead of it resolving instantly.
        time.sleep(0.1)
        q = (params.get('q', [''])[0]).lower()
        limit = int(params.get('limit', ['10'])[0])
        hits = [(t, d) for t, d in TITLE_SUGGESTIONS if t.lower().startswith(q)]
        pages = [{"title": t, "description": d} for t, d in hits[:limit]]
        self._send_json({"pages": pages})

    def _serve_search_page(self, params):
        q = params.get('q', [''])[0]
        limit = int(params.get('limit', ['20'])[0])
        ql = q.lower().strip()
        hits = [
            p for p in SEARCH_PAGES
            if ql and (ql in p["title"].lower() or ql in p["text"].lower())
        ]
        hits = hits[:limit]
        pages = [
            {
                "title": p["title"],
                "description": p["description"],
                "excerpt": make_excerpt(p["text"], q),
                "size": p["size"],
                "wordcount": p["wordcount"],
                "timestamp": p["timestamp"],
            }
            for p in hits
        ]
        body = {"pages": pages}
        if not pages and ql in DID_YOU_MEAN:
            body["suggestion"] = DID_YOU_MEAN[ql]
        self._send_json(body)

    def _send_json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass

http.server.HTTPServer(('127.0.0.1', 8943), Handler).serve_forever()
