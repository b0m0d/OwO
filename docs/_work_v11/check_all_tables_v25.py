# -*- coding: utf-8 -*-
"""全表校验：docx 里每张表的每个单元格文本，是否都出现在 PDF 中。"""
import re

import docx
import pymupdf

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.docx"
PDF = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.pdf"

d = docx.Document(DOCX)
doc = pymupdf.open(PDF)
FLAT = [re.sub(r'\s+', '', doc[i].get_text()) for i in range(doc.page_count)]
ALL = ''.join(FLAT)

print('=== 逐表逐格核对 ===')
missing_total = 0
for ti, t in enumerate(d.tables, 1):
    miss = []
    for ri, row in enumerate(t.rows):
        for ci, c in enumerate(row.cells):
            txt = re.sub(r'\s+', '', c.text)
            if len(txt) < 3:
                continue
            if txt not in ALL:
                miss.append((ri, ci, c.text.strip()[:40]))
    head = ' '.join(x.text for x in t.rows[0].cells)[:40]
    flag = '✔' if not miss else '✘ 缺 %d 格' % len(miss)
    print('  表%-3d %-42s %s' % (ti, head, flag))
    for m in miss[:8]:
        print('        r%-2d c%-2d %r' % m)
    missing_total += len(miss)

print('\n合计缺失单元格: %d' % missing_total)
print('物理页数: %d' % doc.page_count)
FOOT = []
for i in range(doc.page_count):
    f = [b[4].replace('\n', '').strip() for b in doc[i].get_text('blocks') if b[1] > 795]
    FOOT.append(f[0] if f else None)
print('页脚序列: %s' % ' '.join(x if x else '—' for x in FOOT))
print('空白页: %s' % ([i + 1 for i in range(doc.page_count)
                      if not doc[i].get_text().strip() and not doc[i].get_images()] or '无'))

