# -*- coding: utf-8 -*-
"""Set explicit right indent and pull tab stops slightly inside the text area so the
dot leader renders in both Word and LibreOffice."""
import glob, os
from docx import Document
from docx.oxml.ns import qn

DOCS = os.path.join(r"T:\创新创业\OwO-master\docs")
SRC = [x for x in glob.glob(os.path.join(DOCS, "Cuttle*v8.docx")) if "预览" not in x][0]
doc = Document(SRC)

CONTENT = 9638
FUDGE = 40   # 略微内收，避免落在版心之外
fixed = 0
for p in doc.paragraphs:
    el = p._element
    tabs = el.find('.//' + qn('w:tabs'))
    if tabs is None:
        continue
    pPr = el.find(qn('w:pPr'))
    if pPr is None:
        continue
    ind = pPr.find(qn('w:ind'))
    if ind is None:
        ind = pPr.makeelement(qn('w:ind'), {})
        pPr.insert(0, ind)
    left = 0
    if ind.get(qn('w:left')):
        try:
            left = int(ind.get(qn('w:left')))
        except ValueError:
            left = 0
    ind.set(qn('w:firstLine'), '0')
    ind.set(qn('w:left'), str(left))
    ind.set(qn('w:right'), '0')
    pos = CONTENT - left - FUDGE
    for t in tabs.findall(qn('w:tab')):
        t.set(qn('w:val'), 'right')
        t.set(qn('w:leader'), 'dot')
        t.set(qn('w:pos'), str(pos))
        fixed += 1
print("entries fixed:", fixed)
doc.save(SRC)
