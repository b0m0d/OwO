# -*- coding: utf-8 -*-
"""第四轮修正：把“过度感知/过度自治”合并进安全门槛表，消除编号断档，并顺排编号。"""
import docx
from docx.oxml.ns import qn
from docx.table import Table

DOC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v6.docx"
doc = docx.Document(DOC)
body = doc.element.body


def els():
    return list(body.iterchildren())


def txt_of(el):
    return ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip()


def find_p(text):
    h = [el for el in els() if el.tag.endswith('}p') and txt_of(el) == text]
    assert len(h) == 1, (text, len(h))
    return h[0]


def set_p(el, text):
    rs = el.findall(qn('w:r'))
    t0 = rs[0].findall(qn('w:t'))
    t0[0].text = text
    for t in t0[1:]:
        t.text = ""
    for r in rs[1:]:
        for t in r.findall(qn('w:t')):
            t.text = ""


def find_tbl(starting_with):
    for el in els():
        if el.tag.endswith('}tbl') and txt_of(el).startswith(starting_with):
            return el
    raise KeyError(starting_with)


# ---- A) 删除独立的过度感知表与题注
ov_cap = find_p("表 10 过度感知与过度自治指标")
ov_tbl = find_tbl("专有指标定义典型反例")
ov_tbl.getparent().remove(ov_tbl)
ov_cap.getparent().remove(ov_cap)

# ---- B) 把两行并入安全门槛表
safe_tbl = find_tbl("安全目标指标阶段门槛")
tbl = Table(safe_tbl, doc)
import copy
ref_tr = safe_tbl.findall(qn('w:tr'))[-1]
new_rows = [
    ["情境过度感知", "为完成当前任务读取了并不必需的数据（Context Overreach Rate）",
     "误读取率低于 5% 设计红线下须为 0", "收缩读取范围 复核意图绑定"],
    ["自治过度升级", "系统采取了超出用户意图所需的执行层级（Autonomy Overreach Rate）",
     "过度升级率低于 10%", "强制回退层级 增加确认"],
]
for vals in new_rows:
    tr = copy.deepcopy(ref_tr)
    ref_tr.addnext(tr)
    ref_tr = tr
tbl = Table(safe_tbl, doc)
for i, vals in enumerate(new_rows, start=len(tbl.rows) - 2):
    for ci, v in enumerate(vals):
        cell = tbl.rows[i].cells[ci]
        while len(cell._tc.findall(qn('w:p'))) > 1:
            cell._tc.remove(cell._tc.findall(qn('w:p'))[-1])
        p = cell.paragraphs[0]
        if p.runs:
            p.runs[0].text = v
            for r in p.runs[1:]:
                r.text = ""
        else:
            p.add_run(v)

# ---- C) 编号顺排：安全门槛表 表 12 -> 表 11，其后依次前移一位
renum = [
    ("表 12 安全门槛 设计红线 测试指标与生产事故指标", "表 11 安全门槛 设计红线 测试指标与生产事故指标"),
    ("表 13 自下而上的三层市场测算", "表 12 自下而上的三层市场测算"),
    ("表 14 竞争类别与差异化策略", "表 13 竞争类别与差异化策略"),
    ("表 15 产品版本与收入结构", "表 14 产品版本与收入结构"),
    ("表 16 单位经济模型的规划假设", "表 15 单位经济模型的规划假设"),
    ("表 17 分阶段研发与市场决策门", "表 16 分阶段研发与市场决策门"),
    ("表 18 项目十二个月核心指标", "表 17 项目十二个月核心指标"),
    ("表 19 团队分工与贡献证据", "表 18 团队分工与贡献证据"),
    ("表 20 项目驱动的人才培养路径", "表 19 项目驱动的人才培养路径"),
    ("表 21 三年经营情景测算（保守 基准 进取）", "表 20 三年经营情景测算（保守 基准 进取）"),
    ("表 22 分层资源需求与用途", "表 21 分层资源需求与用途"),
    ("表 23 项目风险登记表", "表 22 项目风险登记表"),
]
for old, new in renum:
    for el in els():
        if el.tag.endswith('}p') and txt_of(el) == old:
            set_p(el, "@@" + new)
for el in els():
    if el.tag.endswith('}p'):
        t = txt_of(el)
        if t.startswith("@@"):
            set_p(el, t[2:])

# ---- D) 正文交叉引用与说明微调
for el in els():
    if not el.tag.endswith('}p'):
        continue
    t = txt_of(el)
    if t.startswith("Context Overreach Rate 与 Autonomy Overreach Rate 的定义"):
        set_p(el, "情境过度读取率与自治过度升级率的定义、反例与阈值见 5.3 节的安全门槛表，"
                  "两者是判定意图胶囊设计是否合格的核心安全指标。")

doc.save(DOC)

# ---- E) 校验
order = []
for el in els():
    if el.tag.endswith('}p'):
        t = txt_of(el)
        if t.startswith('表 ') or t.startswith('图 '):
            order.append(t[:26])
print("\n".join(order))
print("tables:", len(doc.tables))
