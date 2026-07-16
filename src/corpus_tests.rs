//! PRD §9 renderer test corpus: fixtures and tests proving the
//! `doc::parse_article_html` → `layout::layout_document` pipeline (and its
//! SEC-1/SEC-3 hardening) holds up across the specific corpus §9 names by
//! title — a Featured Article, a 1.5 MB+ page, 500+ references, deeply
//! nested rowspan/colspan tables, math-heavy content, CJK (ja/zh), RTL
//! (ar/he), IPA/diacritics, galleries, disambiguation, redirects, and stubs —
//! plus the width/grapheme property sweep and the SEC-1 sanitizer property
//! corpus §9 separately calls for.
//!
//! **Why its own file, not folded into `doc.rs`'s or `layout.rs`'s existing
//! `mod tests`**: this crate is a binary with no library target (see
//! `main.rs`'s flat `mod` list), so a `tests/*.rs` integration file cannot
//! link against `doc`/`layout`/`sanitize` at all — colocation is the only
//! option available, the same constraint every other module's own `mod
//! tests` already lives under. Almost every test here is inherently
//! cross-module (parse with `doc::parse_article_html`, lay out with
//! `layout::layout_document`, assert on both, sometimes also on
//! `bidi::reorder_laid_lines`) — nesting it inside one module's own test mod
//! would either duplicate every fixture into the other module too or force
//! one module's tests to depend on the other's private test-only fixtures,
//! which Rust's privacy rules don't allow across sibling modules anyway. So
//! this file uses only the `pub` surface of `doc`/`layout`/`sanitize`/`bidi`,
//! exactly as an external caller (`app.rs`, `ui.rs`) would.
//!
//! **Snapshot strategy** (PRD §9: "recorded Parsoid-HTML fixtures → document
//! model → laid-out text snapshots"): exact string snapshots for the
//! genuinely small, fully hand-authored fixtures (the stub, the
//! disambiguation page, the redirect page) where a byte-for-byte assertion
//! stays readable in a diff and a real regression is easy to spot;
//! **structural/property** assertions (block-kind counts and shapes,
//! expected headings/links/citations/table geometry present, no-overflow,
//! grapheme-safety) for everything larger. A full-text snapshot of a
//! multi-section featured article or the 1.5 MB pathological page would
//! either be unreadable to review or so brittle that an unrelated spacing
//! tweak elsewhere in `layout.rs` breaks a dozen unrelated corpus tests —
//! exactly the fragile-snapshot failure mode PRD §9 does not ask for.
//!
//! **Mock server decision**: none of this needs it, and nothing was added to
//! it. Every test here parses a hand-built HTML string directly via
//! `doc::parse_article_html`, exactly like the existing `doc.rs`/`layout.rs`
//! fixtures — there is no pty/interactive surface being exercised, so a live
//! mock endpoint would add process overhead for zero additional coverage.
//! `tests/mock-server/server.py` already carries a CJK (ja) and an RTL (ar)
//! fixture for pty verification of those two specific features (FR-RD-10,
//! FR-ML-7); this module's zh/he additions are deliberately kept as pure
//! test consts rather than new mock endpoints for the same reason — nothing
//! here drives the interactive app at all.

use crate::bidi::{self, Direction};
use crate::doc::{
    self, Block, Document, SpanStyle, collect_links, parse_article_html, render_plain,
};
use crate::layout::{LaidLine, LayoutOptions, layout_document};
use std::sync::OnceLock;
use unicode_segmentation::UnicodeSegmentation;

// ===========================================================================
// Fixtures (PRD §9's named corpus members)
// ===========================================================================

mod fixtures {
    /// **Featured Article** member: a realistic multi-section article — lead,
    /// infobox, an image, several `h2`/`h3` sections, a blockquote, a list,
    /// internal and external links, and ten references — the "happy path at
    /// scale" PRD §9 asks for. Original prose (not copied from any real
    /// Wikipedia article), but shaped exactly like real Parsoid output for
    /// every structure it exercises.
    pub const FEATURED_ARTICLE_HTML: &str = r##"
    <html><head><title>Ada Lovelace</title></head><body>
    <table class="infobox"><tbody>
      <tr><th colspan="2">Ada Lovelace</th></tr>
      <tr><th>Born</th><td>10 December 1815, London, England</td></tr>
      <tr><th>Died</th><td>27 November 1852 (aged 36)</td></tr>
      <tr><th>Known for</th><td>Notes on the <a href="./Analytical_Engine">Analytical Engine</a></td></tr>
      <tr><th>Fields</th><td>Mathematics, computing</td></tr>
    </tbody></table>
    <p><b>Augusta Ada King, Countess of Lovelace</b> (born <b>Ada Byron</b>; 10 December 1815
    – 27 November 1852) was an English mathematician chiefly known for her work on Charles
    Babbage's proposed mechanical general-purpose computer, the
    <a href="./Analytical_Engine">Analytical Engine</a>. She was the first to recognise that the
    machine had applications beyond pure calculation, and published what is often considered the
    first algorithm intended to be carried out by such a machine<sup class="reference">
    <a href="#cite_note-1">[1]</a></sup>. As a result, she is often regarded as one of the first
    computer programmers.</p>
    <figure typeof="mw:File"><img src="https://ex.org/ada.jpg" alt="Portrait of Ada Lovelace"/>
    <figcaption>Portrait, c. 1840</figcaption></figure>
    <h2>Early life</h2>
    <p>Ada Lovelace was the only legitimate child of the poet <a href="./Lord_Byron">Lord Byron</a>
    and his wife Annabella Milbanke. Byron separated from his wife a month after Ada was born and
    left England forever four months later, dying in Greece when Ada was eight years
    old<sup class="reference"><a href="#cite_note-2">[2]</a></sup>. Ada's mother remained bitter
    towards Byron and promoted Ada's interest in mathematics and logic in an effort to prevent her
    from developing what she saw as her father's insanity.</p>
    <h2>Collaboration with Babbage</h2>
    <p>In her twenties, Ada corresponded with several scientists, including the astronomer
    <a href="./Mary_Somerville">Mary Somerville</a>, who introduced her to
    <a href="./Charles_Babbage">Charles Babbage</a> in 1833. Babbage was impressed by Ada's
    intellect and analytical skills; he called her "The Enchantress of Numbers"<sup class="reference">
    <a href="#cite_note-3">[3]</a></sup>. Between 1842 and 1843, Ada translated an article by the
    Italian military engineer Luigi Menabrea on the Analytical Engine, supplementing it with an
    extensive set of notes, simply called <i>Notes</i>.</p>
    <blockquote><p>"That brain of mine is something more than merely mortal, as time will show."</p></blockquote>
    <h3>The first algorithm</h3>
    <p>Ada's notes included what is recognised as the first algorithm intended to be carried out
    by a machine. Her ideas about the machine's potential went beyond Babbage's own, envisioning
    applications well outside pure number-crunching, including the composition of music.</p>
    <h2>Legacy</h2>
    <p>Ada Lovelace died of uterine cancer in 1852 at the age of 36. Her contributions were
    largely forgotten for a century, until Alan Turing referenced her work in his own 1950 paper
    on machine intelligence<sup class="reference"><a href="#cite_note-4">[4]</a></sup>. Today, the
    Ada programming language is named in her honour, and Ada Lovelace Day is celebrated every
    October to highlight the achievements of women in science and technology.</p>
    <ul>
      <li>1815: Born in London</li>
      <li>1833: Meets Charles Babbage</li>
      <li>1843: Publishes translation and <i>Notes</i> on the Analytical Engine</li>
      <li>1852: Dies in London</li>
    </ul>
    <h2>References</h2>
    <div class="mw-references-wrap"><ol class="references">
      <li id="cite_note-1"><span class="reference-text">Smith, J. "Ada Lovelace and the
        Analytical Engine." Journal of Computing History, 1998.
        <a href="https://example.org/refs/1">https://example.org/refs/1</a></span></li>
      <li id="cite_note-2"><span class="reference-text">Jones, R. Byron: A Life. Example Press,
        2001.</span></li>
      <li id="cite_note-3"><span class="reference-text">Babbage, C. Passages from the Life of a
        Philosopher. 1864. <a href="https://example.org/refs/3">https://example.org/refs/3</a></span></li>
      <li id="cite_note-4"><span class="reference-text">Turing, A. "Computing Machinery and
        Intelligence." Mind, 1950.</span></li>
      <li id="cite_note-5"><span class="reference-text">Toole, B. Ada, the Enchantress of
        Numbers. Example Press, 1992.
        <a href="https://example.org/refs/5">https://example.org/refs/5</a></span></li>
      <li id="cite_note-6"><span class="reference-text">Menabrea, L. "Sketch of the Analytical
        Engine." 1842.</span></li>
      <li id="cite_note-7"><span class="reference-text">Fuegi, J.; Francis, J. "Lovelace &amp;
        Babbage and the creation of the 1843 'notes'." IEEE Annals, 2003.
        <a href="https://example.org/refs/7">https://example.org/refs/7</a></span></li>
      <li id="cite_note-8"><span class="reference-text">Stein, D. Ada: A Life and a Legacy. MIT
        Press, 1985.</span></li>
      <li id="cite_note-9"><span class="reference-text">Woolley, B. The Bride of Science. Example
        Press, 1999. <a href="https://example.org/refs/9">https://example.org/refs/9</a></span></li>
      <li id="cite_note-10"><span class="reference-text">Isaacson, W. The Innovators. Example
        Press, 2014.</span></li>
    </ol></div>
    </body></html>
    "##;

    /// **1.5 MB+ page** member (also the shared generator for the SEC-3 10 MB
    /// cap boundary cases): repeats a themed section/paragraph pattern until
    /// the HTML reaches at least `target_bytes`. "Large" and "just under/over
    /// the SEC-3 cap" are the same shape of content at different scale, not
    /// two unrelated fixtures — every section gets a unique heading and a
    /// unique link target so `section_outline`/`collect_links` scale with the
    /// content instead of collapsing to duplicates.
    pub fn large_article_html(target_bytes: usize) -> String {
        let mut html = String::from("<html><head><title>Large Article</title></head><body>");
        let mut i: usize = 0;
        while html.len() < target_bytes {
            html.push_str(&format!(
                "<h2>Section {i}</h2><p>This is paragraph {i} of a deliberately large \
                 pathological-size test article. It contains a \
                 <a href=\"./Topic_{i}\">link to topic {i}</a> and enough repeated prose to reach \
                 a substantial byte size without being degenerate single-token text: the quick \
                 brown fox jumps over the lazy dog, repeated for bulk, sentence {i} of many in \
                 this section.</p>"
            ));
            i += 1;
        }
        html.push_str("</body></html>");
        html
    }

    /// **500+ references** member: `n` distinct `ol.references li` entries,
    /// each with unique citation text and a unique external URL, referenced
    /// from a lead paragraph. Used both at `n` just past 500 (the corpus
    /// member itself) and at `n` far past `MAX_CITATIONS` (the SEC-3 cap
    /// "still bounded at hostile scale" check, complementing doc.rs's own
    /// exact-boundary test which has access to the private constant).
    pub fn many_references_html(n: usize) -> String {
        let mut html = String::from(
            "<html><head><title>Big Refs</title></head><body>\
             <p>Lead paragraph with a citation.\
             <sup class=\"reference\"><a href=\"#cite_note-0\">[1]</a></sup></p>\
             <h2>References</h2>\
             <div class=\"mw-references-wrap\"><ol class=\"references\">",
        );
        for i in 0..n {
            let year = 2000 + (i % 25);
            html.push_str(&format!(
                "<li id=\"cite_note-{i}\"><span class=\"reference-text\">Source {i}, Example \
                 Publisher, {year}. \
                 <a href=\"https://example.org/ref/{i}\">https://example.org/ref/{i}</a></span></li>"
            ));
        }
        html.push_str("</ol></div></body></html>");
        html
    }

    /// **Deeply nested rowspan/colspan table** member: three levels of span
    /// nesting (a row-spanning label column, colspan'd year-group headers, a
    /// row-spanning sub-label, and a full-width colspan footer row) — the
    /// grid-expansion path (PRD FR-RD-4). Deliberately stays within
    /// `MAX_TABLE_COLS`/`MAX_TABLE_ROWS` — the cap-exceeding case is already
    /// covered by doc.rs's own `giant_colspan_is_capped_and_flagged_truncated`
    /// (or equivalent) test, which has access to the private cap constants;
    /// this fixture's job is to prove the nesting *shape* itself expands
    /// correctly, not to re-test the cap.
    pub const NESTED_TABLE_HTML: &str = r##"
    <html><head><title>Nested Table</title></head><body>
    <p>A table exercising multi-level rowspan and colspan nesting.</p>
    <table class="wikitable"><tbody>
      <tr><th rowspan="3">Region</th><th colspan="2">2020</th><th colspan="2">2021</th></tr>
      <tr><td rowspan="2">Metric A</td><td>10</td><td rowspan="2">Metric B</td><td>20</td></tr>
      <tr><td>11</td><td>21</td></tr>
      <tr><td colspan="5">Notes: figures in thousands</td></tr>
    </tbody></table>
    </body></html>
    "##;

    /// **Math-heavy** member: Maxwell's four equations (as `<dl><dd>` display
    /// nodes, Parsoid's leading-colon shape) plus inline math for the speed
    /// of light and the constants it relates — exercising all three TeX
    /// extraction tiers `doc::extract_math` documents: `alttext`-wins (the
    /// inline nodes and Ampère's law here), the `annotation` fallback (Gauss's
    /// and Faraday's laws, whose `<math>` carries no `alttext`), and the
    /// accessible-fallback-image tier (the last paragraph, no `<math>`
    /// element at all).
    pub const MATH_HEAVY_HTML: &str = r##"
    <html><head><title>Maxwell's equations</title></head><body>
    <p>Maxwell's equations describe how electric and magnetic fields are generated. The speed of
    light satisfies <span typeof="mw:Extension/math"><math alttext="c^2 = \frac{1}{\mu_0 \varepsilon_0}">
    <semantics><mrow></mrow><annotation encoding="application/x-tex">c^2 = \frac{1}{\mu_0 \varepsilon_0}</annotation>
    </semantics></math></span>, relating the vacuum permeability <span typeof="mw:Extension/math">
    <math alttext="\mu_0"><semantics><mrow></mrow><annotation encoding="application/x-tex">\mu_0</annotation>
    </semantics></math></span> and permittivity <span typeof="mw:Extension/math">
    <math alttext="\varepsilon_0"><semantics><mrow></mrow><annotation encoding="application/x-tex">\varepsilon_0</annotation>
    </semantics></math></span>.</p>
    <h2>Gauss's law</h2>
    <dl><dd><span typeof="mw:Extension/math"><math display="block"><semantics><mrow></mrow>
    <annotation encoding="application/x-tex">\nabla \cdot \vec{E} = \frac{\rho}{\varepsilon_0}</annotation>
    </semantics></math></span></dd></dl>
    <h2>Gauss's law for magnetism</h2>
    <dl><dd><span typeof="mw:Extension/math"><math display="block"><semantics><mrow></mrow>
    <annotation encoding="application/x-tex">\nabla \cdot \vec{B} = 0</annotation></semantics></math>
    </span></dd></dl>
    <h2>Faraday's law</h2>
    <dl><dd><span typeof="mw:Extension/math"><math display="block"><semantics><mrow></mrow>
    <annotation encoding="application/x-tex">\nabla \times \vec{E} = -\frac{\partial \vec{B}}{\partial t}</annotation>
    </semantics></math></span></dd></dl>
    <h2>Ampère's law (with Maxwell's correction)</h2>
    <dl><dd><span typeof="mw:Extension/math">
    <math display="block" alttext="\nabla \times \vec{B} = \mu_0 \vec{J} + \mu_0 \varepsilon_0 \frac{\partial \vec{E}}{\partial t}">
    <semantics><mrow></mrow></semantics></math></span></dd></dl>
    <h2>Accessible fallback</h2>
    <p>A math node with no MathML at all still degrades to its accessible fallback image's alt
    text: <span typeof="mw:Extension/math">
    <img class="mwe-math-fallback-image-inline" alt="{\displaystyle \oint \vec{E} \cdot d\vec{l} = -\frac{d\Phi_B}{dt}}"/>
    </span>.</p>
    </body></html>
    "##;

    /// **CJK (ja)** member: a Japanese article about Mount Fuji — long
    /// unbroken CJK prose runs, internal links, and multiple headings, for
    /// per-character wrapping/kinsoku correctness (FR-RD-10).
    pub const JA_HTML: &str = r##"
    <html><head><title>富士山</title></head><body>
    <p>富士山は、静岡県と山梨県にまたがる標高3776メートルの日本最高峰の成層火山である。その優美な稜線は
    古くから日本文化の象徴とされ、多くの<a href="./浮世絵">浮世絵</a>や文学作品の題材となってきた。
    2013年には「富士山―信仰の対象と芸術の源泉」として<a href="./世界遺産">世界遺産</a>に登録された。</p>
    <h2>地理</h2>
    <p>富士山は約10万年前から活動を続けてきた活火山であり、現在も気象庁によって常時観測火山に指定されている。
    最後の噴火は1707年の宝永大噴火であり、それ以降300年以上大きな噴火は起きていない。山頂からは晴れた日には
    遠く関東平野まで見渡すことができる。</p>
    <h2>登山</h2>
    <p>毎年7月から9月にかけての夏山シーズンには、国内外から多くの登山者が訪れる。主な登山道には吉田口、
    富士宮口、須走口、御殿場口の四つがあり、それぞれ異なる標高の五合目から出発する。</p>
    </body></html>
    "##;

    /// **CJK (zh)** member: a Simplified Chinese article about computer
    /// science, mentioning Alan Turing — the corpus's Chinese-script
    /// counterpart to the mock server's existing Japanese pty fixture (the
    /// mock has ja only; this module adds zh as a pure test const per this
    /// file's own mock-server decision above).
    pub const ZH_HTML: &str = r##"
    <html><head><title>计算机科学</title></head><body>
    <p>计算机科学是研究计算机及其应用的一门学科,涵盖了算法设计、数据结构、编程语言理论、软件工程、
    人工智能、计算机体系结构等多个分支领域。它既是一门理论学科,也是一门应用学科,与数学、电子工程、
    语言学等领域都有密切联系。</p>
    <h2>历史</h2>
    <p>现代计算机科学的理论基础可以追溯到二十世纪三十年代,英国数学家<a href="./艾伦·图灵">艾伦·图灵</a>
    提出了一种抽象的计算模型,后世称为图灵机。这一模型为可计算性理论奠定了基础,也深刻影响了后来
    <a href="./人工智能">人工智能</a>领域的发展。第二次世界大战期间,图灵还参与了德国恩尼格玛密码机的
    破译工作,为盟军的胜利作出了重要贡献。</p>
    <h2>主要分支</h2>
    <p>计算机科学包含众多分支,例如算法与数据结构、编程语言与编译原理、计算机网络、数据库系统、
    人工智能与机器学习、计算机图形学等。这些分支既相对独立,又彼此交叉,共同构成了这门学科的完整体系。</p>
    </body></html>
    "##;

    /// **RTL (ar)** member: an Arabic article, embedding one Latin run
    /// ("Turing Award") mid-sentence — the "LTR run embedded in RTL" case
    /// `bidi::reorder_line_for_display` must leave untouched — plus a real
    /// internal link so navigation on an RTL article is exercised too.
    pub const AR_HTML: &str = r##"
    <html><head><title>الحوسبة</title></head><body>
    <p>هذه المقالة تختبر دعم الكتابة من اليمين إلى اليسار (RTL) في القارئ (تجريبي، FR-ML-7). يُعدّ
    علم الحاسوب أحد أهم المجالات العلمية في العصر الحديث، وهو يشمل دراسة الخوارزميات وبنية البيانات
    ولغات البرمجة. من أبرز رواد هذا المجال عالم الرياضيات البريطاني آلان تورينج، الذي يُمنح باسمه
    سنويًا وسام Turing Award تكريمًا لإسهاماته الرائدة.</p>
    <h2>علم الحاسوب</h2>
    <p>يرتبط <a href="./Computer_science">علم الحاسوب</a> ارتباطًا وثيقًا بالرياضيات والهندسة
    الكهربائية، وقد ساهم فيه العديد من الباحثين حول العالم عبر العقود الماضية.</p>
    </body></html>
    "##;

    /// **RTL (he)** member: the Hebrew counterpart to [`AR_HTML`], same shape
    /// (embedded "Turing Award" Latin run, a heading, a real internal link).
    pub const HE_HTML: &str = r##"
    <html><head><title>מדעי המחשב</title></head><body>
    <p>מאמר זה בוחן את התמיכה בכתיבה מימין לשמאל (RTL) בקורא (ניסיוני, FR-ML-7). מדעי המחשב הם
    אחד התחומים המדעיים החשובים ביותר בעידן המודרני, והם כוללים את חקר האלגוריתמים, מבני הנתונים
    ושפות התכנות. אחד מחלוצי התחום המפורסמים ביותר הוא המתמטיקאי הבריטי אלן טיורינג, שעל שמו מוענק
    מדי שנה הפרס היוקרתי Turing Award.</p>
    <h2>מדעי המחשב</h2>
    <p>קיים קשר הדוק בין <a href="./Computer_science">מדעי המחשב</a> לבין המתמטיקה וההנדסה
    החשמלית, ותחום זה התפתח בזכות תרומתם של חוקרים רבים לאורך העשורים האחרונים.</p>
    </body></html>
    "##;

    /// A base letter with two stacked combining marks (circumflex + dot
    /// below) — the shape multi-diacritic orthographies (Vietnamese,
    /// several African-language transcriptions) use. Built explicitly from
    /// combining codepoints, never a precomposed glyph, so this genuinely
    /// exercises multi-mark grapheme-cluster handling rather than a single
    /// already-composed character.
    pub const COMBINING_O: &str = "o\u{0302}\u{0323}";
    /// A second, differently-shaped combining stack (dot below + acute).
    pub const COMBINING_S: &str = "s\u{0323}\u{0301}";
    /// A ZWJ emoji sequence (man+ZWJ+woman+ZWJ+girl+ZWJ+boy — a "family").
    pub const FAMILY_EMOJI: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    /// A real IPA transcription (of "international"): stress marks (ˈ ˌ),
    /// schwa, esh, and a nasal velar, each its own single codepoint (no
    /// combining marks needed here — the point is ordinary IPA characters
    /// outside the ASCII/Latin-1 range must still measure/wrap correctly).
    pub const IPA_TRANSCRIPTION: &str = "ˌɪntəˈnæʃənəl";

    /// **IPA/diacritics** member: the transcription plus both combining
    /// stacks plus the ZWJ family emoji plus one 300-character unbroken
    /// ASCII token (the hard-wrap edge case) — consolidating PRD §9's
    /// "IPA/diacritics" corpus entry with the "Unicode/width property
    /// tests" bullet's ZWJ/combining-mark requirement in one fixture.
    pub fn ipa_diacritics_html() -> String {
        let long_token = "x".repeat(300);
        format!(
            "<html><head><title>IPA Test</title></head><body>\
             <p>English pronunciation guide: international is transcribed /{IPA_TRANSCRIPTION}/ \
             in the International Phonetic Alphabet.</p>\
             <p>Combining diacritics must stay whole: {COMBINING_O} and {COMBINING_S}.</p>\
             <p>Family emoji stays one grapheme cluster: {FAMILY_EMOJI}.</p>\
             <p>{long_token}</p>\
             </body></html>"
        )
    }

    /// **Galleries** member (PRD FR-RD-8 / B6): a three-item
    /// `<ul class="gallery">`.
    pub const GALLERY_HTML: &str = r#"
    <html><head><title>Gallery Test</title></head><body>
    <p>A gallery of test images.</p>
    <ul class="gallery mw-gallery-traditional">
      <li class="gallerybox"><div class="thumb"><img src="https://ex.org/1.png" alt="alpha"/></div>
        <div class="gallerytext">Alpha</div></li>
      <li class="gallerybox"><div class="thumb"><img src="https://ex.org/2.png" alt="beta"/></div>
        <div class="gallerytext">Beta</div></li>
      <li class="gallerybox"><div class="thumb"><img src="https://ex.org/3.png" alt="gamma"/></div>
        <div class="gallerytext">Gamma</div></li>
    </ul>
    </body></html>
    "#;

    /// **Disambiguation** member (§7): the shape a disambiguation page's
    /// Parsoid HTML actually has — a short lead sentence followed by a list
    /// of links each with a one-line description. `doc.rs` has no special
    /// disambiguation handling (detection is pageprops-driven, at the
    /// app/API layer per §7 — out of this renderer corpus's scope); this
    /// fixture proves the *ordinary* parse/layout pipeline renders that
    /// shape correctly (a plain list, every link followable), which is what
    /// actually reaches the screen once the app-level chooser decides to
    /// show it as such.
    pub const DISAMBIGUATION_HTML: &str = r#"<html><head><title>Mercury (disambiguation)</title></head><body>
<p><b>Mercury</b> may refer to:</p>
<ul>
<li><a href="./Mercury_(planet)">Mercury</a>, the smallest planet in the Solar System</li>
<li><a href="./Mercury_(element)">Mercury</a>, a chemical element</li>
<li><a href="./Mercury_(mythology)">Mercury</a>, a Roman god</li>
<li><a href="./Mercury_(automobile)">Mercury</a>, an American car brand</li>
<li><a href="./Freddie_Mercury">Freddie Mercury</a>, a British musician</li>
</ul>
</body></html>"#;

    /// **Redirect** member (§7): the shape a redirect page's own Parsoid HTML
    /// actually renders as (MediaWiki's "Redirect to: <target>" notice) — the
    /// content a reader sees when `:noredirect` opts to view the redirect
    /// page itself rather than following it (the actual following-a-redirect
    /// behavior is an app/API-layer concern, not `doc.rs`'s — this proves the
    /// renderer's own pipeline handles the notice shape, which is the one
    /// real path where `doc.rs` does see it).
    pub const REDIRECT_HTML: &str = r#"<html><head><title>UK</title></head><body>
<div class="redirectMsg"><p>Redirect to:</p><ul class="redirectText">
<li><a href="./United_Kingdom">United Kingdom</a></li></ul></div>
</body></html>"#;

    /// **Stub** member: the tiniest legitimate article shape.
    pub const STUB_HTML: &str = r#"<html><head><title>Tiny Stub</title></head><body>
<p>Tiny Stub is a very short placeholder article about a minor topic.</p>
<p><i>This short article about a minor topic can be expanded.</i></p>
</body></html>"#;
}

// ===========================================================================
// Shared corpus + helpers
// ===========================================================================

/// Every corpus member cheap enough to lay out repeatedly at several
/// widths/modes without dominating the test suite's runtime — everything
/// from PRD §9's list except the 1.5 MB+ page, which gets its own,
/// deliberately smaller sweep below (see
/// `no_line_exceeds_width_on_the_1_5mb_pathological_fixture`'s doc comment
/// for why). Parsed once per test binary (`OnceLock`), not once per test.
fn corpus() -> &'static [(&'static str, Document)] {
    static CORPUS: OnceLock<Vec<(&'static str, Document)>> = OnceLock::new();
    CORPUS.get_or_init(|| {
        vec![
            (
                "featured_article",
                parse_article_html("Ada Lovelace", fixtures::FEATURED_ARTICLE_HTML),
            ),
            (
                "nested_table",
                parse_article_html("Nested Table", fixtures::NESTED_TABLE_HTML),
            ),
            (
                "math_heavy",
                parse_article_html("Maxwell's equations", fixtures::MATH_HEAVY_HTML),
            ),
            ("cjk_ja", parse_article_html("富士山", fixtures::JA_HTML)),
            (
                "cjk_zh",
                parse_article_html("计算机科学", fixtures::ZH_HTML),
            ),
            ("rtl_ar", parse_article_html("الحوسبة", fixtures::AR_HTML)),
            (
                "rtl_he",
                parse_article_html("מדעי המחשב", fixtures::HE_HTML),
            ),
            (
                "ipa_diacritics",
                parse_article_html("IPA Test", &fixtures::ipa_diacritics_html()),
            ),
            (
                "gallery",
                parse_article_html("Gallery Test", fixtures::GALLERY_HTML),
            ),
            (
                "disambiguation",
                parse_article_html("Mercury (disambiguation)", fixtures::DISAMBIGUATION_HTML),
            ),
            (
                "redirect",
                parse_article_html("UK", fixtures::REDIRECT_HTML),
            ),
            ("stub", parse_article_html("Tiny Stub", fixtures::STUB_HTML)),
            (
                "many_references_550",
                parse_article_html("Big Refs", &fixtures::many_references_html(550)),
            ),
        ]
    })
}

fn corpus_doc(name: &str) -> &'static Document {
    &corpus()
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no corpus fixture named {name:?}"))
        .1
}

/// The 1.5 MB+ pathological fixture, parsed once and reused across the tests
/// that need it (parsing ~1.5 MB of HTML is the expensive step; laying it
/// out repeatedly is comparatively cheap, but there is no reason to re-parse
/// it per test either).
fn large_doc() -> &'static Document {
    static DOC: OnceLock<Document> = OnceLock::new();
    DOC.get_or_init(|| {
        let html = fixtures::large_article_html(1_600_000);
        parse_article_html("Large Article", &html)
    })
}

fn line_text(line: &LaidLine) -> String {
    line.spans.iter().map(|s| s.text.as_str()).collect()
}

fn assert_no_overflow(doc: &Document, width: u16, opts: LayoutOptions, ctx: &str) {
    let layout = layout_document(doc, width, opts);
    for (i, line) in layout.lines.iter().enumerate() {
        let w = line.width(opts.ambiguous_wide);
        assert!(
            w <= width as usize,
            "{ctx}: line {i} width {w} exceeds {width}: {:?}",
            line_text(line)
        );
    }
}

// ===========================================================================
// Snapshot / structural tests (PRD §9's "laid-out text snapshots")
// ===========================================================================

#[test]
fn stub_render_plain_matches_an_exact_snapshot() {
    let doc = corpus_doc("stub");
    let plain = render_plain(doc, "en");
    assert_eq!(
        plain,
        "Tiny Stub\n\
         =========\n\
         \n\
         Tiny Stub is a very short placeholder article about a minor topic.\n\
         \n\
         This short article about a minor topic can be expanded.\n\
         \n"
    );
}

#[test]
fn disambiguation_render_plain_matches_an_exact_snapshot() {
    let doc = corpus_doc("disambiguation");
    let plain = render_plain(doc, "en");
    assert_eq!(
        plain,
        "Mercury (disambiguation)\n\
         ========================\n\
         \n\
         Mercury may refer to:\n\
         \n\
         - Mercury[link: https://en.wikipedia.org/wiki/Mercury_(planet)], the smallest planet in the Solar System\n\
         - Mercury[link: https://en.wikipedia.org/wiki/Mercury_(element)], a chemical element\n\
         - Mercury[link: https://en.wikipedia.org/wiki/Mercury_(mythology)], a Roman god\n\
         - Mercury[link: https://en.wikipedia.org/wiki/Mercury_(automobile)], an American car brand\n\
         - Freddie Mercury[link: https://en.wikipedia.org/wiki/Freddie_Mercury], a British musician\n"
    );
}

#[test]
fn redirect_render_plain_matches_an_exact_snapshot() {
    let doc = corpus_doc("redirect");
    let plain = render_plain(doc, "en");
    assert_eq!(
        plain,
        "UK\n\
         ==\n\
         \n\
         Redirect to:\n\
         \n\
         - United Kingdom[link: https://en.wikipedia.org/wiki/United_Kingdom]\n"
    );
    // The redirect target must also be a real, followable link — proving
    // `:noredirect`'s "view the redirect page" leaves the reader somewhere
    // they can still navigate onward from, not a dead end.
    let links = collect_links(doc);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].internal_title.as_deref(), Some("United Kingdom"));
}

#[test]
fn featured_article_has_the_expected_block_shape() {
    let doc = corpus_doc("featured_article");

    assert_eq!(
        doc.blocks
            .iter()
            .filter(|b| matches!(b, Block::Infobox(_)))
            .count(),
        1
    );
    assert_eq!(
        doc.blocks
            .iter()
            .filter(|b| matches!(b, Block::Image { .. }))
            .count(),
        1
    );
    assert_eq!(
        doc.blocks
            .iter()
            .filter(|b| matches!(b, Block::Blockquote(_)))
            .count(),
        1
    );
    let headings: Vec<_> = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Heading { level, spans } => Some((
                *level,
                spans.iter().map(|s| s.text.as_str()).collect::<String>(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        headings,
        vec![
            (2, "Early life".to_string()),
            (2, "Collaboration with Babbage".to_string()),
            (3, "The first algorithm".to_string()),
            (2, "Legacy".to_string()),
            (2, "References".to_string()),
        ]
    );
    assert_eq!(
        doc.citations.len(),
        10,
        "ten references in the corpus fixture"
    );
    assert!(doc.citations.iter().all(|c| !c.text.is_empty()));
    assert!(
        collect_links(doc).len() >= 6,
        "expects several internal links across the lead and body"
    );
    assert!(!doc.truncated);
}

#[test]
fn nested_table_grid_expands_rowspan_and_colspan_correctly() {
    let doc = corpus_doc("nested_table");
    let table = doc
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Table(t) => Some(t),
            _ => None,
        })
        .expect("fixture has one table");

    assert_eq!(table.cols(), 5);
    assert_eq!(table.rows.len(), 4);
    assert!(!table.truncated);
    assert!(table.has_header_row());

    let text_grid: Vec<Vec<&str>> = table
        .rows
        .iter()
        .map(|row| row.iter().map(|c| c.text.as_str()).collect())
        .collect();
    assert_eq!(
        text_grid,
        vec![
            vec!["Region", "2020", "", "2021", ""],
            vec!["", "Metric A", "10", "Metric B", "20"],
            vec!["", "", "11", "", "21"],
            vec!["Notes: figures in thousands", "", "", "", ""],
        ],
        "rowspan/colspan expansion must fill origin cells and blank the spanned ones"
    );

    // The collapse-to-list view (accessible mode / `--dump`) must not panic
    // on this shape either, and must skip blank spanned cells.
    let lines = table.to_list_lines();
    assert!(!lines.is_empty());
    assert!(lines.iter().all(|l| !l.contains("blank")));
}

#[test]
fn math_heavy_fixture_extracts_all_four_maxwell_equations_and_inline_constants() {
    let doc = corpus_doc("math_heavy");

    let display_tex: Vec<&str> = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Math { tex, display: true } => Some(tex.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        display_tex,
        vec![
            r"\nabla \cdot \vec{E} = \frac{\rho}{\varepsilon_0}",
            r"\nabla \cdot \vec{B} = 0",
            r"\nabla \times \vec{E} = -\frac{\partial \vec{B}}{\partial t}",
            r"\nabla \times \vec{B} = \mu_0 \vec{J} + \mu_0 \varepsilon_0 \frac{\partial \vec{E}}{\partial t}",
        ],
        "all four Maxwell equations must round-trip as exact TeX passthrough"
    );

    let inline_tex: Vec<&str> = doc
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Paragraph(spans) => Some(spans),
            _ => None,
        })
        .flatten()
        .filter_map(|s| match &s.style {
            SpanStyle::Math(tex) => Some(tex.as_str()),
            _ => None,
        })
        .collect();
    assert!(inline_tex.contains(&r"c^2 = \frac{1}{\mu_0 \varepsilon_0}"));
    assert!(inline_tex.contains(&r"\mu_0"));
    assert!(inline_tex.contains(&r"\varepsilon_0"));

    // The accessible-fallback-image tier (no `<math>` element at all): the
    // `\displaystyle` wrapper must be stripped, the TeX kept.
    assert!(
        inline_tex
            .iter()
            .any(|t| t.contains(r"\oint \vec{E}") && !t.contains("displaystyle")),
        "fallback-image tex must survive with its display-mode wrapper stripped: {inline_tex:?}"
    );
}

#[test]
fn gallery_fixture_has_three_captioned_items() {
    let doc = corpus_doc("gallery");
    let items = doc
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Gallery(items) => Some(items),
            _ => None,
        })
        .expect("fixture has a gallery block");
    let captions: Vec<&str> = items.iter().map(|i| i.caption.as_str()).collect();
    assert_eq!(captions, vec!["Alpha", "Beta", "Gamma"]);
    assert!(items.iter().all(|i| i.src.is_some()));
}

#[test]
fn cjk_fixtures_wrap_into_many_lines_at_narrow_width() {
    for name in ["cjk_ja", "cjk_zh"] {
        let doc = corpus_doc(name);
        let layout = layout_document(doc, 30, LayoutOptions::default());
        assert!(
            layout.lines.len() > 8,
            "{name}: expected long CJK prose to wrap across many lines at width 30, got {}",
            layout.lines.len()
        );
        // Every line the CJK prose actually wraps into must still respect
        // the width — the per-character/kinsoku breaking path, not just the
        // space-based one the Latin-script fixtures exercise.
        assert_no_overflow(doc, 30, LayoutOptions::default(), name);
    }
}

#[test]
fn rtl_fixtures_are_detected_and_survive_app_side_reorder() {
    for (name, lang) in [("rtl_ar", "ar"), ("rtl_he", "he")] {
        assert_eq!(
            bidi::direction(lang),
            Direction::Rtl,
            "{name}: {lang} must resolve to RTL (PRD FR-ML-7 / D1)"
        );
        let doc = corpus_doc(name);
        let layout = layout_document(doc, 80, LayoutOptions::default());
        let reordered = bidi::reorder_laid_lines(&layout.lines, Direction::Rtl);

        assert_eq!(
            reordered.len(),
            layout.lines.len(),
            "{name}: reorder must not add or drop lines"
        );
        // The load-bearing invariant `reorder_laid_lines` must hold — the
        // one `layout::Layout::link_cols`/`find_matches` grapheme-index
        // mappings actually depend on — is that each span's GRAPHEME-CLUSTER
        // COUNT is preserved: those position maps are counted in clusters
        // from the line start (never display cells), and character reversal
        // within a token changes no cluster's identity or the count. That is
        // what is asserted here, per span, not display-cell width.
        //
        // Display-cell WIDTH is deliberately NOT asserted equal: it is not
        // preserved for Arabic, and that is a genuine (documented-below)
        // limitation of D1's char-level reversal, not a property this test
        // should pretend holds — see this module's report note. Concretely,
        // `unicode-width` models the Lam-Alef ligature (LAM+ALEF measures one
        // cell, ALEF+LAM two), so naive character reversal can create or
        // destroy such an adjacency and shift a line's measured width by a
        // cell. Grapheme-cluster count is immune to that (both orderings have
        // the same clusters), which is exactly why it — not width — is the
        // invariant the app's position mappings rely on.
        for (i, (orig, reord)) in layout.lines.iter().zip(&reordered).enumerate() {
            for (orig_span, reord_span) in orig.spans.iter().zip(&reord.spans) {
                assert_eq!(
                    orig_span.text.graphemes(true).count(),
                    reord_span.text.graphemes(true).count(),
                    "{name}: line {i} span grapheme-cluster count changed under reorder \
                     (orig {:?} vs reord {:?})",
                    orig_span.text,
                    reord_span.text
                );
            }
        }
        // The embedded Latin run ("Turing Award") must survive intact
        // somewhere in the reordered output — an LTR token is left untouched
        // by `reorder_line_for_display`.
        let joined: String = reordered.iter().map(line_text).collect();
        assert!(
            joined.contains("Turing Award"),
            "{name}: embedded LTR run must survive reordering: {joined:?}"
        );
    }
}

/// Regression + characterization for the D1 (PRD FR-ML-7) finding this
/// corpus surfaced: `bidi::reorder_line_for_display`'s doc comment claims
/// character reversal within a token leaves "the line's total width"
/// unchanged, but that is not true for Arabic. `unicode-width` (0.2.x)
/// models the Lam-Alef ligature — LAM (U+0644) immediately followed by ALEF
/// (U+0627) shapes to a single cell, while ALEF-then-LAM stays two — so
/// naive character reversal can create or destroy that adjacency and change
/// the measured display width by a cell. This test pins that behavior
/// explicitly (both directions of the ±1 shift) so a future change to the
/// reorder or the width tables is caught, and documents that the reorder is
/// therefore width-approximate for Arabic — the practical impact is bounded
/// to a cell and cosmetic (the reorder is already a documented crude
/// approximation, and its output is a display-only painted copy the app
/// never re-measures for layout), but the doc comment's "no width change"
/// claim overstates the guarantee. Flagged for the review phase; not fixed
/// here (redesigning experimental RTL shaping is out of scope for a
/// test-infrastructure change).
#[test]
fn char_reversal_is_not_width_preserving_for_arabic_lam_alef() {
    use crate::layout::display_width;
    // A word whose reversal CREATES a Lam-Alef adjacency (the definite
    // article "al-": ALEF, LAM in logical order → LAM, ALEF reversed).
    let widens_to_ligature = "\u{0627}\u{0644}\u{062D}"; // ALEF LAM HAH ("الح")
    let reversed_creates_ligature: String = widens_to_ligature.chars().rev().collect();
    assert_eq!(display_width(widens_to_ligature, false), 3);
    assert_eq!(
        display_width(&reversed_creates_ligature, false),
        2,
        "reversal that forms a LAM+ALEF adjacency measures one cell narrower"
    );

    // And the reverse-of-the-reverse: a word storing LAM, ALEF (a genuine
    // ligature in logical order) whose reversal DESTROYS it, widening the
    // line — the direction that could nudge a reordered line past the edge.
    let ligature_in_logical_order = "\u{0644}\u{0627}"; // LAM ALEF ("لا")
    let reversed_destroys_ligature: String = ligature_in_logical_order.chars().rev().collect();
    assert_eq!(display_width(ligature_in_logical_order, false), 1);
    assert_eq!(
        display_width(&reversed_destroys_ligature, false),
        2,
        "reversal that breaks a LAM+ALEF ligature measures one cell wider"
    );
}

#[test]
fn five_hundred_plus_references_are_all_extracted_with_correct_text_and_urls() {
    let doc = corpus_doc("many_references_550");
    assert_eq!(doc.citations.len(), 550);
    for i in [0usize, 274, 549] {
        let c = &doc.citations[i];
        assert_eq!(c.id, format!("cite_note-{i}"));
        assert!(
            c.text.contains(&format!("Source {i}")),
            "citation {i} text: {:?}",
            c.text
        );
        assert_eq!(
            c.url.as_deref(),
            Some(format!("https://example.org/ref/{i}").as_str())
        );
    }
}

#[test]
fn reference_harvest_stays_bounded_when_padded_far_beyond_the_cap() {
    // Complements doc.rs's own exact-boundary test (which has access to the
    // private `MAX_CITATIONS` constant) with the corpus-shaped version: this
    // module only sees the public API, so it asserts the *qualitative*
    // property — padding well beyond any plausible cap must not make the
    // harvested count scale with the input — rather than the exact number.
    let html = fixtures::many_references_html(6000);
    let doc = parse_article_html("Massive Refs", &html);
    assert!(
        doc.citations.len() < 6000,
        "citation harvest must be capped, not scale with a hostile reference list: got {}",
        doc.citations.len()
    );
    assert!(
        doc.citations.len() >= 550,
        "cap must still be generous enough for a real large article"
    );
}

#[test]
fn structured_large_article_just_under_the_cap_is_not_truncated() {
    let html = fixtures::large_article_html(doc::MAX_ARTICLE_HTML_BYTES - 4096);
    assert!(html.len() < doc::MAX_ARTICLE_HTML_BYTES);
    let d = parse_article_html("Large", &html);
    assert!(
        !d.truncated,
        "a structured article just under the cap must not be truncated"
    );
}

#[test]
fn structured_large_article_over_the_cap_is_truncated_with_a_banner_and_parses_without_panic() {
    let html = fixtures::large_article_html(doc::MAX_ARTICLE_HTML_BYTES + 2_000_000);
    let d = parse_article_html("Large", &html);
    assert!(
        d.truncated,
        "a structured article well past the SEC-3 cap must set the truncated flag"
    );
    let has_banner = d.blocks.first().is_some_and(|b| {
        matches!(b, Block::Paragraph(spans) if spans.iter().any(|s| s.text.contains("Degraded rendering")))
    });
    assert!(
        has_banner,
        "the first block must be the degraded-rendering banner"
    );

    // The byte cap is not tag-aware — cutting a structured, tag-heavy
    // document can land mid-tag. html5ever's error recovery must swallow
    // that gracefully; neither parsing nor a subsequent layout may panic.
    let layout = layout_document(&d, 80, LayoutOptions::default());
    assert!(!layout.lines.is_empty());
}

// ===========================================================================
// Width / grapheme property tests (PRD §9 "Unicode/width property tests"),
// extended across the whole corpus.
// ===========================================================================

#[test]
fn no_line_exceeds_width_across_the_prd_9_corpus() {
    for (name, doc) in corpus() {
        for width in [40u16, 80, 120, 200] {
            for ambiguous_wide in [false, true] {
                assert_no_overflow(
                    doc,
                    width,
                    LayoutOptions {
                        measure: 88,
                        ambiguous_wide,
                        ..LayoutOptions::default()
                    },
                    &format!("{name} width={width} ambiguous_wide={ambiguous_wide}"),
                );
            }
        }
    }
}

#[test]
fn no_line_exceeds_width_under_justify_and_hyphenate_across_the_corpus() {
    // A smaller width set than the plain sweep above: this test's own claim
    // is "the no-overflow invariant also holds with C11's justify/hyphenate
    // on", not "re-run the full width matrix a third time" — the wrap
    // algorithm's per-width behavior is already exhaustively covered above.
    for (name, doc) in corpus() {
        for width in [40u16, 80, 120] {
            for (justify, hyphenate) in [(true, false), (false, true), (true, true)] {
                assert_no_overflow(
                    doc,
                    width,
                    LayoutOptions {
                        measure: 88,
                        justify,
                        hyphenate,
                        ..LayoutOptions::default()
                    },
                    &format!("{name} width={width} justify={justify} hyphenate={hyphenate}"),
                );
            }
        }
    }
}

#[test]
fn no_line_exceeds_width_on_the_1_5mb_pathological_fixture() {
    // Deliberately a smaller width×mode sweep than the rest of the corpus:
    // the wrap algorithm's per-width/per-mode behavior is already
    // exhaustively covered by the (much cheaper to lay out) smaller fixtures
    // above; multiplying that same full sweep onto a 1.5 MB document would
    // make this one test dominate the whole suite's runtime for no
    // additional coverage of the algorithm itself. What this test uniquely
    // covers — that the invariant holds at all at pathological *size*, not
    // just pathological content shape — only needs a couple of widths.
    let doc = large_doc();
    for width in [40u16, 120] {
        for ambiguous_wide in [false, true] {
            assert_no_overflow(
                doc,
                width,
                LayoutOptions {
                    measure: 88,
                    ambiguous_wide,
                    ..LayoutOptions::default()
                },
                &format!("1.5MB fixture width={width} ambiguous_wide={ambiguous_wide}"),
            );
        }
    }
}

#[test]
fn ipa_and_zwj_grapheme_clusters_never_split_at_any_width() {
    let doc = corpus_doc("ipa_diacritics");
    for width in [10u16, 15, 20, 40, 80] {
        for ambiguous_wide in [false, true] {
            let layout = layout_document(
                doc,
                width,
                LayoutOptions {
                    ambiguous_wide,
                    ..LayoutOptions::default()
                },
            );
            let joined: String = layout.lines.iter().map(line_text).collect();
            assert!(
                joined.contains(fixtures::FAMILY_EMOJI),
                "width={width} aw={ambiguous_wide}: ZWJ family emoji was split"
            );
            assert!(
                joined.contains(fixtures::COMBINING_O),
                "width={width} aw={ambiguous_wide}: combining-mark stack (o) was split"
            );
            assert!(
                joined.contains(fixtures::COMBINING_S),
                "width={width} aw={ambiguous_wide}: combining-mark stack (s) was split"
            );
            assert!(
                joined.contains(fixtures::IPA_TRANSCRIPTION),
                "width={width} aw={ambiguous_wide}: IPA transcription was split"
            );
        }
    }
}

#[test]
fn link_occurrence_ordering_matches_collect_links_across_the_corpus() {
    for (name, doc) in corpus() {
        let links = collect_links(doc);
        let layout = layout_document(doc, 80, LayoutOptions::default());
        // The cross-module ordering invariant: one laid link-line/col entry
        // per collected link, in the same index space, in document order —
        // the same shape `layout::tests::link_occurrence_order_matches_
        // collect_links` guards on a single fixture, here across the corpus.
        assert_eq!(
            layout.link_lines.len(),
            links.len(),
            "{name}: link_lines must be indexed identically to collect_links"
        );
        assert_eq!(
            layout.link_cols.len(),
            links.len(),
            "{name}: link_cols must be indexed identically to collect_links"
        );
        for w in layout.link_lines.windows(2) {
            assert!(
                w[0] <= w[1],
                "{name}: link occurrences must be in document order"
            );
        }
        for (occ, link) in links.iter().enumerate() {
            if !layout.link_visible[occ] {
                continue;
            }
            let line = &layout.lines[layout.link_lines[occ]];
            let text = line_text(line);
            let graphemes: Vec<&str> = text.graphemes(true).collect();
            let span = layout.link_cols[occ];
            let sliced: String = graphemes[span.start..span.end].concat();
            // `link_cols`/`link_lines` record only the FIRST line a link's
            // text lands on (per `Layout::link_cols`'s own doc comment); a
            // link whose text soft-wraps across lines therefore has its cols
            // bound only its first-line portion, which is a non-empty prefix
            // of the whole link text — never a mismatch of the *wrong* link.
            // Asserting the full text here would wrongly fail on any wrapped
            // link (e.g. "Analytical Engine" splitting after "Analytical").
            assert!(
                !sliced.is_empty() && link.text.starts_with(&sliced),
                "{name}: link_cols[{occ}] must bound a leading run of the link's own text, \
                 got {sliced:?} which is not a prefix of {:?}",
                link.text
            );
        }
    }
}

// ===========================================================================
// SEC-1 sanitizer property tests (PRD §9 "Fuzzing ... the SEC-1 sanitizer")
// ===========================================================================

mod sanitizer_property {
    use super::*;

    /// Hostile fragments covering every category `sanitize.rs` documents it
    /// strips: CSI/OSC/DCS/APC 7-bit escape sequences (terminated and
    /// unterminated), the 8-bit C1 equivalents, bare C0 controls, DEL, bidi
    /// overrides and isolates (nested), NUL, and a bare/broken ESC.
    const HOSTILE_FRAGMENTS: &[&str] = &[
        "\x1b[31mred\x1b[0m",                                  // CSI SGR, terminated
        "\x1b]0;pwned-title\x07",                              // OSC set-title, BEL-terminated
        "\x1b]8;;http://evil.example\x1b\\link\x1b]8;;\x1b\\", // OSC 8 hyperlink hijack, ST-terminated
        "\x1bP+q436e\x1b\\",                                   // DCS, ST-terminated
        "\x1b_Gsomething\x1b\\",    // APC (kitty graphics protocol shape)
        "\u{9B}31m",                // C1 CSI introducer
        "\u{9D}0;pwned\u{9C}",      // C1 OSC ... C1 ST
        "\u{90}data\u{9C}",         // C1 DCS ... C1 ST
        "\x00null\x00",             // NUL
        "\x07bell\x07",             // BEL
        "\x08back\x08",             // backspace
        "\x1b",                     // bare, unterminated ESC
        "\x1b[",                    // broken, unterminated CSI
        "\x1b]8;;",                 // broken, unterminated OSC
        "\u{202E}reversed\u{202C}", // RLO .. PDF
        "\u{202A}\u{202B}\u{202D}nested\u{202E}\u{202C}", // nested overrides
        "\u{2066}iso\u{2069}",      // LRI .. PDI
        "\u{2067}\u{2068}nested\u{2069}\u{2069}", // nested isolates
        "\x7f",                     // DEL
        "\u{80}\u{9F}",             // C1 range edges
    ];

    /// Every text-bearing position `doc::sanitize_document` must cover,
    /// keyed by `slot % SLOT_COUNT` — headings, paragraphs, list items,
    /// blockquotes, table header/data cells, infobox label/value, image alt/
    /// caption, citation id/text/url, gallery caption, code, and math
    /// alttext. `payload` is embedded once per slot so each iteration
    /// exercises exactly one position at a time.
    const SLOT_COUNT: usize = 12;

    fn build_html(slot: usize, payload: &str) -> String {
        match slot % SLOT_COUNT {
            0 => format!(
                "<html><head><title>Before{payload}After</title></head><body><p>x</p></body></html>"
            ),
            1 => format!("<html><body><h2>Head{payload}ing</h2><p>x</p></body></html>"),
            2 => format!("<html><body><p>Para{payload}graph text</p></body></html>"),
            3 => format!("<html><body><ul><li>Item{payload}text</li></ul></body></html>"),
            4 => format!(
                "<html><body><blockquote><p>Quote{payload}text</p></blockquote></body></html>"
            ),
            5 => format!(
                "<html><body><table class=\"wikitable\"><tbody><tr><th>H{payload}</th><td>D{payload}</td></tr></tbody></table></body></html>"
            ),
            6 => format!(
                "<html><body><table class=\"infobox\"><tbody><tr><th>Label{payload}</th><td>Value{payload}</td></tr></tbody></table></body></html>"
            ),
            7 => format!(
                "<html><body><figure><img src=\"http://ex.org/a.png\" alt=\"Alt{payload}text\"/><figcaption>Cap{payload}tion</figcaption></figure></body></html>"
            ),
            8 => format!(
                "<html><body><div class=\"mw-references-wrap\"><ol class=\"references\"><li id=\"cite_note-1\"><span class=\"reference-text\">Ref{payload}text <a href=\"https://ex.org/{payload}\">link</a></span></li></ol></div></body></html>"
            ),
            9 => format!(
                "<html><body><ul class=\"gallery\"><li class=\"gallerybox\"><img src=\"http://ex.org/a.png\" alt=\"a\"/><div class=\"gallerytext\">Cap{payload}tion</div></li></ul></body></html>"
            ),
            10 => format!("<html><body><pre>code{payload}block</pre></body></html>"),
            11 => format!(
                r#"<html><body><p>Math <span typeof="mw:Extension/math"><math alttext="x{payload}y"><semantics><mrow></mrow><annotation encoding="application/x-tex">x{payload}y</annotation></semantics></math></span></p></body></html>"#
            ),
            _ => unreachable!(),
        }
    }

    /// `true` if `c` is one of the hostile categories SEC-1 must never let
    /// survive: any C0 control other than `\n`/`\t`, DEL, any C1 control, or
    /// a bidi override/isolate character.
    fn is_forbidden(c: char) -> bool {
        let u = c as u32;
        (u < 0x20 && c != '\n' && c != '\t')
            || u == 0x7F
            || (0x80..=0x9F).contains(&u)
            || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    }

    fn assert_span_clean(text: &str, ctx: &str) {
        for c in text.chars() {
            assert!(
                !is_forbidden(c),
                "hostile char U+{:04X} survived sanitization in {ctx}: {text:?}",
                c as u32
            );
        }
    }

    fn assert_document_clean(doc: &Document, ctx: &str) {
        assert_span_clean(&doc.title, &format!("{ctx} (title)"));
        for block in &doc.blocks {
            match block {
                Block::Heading { spans, .. }
                | Block::Paragraph(spans)
                | Block::ListItem { spans, .. }
                | Block::Blockquote(spans) => {
                    for s in spans {
                        assert_span_clean(&s.text, ctx);
                        match &s.style {
                            SpanStyle::Link(h) | SpanStyle::RedLink(h) => {
                                assert_span_clean(h, &format!("{ctx} (href)"))
                            }
                            SpanStyle::Math(t) => assert_span_clean(t, &format!("{ctx} (math)")),
                            _ => {}
                        }
                    }
                }
                Block::Code(t) => assert_span_clean(t, &format!("{ctx} (code)")),
                Block::Table(table) => {
                    for row in &table.rows {
                        for cell in row {
                            assert_span_clean(&cell.text, &format!("{ctx} (table cell)"));
                        }
                    }
                }
                Block::Infobox(rows) => {
                    for (l, v) in rows {
                        assert_span_clean(l, &format!("{ctx} (infobox label)"));
                        assert_span_clean(v, &format!("{ctx} (infobox value)"));
                    }
                }
                Block::Image { alt, caption, .. } => {
                    assert_span_clean(alt, &format!("{ctx} (alt)"));
                    if let Some(c) = caption {
                        assert_span_clean(c, &format!("{ctx} (caption)"));
                    }
                }
                Block::Gallery(items) => {
                    for it in items {
                        assert_span_clean(&it.caption, &format!("{ctx} (gallery caption)"));
                    }
                }
                Block::Math { tex, .. } => assert_span_clean(tex, &format!("{ctx} (display math)")),
                Block::Rule => {}
            }
        }
        for cit in &doc.citations {
            assert_span_clean(&cit.id, &format!("{ctx} (citation id)"));
            assert_span_clean(&cit.text, &format!("{ctx} (citation text)"));
            if let Some(u) = &cit.url {
                assert_span_clean(u, &format!("{ctx} (citation url)"));
            }
        }
    }

    fn assert_laid_lines_clean(lines: &[LaidLine], ctx: &str) {
        for (i, line) in lines.iter().enumerate() {
            for span in &line.spans {
                assert_span_clean(&span.text, &format!("{ctx} (laid line {i})"));
            }
        }
    }

    /// The property test itself: a deterministic, index-based generative
    /// loop (not a fuzzing framework — none is a dependency here) combining
    /// every hostile fragment with every text-bearing slot at varying
    /// positions within the slot's own text (fragment alone, fragment
    /// prefixed, fragment suffixed), asserting the whole `Document` — and
    /// every laid-out line built from it — is clean either way.
    #[test]
    fn hostile_fragments_never_survive_across_every_text_position() {
        const ITERATIONS: usize = HOSTILE_FRAGMENTS.len() * SLOT_COUNT * 3;
        for i in 0..ITERATIONS {
            let fragment = HOSTILE_FRAGMENTS[i % HOSTILE_FRAGMENTS.len()];
            let slot = (i / HOSTILE_FRAGMENTS.len()) % SLOT_COUNT;
            let payload = match i % 3 {
                0 => fragment.to_string(),
                1 => format!("normal{fragment}"),
                _ => format!("{fragment}normal"),
            };
            let html = build_html(slot, &payload);
            let ctx = format!("iteration {i} slot {slot} fragment {fragment:?}");

            let doc = parse_article_html("Hostile", &html);
            assert_document_clean(&doc, &ctx);

            let layout = layout_document(&doc, 80, LayoutOptions::default());
            assert_laid_lines_clean(&layout.lines, &ctx);
        }
    }

    /// SEC-1 companion to the document test above, for the account/social and
    /// ZIM surfaces added in later chunks (C2/C3): every hostile fragment,
    /// embedded in every remote-derived field those `parse_*` functions read,
    /// must be stripped at the parse boundary before it can reach a `ratatui`
    /// render site or the persisted store. Uses `serde_json` to build the
    /// bodies so even the raw-control-byte fragments (NUL, ESC, C1) are encoded
    /// as valid JSON, exactly as a real server would send them.
    #[test]
    fn account_and_zim_fields_never_survive_hostile_input_at_the_parse_boundary() {
        use crate::account;

        for (i, fragment) in HOSTILE_FRAGMENTS.iter().enumerate() {
            let payload = match i % 3 {
                0 => (*fragment).to_string(),
                1 => format!("normal{fragment}"),
                _ => format!("{fragment}normal"),
            };
            let ctx = format!("fragment {fragment:?}");

            let body = serde_json::json!({
                "query": { "usercontribs": [
                    { "title": payload, "timestamp": payload, "comment": payload,
                      "revid": 1, "sizediff": 0 }
                ]}
            })
            .to_string();
            let contribs = account::parse_usercontribs(body.as_bytes()).unwrap();
            assert_span_clean(&contribs[0].title, &format!("{ctx} usercontribs.title"));
            assert_span_clean(
                &contribs[0].timestamp,
                &format!("{ctx} usercontribs.timestamp"),
            );
            assert_span_clean(
                contribs[0].comment.as_deref().unwrap(),
                &format!("{ctx} usercontribs.comment"),
            );

            let body = serde_json::json!({
                "query": { "notifications": { "list": [
                    { "id": "1", "type": "alert", "text": payload, "read": false,
                      "timestamp": payload }
                ]}}
            })
            .to_string();
            let list = account::parse_notif_list(body.as_bytes()).unwrap();
            assert_span_clean(&list[0].text, &format!("{ctx} notif.text"));

            let body = serde_json::json!({
                "query": { "watchlist": [
                    { "title": payload, "user": payload, "timestamp": payload,
                      "comment": payload, "revid": 1, "old_revid": 0 }
                ]}
            })
            .to_string();
            let changes = account::parse_watchlist_changes(body.as_bytes()).unwrap();
            assert_span_clean(&changes[0].title, &format!("{ctx} watchlist.title"));
            assert_span_clean(&changes[0].user, &format!("{ctx} watchlist.user"));
            assert_span_clean(&changes[0].timestamp, &format!("{ctx} watchlist.timestamp"));
            assert_span_clean(
                changes[0].comment.as_deref().unwrap(),
                &format!("{ctx} watchlist.comment"),
            );

            let body = serde_json::json!({ "watchlistraw": [ { "ns": 0, "title": payload } ] })
                .to_string();
            let titles = account::parse_watchlistraw(body.as_bytes()).unwrap();
            assert_span_clean(&titles[0], &format!("{ctx} watchlistraw.title"));

            let body = serde_json::json!({
                "query": { "userinfo": { "id": 1, "name": "U",
                    "options": { "skin": payload, "language": payload } } }
            })
            .to_string();
            let prefs = account::parse_userinfo_options(body.as_bytes()).unwrap();
            assert_span_clean(prefs.skin.as_deref().unwrap(), &format!("{ctx} prefs.skin"));
            assert_span_clean(
                prefs.language.as_deref().unwrap(),
                &format!("{ctx} prefs.language"),
            );

            let body = serde_json::json!({
                "readinglists": { "entries": [ { "id": 7, "project": "", "title": payload } ] }
            })
            .to_string();
            let entries = account::parse_readinglist_entries(body.as_bytes());
            assert_span_clean(&entries[0].title, &format!("{ctx} readinglist.title"));
        }
    }
}

// ===========================================================================
// Perf smoke (PRD §6.8, light)
// ===========================================================================

/// PRD §9's "performance regression: criterion benches ... wired to CI
/// thresholds (§6.8)", scaled down to something this environment can run
/// honestly. §6.8's own target ("<500 ms on a 2020-era laptop" for
/// parse+layout of a 1.5 MB / 500+-ref pathological article) is a wall-clock
/// promise this sandboxed, possibly shared/throttled environment cannot be
/// trusted to hold to — the same reason `doc.rs`'s own
/// `oversized_html_is_truncated_and_flagged_with_a_banner` deliberately
/// dropped its own timing assertion entirely rather than tighten it (a
/// hard bound flakes under load from an unrelated cause, which is a false
/// failure, not a real regression signal).
///
/// This test keeps a *generous* bound (a large multiple of the PRD target)
/// so it still catches a catastrophic regression (an accidental O(n²) in
/// the parser or the wrap loop), and is `#[ignore]`d so it never
/// contributes flaky noise to a normal `cargo test` run. Run explicitly with:
/// `cargo test --release corpus_tests::perf_smoke -- --ignored`
#[test]
#[ignore]
fn perf_smoke_parse_and_layout_the_pathological_corpus() {
    let html = fixtures::large_article_html(1_600_000);
    let refs_html = fixtures::many_references_html(550);

    let start = std::time::Instant::now();
    let big_doc = parse_article_html("Large Article", &html);
    let refs_doc = parse_article_html("Big Refs", &refs_html);
    let _layout = layout_document(&big_doc, 80, LayoutOptions::default());
    let _refs_layout = layout_document(&refs_doc, 80, LayoutOptions::default());
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "parse+layout of the 1.5 MB / 500+-ref pathological corpus took {elapsed:?} — a \
         generous 10s bound (20x the PRD §6.8 500ms target) meant to catch a catastrophic \
         regression, not to hold this sandboxed environment to the PRD's real \
         2020-laptop target"
    );
}
