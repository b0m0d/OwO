# -*- coding: utf-8 -*-
"""Measure the actual TOC row pitch in the rendered PDF."""
import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v15.pdf"
page = pymupdf.open(PDF)[1]

rows = []
for blk in page.get_text('dict')['blocks']:
    if blk['type'] != 0:
        continue
    for l in blk['lines']:
        t = ''.join(s['text'] for s in l['spans']).strip()
        rows.append((round(l['bbox'][1], 1), round(l['bbox'][3], 1), t[:34]))

rows.sort()
print('前 12 个文本行的 y 及相邻间距:')
prev = None
for y0, y1, t in rows[:12]:
    d = '' if prev is None else 'Δ=%.1f' % (y0 - prev)
    print('  y0=%-7.1f y1=%-7.1f %-8s %s' % (y0, y1, d, t))
    prev = y0

# left column only: pitch between consecutive 1.x / 2.x lines
lefts = [(y0, t) for y0, y1, t in rows if t[:2] in ('1.', '2.', '3.', '4.', '5.', '6.') and t[1].isdigit()]
print('\n左侧小节行 pitch:')
prev = None
deltas = []
for y0, t in lefts[:10]:
    if prev is not None:
        deltas.append(y0 - prev)
    print('  y=%-7.1f %s' % (y0, t))
    prev = y0
if deltas:
    print('  平均 pitch = %.2f pt' % (sum(deltas) / len(deltas)))

total = max(y1 for _, y1, _ in rows if y1 < 700)
print('\n目录条目区: 上 %.1f -> 下 %.1f，高 %.1f pt（可用 687 pt）' % (
    rows[0][0], total, total - rows[0][0]))
