# -*- coding: utf-8 -*-
"""Fix TOC tab stops: right-aligned dot leader must sit at the right text margin,
which is 9638 - indent for indented entries."""
import glob, os
from docx import Document
from docx.oxml.ns import qn

DOCS = os.path.join(r"T:\创新创业\OwO-master\docs")
SRC = [x for x in glob.glob(os.path.join(DOCS, "Cuttle*v8.docx")) if "预览" not in x][0]
doc = Document(SRC)

CONTENT = 9638
n = 0
for p in doc.paragraphs:
    el = p._element
    tabs = el.find('.//' + qn('w:tabs'))
    if tabs is None:
        continue
    pPr = el.find(qn('w:pPr'))
    ind = pPr.find(qn('w:ind')) if pPr is not None else None
    left = 0
    if ind is not None and ind.get(qn('w:left')):
        try:
            left = int(ind.get(qn('w:left')))
        except ValueError:
            left = 0
    pos = CONTENT - left
    for t in tabs.findall(qn('w:tab')):
        t.set(qn('w:val'), 'right')
        t.set(qn('w:leader'), 'dot')
        t.set(qn('w:pos'), str(pos))
        n += 1
print("tab stops fixed:", n)
doc.save(SRC)
