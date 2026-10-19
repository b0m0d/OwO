# -*- coding: utf-8 -*-
"""Count non-empty characters per chapter (paragraphs + tables), to target rebalancing."""
import io, re
from docx import Document
from docx.table import Table
from docx.text.paragraph import Paragraph
from docx.oxml.ns import qn

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
body = doc.element.body

def txt_of(el):
    return "".join(t.text or "" for t in el.iter(qn('w:t')))

chapters = []
cur = {"name": "前置(封面/目录)", "p": 0, "t": 0}
def close(c):
    chapters.append(dict(c))

for ch in body.iterchildren():
    tag = ch.tag.split('}')[-1]
    if tag == 'p':
        p = Paragraph(ch, doc)
        t = p.text.strip()
        if p.style.name == "Heading 1" and t:
            close(cur)
            cur = {"name": t, "p": 0, "t": 0}
        else:
            cur["p"] += len(t)
    elif tag == 'tbl':
        tb = Table(ch, doc)
        cells = set()
        for row in tb.rows:
            for c in row.cells:
                cells.add(id(c._tc))
        n = 0
        for row in tb.rows:
            seen = set()
            for c in row.cells:
                if id(c._tc) in seen: continue
                seen.add(id(c._tc))
                n += len(c.text.strip())
        cur["t"] += n
close(cur)

total = sum(c["p"] + c["t"] for c in chapters if not c["name"].startswith("前置"))
lines = []
for c in chapters:
    s = c["p"] + c["t"]
    pct = (s / total * 100) if total else 0
    lines.append(f"{c['name'][:34]:36s} 正文{c['p']:6d}  表格{c['t']:6d}  合计{s:6d}  {pct:5.1f}%")
lines.append(f"{'合计（不含前置）':36s} {total}")
out = "\n".join(lines)
io.open(r"T:\创新创业\OwO-master\docs\.qa_v5\counts.txt","w",encoding="utf-8").write(out)
print(out)