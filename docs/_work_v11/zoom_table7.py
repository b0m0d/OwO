# -*- coding: utf-8 -*-
"""裁出 v25 中表7 区域原尺寸图，量出题注/表格的 x 范围与表格列宽。"""
import os

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.pdf"
OUT = r"T:\创新创业\OwO-master\docs\_work_v11\inspect"

doc = pymupdf.open(PDF)
page = doc[19]   # 物理 p20

# 找题注与表格的 bbox
cap = page.search_for('表7  产品版本、定价与单位经济')
print('题注 bbox: %s' % (cap,))
rects = [d['rect'] for d in page.get_drawings()]
tbl = None
for r in rects:
    if r.width > 400 and r.height > 150:
        tbl = r if tbl is None else tbl | r
print('表格外框: %s' % tbl)

if cap and tbl:
    print('题注中心 x = %.1f；表格中心 x = %.1f；差 %.1f pt' % (
        (cap[0].x0 + cap[0].x1) / 2, (tbl.x0 + tbl.x1) / 2,
        abs((cap[0].x0 + cap[0].x1) / 2 - (tbl.x0 + tbl.x1) / 2)))
    print('题注 x %.1f..%.1f，表格 x %.1f..%.1f' % (cap[0].x0, cap[0].x1, tbl.x0, tbl.x1))

# 竖线 x -> 列宽
xs = sorted({round(d['rect'].x0, 1) for d in page.get_drawings()
             if d['rect'].width < 2 and d['rect'].height > 20 and 460 < d['rect'].y0 < 710})
print('表格竖线 x: %s' % xs)
for a, b in zip(xs, xs[1:]):
    print('   列宽 %.1f pt = %.2f cm' % (b - a, (b - a) / 72 * 2.54))

# 裁图
clip = pymupdf.Rect(50, 660, 560, 900)
pix = page.get_pixmap(dpi=170, clip=clip)
p = os.path.join(OUT, 'v25_table7_zoom.png')
os.makedirs(OUT, exist_ok=True)
pix.save(p)
print('已裁出 %s (%dx%d)' % (p, pix.width, pix.height))
