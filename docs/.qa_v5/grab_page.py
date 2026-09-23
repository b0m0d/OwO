# -*- coding: utf-8 -*-
"""Locate the page holding a given table caption and dump a PNG crop."""
import sys, os
import fitz

PDF = r"T:\创新创业\OwO-master\docs\.qa_v5\render_check\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.pdf"
doc = fitz.open(PDF)
target = sys.argv[1] if len(sys.argv) > 1 else "意图胶囊的字段定义"
pages = []
for i, page in enumerate(doc):
    txt = page.get_text()
    if target in txt:
        pages.append(i)
print("pages containing %r: %s" % (target, [p + 1 for p in pages]))
for p in pages[:3]:
    page = doc[p]
    pix = page.get_pixmap(dpi=150)
    out = os.path.join(os.path.dirname(PDF), "page_%02d.png" % (p + 1))
    pix.save(out)
    print("saved", out)
