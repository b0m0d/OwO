# -*- coding: utf-8 -*-
"""Refresh the TOC field's cached page numbers to match the final pagination."""
import copy, re
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement
from docx.text.paragraph import Paragraph

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
PAGES = {"项目摘要": "3", "第一章": "4", "第二章": "7", "第三章": "10", "第四章": "13",
         "第五章": "18", "第六章": "20", "第七章": "22", "第八章": "25", "第九章": "27",
         "第十章": "29", "第十一章": "32", "第十二章": "33", "参考资料": "34"}

doc = Document(SRC)

def ptxt(p):
    return "".join(t.text or "" for t in p._element.iter(qn('w:t')))

for p in doc.paragraphs:
    t = ptxt(p)
    if "\t" not in t:
        continue
    left = t.split("\t")[0].strip()
    for k, v in PAGES.items():
        if left.startswith(k):
            # 重写该段落的文本，但保留制表位设置
            runs = p.runs
            if not runs:
                continue
            runs[0].text = left + "\t" + v
            for r in runs[1:]:
                r.text = ""
                r._element.getparent().remove(r._element)
            print(f"  {left[:22]:24s} -> {v}")
            break

doc.save(SRC)
print("TOC cache refreshed")
