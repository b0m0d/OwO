# -*- coding: utf-8 -*-
"""Prevent table rows from splitting across pages, and keep the header row repeating."""
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
n = 0
for t in doc.tables:
    for ri, tr in enumerate(t._element.findall(qn('w:tr'))):
        trPr = tr.find(qn('w:trPr'))
        if trPr is None:
            trPr = OxmlElement('w:trPr')
            tr.insert(0, trPr)
        if trPr.find(qn('w:cantSplit')) is None:
            trPr.append(OxmlElement('w:cantSplit'))
            n += 1
        if ri == 0 and trPr.find(qn('w:tblHeader')) is None:
            trPr.append(OxmlElement('w:tblHeader'))
doc.save(SRC)
print("cantSplit added to", n, "rows")