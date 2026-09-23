# -*- coding: utf-8 -*-
"""Add a row for average annual paying users and recompute subscription revenue from it,
so the table matches the stated method (年均在付 × 240)."""
import copy
from docx import Document
from docx.oxml.ns import qn
from docx.table import _Row

import glob, os
SRC = [x for x in glob.glob(os.path.join(r"T:\创新创业\OwO-master\docs", "Cuttle*v8.docx")) if "预览" not in x][0]
doc = Document(SRC)

def first_cells(t):
    return [c.text.strip() for c in t.rows[0].cells]

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

def set_row(t, ri, vals):
    for ci, v in enumerate(vals):
        set_cell(t.rows[ri].cells[ci], v)

# 财务表：指标 / 第一年 / 第二年 / 第三年 / 口径与依据
fin = None
for t in doc.tables:
    fc = first_cells(t)
    if fc[:2] == ["指标", "第一年"] and len(fc) == 5:
        fin = t
        break
assert fin is not None, "financial table not found"

# 年均在付 = 上年末 × 0.6 + 当年新增 / 2
YE = {"保守": [80, 600, 1300], "基准": [120, 1200, 2800], "进取": [180, 2000, 5000]}
NEW = {"保守": [40, 395, 860], "基准": [60, 855, 1850], "进取": [90, 1435, 3300]}

def rev(avg, team, dep):
    return round((avg * 240 + team * 40000 + dep * 180000) / 10000.0, 1)

# 计算三情景收入
TEAM = {"保守": [0, 3, 8], "基准": [0, 5, 24], "进取": [0, 10, 40]}
DEP = {"保守": [0, 0, 0], "基准": [0, 0, 2], "进取": [0, 1, 4]}
COST = {"保守": [40, 55, 80], "基准": [50, 105, 190], "进取": [65, 190, 260]}
R = {k: [rev(NEW[k][y], TEAM[k][y], DEP[k][y]) for y in range(3)] for k in YE}
RES = {k: [round(R[k][y] - COST[k][y], 1) for y in range(3)] for k in YE}
for k in YE:
    print(k, "avg", NEW[k], "rev", R[k], "cost", COST[k], "res", RES[k],
          "cum2", round(RES[k][0] + RES[k][1], 1))

# 在“个人专业版 付费个人（年末在付）”行后插入“年均在付”行
pay_row = None
for r in fin.rows:
    if r.cells[0].text.strip().startswith("个人专业版"):
        pay_row = r
        break
assert pay_row is not None
new_tr = copy.deepcopy(pay_row._element)
pay_row._element.addnext(new_tr)
row_avg = _Row(new_tr, fin)
for ci, v in enumerate(["个人专业版 年均在付（测算基数）",
                        " / ".join(str(NEW[k][0]) for k in ("保守", "基准", "进取")),
                        " / ".join(str(NEW[k][1]) for k in ("保守", "基准", "进取")),
                        " / ".join(str(NEW[k][2]) for k in ("保守", "基准", "进取")),
                        "按上年末在付规模 60% 续订、当年新增按半年加权估算"]):
    set_cell(row_avg.cells[ci], v)

# 更新“个人专业版 付费个人”行的口径说明
for r in fin.rows:
    lab = r.cells[0].text.strip()
    if lab.startswith("个人专业版 付费个人"):
        set_cell(r.cells[4], "保守 / 基准 / 进取；年费 240 元，用于描述用户规模")
    elif lab.startswith("营业收入 保守"):
        set_row(fin, list(fin.rows).index(r), ["营业收入 保守"] + [f"{R['保守'][y]} 万元" for y in range(3)] +
                [f"年均在付 {NEW['保守'][0]}/{NEW['保守'][1]}/{NEW['保守'][2]} 人 × 240 元；团队 0/12/32 万元"])
    elif lab.startswith("营业收入 基准"):
        set_row(fin, list(fin.rows).index(r), ["营业收入 基准"] + [f"{R['基准'][y]} 万元" for y in range(3)] +
                [f"年均在付 {NEW['基准'][0]}/{NEW['基准'][1]}/{NEW['基准'][2]} 人 × 240 元；团队 0/20/96 万元；部署 0/0/36 万元"])
    elif lab.startswith("营业收入 进取"):
        set_row(fin, list(fin.rows).index(r), ["营业收入 进取"] + [f"{R['进取'][y]} 万元" for y in range(3)] +
                [f"年均在付 {NEW['进取'][0]}/{NEW['进取'][1]}/{NEW['进取'][2]} 人 × 240 元；团队 0/40/160 万元；部署 0/18/72 万元"])
    elif lab.startswith("经营结果 保守"):
        set_row(fin, list(fin.rows).index(r), ["经营结果 保守"] + [(f"负 {abs(RES['保守'][y])} 万元" if RES['保守'][y] < 0 else f"正 {RES['保守'][y]} 万元") for y in range(3)] + ["三年均未转正，用于观察下限"])
    elif lab.startswith("经营结果 基准"):
        set_row(fin, list(fin.rows).index(r), ["经营结果 基准"] + [(f"负 {abs(RES['基准'][y])} 万元" if RES['基准'][y] < 0 else f"正 {RES['基准'][y]} 万元") for y in range(3)] + ["第三年进入盈亏平衡上方"])
    elif lab.startswith("经营结果 进取"):
        set_row(fin, list(fin.rows).index(r), ["经营结果 进取"] + [(f"负 {abs(RES['进取'][y])} 万元" if RES['进取'][y] < 0 else f"正 {RES['进取'][y]} 万元") for y in range(3)] + ["第三年转正"])
    elif lab.startswith("经营成本"):
        set_row(fin, list(fin.rows).index(r), ["经营成本 保守 / 基准 / 进取"] + [" / ".join(f"{COST[k][y]}" for k in ("保守","基准","进取")) + " 万元" for y in range(3)] + ["研发、模型、市场与交付"])
    elif lab.startswith("私有部署"):
        set_cell(r.cells[4], "18 万元每项目，按验收确认")

# 正文口径说明改为与表一致
for p in doc.paragraphs:
    if p.text.strip().startswith("财务测算采用个人订阅"):
        runs = p.runs
        runs[0].text = ("财务测算采用个人订阅、团队许可和私有部署三类收入，不代表项目已经取得收入、融资或用户规模。"
                        "个人订阅收入按年度平均在付用户数乘年费 240 元测算：年均在付由上年在付规模的 60% 续订"
                        "与当年新增按半年加权估算；团队许可按 4 万元每团队每年计价，自第二年起计入；"
                        "私有部署按 18 万元每项目、以验收确认。本轮预测仅计入基础订阅、团队许可与私有部署收入，"
                        "超额智能体调用收入及其对应的推理成本暂不计入。三种情景分别设定用户与客户数量，"
                        "按同一单价假设计算收入；第一年的规模由第六章的验证容量约束，个人付费用户不超过 200 人，"
                        "三种情景分别按 80、120 与 180 人测算。全部单价与转化率均属待验证假设，验证方式与门槛见表 7。")
        for r in runs[1:]:
            r.text = ""
            r._element.getparent().remove(r._element)
        print("  10.1 口径同步")

doc.save(SRC)
print("financial table recomputed")
