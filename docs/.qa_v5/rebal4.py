# -*- coding: utf-8 -*-
"""Trim chapter 4 a little more (merge overlapping 4.1 and 4.3 material)."""
from docx import Document
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, t):
    runs = p.runs
    runs[0].text = t
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

# 4.1 合并两条重复的 IC 说明
set_text(find("IC 的评价对象是情境质量"),
 "IC 的评价对象是情境质量：来源绑定是否正确、最小情境是否漏读、是否多读、是否仍在使用已失效上下文、"
 "证据是否完整，以及用户是否需要重复交代背景。路由类指标属于 4.2 节的评价对象。")
set_text(find("现有桌面 Agent 在情境获取上存在三个结构性缺陷"),
 "现有桌面 Agent 在情境获取上存在三个结构性缺陷：每次请求都要重新交代背景，因为工作台不在任务现场；"
 "情境读取没有边界，因为系统不知道读取是为了哪一件事；任务结束后上下文残留，因为没有失效条件。"
 "IC 用 Origin、Context、Freshness 三个字段分别对应解决，并把来源与权限边界写成可校验字段。")
doc.save(SRC)
print("trim done")