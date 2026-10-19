# -*- coding: utf-8 -*-
"""Inspect the TOC paragraphs: style, tab elements, and current text."""
from docx import Document
from docx.oxml.ns import qn

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
for i, p in enumerate(doc.paragraphs[:30]):
    el = p._element
    ntabs = len(el.findall('.//' + qn('w:tab')))
    ntabsdef = len(el.findall('.//' + qn('w:tabs')))
    if ntabs or ntabsdef:
        print(f"P{i:03d} style={p.style.name} w:tab={ntabs} tabsdef={ntabsdef} text={p.text.strip()[:56]!r}")
