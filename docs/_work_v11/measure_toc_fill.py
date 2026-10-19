# -*- coding: utf-8 -*-
"""Measure the real vertical extent of the TOC entries (excluding header/footer)."""
import os

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v15.pdf"
doc = pymupdf.open(PDF)
page = doc[1]
HEADER_Y = 1.90 / 2.54 * 72 - 20    # above the top margin = header zone
FOOTER_Y = page.rect.height - 1.75 / 2.54 * 72

blocks = page.get_text('blocks')
body = [b for b in blocks if b[1] > HEADER_Y and b[3] < FOOTER_Y + 30]
top = min(b[1] for b in body)
bottom = max(b[3] for b in body)
print('页面高 %.1f pt，下边距起始 %.1f pt' % (page.rect.height, FOOTER_Y))
print('目录实际内容: 上 %.1f -> 下 %.1f pt' % (top, bottom))
print('距下边距余量: %.1f pt (%.2f cm)' % (FOOTER_Y - bottom, (FOOTER_Y - bottom) / 72 * 2.54))
print()
print('最下面 6 个块:')
for b in sorted(body, key=lambda x: -x[3])[:6]:
    txt = b[4].replace('\n', ' ')[:60]
    print('   y=%.1f..%.1f  %s' % (b[1], b[3], txt))

os.makedirs(r"T:\创新创业\OwO-master\docs\_work_v11\pdf\pages", exist_ok=True)
page.get_pixmap(dpi=130).save(r"T:\创新创业\OwO-master\docs\_work_v11\pdf\pages\v15_toc.png")
print('\nrendered TOC preview')
