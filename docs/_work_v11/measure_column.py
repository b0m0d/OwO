# -*- coding: utf-8 -*-
"""Measure the actual text column geometry in the rendered PDF and compare with pgMar."""
import collections

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf"
doc = pymupdf.open(PDF)

PT = 1 / 72 * 2.54
print('页宽 %.2f pt (%.2f cm)' % (doc[0].rect.width, doc[0].rect.width * PT))
print('pgMar 左/右 = 2.05 / 2.05 cm -> 期望文本列 [%.2f, %.2f] pt' % (
    2.05 / 2.54 * 72, 595.276 - 2.05 / 2.54 * 72))

lefts = collections.Counter()
rights = collections.Counter()
for pno in range(2, doc.page_count):
    page = doc[pno]
    for blk in page.get_text('dict')['blocks']:
        if blk['type'] != 0:
            continue
        for line in blk['lines']:
            txt = ''.join(s['text'] for s in line['spans']).strip()
            if len(txt) < 12:
                continue
            lefts[round(line['bbox'][0], 1)] += 1
            rights[round(line['bbox'][2], 1)] += 1

print('\n最常见的行左边界 (pt -> 次数):')
for v, c in lefts.most_common(6):
    print('   %.1f pt = %.3f cm   x%d' % (v, v * PT, c))
print('\n最常见的行右边界 (pt -> 次数):')
for v, c in rights.most_common(6):
    print('   %.1f pt = %.3f cm   x%d' % (v, v * PT, c))
