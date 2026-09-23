# -*- coding: utf-8 -*-
"""Compare overflow across the margin experiment variants."""
import pymupdf

PDFS = {
    'v13 (right=2.05cm)': (r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf", 2.05),
    'v13 right=2.29cm': (r"T:\创新创业\OwO-master\docs\_work_v11\exp\v13_rmar1300.pdf", 2.29),
    'v13 right=2.38cm': (r"T:\创新创业\OwO-master\docs\_work_v11\exp\v13_rmar1350.pdf", 2.38),
}

for label, (path, rcm) in PDFS.items():
    RIGHT = 595.276 - (rcm / 2.54 * 72)
    doc = pymupdf.open(path)
    over = total = 0
    worst = 0.0
    pages = set()
    for pno in range(doc.page_count):
        for blk in doc[pno].get_text('dict')['blocks']:
            if blk['type'] != 0:
                continue
            for line in blk['lines']:
                txt = ''.join(s['text'] for s in line['spans']).strip()
                if len(txt) < 12:
                    continue
                total += 1
                if line['bbox'][2] > RIGHT + 1:
                    over += 1
                    pages.add(pno + 1)
                    worst = max(worst, line['bbox'][2] - RIGHT)
    print('%-22s 页数 %-3d 长行 %-4d 越界 %-3d (%.1f%%) 最严重 +%.2f cm 涉及 %d 页' % (
        label, doc.page_count, total, over, over / total * 100 if total else 0,
        worst / 72 * 2.54, len(pages)))
