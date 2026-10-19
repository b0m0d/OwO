# -*- coding: utf-8 -*-
"""Insert the year-one capacity-constraint row and correct every revenue breakdown label."""
import copy
from docx import Document
from docx.oxml.ns import qn
from docx.table import _Row

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_cell(cell, text):
    ps = cell.paragraphs
    runs = ps[0].runs
    if runs:
        runs[0].text = text
        for r in runs[1:]:
            r.text = ""
            r._element.getparent().remove(r._element)
    else:
        ps[0].add_run(text)
    for extra in ps[1:]:
        extra._element.getparent().remove(extra._element)

t = doc.tables[20]
trs = t._element.findall(qn('w:tr'))

# 是否已存在约束行
exists = any("第一年规模约束" in "".join(x.text or "" for x in tr.iter(qn('w:t'))) for tr in trs)
if not exists:
    deploy_tr = None
    for tr in trs:
        txt = "".join(x.text or "" for x in tr.iter(qn('w:t')))
        if txt.strip().startswith("私有部署"):
            deploy_tr = tr
    assert deploy_tr is not None, "私有部署 row not found"
    new_tr = copy.deepcopy(deploy_tr)
    deploy_tr.addnext(new_tr)
    row = _Row(new_tr, t)
    for ci, v in enumerate(["第一年规模约束", "首批可服务用户约 200 人", "—", "—",
                           "来自表 11 的验证容量，不是市场推算"]):
        set_cell(row.cells[ci], v)
    print("inserted constraint row")

# 按标签修正收入/成本拆分
BREAK = [
 ("营业收入 保守", "个人订阅 3.6 万 + 团队 12 万 + 部署 18 万 + 个人 120 万"),
 ("营业收入 基准", "个人订阅 6 万 + 团队 20 万 + 部署 36 万 + 个人 241.6 万"),
 ("营业收入 进取", "个人订阅 10.8 万 + 团队 48 万 + 部署 72 万 + 个人 447 万"),
 ("经营成本", "研发、模型、市场与交付"),
]
for r in t.rows:
    label = r.cells[0].text.strip()
    for key, val in BREAK:
        if label.startswith(key):
            set_cell(r.cells[4], val)
doc.save(SRC)

d = Document(SRC)
t = d.tables[20]
print("rows", len(t.rows))
for ri, r in enumerate(t.rows):
    print(f"R{ri}: " + " | ".join(c.text.strip()[:50] for c in r.cells))