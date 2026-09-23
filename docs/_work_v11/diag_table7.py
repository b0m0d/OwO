# -*- coding: utf-8 -*-
"""检查表7（产品版本、定价与单位经济）在 v25 的分页与渲染。"""
import re

import docx
import pymupdf

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.docx"
PDF = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.pdf"

d = docx.Document(DOCX)
for i, t in enumerate(d.tables, 1):
    head = ' '.join(c.text.strip() for c in t.rows[0].cells)[:34]
    if '产品版本' in head:
        print('docx 表%d: %d 行' % (i, len(t.rows)))
        for r, row in enumerate(t.rows):
            print('   r%d %s' % (r, ' | '.join(c.text.strip()[:22] for c in row.cells)))

doc = pymupdf.open(PDF)
print('\n=== PDF ===')
for i in range(doc.page_count):
    txt = doc[i].get_text()
    if '产品版本、定价与单位经济' in txt:
        lines = [l.strip() for l in txt.splitlines() if l.strip()]
        foot = [b[4].strip() for b in doc[i].get_text('blocks') if b[1] > 795]
        print('表7 起于物理 p%d（页脚 %s），该页共 %d 行' % (i + 1, foot, len(lines)))
        j = lines.index('表7  产品版本、定价与单位经济') if '表7  产品版本、定价与单位经济' in lines else 0
        print('  ---- 从题注起 ----')
        for line in lines[j:]:
            print('    ', line[:74])
        # 表格绘制区域
        rects = [x['rect'] for x in doc[i].get_drawings()]
        if rects:
            print('  图元 y 范围 %.0f..%.0f' % (min(r.y0 for r in rects), max(r.y1 for r in rects)))
        # 下一页开头
        if i + 1 < doc.page_count:
            nxt = [l.strip() for l in doc[i + 1].get_text().splitlines() if l.strip()]
            print('  ---- 下一页开头 ----')
            for line in nxt[:10]:
                print('    ', line[:74])
        break
