# -*- coding: utf-8 -*-
import io
from docx import Document
from docx.oxml.ns import qn
SRC = r"T:\创新创业\附件1-4：中国国际大学生创新大赛（2026）西南大学校内选拔赛预通知相关材料\附件1-4：中国国际大学生创新大赛（2026）西南大学校内选拔赛预通知相关材料\附件2：中国国际大学生创新大赛（2025）评审规则.docx"
d = Document(SRC)
out = []
for p in d.paragraphs:
    t = p.text.strip()
    if t:
        out.append(t)
for ti, t in enumerate(d.tables):
    out.append(f"=== TABLE {ti} ===")
    for row in t.rows:
        out.append(" | ".join(c.text.strip().replace("\n", " ") for c in row.cells))
io.open(r"T:\创新创业\OwO-master\docs\.qa_v5\rules.txt","w",encoding="utf-8").write("\n".join(out))
print("paragraphs:", len(out))