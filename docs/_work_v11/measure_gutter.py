# -*- coding: utf-8 -*-
"""量出目录各段距离：条目↔页码、页码↔中缝竖线、竖线↔右栏条目。"""
import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v23.pdf"
page = pymupdf.open(PDF)[1]

spans = [(s['bbox'], s['text']) for blk in page.get_text('dict')['blocks'] if blk['type'] == 0
         for l in blk['lines'] for s in l['spans'] if s['text'].strip()]

# 按行归并：左栏（x<300）与右栏（x>=300）
rows = {}
for bbox, t in spans:
    y = round(bbox[1], 0)
    rows.setdefault(y, []).append((bbox, t))

left_lab_end, left_num_end, right_lab_start, right_num_start = [], [], [], []
for y, items in sorted(rows.items()):
    items.sort(key=lambda x: x[0][0])
    for bbox, t in items:
        s = t.strip()
        if bbox[0] < 296:                      # 左半
            if s.isdigit():
                left_num_end.append(bbox[2])
            else:
                left_lab_end.append(bbox[2])
        elif bbox[0] >= 300:                   # 右半
            if s.isdigit():
                right_num_start.append(bbox[0])
            else:
                right_lab_start.append(bbox[0])

def rng(name, v):
    if v:
        print('  %-16s min=%.1f max=%.1f' % (name, min(v), max(v)))

print('=== 各元素边界 (pt) ===')
rng('左栏条目文字右端', left_lab_end)
rng('左栏页码右端', left_num_end)
rng('右栏条目文字左端', right_lab_start)

# 竖线位置
sep = [d for d in page.get_drawings() if d['rect'].width < 2 and d['rect'].height > 300]
sep_x = sep[0]['rect'].x0 if sep else None
print('  中缝竖线 x = %.1f' % sep_x)

if left_lab_end and left_num_end:
    print('\n=== 距离 ===')
    g1 = min(left_num_end) - max(left_lab_end)
    print('  条目 → 自己的页码      : %.1f pt = %.2f cm（最大值口径 %.1f）' % (
        g1, g1 / 72 * 2.54, min(left_num_end) - min(left_lab_end)))
    g2 = sep_x - max(left_num_end)
    print('  左栏页码 → 竖线        : %.1f pt = %.2f cm' % (g2, g2 / 72 * 2.54))
    g3 = min(right_lab_start) - sep_x
    print('  竖线 → 右栏条目        : %.1f pt = %.2f cm' % (g3, g3 / 72 * 2.54))
    print('  中缝净空（左页码→右条目）: %.1f pt = %.2f cm' % (
        min(right_lab_start) - max(left_num_end),
        (min(right_lab_start) - max(left_num_end)) / 72 * 2.54))
