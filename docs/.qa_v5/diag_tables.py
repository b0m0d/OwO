# -*- coding: utf-8 -*-
"""Identify each table by its first-row text, and dump raw pPr XML of one body cell."""
import glob, os
from docx import Document
from docx.oxml.ns import qn
from lxml import etree

DOCS = os.path.join(r"T:\创新创业\OwO-master\docs")
SRC = [x for x in glob.glob(os.path.join(DOCS, "Cuttle*v8.docx")) if "预览" not in x][0]
doc = Document(SRC)

for ti, t in enumerate(doc.tables):
    hdr = " | ".join(c.text.strip().replace("\n", "/")[:20] for c in t.rows[0].cells)
    print("T%02d  %s" % (ti, hdr))
    if "胶囊" in hdr or "字段" in hdr:
        print("   >>> BODY CELL XML SAMPLE (row1 col2):")
        c = t.rows[1].cells[2]
        x = etree.tostring(c._tc, pretty_print=True, encoding="unicode")
        print("\n".join("      " + l for l in x.splitlines()[:40]))
