#!/usr/bin/env python3
"""Deterministic generator for wikitui's PRD §6.8 performance fixtures.

Writes two committed fixtures next to this file:

- ``pathological.html`` — the §6.8 "pathological article" row: ~1.55 MB of
  Parsoid-shaped HTML carrying 500+ references in ONE article (the old
  ``corpus_tests`` perf smoke used two separate synthetic pages, one big and
  one reference-heavy, neither of which looked like Parsoid output).
- ``typical.html`` — a ~150 KB article, the size class most article opens
  actually land on, used as the baseline row next to the pathological one.

and also exposes ``corpus_article(index)`` so ``tests/mock-server/server.py``
(perf mode) can serve a whole seeded, cross-linked corpus of articles
(``Perf_Corpus_<n>``) with a documented size distribution for the network,
memory and cache-hit-rate scenarios in ``tests/perf/harness.py``.

Everything is driven by ``random.Random(seed)`` and plain string building —
no wall clock, no hash randomization, no dict-order dependence — so a given
seed produces byte-identical output on any Python 3.8+. ``--check`` verifies
the committed files still match what this script generates (CI runs it), so
the fixtures can't silently drift from their documented shape.

The prose is original filler built from a fixed vocabulary; the MARKUP is
what's realistic. It mirrors the attribute density of real Parsoid HTML 2.x
(``GET /w/rest.php/v1/page/{title}/html``): an ``id="mw…"`` on nearly every
element, ``<section data-mw-section-id>`` nesting with ``div.mw-heading``
wrappers, ``typeof="mw:Transclusion"`` + ``about="#mwt…"`` + a ``data-mw``
JSON blob on every template output (infobox, hatnotes, convert/lang
templates, every CS1 citation, navboxes, the reflist), ``sup.mw-ref`` markers
with ``typeof="mw:Extension/ref"`` and ``span.mw-reflink-text``, an
``ol.mw-references`` list with ``cite_note-…`` ids and (multi-)backlinks,
COinS ``span.Z3988`` metadata, TemplateStyles ``<style>``/dedup ``<link>``
pairs, ``rowspan``/``colspan`` wikitables, MathML + fallback-image math
nodes, ``figure[typeof=mw:File/Thumb]`` with ``srcset``, and category
``<link rel=mw:PageProp/Category>`` tails. See ``tests/perf/README.md`` for
the measured shape of each committed fixture.
"""

import argparse
import base64
import json
import pathlib
import random
import sys

HERE = pathlib.Path(__file__).resolve().parent

# Fixed seeds: changing one regenerates a different (but equally valid)
# fixture, which invalidates every recorded number in docs/PERFORMANCE.md —
# don't, without re-measuring.
PATHOLOGICAL_SEED = 0x0608_1500
TYPICAL_SEED = 0x0608_0150
CORPUS_SEED = 0x0608_C0DE

# The mock's perf corpus: how many distinct `Perf_Corpus_<n>` titles exist.
# Every generated link targets one of them, so a link-following walk never
# leaves the corpus.
CORPUS_SIZE = 400

# (label, weight, target HTML bytes, target references) — the size mix the
# network scenario samples from. See tests/perf/README.md ("Article size
# mix") for the basis and its caveats; per-class results are reported
# separately so no verdict hinges on these weights alone.
SIZE_CLASSES = [
    ("short", 0.30, 80_000, 12),
    ("typical", 0.40, 150_000, 45),
    ("long", 0.20, 450_000, 150),
    ("very-long", 0.10, 1_000_000, 330),
]

WORDS = """
the of and to in a was is for on as by with that from at his an which were
are it be or its has had first also after their one this new two her who but
not they have been other into during most would time when more some years all
where while between including three later such used several many under early
known over only then both these however through part would city world year
state work may well since four based made while second system number named
member war group united national people until each them following around
family series became three against life before public within major large
school high government served second both small final important received
century across development early along among university modern century
original several design research program theory method result report study
""".split()

NOUNS = """
archive engine lattice harbor treaty council manuscript observatory railway
cathedral algorithm theorem valley province dynasty charter orchestra glacier
laboratory parliament expedition festival monastery peninsula telescope
senate republic frontier cipher compiler reactor delta estuary plateau canal
fortress guild academy almanac atlas chronicle tribunal conservatory league
foundry mill beacon quarry vineyard garrison embassy ministry colony island
""".split()

ADJECTIVES = """
northern southern eastern western royal imperial ancient medieval modern
early late federal provincial municipal naval coastal alpine urban rural
civil military scientific literary musical industrial agricultural maritime
colonial classical baroque gothic romantic experimental theoretical applied
""".split()

GIVEN = """Ada Alan Grace Charles Mary John Emmy Kurt Sofia Niels Rosalind Paul
Hedy Alonzo Barbara Edsger Frances Donald Margaret Tim Lise Srinivasa Emil
Katherine Hermann Dorothy Leonhard Marie Blaise Hypatia""".split()

FAMILY = """Lovelace Turing Hopper Babbage Somerville Neumann Noether Godel
Kovalevskaya Bohr Franklin Dirac Lamarr Shannon Liskov Dijkstra Allen Knuth
Hamilton Berners-Lee Meitner Ramanujan Artin Johnson Weyl Hodgkin Euler
Curie Pascal Alexandria""".split()

PUBLISHERS = """Oxford University Press|Cambridge University Press|Springer|
Penguin Books|MIT Press|Princeton University Press|Routledge|Wiley|
Faber and Faber|HarperCollins|Academic Press|Elsevier""".replace("\n", "").split("|")

WEBSITES = ["BBC News", "The Guardian", "The New York Times", "Nature",
            "Reuters", "The Times", "Science", "Le Monde", "Der Spiegel",
            "The Economist", "Smithsonian Magazine", "History Today"]

JOURNALS = ["Proceedings of the Royal Society A", "Annals of Mathematics",
            "Journal of Historical Studies", "Physical Review Letters",
            "Notes and Records", "Communications of the ACM",
            "The Mathematical Gazette", "Isis", "Technology and Culture"]

MONTHS = ["January", "February", "March", "April", "May", "June", "July",
          "August", "September", "October", "November", "December"]

TEX = [
    r"E=mc^{2}",
    r"\sum _{k=1}^{n}k={\frac {n(n+1)}{2}}",
    r"\int _{0}^{\infty }e^{-x^{2}}\,dx={\frac {\sqrt {\pi }}{2}}",
    r"\alpha +\beta =\gamma ",
    r"f(x)=\lim _{h\to 0}{\frac {f(x+h)-f(x)}{h}}",
    r"a^{2}+b^{2}=c^{2}",
    r"\nabla \cdot \mathbf {E} ={\frac {\rho }{\varepsilon _{0}}}",
    r"P(A\mid B)={\frac {P(B\mid A)\,P(A)}{P(B)}}",
]

# A condensed stand-in for the CS1 TemplateStyles sheet real articles inline
# once (later uses dedup to a <link rel=mw-deduplicated-inline-style>).
CS1_CSS = (
    ".mw-parser-output cite.citation{font-style:inherit;word-wrap:break-word}"
    ".mw-parser-output .citation q{quotes:\"\\\"\"\"\\\"\"\"'\"\"'\"}"
    ".mw-parser-output .citation:target{background-color:rgba(0,127,255,0.133)}"
    ".mw-parser-output .id-lock-free.id-lock-free a{background:url(\"//upload.wikimedia.org/"
    "wikipedia/commons/6/65/Lock-green.svg\")right 0.1em center/9px no-repeat}"
    ".mw-parser-output .id-lock-limited.id-lock-limited a,.mw-parser-output "
    ".id-lock-registration.id-lock-registration a{background:url(\"//upload.wikimedia.org/"
    "wikipedia/commons/d/d6/Lock-gray-alt-2.svg\")right 0.1em center/9px no-repeat}"
    ".mw-parser-output .cs1-ws-icon a{background:url(\"//upload.wikimedia.org/wikipedia/"
    "commons/4/4c/Wikisource-logo.svg\")right 0.1em center/12px no-repeat}"
    ".mw-parser-output .cs1-code{color:inherit;background:inherit;border:none;padding:inherit}"
    ".mw-parser-output .cs1-hidden-error{display:none;color:#d33}"
    ".mw-parser-output .cs1-visible-error{color:#d33}"
    ".mw-parser-output .cs1-maint{display:none;color:#2C882D;margin-left:0.3em}"
    ".mw-parser-output .cs1-format{font-size:95%}"
    ".mw-parser-output .cs1-kern-left{padding-left:0.2em}"
    ".mw-parser-output .cs1-kern-right{padding-right:0.2em}"
    ".mw-parser-output .citation .mw-selflink{font-weight:inherit}"
) * 3


def esc_text(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def esc_attr_sq(s):
    """Escape for a single-quoted attribute, the quoting Parsoid picks for
    JSON-bearing attributes like ``data-mw``."""
    return (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
            .replace("'", "&#39;"))


def esc_attr_dq(s):
    return (s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
            .replace('"', "&quot;"))


def data_mw(obj):
    return "data-mw='" + esc_attr_sq(json.dumps(obj, ensure_ascii=False, separators=(",", ":"))) + "'"


def href_title(title):
    return title.replace(" ", "_")


def corpus_title(n):
    return "Perf_Corpus_%d" % n


def lead_marker(title):
    """The unique token each generated article carries in its first lead
    sentence — what the pty harness waits for to call the lead section
    painted. Derived from the title alone so the harness can recompute it."""
    h = 0
    for ch in title.encode("utf-8"):
        h = (h * 131 + ch) % 0xFFFFF
    return "LEAD%05X" % h


def tail_marker(title):
    """The unique token in the article's final paragraph (scroll-to-end
    verification)."""
    return "TAIL" + lead_marker(title)[4:]


class Ref:
    __slots__ = ("num", "name", "kind", "params", "uses", "short")

    def __init__(self, num, name, kind, params, short):
        self.num = num
        self.name = name
        self.kind = kind
        self.params = params
        self.uses = 0
        self.short = short

    def note_id(self):
        return "cite_note-%s-%d" % (self.name, self.num) if self.name else "cite_note-%d" % self.num

    def ref_id(self, use):
        if self.name:
            return "cite_ref-%s_%d-%d" % (self.name, self.num, use)
        return "cite_ref-%d" % self.num


class Article:
    def __init__(self, title, seed, target_bytes, target_refs, corpus_size=CORPUS_SIZE,
                 navboxes=1, tables=1, figures=2, math=0):
        self.rng = random.Random(seed)
        self.title = title
        self.display = title.replace("_", " ")
        self.target_bytes = target_bytes
        self.target_refs = target_refs
        self.corpus_size = corpus_size
        self.n_navboxes = navboxes
        self.n_tables = tables
        self.n_figures = figures
        self.n_math = math
        self.next_id = 0
        self.next_about = 0
        self.refs = []
        self.named = []
        self.section_id = 0
        self.prose_start = 0
        self.prose_budget = 0
        self.css_inlined = False
        self.bytes = 0
        self.out = []

    # -- ids -----------------------------------------------------------------

    def mwid(self):
        n = self.next_id
        self.next_id += 1
        raw = n.to_bytes(max(1, (n.bit_length() + 7) // 8), "big")
        return 'id="mw' + base64.b64encode(raw, altchars=b"-_").decode().rstrip("=") + '"'

    def about(self):
        self.next_about += 1
        return "#mwt%d" % self.next_about

    def emit(self, s):
        self.out.append(s)
        self.bytes += len(s.encode("utf-8"))

    # -- words ---------------------------------------------------------------

    def word(self):
        return self.rng.choice(WORDS)

    def phrase(self, lo=1, hi=3):
        n = self.rng.randint(lo, hi)
        parts = []
        for _ in range(n):
            r = self.rng.random()
            if r < 0.45:
                parts.append(self.rng.choice(NOUNS))
            elif r < 0.75:
                parts.append(self.rng.choice(ADJECTIVES))
            else:
                parts.append(self.word())
        return " ".join(parts)

    def person(self):
        return self.rng.choice(GIVEN), self.rng.choice(FAMILY)

    def year(self):
        return self.rng.randint(1780, 2024)

    def date(self):
        return "%d %s %d" % (self.rng.randint(1, 28), self.rng.choice(MONTHS), self.year())

    def iso_date(self):
        return "%04d-%02d-%02d" % (self.rng.randint(2005, 2025), self.rng.randint(1, 12),
                                   self.rng.randint(1, 28))

    # -- inline markup -------------------------------------------------------

    def wikilink(self, text=None, target=None):
        target = target or corpus_title(self.rng.randrange(self.corpus_size))
        text = text or self.phrase(1, 3)
        if self.rng.random() < 0.02:
            # A redlink Parsoid pre-marks with class="new".
            return ('<a rel="mw:WikiLink" href="./%s?action=edit&amp;redlink=1" title="%s" '
                    'class="new" typeof="mw:LocalizedAttrs" %s>%s</a>'
                    % (href_title(target) + "_(stub)", target.replace("_", " ") + " (stub)",
                       self.mwid(), esc_text(text)))
        return '<a rel="mw:WikiLink" href="./%s" title="%s" %s>%s</a>' % (
            href_title(target), target.replace("_", " "), self.mwid(), esc_text(text))

    def extlink(self, text):
        host = self.rng.choice(["www.example.org", "archive.example.net", "news.example.com",
                                "library.example.edu"])
        path = "/".join(self.phrase(2, 4).split())
        return ('<a rel="mw:ExtLink nofollow" href="https://%s/%s" class="external text" %s>%s</a>'
                % (host, path, self.mwid(), esc_text(text)))

    def convert(self):
        v = self.rng.randint(2, 900)
        unit, other, factor = self.rng.choice([("km", "mi", 0.621), ("m", "ft", 3.281),
                                               ("kg", "lb", 2.205), ("ha", "acres", 2.471)])
        about = self.about()
        dm = data_mw({"parts": [{"template": {"target": {"wt": "convert", "href": "./Template:Convert"},
                                              "params": {"1": {"wt": str(v)}, "2": {"wt": unit}},
                                              "i": 0}}]})
        return ('<span about="%s" typeof="mw:Transclusion" %s %s>%d&nbsp;%s (%.1f&nbsp;%s)</span>'
                % (about, dm, self.mwid(), v, unit, v * factor, other))

    def lang_term(self):
        about = self.about()
        word = self.phrase(1, 2)
        lang = self.rng.choice(["de", "fr", "la", "it"])
        dm = data_mw({"parts": [{"template": {"target": {"wt": "lang", "href": "./Template:Lang"},
                                              "params": {"1": {"wt": lang}, "2": {"wt": word}},
                                              "i": 0}}]})
        return ('<i about="%s" typeof="mw:Transclusion" %s %s><span lang="%s">%s</span></i>'
                % (about, dm, self.mwid(), lang, esc_text(word)))

    def inline_math(self):
        return self.math_node(self.rng.choice(TEX), display=False)

    def math_node(self, tex, display):
        about = self.about()
        wrapped = "{\\displaystyle %s}" % tex
        dm = data_mw({"name": "math", "attrs": {}, "body": {"extsrc": tex}})
        mathml = ('<math xmlns="http://www.w3.org/1998/Math/MathML" %s alttext="%s">'
                  '<semantics><mrow class="MJX-TeXAtom-ORD"><mstyle displaystyle="true" scriptlevel="0">'
                  '<mi>x</mi><mo>=</mo><mi>y</mi></mstyle></mrow>'
                  '<annotation encoding="application/x-tex">%s</annotation></semantics></math>'
                  % ('display="block"' if display else "", esc_attr_dq(wrapped), esc_text(wrapped)))
        kind = "display" if display else "inline"
        return ('<span class="mwe-math-element" about="%s" typeof="mw:Extension/math" %s %s>'
                '<span class="mwe-math-mathml-%s mwe-math-mathml-a11y" style="display: none;">%s</span>'
                '<img src="https://wikimedia.org/api/rest_v1/media/math/render/svg/%032x" '
                'class="mwe-math-fallback-image-%s mw-invert skin-invert" aria-hidden="true" '
                'style="vertical-align: -0.838ex; width:%.3fex; height:2.843ex;" alt="%s"/></span>'
                % (about, dm, self.mwid(), kind, mathml, self.rng.getrandbits(128), kind,
                   4 + self.rng.random() * 30, esc_attr_dq(wrapped)))

    def ref_marker(self):
        """One inline citation marker: a new reference most of the time, a
        reuse of an earlier named one otherwise (real articles cite a handful
        of sources over and over, which is what multi-backlinks model)."""
        reuse = self.named and (self.rng.random() < 0.22 or len(self.refs) >= self.target_refs)
        if reuse:
            ref = self.rng.choice(self.named)
        else:
            ref = self.new_ref()
        use = ref.uses
        ref.uses += 1
        attrs = {"name": ref.name} if ref.name else {}
        body = {"id": "mw-reference-text-" + ref.note_id()} if use == 0 else None
        dmo = {"name": "ref", "attrs": attrs}
        if body:
            dmo["body"] = body
        return ('<sup about="%s" class="mw-ref reference" id="%s" rel="dc:references" '
                'typeof="mw:Extension/ref" %s><a href="./%s#%s" %s><span class="mw-reflink-text" %s>'
                '<span class="cite-bracket" %s>[</span>%d<span class="cite-bracket" %s>]</span>'
                '</span></a></sup>'
                % (self.about(), ref.ref_id(use), data_mw(dmo), href_title(self.title),
                   ref.note_id(), self.mwid(), self.mwid(), self.mwid(), ref.num, self.mwid()))

    def new_ref(self):
        num = len(self.refs) + 1
        named = self.rng.random() < 0.25
        last = self.rng.choice(FAMILY)
        name = ("%s%d" % (last, self.year())) if named else None
        if name and any(r.name == name for r in self.named):
            name = "%s_%d" % (name, num)
        short = self.rng.random() < 0.2
        kind = self.rng.choice(["web", "web", "book", "book", "journal", "news"])
        ref = Ref(num, name, kind, None, short)
        self.refs.append(ref)
        if name:
            self.named.append(ref)
        return ref

    def sentence(self, ref_prob, allow_math):
        n = self.rng.randint(9, 30)
        words = []
        i = 0
        while i < n:
            r = self.rng.random()
            if r < 0.085:
                words.append(self.wikilink())
            elif r < 0.095:
                words.append("<b %s>%s</b>" % (self.mwid(), esc_text(self.phrase(1, 2))))
            elif r < 0.11:
                words.append("<i %s>%s</i>" % (self.mwid(), esc_text(self.phrase(1, 3))))
            elif r < 0.114:
                words.append(self.convert())
            elif r < 0.117:
                words.append(self.lang_term())
            elif r < 0.12:
                words.append('<span typeof="mw:Entity" %s>–</span>' % self.mwid())
            elif allow_math and r < 0.13:
                words.append(self.inline_math())
            elif r < 0.16:
                words.append(str(self.year()))
            else:
                words.append(self.word())
            i += 1
        text = " ".join(words)
        text = text[0].upper() + text[1:] if text[0].isalpha() else text
        if self.rng.random() < 0.3:
            text = text.replace(" ", ", ", 1)
        text += "."
        # Markers cluster: a claim sometimes carries two or three citations.
        while self.rng.random() < ref_prob:
            text += self.ref_marker()
            ref_prob *= 0.45
        return text

    def ref_prob(self):
        """Per-sentence citation-marker chance, re-aimed before every
        paragraph so the prose budget and the reference target run out
        together (a fixed rate over- or undershoots one of them)."""
        remaining_refs = self.target_refs - len(self.refs)
        if remaining_refs <= 0:
            # Past the target, markers only reuse named sources (see
            # `ref_marker`) — prose keeps citing what it already cited.
            return 0.08
        remaining_bytes = self.prose_budget - (self.bytes - self.prose_start)
        remaining_sentences = max(8.0, remaining_bytes / 380.0)
        return min(0.9, remaining_refs / 0.78 / remaining_sentences)

    def paragraph(self, allow_math=False, first=None):
        ref_prob = self.ref_prob()
        sentences = []
        if first:
            sentences.append(first)
        for _ in range(self.rng.randint(3, 7)):
            sentences.append(self.sentence(ref_prob, allow_math))
        return "<p %s>%s</p>\n" % (self.mwid(), " ".join(sentences))

    # -- block templates -----------------------------------------------------

    def templatestyles(self, src):
        about = self.about()
        dm = data_mw({"name": "templatestyles", "attrs": {"src": src}, "body": {"extsrc": ""}})
        if not self.css_inlined:
            self.css_inlined = True
            return ('<style data-mw-deduplicate="TemplateStyles:r1238218222" typeof="mw:Extension/templatestyles" '
                    'about="%s" %s %s>%s</style>' % (about, dm, self.mwid(), CS1_CSS))
        return ('<link rel="mw-deduplicated-inline-style" href="mw-data:TemplateStyles:r1238218222" '
                'about="%s" typeof="mw:Extension/templatestyles" %s %s/>' % (about, dm, self.mwid()))

    def hatnote(self):
        about = self.about()
        target = corpus_title(self.rng.randrange(self.corpus_size))
        dm = data_mw({"parts": [{"template": {"target": {"wt": "Main", "href": "./Template:Main"},
                                              "params": {"1": {"wt": target.replace("_", " ")}},
                                              "i": 0}}]})
        return ('<div role="note" class="hatnote navigation-not-searchable" about="%s" typeof="mw:Transclusion" '
                '%s %s>Main article: %s</div>\n'
                % (about, dm, self.mwid(), self.wikilink(target.replace("_", " "), target)))

    def infobox(self):
        about = self.about()
        rows = []
        params = {}
        labels = ["Born", "Died", "Nationality", "Alma mater", "Known for", "Spouse", "Awards",
                  "Fields", "Institutions", "Thesis", "Doctoral advisor", "Doctoral students",
                  "Influences", "Influenced", "Signature", "Website", "Era", "Region",
                  "Notable works", "Parents"]
        for label in labels:
            key = label.lower().replace(" ", "_")
            val = self.phrase(2, 6)
            params[key] = {"wt": "[[%s]]" % val}
            cell = ", ".join(self.wikilink(self.phrase(1, 3)) for _ in range(self.rng.randint(1, 3)))
            rows.append('<tr %s><th scope="row" class="infobox-label" %s>%s</th>'
                        '<td class="infobox-data" %s>%s</td></tr>'
                        % (self.mwid(), self.mwid(), label, self.mwid(), cell))
        dm = data_mw({"parts": [{"template": {"target": {"wt": "Infobox person\n",
                                                         "href": "./Template:Infobox_person"},
                                              "params": params, "i": 0}}]})
        img = self.image_tag(220, "infobox")
        return ('<table class="infobox biography vcard" about="%s" typeof="mw:Transclusion" %s %s><tbody>'
                '<tr %s><th colspan="2" class="infobox-above" %s><div class="fn" %s>%s</div></th></tr>'
                '<tr %s><td colspan="2" class="infobox-image" %s>%s<div class="infobox-caption" %s>%s</div></td></tr>'
                '%s</tbody></table>\n'
                % (about, dm, self.mwid(), self.mwid(), self.mwid(), self.mwid(), esc_text(self.display),
                   self.mwid(), self.mwid(), img, self.mwid(), esc_text(self.phrase(3, 6)),
                   "".join(rows)))

    def image_tag(self, width, stem):
        name = "%s_%s_%d.jpg" % (stem.capitalize(), self.rng.choice(NOUNS).capitalize(),
                                 self.rng.randint(1000, 9999))
        h = self.rng.getrandbits(16)
        d1 = "%x" % (h & 0xF)
        d2 = "%02x" % (h & 0xFF)
        base = "//upload.wikimedia.org/wikipedia/commons/thumb/%s/%s/%s" % (d1, d2, name)
        height = int(width * (0.6 + self.rng.random() * 0.8))
        return ('<a href="./File:%s" class="mw-file-description" %s><img alt="%s" resource="./File:%s" '
                'src="%s/%dpx-%s" decoding="async" data-file-width="%d" data-file-height="%d" '
                'data-file-type="bitmap" height="%d" width="%d" srcset="%s/%dpx-%s 1.5x, %s/%dpx-%s 2x" '
                'class="mw-file-element" %s/></a>'
                % (name, self.mwid(), esc_attr_dq(self.phrase(2, 5)), name, base, width, name,
                   width * 6, height * 6, height, width, base, int(width * 1.5), name, base,
                   width * 2, name, self.mwid()))

    def figure(self):
        return ('<figure class="mw-default-size" typeof="mw:File/Thumb" %s>%s<figcaption %s>%s %s.</figcaption>'
                '</figure>\n'
                % (self.mwid(), self.image_tag(250, "figure"), self.mwid(),
                   esc_text(self.phrase(3, 7).capitalize()), self.wikilink()))

    def display_math(self):
        return "<dl %s><dd %s>%s</dd></dl>\n" % (self.mwid(), self.mwid(),
                                                 self.math_node(self.rng.choice(TEX), display=True))

    def wikitable(self):
        cols = self.rng.randint(4, 7)
        rows = self.rng.randint(12, 36)
        out = ['<table class="wikitable sortable" %s><caption %s>%s</caption><tbody>'
               % (self.mwid(), self.mwid(), esc_text(self.phrase(3, 6).capitalize()))]
        # Two header rows: a rowspan'd first/last column around a colspan'd group.
        out.append('<tr %s><th rowspan="2" %s>Year</th><th colspan="%d" %s>%s</th><th rowspan="2" %s>Notes</th></tr>'
                   % (self.mwid(), self.mwid(), cols - 2, self.mwid(),
                      esc_text(self.phrase(1, 3).capitalize()), self.mwid()))
        out.append("<tr %s>%s</tr>" % (self.mwid(), "".join(
            "<th %s>%s</th>" % (self.mwid(), esc_text(self.phrase(1, 2).capitalize()))
            for _ in range(cols - 2))))
        pending_rowspan = 0
        for _ in range(rows):
            cells = []
            if pending_rowspan > 0:
                pending_rowspan -= 1
            else:
                span = self.rng.choice([1, 1, 1, 2, 3])
                attr = ' rowspan="%d"' % span if span > 1 else ""
                cells.append("<td%s %s>%d</td>" % (attr, self.mwid(), self.year()))
                pending_rowspan = span - 1
            c = 0
            while c < cols - 2:
                if self.rng.random() < 0.08 and c < cols - 3:
                    cells.append('<td colspan="2" %s>%s</td>' % (self.mwid(), self.wikilink()))
                    c += 2
                else:
                    r = self.rng.random()
                    if r < 0.3:
                        content = self.wikilink()
                    elif r < 0.6:
                        content = "%d" % self.rng.randint(1, 99999)
                    else:
                        content = esc_text(self.phrase(1, 4))
                    cells.append("<td %s>%s</td>" % (self.mwid(), content))
                    c += 1
            notes = self.ref_marker() if self.rng.random() < 0.25 else ""
            cells.append("<td %s>%s%s</td>" % (self.mwid(), esc_text(self.phrase(0, 3)), notes))
            out.append("<tr %s>%s</tr>" % (self.mwid(), "".join(cells)))
        out.append("</tbody></table>\n")
        return "".join(out)

    def bullet_list(self, n, ref_prob):
        items = []
        for _ in range(n):
            body = "%s (%d) %s" % (self.wikilink(), self.year(), esc_text(self.phrase(2, 8)))
            if self.rng.random() < ref_prob:
                body += self.ref_marker()
            items.append("<li %s>%s</li>" % (self.mwid(), body))
        return "<ul %s>%s</ul>\n" % (self.mwid(), "".join(items))

    def blockquote(self):
        about = self.about()
        g, f = self.person()
        dm = data_mw({"parts": [{"template": {"target": {"wt": "Blockquote", "href": "./Template:Blockquote"},
                                              "params": {"text": {"wt": "…"}, "author": {"wt": "%s %s" % (g, f)}},
                                              "i": 0}}]})
        return ('<blockquote class="templatequote" about="%s" typeof="mw:Transclusion" %s %s><p %s>%s</p>'
                '<div class="templatequotecite" %s>— <cite %s>%s %s</cite></div></blockquote>\n'
                % (about, dm, self.mwid(), self.mwid(), self.sentence(0, False) + " " + self.sentence(0, False),
                   self.mwid(), self.mwid(), g, f))

    def navbox(self):
        about = self.about()
        title = self.phrase(2, 4).title()
        dm = data_mw({"parts": [{"template": {"target": {"wt": "Navbox", "href": "./Template:Navbox"},
                                              "params": {"name": {"wt": title}, "title": {"wt": title}},
                                              "i": 0}}]})
        groups = []
        for _ in range(self.rng.randint(5, 9)):
            links = "".join("<li %s>%s</li>" % (self.mwid(), self.wikilink())
                            for _ in range(self.rng.randint(6, 16)))
            groups.append('<tr %s><th scope="row" class="navbox-group" style="width:1%%" %s>%s</th>'
                          '<td class="navbox-list-with-group navbox-list navbox-odd hlist" '
                          'style="width:100%%;padding:0" %s><div style="padding:0 0.25em" %s><ul %s>%s</ul></div></td></tr>'
                          % (self.mwid(), self.mwid(), esc_text(self.phrase(1, 3).capitalize()), self.mwid(),
                             self.mwid(), self.mwid(), links))
        return ('<div role="navigation" class="navbox" aria-label="Navbox" style="padding:3px" about="%s" '
                'typeof="mw:Transclusion" %s %s><table class="nowraplinks mw-collapsible autocollapse navbox-inner" '
                'style="border-spacing:0;background:transparent;color:inherit" %s><tbody><tr %s>'
                '<th scope="col" class="navbox-title" colspan="2" %s>%s</th></tr>%s</tbody></table></div>\n'
                % (about, dm, self.mwid(), self.mwid(), self.mwid(), self.mwid(), esc_text(title),
                   "".join(groups)))

    # -- references ----------------------------------------------------------

    def cite(self, ref):
        g, f = self.person()
        year = self.year()
        about = self.about()
        title = self.phrase(3, 9).capitalize()
        if ref.short:
            target = "CITEREF%s%d" % (f, year)
            return ('<a href="./%s#%s" %s>%s %d</a>, p.&nbsp;%d.'
                    % (href_title(self.title), target, self.mwid(), f, year, self.rng.randint(1, 600)))
        params = {"last": {"wt": f}, "first": {"wt": g}, "title": {"wt": title}}
        if ref.kind == "book":
            pub = self.rng.choice(PUBLISHERS)
            isbn = "978-%d-%02d-%06d-%d" % (self.rng.randint(0, 1), self.rng.randint(0, 99),
                                             self.rng.randint(0, 999999), self.rng.randint(0, 9))
            params.update({"year": {"wt": str(year)}, "publisher": {"wt": pub},
                           "location": {"wt": "London"}, "isbn": {"wt": isbn},
                           "page": {"wt": str(self.rng.randint(1, 800))}})
            rendered = ('%s, %s (%d). <i %s>%s</i>. London: %s. p.&nbsp;%s. '
                        '<a rel="mw:WikiLink" href="./ISBN_(identifier)" title="ISBN (identifier)" %s>ISBN</a>&nbsp;'
                        '<a rel="mw:WikiLink" href="./Special:BookSources/%s" title="Special:BookSources/%s" %s>'
                        '<bdi %s>%s</bdi></a>.'
                        % (f, g, year, self.mwid(), esc_text(title), esc_text(pub), params["page"]["wt"],
                           self.mwid(), isbn, isbn, self.mwid(), self.mwid(), isbn))
            rft = "rft_val_fmt=info%%3Aofi%%2Ffmt%%3Akev%%3Amtx%%3Abook&amp;rft.genre=book&amp;rft.btitle=%s&amp;rft.pub=%s&amp;rft.isbn=%s" % (
                title.replace(" ", "+"), pub.replace(" ", "+"), isbn)
        elif ref.kind == "journal":
            journal = self.rng.choice(JOURNALS)
            doi = "10.%d/%s.%d" % (self.rng.randint(1000, 9999), self.rng.choice(NOUNS), self.rng.randint(1, 99999))
            params.update({"journal": {"wt": journal}, "volume": {"wt": str(self.rng.randint(1, 300))},
                           "issue": {"wt": str(self.rng.randint(1, 12))}, "year": {"wt": str(year)},
                           "pages": {"wt": "%d–%d" % (self.rng.randint(1, 400), self.rng.randint(401, 900))},
                           "doi": {"wt": doi}})
            rendered = ('%s, %s (%d). "%s". <i %s>%s</i>. <b %s>%s</b> (%s): %s. '
                        '<a rel="mw:WikiLink" href="./Doi_(identifier)" title="Doi (identifier)" %s>doi</a>:'
                        '<a rel="mw:ExtLink nofollow" href="https://doi.org/%s" class="external text" %s>%s</a>.'
                        % (f, g, year, esc_text(title), self.mwid(), esc_text(journal), self.mwid(),
                           params["volume"]["wt"], params["issue"]["wt"], params["pages"]["wt"],
                           self.mwid(), doi, self.mwid(), doi))
            rft = "rft_val_fmt=info%%3Aofi%%2Ffmt%%3Akev%%3Amtx%%3Ajournal&amp;rft.genre=article&amp;rft.jtitle=%s&amp;rft.atitle=%s&amp;rft_id=info%%3Adoi%%2F%s" % (
                journal.replace(" ", "+"), title.replace(" ", "+"), doi)
        else:
            site = self.rng.choice(WEBSITES)
            url = "https://www.example.org/%s/%d/%s" % (site.lower().replace(" ", "-"), year,
                                                        "-".join(title.lower().split()))
            access = self.iso_date()
            params.update({"url": {"wt": url}, "website" if ref.kind == "web" else "work": {"wt": site},
                           "date": {"wt": self.date()}, "access-date": {"wt": access}})
            archived = ""
            if self.rng.random() < 0.4:
                arch = "https://web.archive.org/web/%s000000/%s" % (access.replace("-", ""), url)
                params.update({"archive-url": {"wt": arch}, "archive-date": {"wt": access},
                               "url-status": {"wt": "live"}})
                archived = (' <a rel="mw:ExtLink nofollow" href="%s" class="external text" %s>Archived</a> '
                            'from the original on %s.' % (arch, self.mwid(), access))
            rendered = ('%s, %s (%s). <a rel="mw:ExtLink nofollow" href="%s" class="external text" %s>"%s"</a>. '
                        '<i %s>%s</i>.%s Retrieved <span class="nowrap" %s>%s</span>.'
                        % (f, g, params["date"]["wt"], url, self.mwid(), esc_text(title), self.mwid(),
                           esc_text(site), archived, self.mwid(), access))
            rft = "rft_val_fmt=info%%3Aofi%%2Ffmt%%3Akev%%3Amtx%%3Ajournal&amp;rft.genre=unknown&amp;rft.jtitle=%s&amp;rft.atitle=%s&amp;rft_id=%s" % (
                site.replace(" ", "+"), title.replace(" ", "+"), url.replace(":", "%3A").replace("/", "%2F"))
        tmpl = {"web": "cite web", "book": "cite book", "journal": "cite journal", "news": "cite news"}[ref.kind]
        dm = data_mw({"parts": [{"template": {"target": {"wt": tmpl, "href": "./Template:" + tmpl.capitalize().replace(" ", "_")},
                                              "params": params, "i": 0}}]})
        coins = ('<span title="ctx_ver=Z39.88-2004&amp;%s&amp;rft.date=%d&amp;rft.aulast=%s&amp;rft.aufirst=%s'
                 '&amp;rfr_id=info%%3Asid%%2Fen.wikipedia.org%%3A%s" class="Z3988" about="%s" %s></span>'
                 % (rft, year, f, g, href_title(self.title), about, self.mwid()))
        return ('%s<cite id="CITEREF%s%d" class="citation %s cs1" about="%s" typeof="mw:Transclusion" %s>%s</cite>%s'
                % (self.templatestyles("Module:Citation/CS1/styles.css"), f, year, ref.kind, about, dm,
                   rendered, coins))

    def references(self):
        about = self.about()
        dm = data_mw({"parts": [{"template": {"target": {"wt": "Reflist", "href": "./Template:Reflist"},
                                              "params": {"colwidth": {"wt": "30em"}}, "i": 0}}]})
        items = []
        page = href_title(self.title)
        for ref in self.refs:
            nid = ref.note_id()
            if ref.uses <= 1:
                back = ('<span class="mw-cite-backlink" %s><a href="./%s#%s" rel="mw:referencedBy" %s>'
                        '<span class="mw-linkback-text" %s>↑ </span></a></span>'
                        % (self.mwid(), page, ref.ref_id(0), self.mwid(), self.mwid()))
            else:
                links = "".join('<a href="./%s#%s" %s><span class="mw-linkback-text" %s>%d </span></a>'
                                % (page, ref.ref_id(u), self.mwid(), self.mwid(), u + 1)
                                for u in range(ref.uses))
                back = ('<span class="mw-cite-backlink" %s><span rel="mw:referencedBy" %s>%s</span></span>'
                        % (self.mwid(), self.mwid(), links))
            items.append('<li about="#%s" id="%s">%s <span id="mw-reference-text-%s" '
                         'class="mw-reference-text reference-text">%s</span></li>'
                         % (nid, nid, back, nid, self.cite(ref)))
        return ('<div class="reflist reflist-columns references-column-width" style="column-width: 30em;" '
                'about="%s" typeof="mw:Transclusion" %s %s><div class="mw-references-wrap mw-references-columns" '
                'typeof="mw:Extension/references" about="%s" %s %s><ol class="mw-references references" %s>%s'
                '</ol></div></div>\n'
                % (about, dm, self.mwid(), self.about(),
                   data_mw({"name": "references", "attrs": {"group": ""}, "body": {"html": ""}}),
                   self.mwid(), self.mwid(), "".join(items)))

    # -- document ------------------------------------------------------------

    def section_open(self, level, heading):
        self.section_id += 1
        anchor = heading.replace(" ", "_")
        return ('<section data-mw-section-id="%d" %s><div class="mw-heading mw-heading%d" %s>'
                '<h%d id="%s">%s</h%d></div>\n'
                % (self.section_id, self.mwid(), level, self.mwid(), level, esc_attr_dq(anchor),
                   esc_text(heading), level))

    def render(self):
        rng = self.rng
        # References dominate a real long article's bytes; budget prose so
        # the reference list, navboxes, tables and infobox land the total near
        # target. Measured averages in this markup: a reference (list entry
        # with its CS1 cite + ~1.3 inline markers) ~2.1 KB, a navbox ~12 KB, a
        # wikitable ~9.4 KB, the infobox ~8.7 KB.
        fixed = (self.target_refs * 2100 + self.n_navboxes * 12_000 + self.n_tables * 9_400
                 + 8_700 + 6_000)
        self.prose_budget = max(20_000, self.target_bytes - fixed)
        self.emit('<!DOCTYPE html>\n<html prefix="dc: http://purl.org/dc/terms/ mw: http://mediawiki.org/rdf/" '
                  'about="https://en.wikipedia.org/wiki/Special:Redirect/revision/%d"><head prefix="mwr: '
                  'https://en.wikipedia.org/wiki/Special:Redirect/"><meta charset="utf-8"/>'
                  '<meta property="mw:pageId" content="%d"/><meta property="mw:pageNamespace" content="0"/>'
                  '<link rel="dc:replaces" resource="mwr:revision/%d"/><meta property="mw:revisionSHA1" '
                  'content="%040x"/><meta property="dc:modified" content="2026-06-01T12:00:00.000Z"/>'
                  '<meta property="mw:htmlVersion" content="2.8.0"/><meta property="mw:html:version" content="2.8.0"/>'
                  '<link rel="dc:isVersionOf" href="//en.wikipedia.org/wiki/%s"/><base href="//en.wikipedia.org/wiki/"/>'
                  '<title>%s</title><link rel="stylesheet" href="/w/load.php?lang=en&amp;modules=mediawiki.skinning.'
                  'content.parsoid%%7Cmediawiki.skinning.interface%%7Csite.styles&amp;only=styles&amp;skin=vector"/>'
                  '<meta http-equiv="content-language" content="en"/><meta http-equiv="vary" content="Accept"/></head>'
                  '<body id="mwAA" lang="en" class="mw-content-ltr sitedir-ltr ltr mw-body-content parsoid-body '
                  'mediawiki mw-parser-output" dir="ltr" data-mw-parsoid-version="0.21.0.0-alpha3" '
                  'data-mw-html-version="2.8.0">\n'
                  % (rng.randint(10**9, 2 * 10**9), rng.randint(10**5, 10**7), rng.randint(10**9, 2 * 10**9),
                     rng.getrandbits(160), href_title(self.title), esc_text(self.display)))
        self.next_id = 1
        self.prose_start = self.bytes
        # Lead section: short description, hatnote, infobox, lead paragraphs.
        self.emit('<section data-mw-section-id="0" %s>' % self.mwid())
        about = self.about()
        sd = self.phrase(3, 6)
        self.emit('<div class="shortdescription nomobile noexcerpt noprint searchaux" style="display:none" '
                  'about="%s" typeof="mw:Transclusion" %s %s>%s</div><link rel="mw:PageProp/Category" '
                  'href="./Category:Articles_with_short_description" about="%s" %s/>\n'
                  % (about, data_mw({"parts": [{"template": {"target": {"wt": "Short description",
                                                                        "href": "./Template:Short_description"},
                                                             "params": {"1": {"wt": sd}}, "i": 0}}]}),
                     self.mwid(), esc_text(sd), about, self.mwid()))
        self.emit(self.hatnote())
        self.emit(self.infobox())
        first = ('<b %s>%s</b> (catalogue mark %s; born %s) is a %s %s.'
                 % (self.mwid(), esc_text(self.display), lead_marker(self.title), self.date(),
                    esc_text(self.phrase(2, 4)), self.wikilink()))
        self.emit(self.paragraph(first=first))
        for _ in range(rng.randint(2, 3)):
            self.emit(self.paragraph())
        self.emit("</section>")

        figures_left = self.n_figures
        tables_left = self.n_tables
        math_left = self.n_math
        sec = 0
        while self.bytes - self.prose_start < self.prose_budget:
            sec += 1
            heading = self.phrase(1, 4).capitalize()
            self.emit(self.section_open(2, heading))
            if rng.random() < 0.3:
                self.emit(self.hatnote())
            for sub in range(rng.choice([0, 0, 1, 2, 3])):
                self.emit(self.section_open(3, self.phrase(1, 3).capitalize()))
                for _ in range(rng.randint(2, 4)):
                    self.emit(self.paragraph(allow_math=math_left > 0))
                if math_left > 0 and rng.random() < 0.5:
                    self.emit(self.display_math())
                    math_left -= 1
                self.emit("</section>")
            for _ in range(rng.randint(2, 5)):
                self.emit(self.paragraph(allow_math=math_left > 0))
            if figures_left > 0 and rng.random() < 0.5:
                self.emit(self.figure())
                figures_left -= 1
            if tables_left > 0 and rng.random() < 0.35:
                self.emit(self.wikitable())
                tables_left -= 1
            if rng.random() < 0.12:
                self.emit(self.blockquote())
            if rng.random() < 0.15:
                self.emit(self.bullet_list(rng.randint(4, 12), self.ref_prob() * 0.5))
            if math_left > 0 and rng.random() < 0.3:
                self.emit(self.display_math())
                math_left -= 1
            self.emit("</section>\n")
        # Leftover quotas land in one trailing section rather than vanishing.
        if figures_left or tables_left or math_left:
            self.emit(self.section_open(2, "Further details"))
            for _ in range(figures_left):
                self.emit(self.figure())
            for _ in range(tables_left):
                self.emit(self.wikitable())
            for _ in range(math_left):
                self.emit(self.display_math())
            self.emit(self.paragraph())
            self.emit("</section>\n")
        # Top up to the reference target if the prose ran out first: real
        # reference-heavy articles have list-shaped "Works"/"Timeline"
        # sections dense with citations.
        if len(self.refs) < self.target_refs:
            self.emit(self.section_open(2, "Selected works"))
            while len(self.refs) < self.target_refs:
                self.emit(self.bullet_list(10, 0.95))
            self.emit("</section>\n")
        self.emit(self.section_open(2, "See also"))
        self.emit(self.bullet_list(rng.randint(4, 8), 0.0))
        self.emit('<p %s>Closing note %s: %s</p>\n' % (self.mwid(), tail_marker(self.title),
                                                       esc_text(self.phrase(4, 8))))
        self.emit("</section>\n")
        self.emit(self.section_open(2, "References"))
        self.emit(self.references())
        self.emit("</section>\n")
        self.emit(self.section_open(2, "External links"))
        items = "".join("<li %s>%s</li>" % (self.mwid(), self.extlink(self.phrase(2, 5).capitalize()))
                        for _ in range(rng.randint(3, 8)))
        self.emit("<ul %s>%s</ul>\n" % (self.mwid(), items))
        for _ in range(self.n_navboxes):
            self.emit(self.navbox())
        self.emit("</section>\n")
        for _ in range(rng.randint(12, 30)):
            self.emit('<link rel="mw:PageProp/Category" href="./Category:%s" %s/>'
                      % (href_title(self.phrase(2, 4).capitalize()), self.mwid()))
        self.emit("\n</body></html>\n")
        return "".join(self.out)


def pathological():
    """The §6.8 row: ~1.55 MB, 500+ references, one article."""
    return Article("Perf_Pathological", PATHOLOGICAL_SEED, target_bytes=1_640_000,
                   target_refs=520, navboxes=4, tables=6, figures=14, math=10).render()


def typical():
    """A ~150 KB article: the common size class."""
    return Article("Perf_Typical", TYPICAL_SEED, target_bytes=155_000, target_refs=40,
                   navboxes=1, tables=1, figures=2, math=0).render()


def corpus_size_class(index):
    """The (label, target_bytes, target_refs) of `Perf_Corpus_<index>`, drawn
    from SIZE_CLASSES' weights with a per-index seed."""
    r = random.Random(CORPUS_SEED ^ (index * 2654435761 & 0xFFFFFFFF)).random()
    acc = 0.0
    for label, weight, size, refs in SIZE_CLASSES:
        acc += weight
        if r < acc:
            return label, size, refs
    label, _, size, refs = SIZE_CLASSES[-1]
    return label, size, refs


def corpus_article(index):
    """`Perf_Corpus_<index>`: seeded, cross-linked into the same corpus."""
    label, size, refs = corpus_size_class(index)
    big = size >= 400_000
    return Article(corpus_title(index), CORPUS_SEED + index, target_bytes=size, target_refs=refs,
                   navboxes=3 if big else 1, tables=3 if big else 1, figures=6 if big else 2,
                   math=2 if big else 0).render()


# `Perf_Large_<n>`: ten distinct pathological-size articles (PRD §6.8's
# "10 tabs" memory row at its worst case: every tab ~1.55 MB, 520 refs).
LARGE_COUNT = 10
LARGE_SEED = 0x0608_1A26


def large_title(n):
    return "Perf_Large_%d" % n


def large_article(n):
    """`Perf_Large_<n>`: pathological-shaped, distinct per index, linking
    into the same corpus as everything else."""
    return Article(large_title(n), LARGE_SEED + n, target_bytes=1_640_000, target_refs=520,
                   navboxes=4, tables=6, figures=14, math=10).render()


def describe(html):
    return {
        "bytes": len(html.encode("utf-8")),
        "references": html.count('<li about="#cite_note-'),
        "ref_markers": html.count('typeof="mw:Extension/ref"'),
        "transclusions": html.count('typeof="mw:Transclusion"'),
        "data_mw_attrs": html.count("data-mw='"),
        "sections": html.count("<section data-mw-section-id="),
        "tables": html.count('<table class="wikitable'),
        "rowspans": html.count("rowspan="),
        "colspans": html.count("colspan="),
        "math_nodes": html.count('typeof="mw:Extension/math"'),
        "figures": html.count('typeof="mw:File/Thumb"'),
        "navboxes": html.count('class="navbox"'),
        "wikilinks": html.count('rel="mw:WikiLink"'),
        "elements_with_mw_id": html.count(' id="mw'),
    }


FIXTURES = {"pathological.html": pathological, "typical.html": typical}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--check", action="store_true",
                    help="verify the committed fixtures match this generator (exit 1 if not)")
    ap.add_argument("--describe", action="store_true", help="print each fixture's shape as JSON")
    args = ap.parse_args()
    stale = []
    for name, build in FIXTURES.items():
        html = build()
        path = HERE / name
        if args.describe:
            print(name, json.dumps(describe(html)))
        if args.check:
            if not path.exists() or path.read_bytes() != html.encode("utf-8"):
                stale.append(name)
        elif not args.describe:
            path.write_bytes(html.encode("utf-8"))
    if stale:
        print("stale perf fixtures (re-run tests/perf/fixtures/generate.py): " + ", ".join(stale),
              file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
