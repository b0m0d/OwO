# -*- coding: utf-8 -*-
"""Rewrite the finance table with three distinct, arithmetically exact scenarios."""
from docx import Document

P, T, D = 240.0, 40000.0, 180000.0
IND  = {"保守": [300, 700, 1300], "基准": [400, 1200, 2800], "进取": [600, 2000, 5000]}
TEAM = {"保守": [0, 3, 8],         "基准": [0, 5, 24],      "进取": [0, 10, 40]}
DEP  = {"保守": [0, 0, 0],         "基准": [0, 0, 2],       "进取": [0, 1, 4]}
COST = {"保守": [40, 70, 120],     "基准": [50, 105, 190],  "进取": [65, 145, 240]}
KEYS = ("保守", "基准", "进取")

def w(x): return round(x / 10000.0, 1)
REV = {k: [w(IND[k][y]*P + TEAM[k][y]*T + DEP[k][y]*D) for y in range(3)] for k in KEYS}
RES = {k: [round(REV[k][y] - COST[k][y], 1) for y in range(3)] for k in KEYS}
for k in KEYS:
    print(k, "ind", IND[k], "team", TEAM[k], "dep", DEP[k], "rev", REV[k], "cost", COST[k], "res", RES[k],
          "cum2y", round(RES[k][0]+RES[k][1], 1))

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
BREAK = {
 "营业收入 保守": "个人 %s 万元；团队 %s 万元；部署 %s 万元；第一年不计团队与部署" % (
     " / ".join(str(w(IND['保守'][y]*P)) for y in range(3)),
     " / ".join(str(w(TEAM['保守'][y]*T)) for y in range(3)), "0 / 0 / 0"),
 "营业收入 基准": "个人 %s 万元；团队 %s 万元；部署 %s 万元" % (
     " / ".join(str(w(IND['基准'][y]*P)) for y in range(3)),
     " / ".join(str(w(TEAM['基准'][y]*T)) for y in range(3)),
     " / ".join(str(w(DEP['基准'][y]*D)) for y in range(3))),
 "营业收入 进取": "个人 %s 万元；团队 %s 万元；部署 %s 万元" % (
     " / ".join(str(w(IND['进取'][y]*P)) for y in range(3)),
     " / ".join(str(w(TEAM['进取'][y]*T)) for y in range(3)),
     " / ".join(str(w(DEP['进取'][y]*D)) for y in range(3))),
}
NOTE = {"保守": "第三年转正", "基准": "第三年转正", "进取": "第二年即接近平衡"}

for r in t.rows:
    lab = r.cells[0].text.strip()
    if lab.startswith("个人专业版"):
        for y in range(3):
            set_cell(r.cells[1+y], " / ".join(str(IND[k][y]) for k in KEYS))
    elif lab.startswith("团队许可"):
        for y in range(3):
            set_cell(r.cells[1+y], " / ".join(str(TEAM[k][y]) for k in KEYS))
    elif lab.startswith("私有部署"):
        for y in range(3):
            set_cell(r.cells[1+y], " / ".join(str(DEP[k][y]) for k in KEYS))
    elif lab.startswith("营业收入 "):
        k = lab.split(" ")[1]
        for y in range(3):
            set_cell(r.cells[1+y], f"{REV[k][y]} 万元")
        set_cell(r.cells[4], BREAK[lab])
    elif lab.startswith("经营成本"):
        for y in range(3):
            set_cell(r.cells[1+y], " / ".join(f"{COST[k][y]}" for k in KEYS) + " 万元")
    elif lab.startswith("经营结果 "):
        k = lab.split(" ")[1]
        for y in range(3):
            v = RES[k][y]
            set_cell(r.cells[1+y], (f"负 {abs(v)} 万元" if v < 0 else f"正 {v} 万元"))
        set_cell(r.cells[4], NOTE[k])
doc.save(SRC)
d = Document(SRC)
for ri, r in enumerate(d.tables[20].rows):
    print(f"R{ri}: " + " | ".join(c.text.strip()[:60] for c in r.cells))