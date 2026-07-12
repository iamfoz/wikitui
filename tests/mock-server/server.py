import http.server, urllib.parse, json

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

}

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

PAGES["計算機科学"] = """<html><head><title>計算機科学</title></head><body>
  <p>計算機科学は、情報と計算の理論的基礎、およびそのコンピュータ上への実装と応用に関する研究分野である。</p>
</body></html>"""

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        parts = parsed.path.split('/')
        if '/page/' in parsed.path and parsed.path.endswith('/html'):
            title = urllib.parse.unquote(parts[-2])
            html = PAGES.get(title)
            if html is None:
                self.send_response(404)
                self.end_headers()
                return
            body = html.encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/html')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, *a):
        pass

http.server.HTTPServer(('127.0.0.1', 8943), Handler).serve_forever()
