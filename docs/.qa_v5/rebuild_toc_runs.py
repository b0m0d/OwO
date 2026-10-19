# -*- coding: utf-8 -*-
"""Rebuild each TOC entry's runs explicitly: [text][tab][page]."""
import glob, os, copy
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement

DOCS = os.path.join(r"T:\创新创业\OwO-master\docs")
SRC = [x for x in glob.glob(os.path.join(DOCS, "Cuttle*v8.docx")) if "预览" not in x][0]
doc = Document(SRC)

def visible(p):
    return "".join(t.text or "" for t in p._element.iter(qn('w:t')))

CONTENT = 9638
count = 0
for p in doc.paragraphs:
    el = p._element
    tabs = el.find('.//' + qn('w:tabs'))
    if tabs is None:
        continue
    label = visible(p).strip()
    if not label:
        continue
    parts = label.rsplit(".", 1)
    m = None
    import re
    mm = re.match(r"^(.*?)(\d+)$", label)
    if not mm:
        continue
    title, page = mm.group(1).strip(), mm.group(2)
    pPr = el.find(qn('w:pPr'))
    ind = pPr.find(qn('w:ind')) if pPr is not None else None
    left = 0
    if ind is not None and ind.get(qn('w:left')):
        try:
            left = int(ind.get(qn('w:left')))
        except ValueError:
            left = 0
    for t in tabs.findall(qn('w:tab')):
        t.set(qn('w:val'), 'right')
        t.set(qn('w:leader'), 'dot')
        t.set(qn('w:pos'), str(CONTENT - left - 40))
    # 保留首个 run 的 rPr
    rpr = None
    for r in el.findall(qn('w:r')):
        rp = r.find(qn('w:rPr'))
        if rp is not None:
            rpr = copy.deepcopy(rp)
            break
    for r in el.findall(qn('w:r')):
        el.remove(r)
    def mkrun(text=None, tab=False):
        r = OxmlElement('w:r')
        if rpr is not None:
            r.append(copy.deepcopy(rpr))
        if tab:
            r.append(OxmlElement('w:tab'))
        if text is not None:
            t = OxmlElement('w:t')
            t.set(qn('xml:space'), 'preserve')
            t.text = text
            r.append(t)
        return r
    el.append(mkrun(title))
    el.append(mkrun(tab=True))
    el.append(mkrun(page))
    count += 1
print("rebuilt entries:", count)
doc.save(SRC)
