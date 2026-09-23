# -*- coding: utf-8 -*-
"""Split the compressed 7.3 paragraph into three readable paragraphs."""
import copy
from docx import Document
from docx.text.paragraph import Paragraph

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, text):
    runs = p.runs
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

target = None
for p in doc.paragraphs:
    if p.text.strip().startswith("首用转化：用户在十分钟内"):
        target = p; break
assert target is not None

set_text(target,
    "首用转化：用户在十分钟内完成一个低风险任务，理解系统能做什么和不能做什么，"
    "并把验证过的流程沉淀为个人技能，减少下一次重复说明。")
n1 = copy.deepcopy(target._element)
target._element.addnext(n1)
p1 = Paragraph(n1, target._parent)
set_text(p1, "组织扩散：个人用户将验证过的流程带入所在团队与机构，先形成小范围试点，再进入部门采购流程。"
             "个人用户来自开发者社群与专业社区的内容传播、面向高频知识工作者的定向试点，"
             "以及可复用工作流模板的自然扩散，投入以内容与模板为主。")
n2 = copy.deepcopy(target._element)
n1.addnext(n2)
p2 = Paragraph(n2, target._parent)
set_text(p2, "留存诊断：区分新鲜感流失、任务价值不足、信任不足与兼容性问题，分别调整产品。")

doc.save(SRC)
print("7.3 split done")
