# -*- coding: utf-8 -*-
"""Report the horizontal alignment of every cell in every table."""
import glob, os
from docx import Document
from docx.oxml.ns import qn

DOCS = os.path.join(r"T:\创新创业\OwO-master\docs")
SRC = [x for x in glob.glob(os.path.join(DOCS, "Cuttle*v8.docx")) if "预览" not in x][0]
doc = Document(SRC)

def h_align(c):
    vals = []
    for p in c.paragraphs:
        if p.alignment is None:
            vals.append("inherit")
        else:
            vals.append(str(p.alignment).split(" ")[0])
    return vals

for ti, t in enumerate(doc.tables):
    if ti <= 1:
        continue
    hdr = [c.text.strip()[:10] for c in t.rows[0].cells]
    print("=== T%d rows=%d cols=%d" % (ti, len(t.rows), len(t.columns)))
    print("   header:", hdr)
    print("   header align:", [h_align(c) for c in t.rows[0].cells])
    for ri in (1, 2):
        if ri < len(t.rows):
            print("   R%d align: %s | %s" % (ri, [h_align(c) for c in t.rows[ri].cells],
                                             [c.text.strip()[:16] for c in t.rows[ri].cells]))
