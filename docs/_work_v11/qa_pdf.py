# -*- coding: utf-8 -*-
"""Page-level QA on the rendered PDF: font embedding, overflow, orphan captions,
blank pages, figure/caption separation, whitespace distribution."""
import collections
import os
import re

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf"
EMU = 360000

doc = pymupdf.open(PDF)
print('=' * 78)
print('PDF 总览')
print('=' * 78)
print('  页数: %d' % doc.page_count)
p0 = doc[0]
print('  页面尺寸: %.1f x %.1f pt (%.1f x %.1f cm)' % (
    p0.rect.width, p0.rect.height, p0.rect.width / 72 * 2.54, p0.rect.height / 72 * 2.54))
print('  PDF 版本: %s  加密: %s' % (doc.metadata.get('format'), doc.is_encrypted))

print()
print('=' * 78)
print('字体嵌入')
print('=' * 78)
fonts = collections.Counter()
for pno in range(doc.page_count):
    for f in doc[pno].get_fonts(full=True):
        fonts[(f[3], f[4])] += 1
for (name, typ), cnt in sorted(fonts.items()):
    emb = 'CJK' if re.search(r'[\u4e00-\u9fff]', name) or 'YaHei' in name or 'SimSun' in name else 'latin'
    print('  %-45s %-10s 出现在 %d 页' % (name, typ, cnt))

print()
print('=' * 78)
print('溢出 / 空白页检查')
print('=' * 78)
MARGIN_L, MARGIN_R = 2.05 / 2.54 * 72, 2.05 / 2.54 * 72
MARGIN_T, MARGIN_B = 1.90 / 2.54 * 72, 1.75 / 2.54 * 72
viol = 0
blank = []
for pno in range(doc.page_count):
    page = doc[pno]
    blocks = page.get_text('blocks')
    if not blocks and not page.get_images():
        blank.append(pno + 1)
    for b in blocks:
        x0, y0, x1, y1 = b[:4]
        if x0 < MARGIN_L - 6 or x1 > page.rect.width - MARGIN_R + 6:
            viol += 1
            print('  p%-3d 文本越界: x0=%.1f x1=%.1f (版心 %.1f..%.1f)' % (
                pno + 1, x0, x1, MARGIN_L, page.rect.width - MARGIN_R))
            break
print('  文本越界页数: %d' % viol)
print('  空白页: %s' % (blank or '无'))

print()
print('=' * 78)
print('图表与图注是否同页')
print('=' * 78)
caption_re = re.compile(r'^(图|表)\s?(\d+)')
for pno in range(doc.page_count):
    page = doc[pno]
    d = page.get_text('dict')
    imgs = page.get_image_info()
    img_bottoms = [i['bbox'][3] for i in imgs]
    for blk in d['blocks']:
        if blk['type'] != 0:
            continue
        line = ''.join(s['text'] for l in blk['lines'] for s in l['spans']).strip()
        m = caption_re.match(line)
        if m and line.startswith('图'):
            # is there a figure image on this page above the caption?
            above = [b for b in img_bottoms if b < blk['bbox'][1] + 4]
            ok = '同页' if above else '!! 图注与图分离'
            print('  p%-3d %-46s %s' % (pno + 1, line[:46], ok))

print()
print('=' * 78)
print('页面底部留白（>6cm 视为可能的大块空白）')
print('=' * 78)
for pno in range(doc.page_count):
    page = doc[pno]
    blocks = [b for b in page.get_text('blocks')]
    imgs = page.get_image_info()
    bottoms = [b[3] for b in blocks] + [i['bbox'][3] for i in imgs]
    if not bottoms:
        continue
    bottom = max(bottoms)
    gap = (page.rect.height - MARGIN_B) - bottom
    if gap / 72 * 2.54 > 6:
        print('  p%-3d 底部留白 %.2f cm' % (pno + 1, gap / 72 * 2.54))
