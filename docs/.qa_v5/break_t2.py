# -*- coding: utf-8 -*-
from docx import Document
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
for p in doc.paragraphs:
    if p.text.strip() == "表 2 核心验证指标与决策门":
        p.paragraph_format.page_break_before = True
        p.paragraph_format.keep_with_next = True
        print("page break set before 表 2")
doc.save(SRC)