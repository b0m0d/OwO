# -*- coding: utf-8 -*-
"""Inspect the TOC entry XML to find why dot leaders are missing, then restore them."""
import glob, os, copy
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement
from lxml import etree

DOCS = os.path.join(r"T:\创新创业\OwO-master\docs")
SRC = [x for x in glob.glob(os.path.join(DOCS, "Cuttle*v8.docx")) if "预览" not in x][0]
doc = Document(SRC)

def visible(p):
    return "".join(t.text or "" for t in p._element.iter(qn('w:t')))

count = 0
for p in doc.paragraphs:
    el = p._element
    lbl = visible(p).strip()
    if not lbl or len(lbl) > 60:
        continue
    if el.findall('.//' + qn('w:tab')):
        count += 1
        if count <= 2:
            x = etree.tostring(el, pretty_print=True, encoding="unicode")
            import re
            x = re.sub(r'\sxmlns:[a-zA-Z0-9]+="[^"]+"', '', x)
            print("=== entry:", lbl)
            print(x[:900])
print("paragraphs containing w:tab:", count)

# 统计 tabs 定义
ntabsdef = sum(1 for p in doc.paragraphs if p._element.findall('.//' + qn('w:tabs')))
tabels = sum(1 for p in doc.paragraphs for _ in p._element.findall('.//' + qn('w:tab')))
print("paragraphs with tabs def:", ntabsdef, "| w:tab elements:", tabels)
