# -*- coding: utf-8 -*-
"""Cite ref [4] where the early-sample scale is mentioned (education statistics are legitimate for sample sourcing)."""
from docx import Document
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
def set_text(p, t):
    runs = p.runs
    runs[0].text = t
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)
for p in doc.paragraphs:
    t = p.text.strip()
    if t.startswith("学生与科研人员属于上述用户群的重要子集"):
        set_text(p, "学生与科研人员属于上述用户群的重要子集，也是团队最先能够触达的样本来源，"
                    "教育人口统计用于说明这一早期样本池的规模上限[4]。"
                    "单点文本生成任务由通用写作工具即可覆盖，本项目聚焦跨来源取数与口径核验，"
                    "这类工作依赖持续的情境、来源追溯与受控执行。")
        print("  [4] 引用落到早期样本来源处")
doc.save(SRC)