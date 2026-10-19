# -*- coding: utf-8 -*-
"""Remove the 表 6 page break (it created a near-empty page) and let it flow after 图 7."""
from docx import Document
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
for p in doc.paragraphs:
    if p.text.strip() == "表 6 首年验证容量与市场空间口径":
        p.paragraph_format.page_break_before = False
        p.paragraph_format.keep_with_next = True
        print("  page break removed from 表 6 caption")
doc.save(SRC)