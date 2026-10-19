# -*- coding: utf-8 -*-
"""Renumber table captions after removing 表 7 and fix all cross-references."""
from docx import Document
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, text):
    runs = p.runs
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

RENUM = [
 ("表 8 产品版本、定价与单位经济", "表 7 产品版本、定价与单位经济"),
 ("表 9 团队分工与工程基础", "表 8 团队分工与工程基础"),
 ("表 10 三年经营预测", "表 9 三年经营预测"),
 ("表 11 项目风险登记表", "表 10 项目风险登记表"),
]
for old, new in RENUM:
    for p in doc.paragraphs:
        if p.text.strip() == old:
            set_text(p, new); print("  ", old, "->", new); break

for p in doc.paragraphs:
    t = p.text
    if "三年经营口径见表 10" in t:
        set_text(p, t.replace("三年经营口径见表 10", "三年经营口径见表 9"))
        print("  ref fixed: 三年经营口径 -> 表 9")
doc.save(SRC)

d = Document(SRC)
import re
for p in d.paragraphs:
    tt = p.text.strip()
    if re.match(r"^表 \d+ ", tt):
        print("  CAP:", tt)