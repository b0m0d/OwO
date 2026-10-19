# -*- coding: utf-8 -*-
import io, re, zipfile
from docx import Document
from docx.oxml.ns import qn
from docx.table import Table
from docx.text.paragraph import Paragraph

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
body = doc.element.body
out = []
pi = ti = 0
for child in body.iterchildren():
    tag = child.tag.split('}')[-1]
    if tag == 'p':
        p = Paragraph(child, doc)
        t = p.text.strip()
        out.append(f"[P{pi:04d}|{p.style.name}] {t}")
        pi += 1
    elif tag == 'tbl':
        t = Table(child, doc)
        out.append(f"===== TABLE {ti} ({len(t.rows)}x{len(t.columns)}) =====")
        for ri, row in enumerate(t.rows):
            out.append(f"  T{ti}R{ri}: " + " | ".join(c.text.strip().replace('\n',' / ') for c in row.cells))
        out.append(f"===== END TABLE {ti} =====")
        ti += 1
io.open(r"T:\创新创业\OwO-master\docs\.qa_v5\v8_dump.txt","w",encoding="utf-8").write("\n".join(out))

# structural checks
issues = []
paras = doc.paragraphs
if any(not p.text.strip() for p in paras if p.style.name.startswith("Heading")):
    issues.append("EMPTY HEADING")
for p in paras:
    t = p.text.strip()
    if t.endswith(("，", "、", "：", "；")) and len(t) > 25:
        issues.append(f"SUSPECT TRUNCATION: {t[:80]}")
    if t.startswith("[12] Xiaomi") and "[" in t[5:]:
        issues.append("POLLUTED REF: " + t[:60])
# caption continuity
caps_t = [p.text.strip() for p in paras if re.match(r"^表 \d+ ", p.text.strip())]
caps_f = [p.text.strip() for p in paras if re.match(r"^图 \d+ ", p.text.strip())]
tn = [int(re.match(r"^表 (\d+)", c).group(1)) for c in caps_t]
fn = [int(re.match(r"^图 (\d+)", c).group(1)) for c in caps_f]
if tn != list(range(1, len(tn)+1)): issues.append(f"TABLE CAPTION SEQ: {tn}")
if fn != list(range(1, len(fn)+1)): issues.append(f"FIGURE CAPTION SEQ: {fn}")
print("paragraphs", pi, "tables", ti)
print("table captions", len(tn), tn)
print("figure captions", len(fn), fn)
print("ISSUES:", len(issues))
for i in issues: print("  !", i)