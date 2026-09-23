# -*- coding: utf-8 -*-
"""Locate figure pages in the rendered PDF and export them as PNGs."""
import os
import re

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v11.pdf"
OUT = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\pages"
os.makedirs(OUT, exist_ok=True)

doc = pymupdf.open(PDF)
print('pages:', doc.page_count)

targets = ['图1 ', '图2 ', '图3 ', '图4 ', '图5 ', '图6 ', '图7 ', '图8 ', '图9 ', '图10', '图11',
           '3.4 原位回填机制', '4.4 关键工程体系']

hits = {}
for i, page in enumerate(doc):
    text = page.get_text()
    for t in targets:
        if t in text and t not in hits:
            hits[t] = i

for t in targets:
    pno = hits.get(t)
    print('%-22s -> %s' % (t, ('page %d' % (pno + 1)) if pno is not None else 'NOT FOUND'))

pages_to_render = sorted(set(hits.values()))
print('\nrendering pages:', [p + 1 for p in pages_to_render])
for pno in pages_to_render:
    page = doc[pno]
    pix = page.get_pixmap(dpi=110)
    path = os.path.join(OUT, 'p%03d.png' % (pno + 1))
    pix.save(path)
    print('  wrote', path, pix.width, 'x', pix.height)
