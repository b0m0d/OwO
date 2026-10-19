# -*- coding: utf-8 -*-
"""Compare right-margin overflow between the v10 baseline and v13."""
import collections

import pymupdf

PDFS = {
    'v10': r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v10.pdf",
    'v13': r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf",
}
RIGHT = 595.276 - (2.05 / 2.54 * 72)

for label, path in PDFS.items():
    doc = pymupdf.open(path)
    over = 0
    total = 0
    pages = set()
    worst = 0.0
    for pno in range(doc.page_count):
        page = doc[pno]
        for blk in page.get_text('dict')['blocks']:
            if blk['type'] != 0:
                continue
            for line in blk['lines']:
                txt = ''.join(s['text'] for s in line['spans']).strip()
                if len(txt) < 12:
                    continue
                total += 1
                x1 = line['bbox'][2]
                if x1 > RIGHT + 1:
                    over += 1
                    pages.add(pno + 1)
                    worst = max(worst, x1 - RIGHT)
    print('%s: 页数 %d，长行 %d，越界行 %d (%.1f%%)，最严重 +%.2f cm，涉及 %d 页' % (
        label, doc.page_count, total, over, over / total * 100 if total else 0,
        worst / 72 * 2.54, len(pages)))
    print('   越界页: %s' % sorted(pages))
    print()
