# -*- coding: utf-8 -*-
"""Final verification pass on the rendered v11 PDF."""
import os

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v11.pdf"
OUT = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\pages"
os.makedirs(OUT, exist_ok=True)

doc = pymupdf.open(PDF)
print('pages:', doc.page_count)

checks = [
    ('3.4 原位回填机制', 11),
    ('4.4 关键工程体系 原位回填与策略验证恢复', 14),
    ('图7  第一年试点容量与验证容量测算', None),
]
for needle, expected in checks:
    pages = [i + 1 for i, p in enumerate(doc) if needle in p.get_text()]
    if expected is None:
        print('%-42s found on %s' % (needle, pages))
    else:
        print('%-42s expected p%-3d found %-12s %s' % (
            needle, expected, pages, 'OK' if expected in pages else 'MISMATCH'))

# render TOC page and every figure page for a final look
render = sorted(set([2] + [i + 1 for i, p in enumerate(doc)
                           if any(('图%d ' % n) in p.get_text() for n in range(1, 12))]))
print('\nrendering:', render)
for pno in render:
    pix = doc[pno - 1].get_pixmap(dpi=110)
    path = os.path.join(OUT, 'final_p%03d.png' % pno)
    pix.save(path)
print('done')
