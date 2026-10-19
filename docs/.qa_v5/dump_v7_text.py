# -*- coding: utf-8 -*-
"""Dump v7 docx body order (paragraphs + tables) to a UTF-8 text file."""
import sys, io
from docx import Document
from docx.table import Table
from docx.text.paragraph import Paragraph

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v7.docx"
OUT = r"T:\创新创业\OwO-master\docs\.qa_v5\v7_dump.txt"

doc = Document(SRC)
body = doc.element.body

lines = []
pi = 0
ti = 0
for child in body.iterchildren():
    tag = child.tag.split('}')[-1]
    if tag == 'p':
        p = Paragraph(child, doc)
        txt = p.text.strip()
        style = p.style.name if p.style is not None else ''
        if txt:
            lines.append(f"[P{pi:04d}|{style}] {txt}")
        else:
            lines.append(f"[P{pi:04d}|{style}] <empty>")
        pi += 1
    elif tag == 'tbl':
        t = Table(child, doc)
        lines.append(f"===== TABLE {ti} ({len(t.rows)}x{len(t.columns)}) =====")
        for ri, row in enumerate(t.rows):
            cells = [c.text.strip().replace('\n', ' / ') for c in row.cells]
            lines.append(f"  T{ti}R{ri}: " + " | ".join(cells))
        lines.append(f"===== END TABLE {ti} =====")
        ti += 1

with io.open(OUT, 'w', encoding='utf-8') as f:
    f.write("\n".join(lines))

print(f"paragraphs={pi} tables={ti} out={OUT}")
