# -*- coding: utf-8 -*-
"""Verify TOC page numbers still match after the figure resizes."""
import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v11.pdf"
doc = pymupdf.open(PDF)

CHECK = [
    ('3.4 原位回填机制', 10),
    ('4.4 关键工程体系 原位回填与策略验证恢复', 14),
    ('2.2 用户研究方法', 7),
    ('3.3 交互与执行决策', 10),
    ('6.2 市场进入规模', 17),
    ('Table 1', None),
]

# collect heading-like lines: text page contains the heading string
for needle, expected in CHECK:
    if expected is None:
        continue
    pages = [i + 1 for i, p in enumerate(doc) if needle in p.get_text()]
    ok = expected in pages
    print('%-42s expected p%-3d found %-16s %s' % (needle, expected, pages, 'OK' if ok else 'MISMATCH'))
