# -*- coding: utf-8 -*-
"""Identify what overflows the right margin, and compare v10 vs v13."""
import pymupdf

PDFS = {
    'v10': r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v11.pdf",  # placeholder, replaced below
}
V13 = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf"

RIGHT_EDGE = 595.276 - (2.05 / 2.54 * 72)
print('右版心边界 x = %.1f pt (%.2f cm)' % (RIGHT_EDGE, RIGHT_EDGE / 72 * 2.54))

doc = pymupdf.open(V13)
targets = [3, 11, 13, 18, 21]
for pno in targets:
    page = doc[pno - 1]
    print()
    print('--- p%d ---' % pno)
    d = page.get_text('dict')
    worst = []
    for blk in d['blocks']:
        if blk['type'] != 0:
            continue
        for line in blk['lines']:
            x1 = line['bbox'][2]
            if x1 > RIGHT_EDGE + 1:
                txt = ''.join(s['text'] for s in line['spans'])
                worst.append((x1, txt))
    worst.sort(reverse=True)
    for x1, txt in worst[:4]:
        print('   x1=%.1f (+%.1f pt / +%.2f cm)  %s' % (
            x1, x1 - RIGHT_EDGE, (x1 - RIGHT_EDGE) / 72 * 2.54, txt[:60]))
    # drawings/tables
    for dr in page.get_drawings():
        r = dr['rect']
        if r.x1 > RIGHT_EDGE + 1:
            print('   矢量图形 x1=%.1f (+%.1f pt) 高 %.1f' % (r.x1, r.x1 - RIGHT_EDGE, r.height))
            break
    for img in page.get_image_info():
        bb = img['bbox']
        if bb[2] > RIGHT_EDGE + 1:
            print('   图片 x1=%.1f (+%.1f pt)' % (bb[2], bb[2] - RIGHT_EDGE))
