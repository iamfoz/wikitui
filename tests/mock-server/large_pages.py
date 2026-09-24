"""Large Parsoid-like article fixtures for memory measurement (PRD §6.8).

`server.py`'s fixture pages are a few KB each — far too small to say
anything about §6.8's "< 150 MB RSS with 10 tabs; `low_memory` mode < 50 MB"
target, which is about real article sizes. This is a separate, standalone
mock serving *generated* pages shaped like real Parsoid HTML (sections with
`data-mw-section-id`, `mw:WikiLink` anchors, `mw:Extension/ref` footnote
markers, an infobox transclusion with a `data-mw` blob, thumbnail figures,
wikitables, a references list of `cite` entries, a trailing navbox) so the
markup-to-text ratio — and so the parse/DOM/layout cost per byte — is in the
same range as a real article, not a single repeated `<p>`.

Titles `Large_<KB>` (e.g. `Large_150`, `Large_1500`) produce an article of at
least that many KB of HTML; any other title gets a small ordinary page (so a
link-prefetch or link-preview of a generated article's links stays cheap and
doesn't distort the measurement with ten more 1.5 MB pages). Deterministic:
the same title always yields byte-identical HTML and the same revid.

Serves only what an article open touches:

- `GET /w/rest.php/v1/page/{title}/html` — the article (with a Parsoid
  `ETag: W/"{revid}/…"` so the client records a revid).
- `GET /w/rest.php/v1/page/{title}/bare` — `{"latest": {"id": revid}}`.
- `GET /w/api.php?...` — an empty-but-valid Action API reply (no langlinks,
  categories, assessments…), so enrichment degrades quietly.
- anything else — 404.

Run: `python3 tests/mock-server/large_pages.py [port]` (default 8944), then
`WIKITUI_BASE_URL=http://127.0.0.1:8944 wikitui Large_1500`.
"""

import http.server
import json
import sys
import urllib.parse
import zlib

# A ~2,400-word pseudo-vocabulary (syllable combinations) drawn by a small
# LCG, so the prose compresses roughly like real English text rather than
# like a sentence repeated a thousand times — which matters for how much a
# compressed copy of the page costs (`tab::SourceHtml`) and for the L2 cache.
_SYL = ["ta", "ri", "mon", "ka", "le", "sto", "ver", "nu", "pha", "gri", "del",
        "os", "tri", "an", "cu", "bel", "mi", "tor", "sa", "qui", "ne", "zo",
        "ral", "hen", "pli", "ex", "um", "dro", "fa", "gen", "ic", "lo", "wes",
        "bra", "tion", "al", "ist", "ment", "ous", "ing", "er", "ly", "ca", "po",
        "sy", "ther", "cal", "nor"]
WORDS = sorted({a + b + (c if (i % 3) else "") for i, (a, b, c) in enumerate(
    (x, y, z) for x in _SYL[:24] for y in _SYL[12:48] for z in _SYL[30:34]
)})[:2400] + ["the", "of", "and", "in", "to", "a", "was", "is", "for", "on",
              "as", "by", "with", "his", "that", "from", "at", "which"] * 40


def revid_for(title):
    return 100_000 + (zlib.crc32(title.encode()) % 800_000)


def prose(seed, n_words):
    state = (seed * 2654435761 + 12345) & 0xFFFFFFFF
    out = []
    for _ in range(n_words):
        state = (state * 1103515245 + 12345) & 0x7FFFFFFF
        out.append(WORDS[state % len(WORDS)])
    s = " ".join(out)
    return s[0].upper() + s[1:]


class Ids:
    def __init__(self):
        self.n = 0

    def next(self):
        self.n += 1
        return "mw" + format(self.n, "X")


def large_article(title, target_bytes):
    ids = Ids()
    revid = revid_for(title)
    display = title.replace("_", " ")
    parts = [
        '<!DOCTYPE html>\n<html prefix="dc: http://purl.org/dc/terms/ mw: http://mediawiki.org/rdf/" '
        f'about="https://en.wikipedia.org/wiki/Special:Redirect/revision/{revid}">'
        '<head prefix="mwr: https://en.wikipedia.org/wiki/Special:Redirect/">'
        '<meta charset="utf-8"/><meta property="mw:pageId" content="1208"/>'
        '<meta property="mw:pageNamespace" content="0"/>'
        f'<link rel="dc:replaces" resource="mwr:revision/{revid - 1}"/>'
        f'<meta property="mw:revisionSHA1" content="{zlib.crc32(title.encode()):08x}"/>'
        '<meta property="dc:modified" content="2026-09-01T12:00:00.000Z"/>'
        '<meta property="mw:htmlVersion" content="2.8.0"/>'
        '<link rel="dc:isVersionOf" href="//en.wikipedia.org/wiki/' + title + '"/>'
        '<base href="//en.wikipedia.org/wiki/"/>'
        f'<title>{display}</title>'
        '<link rel="stylesheet" href="/w/load.php?lang=en&amp;modules=mediawiki.skinning.content.parsoid%7Cmediawiki.skinning.interface%7Csite.styles&amp;only=styles&amp;skin=vector"/>'
        '</head><body id="mwAA" lang="en" class="mw-content-ltr sitedir-ltr ltr mw-body-content '
        'parsoid-body mediawiki mw-parser-output" dir="ltr">'
    ]
    # Lead section: hatnote, infobox transclusion, lead paragraphs.
    parts.append(f'<section data-mw-section-id="0" id="{ids.next()}">')
    parts.append(
        f'<div role="note" class="hatnote navigation-not-searchable" about="#mwt1" typeof="mw:Transclusion" '
        f'data-mw=\'{{"parts":[{{"template":{{"target":{{"wt":"About","href":"./Template:About"}},'
        f'"params":{{"1":{{"wt":"the subject"}}}},"i":0}}}}]}}\' id="{ids.next()}">'
        f'This article is about {display}. For other uses, see '
        f'<a rel="mw:WikiLink" href="./{title}_(disambiguation)" title="{display} (disambiguation)">'
        f'{display} (disambiguation)</a>.</div>'
    )
    rows = []
    for r in range(14):
        rows.append(
            f'<tr id="{ids.next()}"><th scope="row" class="infobox-label" id="{ids.next()}">Field {r}</th>'
            f'<td class="infobox-data" id="{ids.next()}">{prose(r, 6)} '
            f'<a rel="mw:WikiLink" href="./Infobox_link_{r}" title="Infobox link {r}">link {r}</a></td></tr>'
        )
    params = ",".join(f'"field{r}":{{"wt":"{prose(r, 6)} [[Infobox link {r}|link {r}]]"}}' for r in range(14))
    parts.append(
        f'<table class="infobox biography vcard" about="#mwt3" typeof="mw:Transclusion" '
        f'data-mw=\'{{"parts":[{{"template":{{"target":{{"wt":"Infobox person","href":"./Template:Infobox_person"}},'
        f'"params":{{{params}}},"i":0}}}}]}}\' id="{ids.next()}"><tbody>'
        f'<tr><th colspan="2" class="infobox-above"><div class="fn">{display}</div></th></tr>'
        + "".join(rows)
        + "</tbody></table>"
    )
    ref = 0
    sec = 0
    body_parts = []
    refs = []

    def paragraph(seed):
        nonlocal ref
        bits = []
        for s in range(5):
            words = prose(seed * 5 + s, 28)
            link_t = f"Topic_{(seed * 5 + s) % 400}"
            bits.append(
                f'{words} <a rel="mw:WikiLink" href="./{link_t}" title="{link_t.replace("_", " ")}" '
                f'id="{ids.next()}">{link_t.replace("_", " ").lower()}</a>.'
            )
            if s % 2 == 0:
                ref += 1
                bits.append(
                    f'<sup about="#mwt{1000 + ref}" class="mw-ref reference" id="cite_ref-{ref}" '
                    f'rel="dc:references" typeof="mw:Extension/ref" '
                    f'data-mw=\'{{"name":"ref","attrs":{{}},"body":{{"id":"mw-reference-text-cite_note-{ref}"}}}}\'>'
                    f'<a href="./{title}#cite_note-{ref}" id="{ids.next()}" style="counter-reset: mw-Ref {ref};">'
                    f'<span class="mw-reflink-text" id="{ids.next()}">[{ref}]</span></a></sup>'
                )
                refs.append(ref)
        return f'<p id="{ids.next()}">' + " ".join(bits) + "</p>"

    for p in range(3):
        body_parts.append(paragraph(p))
    parts.extend(body_parts)
    parts.append("</section>")

    size = sum(len(x) for x in parts)
    # Stop the body once it plus its references list (~900 bytes per
    # footnote) reaches ~92% of the target; the navbox pads the rest.
    while size + len(refs) * 900 < int(target_bytes * 0.92):
        sec += 1
        chunk = [
            f'<section data-mw-section-id="{sec}" id="{ids.next()}">'
            f'<h2 id="Section_{sec}">Section {sec}: {prose(sec, 3)}</h2>'
        ]
        for p in range(4):
            chunk.append(paragraph(sec * 10 + p))
        if sec % 3 == 0:
            chunk.append(
                f'<figure class="mw-default-size" typeof="mw:File/Thumb" id="{ids.next()}">'
                f'<a href="./File:Figure_{sec}.jpg" class="mw-file-description" id="{ids.next()}">'
                f'<img alt="" resource="./File:Figure_{sec}.jpg" '
                f'src="//upload.wikimedia.org/wikipedia/commons/thumb/a/ab/Figure_{sec}.jpg/250px-Figure_{sec}.jpg" '
                f'decoding="async" data-file-width="1200" data-file-height="800" data-file-type="bitmap" '
                f'height="167" width="250" srcset="//upload.wikimedia.org/wikipedia/commons/thumb/a/ab/'
                f'Figure_{sec}.jpg/500px-Figure_{sec}.jpg 2x" class="mw-file-element" id="{ids.next()}"/></a>'
                f'<figcaption id="{ids.next()}">{prose(sec, 12)}</figcaption></figure>'
            )
        if sec % 4 == 0:
            trs = "".join(
                f'<tr id="{ids.next()}"><td id="{ids.next()}">{r}</td><td id="{ids.next()}">{prose(r + sec, 4)}</td>'
                f'<td id="{ids.next()}">{(r * 37) % 1000}</td><td id="{ids.next()}">'
                f'<a rel="mw:WikiLink" href="./Row_{r}" title="Row {r}">row {r}</a></td></tr>'
                for r in range(12)
            )
            chunk.append(
                f'<table class="wikitable sortable" id="{ids.next()}"><tbody>'
                f'<tr id="{ids.next()}"><th>#</th><th>Name</th><th>Value</th><th>See</th></tr>{trs}</tbody></table>'
            )
        if sec % 5 == 0:
            lis = "".join(
                f'<li id="{ids.next()}">{prose(sec + i, 10)} '
                f'<a rel="mw:WikiLink" href="./List_item_{i}" title="List item {i}">item {i}</a></li>'
                for i in range(8)
            )
            chunk.append(f'<ul id="{ids.next()}">{lis}</ul>')
        chunk.append("</section>")
        text = "".join(chunk)
        parts.append(text)
        size += len(text)

    # References list.
    sec += 1
    lis = []
    for r in refs:
        lis.append(
            f'<li about="#cite_note-{r}" id="cite_note-{r}"><span class="mw-cite-backlink" id="{ids.next()}">'
            f'<a href="./{title}#cite_ref-{r}" rel="mw:referencedBy" id="{ids.next()}">'
            f'<span class="mw-linkback-text" id="{ids.next()}">↑ </span></a></span> '
            f'<span id="mw-reference-text-cite_note-{r}" class="mw-reference-text reference-text">'
            f'<cite id="CITEREFAuthor{r}" class="citation book cs1" about="#mwt{5000 + r}" typeof="mw:Transclusion" '
            f'data-mw=\'{{"parts":[{{"template":{{"target":{{"wt":"cite book","href":"./Template:Cite_book"}},'
            f'"params":{{"last":{{"wt":"Author{r}"}},"title":{{"wt":"{prose(r, 5)}"}},"year":{{"wt":"{1950 + r % 70}"}}}},"i":0}}}}]}}\'>'
            f'Author{r}, A. ({1950 + r % 70}). <a rel="mw:ExtLink nofollow" href="https://example.org/books/{r}" '
            f'class="external text" id="{ids.next()}"><i id="{ids.next()}">{prose(r, 5)}</i></a>. Example Press. '
            f'<a href="./Special:BookSources/978-0-00-{r:06d}" id="{ids.next()}">ISBN 978-0-00-{r:06d}</a>.</cite>'
            f'</span></li>'
        )
    parts.append(
        f'<section data-mw-section-id="{sec}" id="{ids.next()}"><h2 id="References">References</h2>'
        f'<div class="mw-references-wrap mw-references-columns" typeof="mw:Extension/references" '
        f'about="#mwt9000" data-mw=\'{{"name":"references","attrs":{{}}}}\' id="{ids.next()}">'
        f'<ol class="mw-references references" id="{ids.next()}">' + "".join(lis) + "</ol></div></section>"
    )
    # Navbox, padded until the target is met.
    nav = [
        f'<div role="navigation" class="navbox" aria-labelledby="Nav" about="#mwt9100" '
        f'typeof="mw:Transclusion" data-mw=\'{{"parts":[{{"template":{{"target":{{"wt":"Navbox"}},"params":{{}},"i":0}}}}]}}\' '
        f'style="padding:3px" id="{ids.next()}"><table class="nowraplinks navbox-inner" style="border-spacing:0;background:transparent;color:inherit"><tbody>'
    ]
    size = sum(len(x) for x in parts)
    k = 0
    while size + sum(len(x) for x in nav) < target_bytes:
        k += 1
        cells = " · ".join(
            f'<a rel="mw:WikiLink" href="./Nav_{k}_{j}" title="Nav {k} {j}">nav {k} {j}</a>' for j in range(10)
        )
        nav.append(
            f'<tr><th scope="row" class="navbox-group" style="width:1%">Group {k}</th>'
            f'<td class="navbox-list-with-group navbox-list navbox-odd hlist" style="width:100%;padding:0">'
            f'<div style="padding:0 0.25em">{cells}</div></td></tr>'
        )
    nav.append("</tbody></table></div>")
    parts.extend(nav)
    parts.append("</body></html>")
    return "".join(parts)


def small_article(title):
    display = title.replace("_", " ")
    return (
        f"<html><head><title>{display}</title></head><body><section data-mw-section-id=\"0\">"
        f"<p>{prose(len(title), 60)}</p></section></body></html>"
    )


def article_for(title):
    if title.startswith("Large_"):
        try:
            kb = int(title.split("_", 1)[1])
        except ValueError:
            kb = 0
        if kb > 0:
            return large_article(title, kb * 1024)
    return small_article(title)


CACHE = {}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send(self, code, body, ctype, extra=None):
        data = body.encode("utf-8") if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        path = urllib.parse.unquote(parsed.path)
        marker = "/rest.php/v1/page/"
        if marker in path:
            rest = path.split(marker, 1)[1]
            title, _, kind = rest.rpartition("/")
            revid = revid_for(title)
            if kind == "html":
                if title not in CACHE:
                    CACHE[title] = article_for(title)
                self._send(200, CACHE[title], "text/html; charset=utf-8",
                           {"ETag": f'W/"{revid}/large-pages-mock"'})
                return
            if kind == "bare":
                self._send(200, json.dumps({"latest": {"id": revid}, "title": title}),
                           "application/json")
                return
        if path.endswith("/api.php"):
            self._send(200, json.dumps({"batchcomplete": True, "query": {}}), "application/json")
            return
        self._send(404, json.dumps({"httpCode": 404}), "application/json")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8944
    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
