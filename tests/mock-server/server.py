import http.server, urllib.parse, json, time, os, zlib, struct, hashlib, base64

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
    "Math_Showcase": 1009,
    "Redlink_Showcase": 1010,
    "Hyperlink_Scheme_Test": 1011,
    # PRD FR-ACC-5 talk-page fixture: an ordinary page like any other, just
    # under the "Talk:" namespace prefix `src/talk.rs::to_talk` derives.
    "Talk:Alan_Turing": 1012,
    # PRD FR-DL-4 pty-verification fixture: two `{{citation needed}}`-family
    # markers.
    "Citation_Needed_Showcase": 1013,
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


# PRD FR-ACC-8 (typo-fix editing): the SOURCE wikitext of a page, served by
# `action=query&prop=revisions&rvprop=content|ids|timestamp` — deliberately
# distinct from the rendered PAGES HTML (an edit fixes the source, not the
# render). The lead sentence of Alan_Turing mirrors its PAGES HTML lead (with a
# `[[Computer science|computer science]]` link where the HTML has the anchor),
# so the client's rendered-sentence → wikitext-span fuzzy locate resolves end
# to end. A Talk: fixture exists so the client's main-namespace-only block is
# exercised against a real non-main page (the block is client-side, before any
# fetch — this is here for completeness).
WIKITEXT = {
    "Alan_Turing": (
        "Alan Mathison Turing was an English mathematician, computer scientist, logician, "
        "cryptanalyst, philosopher and theoretical biologist. He was highly influential in the "
        "development of theoretical [[Computer science|computer science]], providing a "
        "formalisation of the concepts of algorithm and computation with the Turing machine, "
        "which can be considered a model of a general-purpose computer. Turing is widely "
        "considered to be the father of theoretical computer science and artificial "
        "intelligence.\n\n"
        "== Early life ==\n"
        "Alan Turing was born in London while his father was on leave from his position with "
        "the Indian Civil Service."
    ),
    "Talk:Alan_Turing": (
        "== Merge proposal ==\n"
        "Should the history section here be merged into [[Computer science]] instead? ~~~~"
    ),
}

# Pristine copy of the wikitext fixtures, so /debug/reset can restore them
# after an edit-flow phase mutated WIKITEXT in place.
WIKITEXT_SEED = {k: v for k, v in WIKITEXT.items()}

# PRD FR-ACC-8 conflict detection: the CURRENT editable revid of each wikitext
# page (mutable — a successful edit bumps it, and `/debug/bump-revid` bumps it
# out of band to simulate another editor changing the page between the client's
# fetch and its save, so a stale `baserevid` produces a real `editconflict`).
WIKITEXT_REVID = {title: REVIDS.get(title, 9000 + i) for i, title in enumerate(WIKITEXT)}
WIKITEXT_TIMESTAMP = "2026-07-15T08:00:00Z"

# PRD FR-ACC-8: every `action=edit` this mock accepts, recorded for pty
# verification (/debug/edits) — the exact title, spliced text, summary, minor
# flag, baserevid, basetimestamp, and token the client sent, so a test can
# confirm the save request shape AND that the wikitext splice byte-preserved
# everything outside the edited sentence (the mock echoes the received text).
RECORDED_EDITS = []


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
      <h2>References</h2>
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

    # The link to Computer_science (PRD FR-HS-3 pty verification fixture)
    # gives the mock a genuine two-hop chain — Alan_Turing -> Enigma_machine
    # -> Computer_science — so a `:trail` session can be built by actually
    # following links rather than needing a third, artificial fixture.
    "Enigma_machine": """<html><head><title>Enigma machine</title></head><body>
      <p>The Enigma machine is a cipher device developed and used in the early to mid-20th
      century to protect commercial, diplomatic, and military communication. It was employed
      extensively by Nazi Germany during World War II, in all branches of the German military.
      Breaking it was an early triumph of what became <a href="./Computer_science">computer
      science</a>.</p>
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

    # PRD FR-RD-2 / SEC-2 pty verification fixture: a mix of link schemes so a
    # pty session reading this article is a genuine end-to-end check of the
    # OSC 8 hyperlink emission gate (src/hyperlink.rs::sanitize_uri) — an
    # internal link (canonical https URL), an already-https external link,
    # and two hostile-scheme hrefs the HTML parser stores verbatim
    # (doc.rs::collect_inline does no scheme filtering of its own) that must
    # still never reach the terminal as an OSC 8 escape.
    "Hyperlink_Scheme_Test": """<html><head><title>Hyperlink Scheme Test</title></head><body>
      <p>An internal link to <a href="./Computer_science">computer science</a>, an external
      <a href="https://example.com/safe">safe https link</a>, a
      <a href="javascript:alert(1)">javascript link</a>, and a
      <a href="data:text/html,evil">data link</a> end this paragraph.</p>
    </body></html>""",

    # PRD FR-ACC-5 talk-page fixture: an ordinary page under the "Talk:"
    # namespace prefix, with some discussion content — proves the `T` /
    # `:talk` toggle fetches and renders it exactly like any other article
    # (headings, a reply-style thread, an internal link back to the
    # article it discusses).
    "Talk:Alan_Turing": """<html><head><title>Talk:Alan Turing</title></head><body>
      <h2>Merge proposal: Alan Turing and Turing machine</h2>
      <p>Should the history section here be merged into
      <a href="./Computer_science">Computer science</a> instead of duplicated? Raising it
      here before making any changes. ~~~~</p>
      <p>Oppose — this article's history section is specific to Turing's own biography,
      not the general computer-science topic. Keep as is. ~~~~</p>
      <h2>Birth date discrepancy</h2>
      <p>A couple of older sources give a different birth date than the one currently
      cited. Can someone with access check the original registry entry? ~~~~</p>
      <p>Checked — the currently cited date matches the birth certificate reproduced in
      the Hodges biography. Marking resolved. ~~~~</p>
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

# PRD FR-RD-7 pty-verification fixture: inline math (with alttext, a
# superscript, and a Greek-letter macro) plus a display equation reached via
# Parsoid's `<dl><dd>` leading-colon shape (`display="block"` on `<math>`, no
# `alttext` — exercising the annotation fallback tier, distinct from the
# inline node's alttext-wins path). Real Parsoid math markup is far more
# verbose (MathML presentation tree, an accessible fallback `<img>`); this
# mock keeps only what `doc::extract_math` actually reads (`alttext`, the
# `annotation` child, `display`) since a hand-rolled fixture that faked the
# rest would just be untested filler.
PAGES["Math_Showcase"] = """<html><head><title>Math Showcase</title></head><body>
  <p>This article exercises math passthrough (FR-RD-7). Mass-energy
  equivalence is written <span typeof="mw:Extension/math">
    <math alttext="E=mc^2"><semantics><mrow></mrow>
    <annotation encoding="application/x-tex">E=mc^2</annotation></semantics></math>
  </span>, and the sum of two angles is <span typeof="mw:Extension/math">
    <math alttext="\\alpha + \\beta = \\gamma"><semantics><mrow></mrow>
    <annotation encoding="application/x-tex">\\alpha + \\beta = \\gamma</annotation></semantics></math>
  </span>.</p>
  <h2>Newton's second law</h2>
  <p>The net force on a body is shown below.</p>
  <dl><dd><span typeof="mw:Extension/math">
    <math display="block"><semantics><mrow></mrow>
    <annotation encoding="application/x-tex">F_{net} = m a</annotation></semantics></math>
  </span></dd></dl>
  <h2>Series and roots</h2>
  <p>The following uses macros the trivial normalizer leaves raw but the
  optional <code>math-layout</code> feature (FR-RD-7 v1.x) renders as Unicode:
  a big operator, a fraction, and a root. The square root of two is
  <span typeof="mw:Extension/math">
    <math alttext="\\sqrt{2}"><semantics><mrow></mrow>
    <annotation encoding="application/x-tex">\\sqrt{2}</annotation></semantics></math>
  </span>.</p>
  <dl><dd><span typeof="mw:Extension/math">
    <math display="block"><semantics><mrow></mrow>
    <annotation encoding="application/x-tex">\\sum_{k=1}^{n} k = \\frac{n(n+1)}{2}</annotation></semantics></math>
  </span></dd></dl>
</body></html>"""

# PRD FR-DL-5 pty-verification fixture: one link Parsoid itself pre-marks as
# a redlink (`class="new"`, the cheap parse-time path — never reaches the
# network), one link that looks ordinary in the HTML but isn't a real PAGES
# title (caught only by the batched `generator=links&prop=info` check, see
# PAGE_LINKS below), and one real link that must render and follow normally.
PAGES["Redlink_Showcase"] = """<html><head><title>Redlink Showcase</title></head><body>
  <p>This article exercises redlink detection (FR-DL-5). It links to a
  <a href="./Nonexistent_Concept_X" class="new" title="Nonexistent Concept X (page does not exist)">nonexistent concept</a>
  that Parsoid itself marks as a redlink, and separately to
  <a href="./Uncharted_Topic_Y">an uncharted topic</a> that isn't pre-marked
  but doesn't exist either — the batched link/info check is what catches
  that one. It also links to a real article, <a href="./Computer_science">computer science</a>,
  which must render and follow as an ordinary link.</p>
</body></html>"""

# PRD FR-DL-4 pty-verification fixture: two `{{citation needed}}`-family
# transclusions in real Parsoid shape — `typeof="mw:Transclusion"` plus a
# `data-mw` naming the template call (`doc::is_citation_needed_transclusion`
# reads `data-mw.parts[].template.target.wt`), and the `noprint` class real
# Wikipedia output carries (the class `doc.rs`'s `is_noise` would otherwise
# drop the node for, if the citation-needed check didn't run ahead of it).
# One uses the canonical "Citation needed" name, the other the "Fact" alias
# — both are in `doc::CITATION_NEEDED_TEMPLATES`. A third, unrelated
# transclusion (an infobox-shaped one) proves an unlisted template name
# is left alone rather than also being treated as a citation-needed marker.
PAGES["Citation_Needed_Showcase"] = """<html><head><title>Citation Needed Showcase</title></head><body>
  <p>This article exercises citation-needed highlighting (FR-DL-4). The
  first claim needs a source<sup class="noprint Inline-Template" typeof="mw:Transclusion" data-mw='{&quot;parts&quot;:[{&quot;template&quot;:{&quot;target&quot;:{&quot;wt&quot;:&quot;Citation needed&quot;,&quot;href&quot;:&quot;./Template:Citation_needed&quot;},&quot;params&quot;:{&quot;date&quot;:{&quot;wt&quot;:&quot;July 2026&quot;}}}}]}'>[<i><a href="./Wikipedia:Citation_needed">citation needed</a></i>]</sup>.
  A second, separate claim also needs one<sup class="noprint Inline-Template" typeof="mw:Transclusion" data-mw='{&quot;parts&quot;:[{&quot;template&quot;:{&quot;target&quot;:{&quot;wt&quot;:&quot;Fact&quot;}}}]}'>[<i><a href="./Wikipedia:Citation_needed">citation needed</a></i>]</sup>.
  An unrelated transclusion<sup typeof="mw:Transclusion" data-mw='{&quot;parts&quot;:[{&quot;template&quot;:{&quot;target&quot;:{&quot;wt&quot;:&quot;Infobox&quot;}}}]}'>(infobox marker)</sup> must not be treated as citation-needed.</p>
</body></html>"""

# PRD FR-ML-7 pty-verification fixture (experimental RTL): a short article in
# genuine Arabic (`bidi::direction("ar")` resolves RTL — the mock has no
# per-language routing, so any lang argument, including `--lang ar`, reaches
# this same flat PAGES dict; the *language argument itself* is what the app
# tags the tab with and what FR-ML-7's detection reads). One paragraph
# embeds a Latin run ("Turing Award") mid-sentence — the "LTR run embedded in
# RTL" case `bidi::reorder_line_for_display` is asked to handle without
# mangling it — and one heading, plus a real internal link (Arabic anchor
# text pointing at the existing `Computer_science` fixture) so link
# navigation/following on an RTL article is exercised too, not just prose.
PAGES["RTL_Showcase"] = """<html><head><title>RTL Showcase</title></head><body>
  <p>تجرِّب هذه المقالة دعم الكتابة من اليمين إلى اليسار (FR-ML-7، تجريبي).
  ولد آلان تورينج عالم الرياضيات البريطاني، وهو يُعرف بإسهاماته في تأسيس
  علم الحاسوب النظري، ويُطلق على الجائزة السنوية المرموقة في هذا المجال اسم
  Turing Award تكريمًا له. تناقش المقالة أيضًا آلة إنجما التي فك تورينج
  شفرتها أثناء الحرب.</p>
  <h2>علم الحاسوب</h2>
  <p>يُعدّ <a href="./Computer_science">علم الحاسوب</a> من أهم المجالات
  العلمية الحديثة، وقد ساهم فيه تورينج إسهامًا كبيرًا.</p>
</body></html>"""

# PRD FR-ML-4 pty-verification fixture: a Wiktionary-shaped dictionary entry
# (etymology, part-of-speech headers, a numbered sense list, a "Related
# terms" section) — proves sister-project articles render "as-is" through
# the same doc.rs pipeline rather than needing per-project special-casing.
# Reachable at the flat PAGES namespace regardless of which `base_url` path
# prefix a `[wiki.<name>]` section points at (this mock has one flat article
# namespace behind any prefix — see `_lang_prefix`'s own comment), so a
# `[wiki.wiktionary] base_url = ".../wiktionary"` config resolves it exactly
# like any other title.
PAGES["computer_(word)"] = """<html><head><title>computer (word)</title></head><body>
  <h2>English</h2>
  <p><i>Etymology:</i> From <a href="./compute">compute</a> +‎ <a href="./-er">-er</a>.</p>
  <h3>Noun</h3>
  <p><b>computer</b> (plural <b>computers</b>)</p>
  <ol>
    <li>A person or thing that computes or calculates.</li>
    <li>An electronic device for storing and processing data, typically in
      binary form, according to instructions given to it in a variable
      program.</li>
  </ol>
  <h3>Related terms</h3>
  <ul>
    <li><a href="./Computer_science">computer science</a></li>
    <li><a href="./computing">computing</a></li>
  </ul>
</body></html>"""

# PRD FR-ML-1/2 (Appendix A "Langlinks") fixture: `action=query&
# prop=langlinks&llprop=autonym|langname|url`, keyed by the display title
# (spaces, matching how `titles=` arrives after this file's usual `_`->` `
# normalisation). Each entry's own `title` is independently fetchable: ja's
# is a real, distinct PAGES key (the existing アラン・チューリング CJK
# fixture); de/fr keep the same spelling as the English title, matching how
# the real German and French Wikipedias title this article too — fetching
# them resolves to the en PAGES entry, same as an untranslated interwiki
# link really would.
LANGLINKS = {
    "Alan Turing": [
        {
            "lang": "de",
            "autonym": "Deutsch",
            "langname": "German",
            "title": "Alan Turing",
            "url": "https://de.wikipedia.org/wiki/Alan_Turing",
        },
        {
            "lang": "ja",
            "autonym": "日本語",
            "langname": "Japanese",
            "title": "アラン・チューリング",
            "url": "https://ja.wikipedia.org/wiki/%E3%82%A2%E3%83%A9%E3%83%B3%E3%83%BB%E3%83%81%E3%83%A5%E3%83%BC%E3%83%AA%E3%83%B3%E3%82%B0",
        },
        {
            "lang": "fr",
            "autonym": "Français",
            "langname": "French",
            "title": "Alan Turing",
            "url": "https://fr.wikipedia.org/wiki/Alan_Turing",
        },
    ],
}

# PRD FR-ML-2 fallback-chain fixture: pointing WIKITUI_BASE_URL at this mock
# with a literal `{lang}` path segment (e.g.
# "http://127.0.0.1:8943/{lang}") makes `_lang_prefix` below see which
# language a request came in on; titles listed here are simulated as
# *absent* on that one language edition even though the flat PAGES dict has
# them — "xx" never has Alan_Turing, so a `languages = ["xx", "en"]`
# fallback chain must fall through to "en" (api::WikiClient::
# fetch_article_html / fetch_bare_metadata both 404). A request with no
# lang segment (every pre-existing fixture path, and every other language)
# is completely unaffected.
LANG_MISSING = {
    "xx": {"Alan_Turing"},
}

# Path roots this server actually routes on (see do_GET) — never a language
# code, so a request's first segment is only ever treated as a `{lang}`
# prefix when it is none of these.
_NON_LANG_PATH_ROOTS = {"w", "api", "media", "debug"}

# ---------------------------------------------------------------------------
# PRD §5.9 / FR-ACC-1: a minimal OAuth 2.0 authorization-code + PKCE provider,
# standing in for meta.wikimedia.org's `/w/rest.php/oauth2/{authorize,
# access_token}`. Point wikitui's `[auth] authorize_url`/`token_url` at this
# server (they default to Meta-Wiki, which the test/CI network can't reach).
#
# Documented simplifications (this is a test fixture, not a real IdP):
#  - There is no consent screen: `/oauth2/authorize` immediately issues a code
#    and 302-redirects to the client's `redirect_uri` (the loopback listener).
#    That is what makes the loopback flow drivable by a headless "browser"
#    (a curl that follows the redirect).
#  - PKCE *is* genuinely enforced at the token endpoint: the `S256` challenge
#    captured at `/authorize` is checked against `sha256(code_verifier)` at
#    `/access_token`, so the test actually exercises the PKCE relationship —
#    a wrong/absent verifier is rejected with `invalid_grant`.
#  - Refresh (`grant_type=refresh_token`) accepts any non-empty refresh token
#    (so a test can pre-seed an `auth.json` with a known refresh token and
#    force a refresh) and mints a fresh access token with a configurable TTL.
#  - One fixed account, MOCK_USERNAME. `meta=userinfo` returns it for any
#    access token this provider issued, and an anonymous session otherwise.
MOCK_USERNAME = "MockWikipedian"
# code -> {"challenge": str, "method": str}
OAUTH_CODES = {}
# access_token -> username
OAUTH_ACCESS = {}
_OAUTH_SEQ = [0]
# Per-grant hit counters + a note of the last access token minted, so a pty
# test can assert (via /debug/oauth) that a refresh actually happened.
OAUTH_STATS = {"authorize": 0, "exchange": 0, "refresh": 0, "userinfo": 0, "last_access": None}
# `expires_in` (seconds) the token endpoint reports. Overridable per process
# via WIKITUI_MOCK_OAUTH_TTL so a test can mint a deliberately short-lived
# access token and observe the client's transparent refresh.
OAUTH_TTL = int(os.environ.get("WIKITUI_MOCK_OAUTH_TTL", "14400"))


def _b64url_nopad(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def _pkce_ok(verifier, challenge, method):
    # Only S256 is issued by wikitui; a `plain` method (not used here) would
    # compare the verifier directly.
    if method and method.lower() == "plain":
        return verifier == challenge
    expected = _b64url_nopad(hashlib.sha256(verifier.encode()).digest())
    return expected == challenge


def _mint_access():
    _OAUTH_SEQ[0] += 1
    token = f"mock-access-{_OAUTH_SEQ[0]}"
    OAUTH_ACCESS[token] = MOCK_USERNAME
    OAUTH_STATS["last_access"] = token
    return token


def _lang_prefix(parts):
    if len(parts) > 1 and parts[1] and parts[1] not in _NON_LANG_PATH_ROOTS:
        return parts[1]
    return None

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
# PRD FR-NV-5 link preview also reads the Wikidata one-line `description` and
# a `thumbnail` URL from this same endpoint. An entry may be either a bare
# extract string (back-compat, no description/thumbnail) or a dict with
# `extract`/`description`/`thumbnail` keys.
SUMMARIES = {
    "Computer_science": {
        "extract": "Computer science is the study of computation, information, and automation.",
        "description": "study of computation",
    },
    "Enigma_machine": {
        "extract": "The Enigma machine was a cipher device used in the early to mid-20th century.",
        "description": "German cipher machine",
        "thumbnail": "/media/potd.png",
    },
    "Alan_Turing": {
        "extract": "Alan Turing was an English mathematician and computer scientist.",
        "description": "English computer scientist (1912-1954)",
    },
}

# PRD FR-OFF-5 bulk-save-by-category fixtures: category name (without the
# Category: prefix, matched case-insensitively) -> member article titles
# (namespace 0). Served via the Action API list=categorymembers shape.
CATEGORIES = {
    "physics": ["Alan Turing", "Computer science", "Enigma machine"],
    "computing": ["Computer science", "Alan Turing"],
}

# PRD FR-PF-3 interest-model fixture: per-article categories as
# `prop=categories` returns them (Category:-prefixed display titles), keyed by
# the article's display title. Deliberately mixes real topic categories with
# maintenance/tracking ones (dead-link, CS1 citation maintenance, webarchive)
# so the client's `interest::is_maintenance_category` filter is exercised end
# to end: the mock returns them ALL (most maintenance categories are not
# flagged `hidden`, so the request's `clshow=!hidden` would not drop them — the
# client's name-pattern filter is what must). The shared "Cryptography" topic
# on both Alan Turing and Enigma machine lets a pty run watch affinity
# accumulate across a short reading session, and drives the `morelike:` seed.
ARTICLE_CATEGORIES = {
    "Alan Turing": [
        "Category:Cryptography",
        "Category:British computer scientists",
        "Category:Articles with dead external links",
        "Category:CS1 maint: multiple names: authors list",
    ],
    "Enigma machine": [
        "Category:Cryptography",
        "Category:Encryption devices",
        "Category:Webarchive template wayback links",
    ],
    "Computer science": [
        "Category:Computer science",
        "Category:Cryptography",
    ],
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

# PRD FR-DL-5 fixture: `generator=links&prop=info`'s batched missing-flag
# check. Keyed by the source article's display title, same shape as
# LINK_PAGEVIEWS above — a source's entry lists exactly the titles a real
# `generator=links` call would generate from its wikitext. Whether one comes
# back `missing` is decided generically in `_serve_missing_links` (any title
# that isn't a real PAGES key), so this dict only needs to say which titles
# the source links to, not which ones are missing.
PAGE_LINKS = {
    "Redlink Showcase": [
        "Nonexistent Concept X",
        "Uncharted Topic Y",
        "Computer science",
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

# PRD FR-SR-5 / Appendix A "Random": `list=random` rotates through this fixed
# list (real PAGES titles, so `gr`/`:random` always opens something real)
# instead of using actual randomness — deterministic-testable, per a pty/CI
# run that wants to assert *something specific* opened. RANDOM_INDEX is a
# single-element list (not a bare int) so `_serve_random` can mutate it
# through a module-level `global`-free closure-friendly reference.
RANDOM_TITLES = [
    "Alan_Turing",
    "Enigma_machine",
    "Computer_science",
    "Rendering_Showcase",
    "Image_Showcase",
    "Terminal_Injection_Test",
]
RANDOM_INDEX = [0]

# PRD FR-DL-3 / FR-SR-5 "Quality" fixture: `prop=pageassessments` classes,
# keyed by display title (spaces, matching `list=random`'s own title form).
# Two titles reach GA/FA — since RANDOM_TITLES cycles with period 6, any
# batch of 10 consecutive draws (`:random good`'s GOOD_BATCH) necessarily
# includes at least one of them, so the pty "no good article found" fallback
# path is reachable only by deliberately shrinking this dict, not by chance.
ASSESSMENTS = {
    "Alan Turing": "FA",
    "Enigma machine": "GA",
}

# PRD FR-DL-3 v2 (§6.2 rule 1's stated Lift Wing exception): the gateway
# `articlequality` predict endpoint, keyed by revid — `api::WikiClient::
# fetch_liftwing_quality`'s own doc comment covers why revid, not title (the
# model scores one specific revision's text). A revid absent from this dict
# comes back with an empty `scores` map, matching a real "no score for this
# revision" response — never invented. Counts how many times the endpoint
# was actually hit (`/debug/liftwing-hits`) so a pty test can verify the
# "pageassessments present -> Lift Wing never called" fallback decision on
# the wire, not just by the badge that (or doesn't) show up.
LIFTWING_SCORES = {
    1001: "GA",  # Alan_Turing — used by the "wiki without pageassessments" pty fixture
}
LIFTWING_HITS = 0

# PRD FR-SR-6 fixture: `morelike:{title}` results, keyed by the display
# title the Related panel searches for. Shaped like `SEARCH_PAGES` entries
# (minus size/wordcount/timestamp, which `morelike:` results don't carry any
# more reliably than a real deployment's do) so `_serve_search_page` can
# return them through the same response shape full-text search uses.
RELATED_ARTICLES = {
    "Alan Turing": [
        {"title": "Enigma machine", "description": "German electro-mechanical rotor cipher machine"},
        {"title": "Computer science", "description": "study of computation, automation, and information"},
    ],
}

# ---------------------------------------------------------------------------
# PRD FR-ACC-2/3/4/6/7 (the account-features chunk that builds on the FR-ACC-1
# OAuth substrate above): watchlist, notifications (Echo), contributions,
# thanks, and read-only prefs. Every write here (`action=watch`, `thank`,
# `echomarkread`) requires the same Bearer token this file's OAuth provider
# issued (`OAUTH_ACCESS`) AND a matching CSRF/watch token minted below — see
# TOKEN_EPOCH's own comment for how a test forces a real `badtoken` error to
# exercise the client's refetch-and-retry-once behavior.

# `list=watchlistraw`/`list=watchlist` (FR-ACC-2) both read this mutable set;
# `action=watch` adds/removes from it, so a picker reopened after `w` reflects
# the toggle without any extra plumbing. Seeded with two real PAGES titles.
WATCHED_TITLES_SEED = {"Alan Turing", "Enigma machine"}
WATCHED_TITLES = set(WATCHED_TITLES_SEED)

# `list=watchlist`'s recent-changes fixture (the "what changed" activity
# feed): fixed timestamps spanning both sides of a plausible "last seen"
# cutoff, so `account::changes_since`'s pure since-last-seen filter has real
# data to split on. Only entries whose title is currently in WATCHED_TITLES
# are served, matching a real watchlist (unwatching a page drops it from the
# feed too).
WATCHLIST_CHANGES = [
    {
        "title": "Alan Turing", "user": "Historian1",
        "timestamp": "2026-07-10T09:00:00Z", "comment": "copyedit lead section",
        "revid": 5101, "old_revid": 5100,
    },
    {
        "title": "Enigma machine", "user": "CryptoFan",
        "timestamp": "2026-07-14T15:30:00Z", "comment": "add postwar section",
        "revid": 5202, "old_revid": 5201,
    },
    {
        "title": "Alan Turing", "user": "Historian1",
        "timestamp": "2026-07-15T08:00:00Z", "comment": "fix citation",
        "revid": 5103, "old_revid": 5101,
    },
]

# Echo notifications fixture (FR-ACC-3): mutable `read` flags so
# `action=echomarkread` visibly changes both `notprop=list` and the
# `notprop=count` badge on the next fetch. Restored by /debug/reset.
NOTIFICATIONS_SEED = [
    {
        "id": "101", "type": "alert",
        "text": "Historian1 thanked you for your edit on Alan Turing",
        "read": False, "timestamp": "2026-07-14T09:00:00Z",
    },
    {
        "id": "102", "type": "alert",
        "text": "Your edit on Enigma machine was reverted",
        "read": True, "timestamp": "2026-07-10T09:00:00Z",
    },
    {
        "id": "201", "type": "message",
        "text": "You have a new message on your talk page",
        "read": False, "timestamp": "2026-07-13T09:00:00Z",
    },
]
NOTIFICATIONS = [dict(n) for n in NOTIFICATIONS_SEED]

# `list=usercontribs` fixture (FR-ACC-4), keyed by username — public, no auth
# required, matching the real endpoint. MOCK_USERNAME's own edits (so
# `:contribs` while logged in resolves) plus a second editor's (so
# `:contribs OtherEditor` — the "works for any username" case — resolves too).
USER_CONTRIBS = {
    "MockWikipedian": [
        {
            "title": "Alan Turing", "timestamp": "2026-07-15T08:00:00Z",
            "comment": "fix citation", "revid": 5103, "sizediff": 12,
        },
        {
            "title": "Computer science", "timestamp": "2026-07-12T11:00:00Z",
            "comment": "expand history section", "revid": 4801, "sizediff": 340,
        },
        {
            "title": "Enigma machine", "timestamp": "2026-07-01T10:00:00Z",
            "comment": "typo", "revid": 4500, "sizediff": -4,
        },
    ],
    "OtherEditor": [
        {
            "title": "Computer science", "timestamp": "2026-07-11T09:00:00Z",
            "comment": "add reference", "revid": 4790, "sizediff": 88,
        },
    ],
}

# Read-only prefs fixture (FR-ACC-7): `meta=userinfo&uiprop=options`. Only
# ever read, never written — this build never calls `action=options`.
USER_PREFS = {
    "skin": "vector-2022",
    "language": "en",
    "editcount": 1234,
    "emailauthenticated": "2020-05-01T00:00:00Z",
}

# PRD FR-BM-5's Extension:ReadingLists mock (`action=readinglists`). SP-7
# flags the real extension's wire shape for third-party OAuth consumers as
# unverified; this fixture is `src/account.rs`'s own documented best-effort
# reading, not a verified live behavior — every response rides one top-level
# "readinglists" key, keyed further by the command (see that module's doc
# comment for the full list of assumptions this mirrors).
READINGLISTS_DEFAULT_LIST_ID = 100
READINGLISTS_SETUP_DONE = [False]
READINGLISTS_NEXT_ENTRY_ID = [1]
# Each entry: {"id": int, "project": str, "title": str}. `/debug/readinglist`
# exposes this for pty verification; `/debug/readinglist/seed` injects an
# entry directly (simulating "another device already added this page"), the
# substrate the pull-side of the two-way sync test needs.
READING_LIST_ENTRIES = []

# CSRF/watch token epoch (PRD §6.2 rule 8's badtoken-retry contract). Tokens
# are minted `f"{kind}TOKEN-{epoch}"`; `/debug/expire-tokens` bumps the epoch,
# instantly invalidating every token issued before the bump, so a test can:
# fetch a token (epoch N) -> call /debug/expire-tokens (epoch N+1) -> attempt
# a write with the now-stale token -> get a real `badtoken` error -> confirm
# the client refetches (gets epoch N+1) and retries successfully.
TOKEN_EPOCH = [0]


def _mint_token(kind):
    return f"{kind}TOKEN-{TOKEN_EPOCH[0]}"


def _token_valid(kind, token):
    return token == _mint_token(kind)


def _bearer_username(headers):
    """The username a request's `Authorization: Bearer` header resolves to,
    or `None` if it's missing/unrecognized — shared by every account-feature
    write action's login check below."""
    auth_header = headers.get('Authorization', '')
    token = auth_header[7:].strip() if auth_header.lower().startswith('bearer ') else ''
    return OAUTH_ACCESS.get(token)


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
        global LIFTWING_HITS
        parsed = urllib.parse.urlparse(self.path)
        parts = parsed.path.split('/')
        params = urllib.parse.parse_qs(parsed.query)

        # PRD NF-NET-2 verification: record every real request's path + UA.
        if not parsed.path.startswith('/debug/'):
            REQUEST_LOG.append({
                "path": self.path,
                "ua": self.headers.get('User-Agent', ''),
            })

        # PRD FR-ML-5 / §6.2 rule 3 fixture: a "wiki" (any base_url ending in
        # this path segment) with NO Parsoid REST at all — a bare 404, no
        # body, exactly the shape `api::WikiClient::fetch_article_html`'s
        # `parsoid_404_is_genuine_miss` must treat as "try legacy
        # action=parse instead," never as "this article doesn't exist"
        # (contrast `_serve_article`'s 404 below, which always carries a
        # JSON error envelope). Checked first so it intercepts before the
        # generic `/page/.../html` dispatch would otherwise serve it.
        if '/legacywiki/' in parsed.path and '/rest.php/v1/page/' in parsed.path and parsed.path.endswith('/html'):
            self.send_response(404)
            self.end_headers()
            return

        if parsed.path == '/debug/media-hits':
            self._send_json({"hits": MEDIA_HITS})
        elif parsed.path == '/debug/liftwing-hits':
            self._send_json({"hits": LIFTWING_HITS})
        elif parsed.path == '/debug/requests':
            self._send_json({"requests": REQUEST_LOG})
        elif parsed.path == '/debug/oauth':
            # PRD §5.9: per-grant hit counters, so a pty test can assert a
            # refresh actually reached the token endpoint.
            self._send_json({"stats": OAUTH_STATS})
        elif parsed.path == '/debug/reset':
            LIFTWING_HITS = 0
            REQUEST_LOG.clear()
            # PRD FR-ACC-2/3: restore the account-feature fixtures' mutable
            # state (watched titles, notification read-flags, token epoch) so
            # a pty/test run can isolate phases exactly like it already does
            # for REQUEST_LOG — nothing pre-existing depended on these globals
            # being reset, since this call added them.
            WATCHED_TITLES.clear()
            WATCHED_TITLES.update(WATCHED_TITLES_SEED)
            NOTIFICATIONS[:] = [dict(n) for n in NOTIFICATIONS_SEED]
            TOKEN_EPOCH[0] = 0
            # PRD FR-BM-5: the Reading List mock's own mutable state, reset
            # the same way the watchlist/notifications fixtures are above.
            READINGLISTS_SETUP_DONE[0] = False
            READINGLISTS_NEXT_ENTRY_ID[0] = 1
            READING_LIST_ENTRIES.clear()
            # PRD FR-ACC-8: restore the editing fixtures' mutable state (the
            # recorded edits, the per-page wikitext + revids) so an edit-flow
            # phase starts clean, same as the account fixtures above.
            RECORDED_EDITS.clear()
            for k, v in WIKITEXT_SEED.items():
                WIKITEXT[k] = v
                WIKITEXT_REVID[k] = REVIDS.get(k, WIKITEXT_REVID[k])
            self._send_json({"ok": True})
        elif parsed.path == '/debug/edits':
            # PRD FR-ACC-8: every recorded typo-fix edit, so a pty test can
            # confirm the save request shape (summary/minor/baserevid/token)
            # AND byte-compare the received wikitext against the expected
            # splice (the mock echoes the text verbatim).
            self._send_json({"edits": list(RECORDED_EDITS)})
        elif parsed.path == '/debug/wikitext':
            # The current source wikitext + revid of a page (post-edit), so a
            # test can confirm a save landed and what it wrote.
            key = urllib.parse.unquote(params.get('title', [''])[0]).replace(' ', '_')
            self._send_json({
                "title": key.replace('_', ' '),
                "revid": WIKITEXT_REVID.get(key),
                "text": WIKITEXT.get(key),
            })
        elif parsed.path == '/debug/bump-revid':
            # PRD FR-ACC-8 conflict simulation: bump a page's current revid out
            # of band (as if another editor saved between the client's fetch
            # and its save), so the client's next `action=edit` with the now-
            # stale `baserevid` gets a real `editconflict`.
            key = urllib.parse.unquote(params.get('title', [''])[0]).replace(' ', '_')
            if key in WIKITEXT_REVID:
                WIKITEXT_REVID[key] += 1
            self._send_json({"title": key.replace('_', ' '), "revid": WIKITEXT_REVID.get(key)})
        elif parsed.path == '/debug/expire-tokens':
            # PRD §6.2 rule 8: bump the token epoch, instantly invalidating
            # every csrf/watch token minted before this call — the fixture a
            # test uses to force a real `badtoken` response and exercise the
            # client's refetch-and-retry-once path end to end.
            TOKEN_EPOCH[0] += 1
            self._send_json({"epoch": TOKEN_EPOCH[0]})
        elif parsed.path == '/debug/watchlist':
            # Introspection for pty verification: the server's current
            # watched-titles set, so a test can confirm a `w` toggle actually
            # mutated server-side state without re-deriving it from the
            # picker's rendered text.
            self._send_json({"watched": sorted(WATCHED_TITLES)})
        elif parsed.path == '/debug/readinglist':
            # PRD FR-BM-5: introspection for pty verification — the mock's
            # current Reading List entries, so a test can confirm `:sync`
            # actually pushed/pulled/deleted server-side state.
            self._send_json({
                "setup_done": READINGLISTS_SETUP_DONE[0],
                "list_id": READINGLISTS_DEFAULT_LIST_ID,
                "entries": list(READING_LIST_ENTRIES),
            })
        elif parsed.path == '/debug/readinglist/seed':
            # Test-only convenience: injects a server-side entry directly,
            # bypassing this client entirely — simulates "another device
            # already added this page to the Reading List," the substrate
            # the pull-side of the two-way sync test needs. Also flips
            # `setup_done` true (a seeded account is, definitionally, one
            # that's already set up).
            title = urllib.parse.unquote(params.get('title', [''])[0]).replace('_', ' ')
            project = urllib.parse.unquote(params.get('project', ['http://127.0.0.1:8943'])[0])
            READINGLISTS_SETUP_DONE[0] = True
            entry_id = READINGLISTS_NEXT_ENTRY_ID[0]
            READINGLISTS_NEXT_ENTRY_ID[0] += 1
            READING_LIST_ENTRIES.append({"id": entry_id, "project": project, "title": title})
            self._send_json({"ok": True, "id": entry_id})
        elif parsed.path.endswith('/oauth2/authorize'):
            self._serve_oauth_authorize(params)
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
        entry = SUMMARIES.get(title, f"{title.replace('_', ' ')} is a topic on Wikipedia.")
        # PRD FR-NV-5: an entry is either a bare extract string or a dict with
        # extract/description/thumbnail. Normalise to the REST summary shape.
        if isinstance(entry, dict):
            extract = entry.get("extract", "")
            description = entry.get("description", "")
            thumb = entry.get("thumbnail")
        else:
            extract, description, thumb = entry, "", None
        payload = {
            "title": title.replace('_', ' '),
            "extract": extract,
            "description": description,
        }
        if thumb:
            payload["thumbnail"] = {"source": thumb}
        self._send_json(payload)

    def _serve_action_api(self, params):
        action = params.get('action', [''])[0]
        listing = params.get('list', [''])[0]
        generator = params.get('generator', [''])[0]
        prop = params.get('prop', [''])[0]
        meta = params.get('meta', [''])[0]
        # PRD §6.2 rule 3 / FR-ML-5: the legacy `action=parse&prop=text`
        # fallback `api::WikiClient::fetch_article_html_legacy` uses when
        # Parsoid REST is unsupported (or `parser = "legacy"` skips it
        # outright). Served from the SAME `PAGES` dict as the Parsoid
        # endpoint — the point of this fixture is that opening an article on
        # a legacy-only wiki renders identical content, just via a different
        # request shape (formatversion=2's `parse.text` is a raw HTML
        # string, not the formatversion=1 `{"*": ...}` wrapper).
        if action == 'parse':
            title = urllib.parse.unquote(params.get('page', [''])[0])
            key = title.replace(' ', '_')
            html = PAGES.get(key)
            if html is None:
                self._send_json({
                    "error": {"code": "missingtitle", "info": "The page you specified doesn't exist."}
                })
                return
            self._send_json({
                "parse": {
                    "title": title.replace('_', ' '),
                    "revid": current_revid(key),
                    "text": current_html(key, html),
                }
            })
            return
        # PRD FR-ACC-1: the authenticated whoami. A Bearer token this OAuth
        # provider issued resolves to MOCK_USERNAME; anything else is an
        # anonymous session (which the client rejects, never mistaking it for
        # a login).
        if action == 'query' and meta == 'userinfo':
            OAUTH_STATS["userinfo"] += 1
            auth_header = self.headers.get('Authorization', '')
            token = auth_header[7:].strip() if auth_header.lower().startswith('bearer ') else ''
            if token and token in OAUTH_ACCESS:
                payload = {"id": 42, "name": OAUTH_ACCESS[token]}
                # PRD FR-ACC-7: `uiprop=options` additionally asks for the
                # read-only prefs surface — folded onto the same whoami
                # response rather than a second endpoint, matching how the
                # real `meta=userinfo` accretes fields per requested `uiprop`.
                if 'options' in params.get('uiprop', [''])[0]:
                    payload["options"] = {
                        "skin": USER_PREFS["skin"],
                        "language": USER_PREFS["language"],
                    }
                    payload["editcount"] = USER_PREFS["editcount"]
                    payload["emailauthenticated"] = USER_PREFS["emailauthenticated"]
                self._send_json({"query": {"userinfo": payload}})
            else:
                self._send_json({
                    "query": {"userinfo": {"id": 0, "name": "127.0.0.1", "anon": True}}
                })
            return
        # PRD FR-ACC-2's watch-toggle status check: `prop=info&inprop=
        # watched` on the article the reader has open, authenticated. A
        # request from a token this provider never issued (or an anonymous
        # one) simply never matches WATCHED_TITLES's membership test, same
        # as a real logged-out `watched` check reporting false.
        if action == 'query' and prop == 'info' and 'watched' in params.get('inprop', [''])[0]:
            title = urllib.parse.unquote(params.get('titles', [''])[0]).replace('_', ' ')
            watched = _bearer_username(self.headers) is not None and title in WATCHED_TITLES
            self._send_json({"query": {"pages": [{"title": title, "watched": watched}]}})
            return
        # PRD Appendix A "Auth (Wikimedia)" / §6.2 rule 8: `meta=tokens&
        # type=csrf|watch`. Real MediaWiki also accepts `|`-joined multiple
        # types in one call; this mock only ever receives one at a time (see
        # `account::TokenCache`), so only the single-type shape is served.
        if action == 'query' and meta == 'tokens':
            ttype = params.get('type', ['csrf'])[0]
            self._send_json({"query": {"tokens": {f"{ttype}token": _mint_token(ttype)}}})
            return
        # PRD FR-BM-5: `action=readinglists` (its own top-level action, not
        # `action=query` — see `account.rs`'s ReadingLists module doc for
        # the assumed shape). `command=list`/`listentries` are reads;
        # `setup`/`createentry`/`deleteentry` are CSRF-token'd writes
        # handled in `_serve_action_api_write` below.
        if action == 'readinglists':
            self._serve_readinglists_read(params)
            return
        # PRD FR-ACC-2: the raw watched-pages list.
        if action == 'query' and listing == 'watchlistraw':
            self._send_json({
                "watchlistraw": [{"ns": 0, "title": t} for t in sorted(WATCHED_TITLES)]
            })
            return
        # PRD FR-ACC-2: the "what changed" activity feed — recent changes to
        # currently-watched pages only (unwatching drops a title's entries,
        # matching a real watchlist).
        if action == 'query' and listing == 'watchlist':
            entries = [c for c in WATCHLIST_CHANGES if c["title"] in WATCHED_TITLES]
            self._send_json({"query": {"watchlist": entries}})
            return
        # PRD FR-ACC-3: the unread-count badge and the split alerts/messages
        # list, both derived from the same mutable NOTIFICATIONS fixture so
        # `action=echomarkread` visibly changes both on the next fetch.
        if action == 'query' and meta == 'notifications':
            notprop = params.get('notprop', [''])[0]
            if notprop == 'count':
                alert = sum(1 for n in NOTIFICATIONS if n["type"] == "alert" and not n["read"])
                message = sum(1 for n in NOTIFICATIONS if n["type"] == "message" and not n["read"])
                self._send_json({
                    "query": {"notifications": {"alert": {"count": alert}, "message": {"count": message}}}
                })
                return
            self._send_json({"query": {"notifications": {"list": NOTIFICATIONS}}})
            return
        # PRD FR-ACC-4: public, no auth required — works for any username,
        # including one that never logged in here at all (empty list, not
        # an error, matching a real editor with zero contributions on this
        # wiki rather than a nonexistent account).
        if action == 'query' and listing == 'usercontribs':
            user = params.get('ucuser', [''])[0]
            limit = int(params.get('uclimit', ['50'])[0])
            self._send_json({
                "query": {"usercontribs": USER_CONTRIBS.get(user, [])[:limit]}
            })
            return
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
        # PRD FR-DL-5: the ONE batched generator=links + prop=info redlink
        # check — a linked title comes back `"missing": true` (and no
        # `pageid`) unless it's a real PAGES key, exactly the real
        # MediaWiki "which of this page's links are broken" technique.
        if action == 'query' and generator == 'links' and prop == 'info':
            source = params.get('titles', [''])[0].replace('_', ' ')
            real_titles = {t.replace('_', ' ') for t in PAGES}
            links = PAGE_LINKS.get(source, [])
            pages = [
                {"title": t} if t in real_titles else {"title": t, "missing": True}
                for t in links
            ]
            self._send_json({"query": {"pages": pages}})
            return
        # PRD FR-SR-5 / Appendix A "Random": rotates through RANDOM_TITLES
        # (real PAGES titles, display form) instead of true randomness — see
        # RANDOM_TITLES's own comment for why this is deterministic-testable.
        if action == 'query' and listing == 'random':
            limit = int(params.get('rnlimit', ['1'])[0])
            picks = []
            for _ in range(limit):
                picks.append(RANDOM_TITLES[RANDOM_INDEX[0] % len(RANDOM_TITLES)])
                RANDOM_INDEX[0] += 1
            self._send_json({
                "query": {"random": [{"title": t.replace('_', ' ')} for t in picks]}
            })
            return
        # PRD FR-DL-3 / FR-SR-5 "Quality": the batched prop=pageassessments
        # call, one WikiProject class per title. A title absent from
        # ASSESSMENTS is returned with no pageassessments at all (never
        # invented), matching a real unassessed page.
        if action == 'query' and prop == 'pageassessments':
            titles = urllib.parse.unquote(params.get('titles', [''])[0]).split('|')
            pages = []
            for t in titles:
                cls = ASSESSMENTS.get(t)
                assessments = {"WikiProject Mock": {"class": cls}} if cls else {}
                pages.append({"title": t, "pageassessments": assessments})
            self._send_json({"query": {"pages": pages}})
            return
        # PRD FR-PF-3: `prop=categories` (with `clshow=!hidden`), batched over
        # `titles=A|B`. Returns each page's categories in `Category:Foo` form —
        # including maintenance categories, on purpose, so the client's own
        # maintenance filter (`interest::is_maintenance_category`) is what
        # drops them (see ARTICLE_CATEGORIES's comment).
        if action == 'query' and prop == 'categories':
            raw = urllib.parse.unquote(params.get('titles', [''])[0])
            titles = raw.split('|')
            pages = []
            for t in titles:
                disp = t.replace('_', ' ')
                cats = ARTICLE_CATEGORIES.get(disp, [])
                pages.append({"title": disp, "categories": [{"title": c} for c in cats]})
            self._send_json({"query": {"pages": pages}})
            return
        # PRD FR-ACC-8: the SOURCE wikitext + base revision the typo-fix edit
        # flow fetches (`prop=revisions&rvprop=content|ids|timestamp&
        # rvslots=main`) — the current wikitext, not the rendered HTML. A page
        # with no WIKITEXT fixture comes back `missing`, which the client
        # treats as "couldn't fetch the source", never as an empty article to
        # overwrite.
        if action == 'query' and prop == 'revisions':
            raw = urllib.parse.unquote(params.get('titles', [''])[0])
            key = raw.replace(' ', '_')
            text = WIKITEXT.get(key)
            if text is None:
                self._send_json({"query": {"pages": [{"title": raw.replace('_', ' '), "missing": True}]}})
                return
            self._send_json({
                "query": {
                    "pages": [{
                        "title": raw.replace('_', ' '),
                        "revisions": [{
                            "revid": WIKITEXT_REVID[key],
                            "timestamp": WIKITEXT_TIMESTAMP,
                            "slots": {"main": {"contentmodel": "wikitext", "content": text}},
                        }],
                    }]
                }
            })
            return
        # PRD FR-ML-1/2 (Appendix A "Langlinks"): `prop=langlinks&
        # llprop=autonym|langname|url`, one queried title, its LANGLINKS
        # entry (or an empty list for a title this fixture has none for —
        # matching a real unfixtured/stub article, not an error).
        if action == 'query' and prop == 'langlinks':
            title = params.get('titles', [''])[0].replace('_', ' ')
            links = LANGLINKS.get(title, [])
            self._send_json({"query": {"pages": [{"title": title, "langlinks": links}]}})
            return
        self.send_response(404)
        self.end_headers()

    def _serve_readinglists_read(self, params):
        # PRD FR-BM-5 / SP-7: `command=list`/`listentries`. Both require
        # login (a real Bearer token this provider issued) and both 404 with
        # the assumed "not set up" error until `command=setup` has run once
        # — the signal `main::fetch_readinglists_with_setup` retries on,
        # mirroring the badtoken-retry idiom.
        command = params.get('command', [''])[0]
        if _bearer_username(self.headers) is None:
            self._send_json({"error": {"code": "notloggedin", "info": "Must be logged in"}})
            return
        if command in ('list', 'listentries') and not READINGLISTS_SETUP_DONE[0]:
            self._send_json({
                "error": {
                    "code": "readinglists-db-error-not-set-up",
                    "info": "reading lists are not set up for this user",
                }
            })
            return
        if command == 'list':
            self._send_json({
                "readinglists": {
                    "lists": [{"id": READINGLISTS_DEFAULT_LIST_ID, "name": "default", "default": True}]
                }
            })
            return
        if command == 'listentries':
            self._send_json({"readinglists": {"entries": list(READING_LIST_ENTRIES)}})
            return
        self.send_response(404)
        self.end_headers()

    def _serve_oauth_authorize(self, params):
        # PRD §5.9: no consent screen — mint a code bound to the PKCE
        # challenge and 302-redirect to the client's loopback redirect_uri.
        OAUTH_STATS["authorize"] += 1
        redirect_uri = params.get('redirect_uri', [''])[0]
        state = params.get('state', [''])[0]
        challenge = params.get('code_challenge', [''])[0]
        method = params.get('code_challenge_method', ['S256'])[0]
        if not redirect_uri:
            self.send_response(400)
            self.end_headers()
            return
        _OAUTH_SEQ[0] += 1
        code = f"mock-code-{_OAUTH_SEQ[0]}"
        OAUTH_CODES[code] = {"challenge": challenge, "method": method}
        sep = '&' if '?' in redirect_uri else '?'
        location = f"{redirect_uri}{sep}code={urllib.parse.quote(code)}"
        if state:
            location += f"&state={urllib.parse.quote(state)}"
        self.send_response(302)
        self.send_header('Location', location)
        self.end_headers()

    def do_POST(self):
        parsed = urllib.parse.urlparse(self.path)
        length = int(self.headers.get('Content-Length', '0') or '0')
        body = self.rfile.read(length).decode('utf-8', 'replace') if length else ''
        form = {k: v[0] for k, v in urllib.parse.parse_qs(body).items()}
        REQUEST_LOG.append({"path": self.path, "ua": self.headers.get('User-Agent', '')})
        if parsed.path.endswith('/oauth2/access_token'):
            self._serve_oauth_token(form)
        elif parsed.path.endswith('/api.php'):
            self._serve_action_api_write(form)
        elif parsed.path.startswith('/service/lw/inference/v1/models/') and parsed.path.endswith(':predict'):
            self._serve_liftwing_predict(parsed.path, body)
        else:
            self.send_response(404)
            self.end_headers()

    def _serve_liftwing_predict(self, path, body):
        # PRD FR-DL-3 v2 / §6.2 rule 1's stated exception: the gateway
        # `articlequality` predict endpoint — see LIFTWING_SCORES's own
        # comment for the request/response shape this mirrors (a JSON body
        # naming `rev_id`, not the form-encoded shape every other write on
        # this mock uses).
        global LIFTWING_HITS
        LIFTWING_HITS += 1
        # `/models/{lang}wiki-articlequality:predict` — the model segment
        # names the wiki; the wiki key in the response body is that same
        # `{lang}wiki` (mirrors the real ORES-legacy-compatible shape
        # `api::LiftWingResponse` parses).
        model = path.rsplit('/', 1)[-1].split(':', 1)[0]
        wiki_db = model[:-len('-articlequality')] if model.endswith('-articlequality') else model
        try:
            payload = json.loads(body) if body else {}
        except ValueError:
            payload = {}
        rev_id = payload.get('rev_id')
        prediction = LIFTWING_SCORES.get(rev_id)
        scores = {}
        if prediction is not None:
            classes = ["FA", "GA", "B", "C", "Start", "Stub"]
            probability = {c: (0.9 if c == prediction else 0.02) for c in classes}
            scores[str(rev_id)] = {
                "articlequality": {
                    "score": {
                        "prediction": prediction,
                        "probability": probability,
                    },
                },
            }
        self._send_json({wiki_db: {"scores": scores}})

    def _serve_action_api_write(self, form):
        # PRD §6.2 rule 8: every write here (`action=watch`/`thank`/
        # `echomarkread`) needs both the OAuth Bearer token (login) and a
        # matching csrf/watch token (`_token_valid`) — a real MediaWiki-shaped
        # `{"error":{"code":...}}` body with HTTP 200, never a 4xx, so the
        # client's badtoken-retry path can actually read and classify it (the
        # same posture `_serve_oauth_token`'s 400s deliberately do NOT share —
        # OAuth token-endpoint errors and action-API errors are different
        # wire shapes on the real APIs too).
        action = form.get('action', '')
        username = _bearer_username(self.headers)
        if action not in ('watch', 'thank', 'echomarkread', 'readinglists', 'edit'):
            self.send_response(404)
            self.end_headers()
            return
        if username is None:
            self._send_json({"error": {"code": "notloggedin", "info": "Must be logged in"}})
            return
        token_kind = 'watch' if action == 'watch' else 'csrf'
        if not _token_valid(token_kind, form.get('token', '')):
            self._send_json({"error": {"code": "badtoken", "info": "Invalid CSRF token"}})
            return
        if action == 'edit':
            self._serve_edit_write(form)
            return
        if action == 'watch':
            # PRD FR-BM-6: `titles=A|B` (the watch-mirror's own batching) is
            # tried first; the single-`title` form the `w` keybinding uses
            # (FR-ACC-2) still works too — both share this one handler and
            # response shape.
            titles_param = form.get('titles', '') or form.get('title', '')
            titles = [t.replace('_', ' ') for t in titles_param.split('|') if t]
            unwatch = form.get('unwatch', '') in ('1', 'true')
            results = []
            for title in titles:
                if unwatch:
                    WATCHED_TITLES.discard(title)
                    results.append({"ns": 0, "title": title, "unwatched": True})
                else:
                    WATCHED_TITLES.add(title)
                    results.append({"ns": 0, "title": title, "watched": True})
            self._send_json({"watch": results})
            return
        if action == 'readinglists':
            self._serve_readinglists_write(form)
            return
        if action == 'thank':
            # PRD FR-ACC-6: purely positive, one-way — nothing here tracks
            # who's been thanked for what, only that the request succeeded.
            self._send_json({"result": {"success": 1, "recipient": username}})
            return
        # action == 'echomarkread' (FR-ACC-3): `all=1` marks every
        # notification read; otherwise `list` is a `|`-joined id set.
        if form.get('all', '') in ('1', 'true'):
            for n in NOTIFICATIONS:
                n["read"] = True
        else:
            ids = set(form.get('list', '').split('|'))
            for n in NOTIFICATIONS:
                if n["id"] in ids:
                    n["read"] = True
        self._send_json({"query": {"echomarkread": {"result": "success"}}})

    def _serve_readinglists_write(self, form):
        # PRD FR-BM-5 / SP-7: the CSRF-token'd `action=readinglists` write
        # commands (`_serve_action_api_write` already checked the token
        # before dispatching here) — `setup` (idempotent), `createentry`,
        # `deleteentry`. See `account.rs`'s ReadingLists module doc for the
        # exact assumed shape this mirrors.
        command = form.get('command', '')
        if command == 'setup':
            READINGLISTS_SETUP_DONE[0] = True
            self._send_json({"readinglists": {"list": READINGLISTS_DEFAULT_LIST_ID}})
            return
        if command == 'createentry':
            if not READINGLISTS_SETUP_DONE[0]:
                self._send_json({
                    "error": {
                        "code": "readinglists-db-error-not-set-up",
                        "info": "reading lists are not set up for this user",
                    }
                })
                return
            project = form.get('project', '')
            title = form.get('title', '').replace('_', ' ')
            entry_id = READINGLISTS_NEXT_ENTRY_ID[0]
            READINGLISTS_NEXT_ENTRY_ID[0] += 1
            entry = {"id": entry_id, "project": project, "title": title}
            READING_LIST_ENTRIES.append(entry)
            self._send_json({"readinglists": {"entry": entry}})
            return
        if command == 'deleteentry':
            entry_id = int(form.get('entry', '0') or '0')
            before = len(READING_LIST_ENTRIES)
            READING_LIST_ENTRIES[:] = [e for e in READING_LIST_ENTRIES if e["id"] != entry_id]
            self._send_json({"readinglists": {"success": len(READING_LIST_ENTRIES) < before}})
            return
        self._send_json({
            "error": {"code": "readinglists-bad-command", "info": f"unknown command {command!r}"}
        })

    def _serve_edit_write(self, form):
        # PRD FR-ACC-8: the ONLY article-write this mock accepts — a typo-fix
        # `action=edit`. Login + a valid csrf token were already checked by
        # `_serve_action_api_write`. Enforces:
        #   - `nocreate`: a title with no WIKITEXT fixture is a `missingtitle`
        #     error (this can only EDIT existing articles, never create one);
        #   - conflict detection: a `baserevid` that doesn't match the page's
        #     current revid is an `editconflict` (the page changed underneath
        #     the client — never force-overwritten);
        # On success it stores the received text verbatim (echoed via
        # /debug/edits so a test can byte-compare the splice), bumps the revid,
        # and reports the new revision.
        raw_title = form.get('title', '')
        key = raw_title.replace(' ', '_')
        if key not in WIKITEXT:
            self._send_json({
                "error": {"code": "missingtitle", "info": "The page you tried to edit doesn't exist."}
            })
            return
        try:
            baserevid = int(form.get('baserevid', '0') or '0')
        except ValueError:
            baserevid = 0
        current = WIKITEXT_REVID[key]
        if baserevid != current:
            self._send_json({
                "error": {"code": "editconflict", "info": "Edit conflict detected."}
            })
            return
        text = form.get('text', '')
        summary = form.get('summary', '')
        minor = form.get('minor', '') in ('1', 'true')
        newrevid = current + 1
        WIKITEXT[key] = text
        WIKITEXT_REVID[key] = newrevid
        RECORDED_EDITS.append({
            "title": raw_title.replace('_', ' '),
            "text": text,
            "summary": summary,
            "minor": minor,
            "nocreate": form.get('nocreate', '') in ('1', 'true'),
            "baserevid": baserevid,
            "basetimestamp": form.get('basetimestamp', ''),
            "token": form.get('token', ''),
        })
        self._send_json({
            "edit": {
                "result": "Success",
                "pageid": 1,
                "title": raw_title.replace('_', ' '),
                "oldrevid": current,
                "newrevid": newrevid,
                "newtimestamp": WIKITEXT_TIMESTAMP,
            }
        })

    def _serve_oauth_token(self, form):
        # PRD §5.9: the token endpoint. `authorization_code` enforces PKCE;
        # `refresh_token` mints a fresh access token.
        grant = form.get('grant_type', '')
        if grant == 'authorization_code':
            OAUTH_STATS["exchange"] += 1
            code = form.get('code', '')
            verifier = form.get('code_verifier', '')
            entry = OAUTH_CODES.pop(code, None)
            if entry is None or not verifier or not _pkce_ok(
                verifier, entry["challenge"], entry.get("method", "S256")
            ):
                self._send_oauth_error("invalid_grant", "bad code or PKCE verifier")
                return
            access = _mint_access()
            self._send_json({
                "access_token": access,
                "refresh_token": "mock-refresh-token",
                "expires_in": OAUTH_TTL,
                "token_type": "Bearer",
            })
            return
        if grant == 'refresh_token':
            OAUTH_STATS["refresh"] += 1
            refresh = form.get('refresh_token', '')
            if not refresh:
                self._send_oauth_error("invalid_grant", "missing refresh token")
                return
            access = _mint_access()
            self._send_json({
                "access_token": access,
                "refresh_token": refresh,
                "expires_in": OAUTH_TTL,
                "token_type": "Bearer",
            })
            return
        self._send_oauth_error("unsupported_grant_type", grant)

    def _send_oauth_error(self, error, desc):
        body = json.dumps({"error": error, "error_description": desc}).encode()
        self.send_response(400)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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

    def _serve_genuine_miss_404(self, title):
        # PRD §6.2 rule 3: the real core REST API's 404 shape for a title
        # that genuinely doesn't exist — a JSON error envelope carrying
        # `errorKey`. This is what `api::WikiClient::
        # parsoid_404_is_genuine_miss` keys on to tell "doesn't exist" apart
        # from "this wiki doesn't run Parsoid REST at all" (the bare,
        # bodyless 404 the `/legacywiki/` route above sends instead).
        body = json.dumps({
            "httpCode": 404,
            "httpReason": "Not Found",
            "errorKey": "rest-nonexistent-title",
            "messageTranslations": {"en": f"The page you specified, {title!r}, doesn't exist."},
        }).encode()
        self.send_response(404)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _serve_article(self, parts):
        title = urllib.parse.unquote(parts[-2])
        lang = _lang_prefix(parts)
        # PRD FR-ML-2: this one language edition doesn't have this one
        # title, even though the flat PAGES dict does — see LANG_MISSING.
        if lang and title in LANG_MISSING.get(lang, ()):
            self._serve_genuine_miss_404(title)
            return
        html = PAGES.get(title)
        if html is None:
            self._serve_genuine_miss_404(title)
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
        lang = _lang_prefix(parts)
        if title not in PAGES or (lang and title in LANG_MISSING.get(lang, ())):
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
        # PRD FR-SR-3: CirrusSearch operators are query-*string* syntax this
        # endpoint itself is meant to interpret — `intitle:`/`morelike:` get
        # minimal real handling here (enough to prove the client's passthrough
        # actually reaches the server unmangled); the other five documented
        # operators (`incategory:`, `insource:`, `hastemplate:`, `deepcat:`,
        # `articletopic:`, `prefix:`) fall through to the plain substring
        # match below like any other query text, which is fine for proving
        # passthrough (asserted at the wire level, not against this mock's
        # interpretation of them).
        q = params.get('q', [''])[0]
        limit = int(params.get('limit', ['20'])[0])
        ql = q.lower().strip()

        if ql.startswith('morelike:'):
            # PRD FR-SR-6: the Related panel's substrate. The argument may
            # carry underscores (a title fetched with them) or spaces; both
            # normalise to the RELATED_ARTICLES keys' space form.
            target = q[len('morelike:'):].strip().replace('_', ' ')
            related = RELATED_ARTICLES.get(target, [])
            pages = [
                {"title": r["title"], "description": r["description"]}
                for r in related[:limit]
            ]
            self._send_json({"pages": pages})
            return

        if ql.startswith('intitle:'):
            # PRD FR-SR-3: title-only match, proving the operator changed
            # what the mock matched against (title, never the body text) —
            # not just that some substring of the raw query survived.
            term = ql[len('intitle:'):].strip()
            hits = [p for p in SEARCH_PAGES if term and term in p["title"].lower()]
            hits = hits[:limit]
            pages = [
                {
                    "title": p["title"],
                    "description": p["description"],
                    "excerpt": make_excerpt(p["text"], term),
                    "size": p["size"],
                    "wordcount": p["wordcount"],
                    "timestamp": p["timestamp"],
                }
                for p in hits
            ]
            self._send_json({"pages": pages})
            return

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
