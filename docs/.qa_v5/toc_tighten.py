# -*- coding: utf-8 -*-
"""Collapse the TOC field boundary paragraphs to zero height (they were adding a visible gap)."""
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def ptxt(p):
    return "".join(t.text or "" for t in p._element.iter(qn('w:t')))

fixed = 0
for p in doc.paragraphs:
    el = p._element
    has_fld = bool(el.findall('.//' + qn('w:fldChar')))
    if not has_fld:
        continue
    pPr = el.find(qn('w:pPr'))
    if pPr is None:
        pPr = OxmlElement('w:pPr'); el.insert(0, pPr)
    for tag in ('w:spacing', 'w:rPr'):
        old = pPr.find(qn(tag))
        if old is not None:
            pPr.remove(old)
    # 段前段后 0、行距固定 1pt
    sp = OxmlElement('w:spacing')
    sp.set(qn('w:before'), '0'); sp.set(qn('w:after'), '0')
    sp.set(qn('w:line'), '20'); sp.set(qn('w:lineRule'), 'exact')
    pPr.append(sp)
    rPr = OxmlElement('w:rPr')
    sz = OxmlElement('w:sz'); sz.set(qn('w:val'), '2'); rPr.append(sz)
    pPr.append(rPr)
    fixed += 1

# 缩小目录标题与首条之间的间距
for p in doc.paragraphs:
    if p.text.strip() == "目录":
        p.paragraph_format.space_after = 0
        break

doc.save(SRC)
print("collapsed field paragraphs:", fixed)
