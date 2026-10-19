# -*- coding: utf-8 -*-
"""Recompute the three-year plan from explicit quantities, then write table + figure."""
from docx import Document

P = 240.0          # 元 / 人 / 年
T = 40000.0        # 元 / 团队 / 年
D = 180000.0       # 元 / 部署

ind = {"保守": [150, 500, 1100], "基准": [250, 1000, 2400], "进取": [450, 1600, 4000]}
team = {"保守": [0, 3, 8], "基准": [0, 5, 24], "进取": [0, 12, 40]}
dep = {"保守": [0, 0, 0], "基准": [0, 0, 2], "进取": [0, 1, 4]}
cost = {"保守": [40, 70, 120], "基准": [50, 110, 190], "进取": [65, 150, 240]}

def wan(x):
    return round(x / 10000.0, 1)

rev = {}
res = {}
for k in ind:
    rev[k] = [wan(ind[k][y] * P + team[k][y] * T + dep[k][y] * D) for y in range(3)]
    res[k] = [round(rev[k][y] - cost[k][y], 1) for y in range(3)]

for k in ("保守", "基准", "进取"):
    print(k, "ind", ind[k], "team", team[k], "dep", dep[k])
    print("   rev", rev[k], "cost", cost[k], "res", res[k])
    print("   cum(2y)", round(res[k][0] + res[k][1], 1))
print("revision3 基准年三应为", rev["基准"][2], "实际=2400*240+24*40000+2*180000 /1e4")
print("check:", (2400*240 + 24*40000 + 2*180000)/1e4)

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
def fmt(vals, unit="万元"):
    return [f"{v} {unit}" for v in vals]
def j(vals, unit):
    return " / ".join(f"{v} {unit}" for v in vals)

BREAK = {
 "营业收入 保守": "个人 3.6 / 12 / 26.4 万元；团队 0 / 12 / 32 万元；部署 0 / 0 / 0",
 "营业收入 基准": "个人 6 / 24 / 57.6 万元；团队 0 / 20 / 96 万元；部署 0 / 0 / 36 万元",
 "营业收入 进取": "个人 10.8 / 38.4 / 96 万元；团队 0 / 48 / 160 万元；部署 0 / 18 / 72 万元",
}
for r in t.rows:
    lab = r.cells[0].text.strip()
    if lab == "个人专业版 付费个人（年末在付）":
        set_cell(r.cells[1], " / ".join(str(v) for v in ind["保守"] and [ind[k][0] for k in ("保守","基准","进取")]))
        set_cell(r.cells[2], " / ".join(str(ind[k][1]) for k in ("保守","基准","进取")))
        set_cell(r.cells[3], " / ".join(str(ind[k][2]) for k in ("保守","基准","进取")))
    elif lab == "团队许可（个）":
        set_cell(r.cells[1], " / ".join(str(team[k][0]) for k in ("保守","基准","进取")))
        set_cell(r.cells[2], " / ".join(str(team[k][1]) for k in ("保守","基准","进取")))
        set_cell(r.cells[3], " / ".join(str(team[k][2]) for k in ("保守","基准","进取")))
    elif lab == "私有部署（个）":
        set_cell(r.cells[1], " / ".join(str(dep[k][0]) for k in ("保守","基准","进取")))
        set_cell(r.cells[2], " / ".join(str(dep[k][1]) for k in ("保守","基准","进取")))
        set_cell(r.cells[3], " / ".join(str(dep[k][2]) for k in ("保守","基准","进取")))
    elif lab.startswith("营业收入 "):
        k = lab.split(" ")[1]
        set_cell(r.cells[1], j(rev[k][0:1], "万元")); set_cell(r.cells[2], f"{rev[k][1]} 万元"); set_cell(r.cells[3], f"{rev[k][2]} 万元")
        set_cell(r.cells[4], BREAK[lab])
    elif lab.startswith("经营成本"):
        set_cell(r.cells[1], j(cost["保守"][0:1], "万元") + " / " + f"{cost['基准'][0]} / {cost['进取'][0]} 万元")
        set_cell(r.cells[2], f"{cost['保守'][1]} / {cost['基准'][1]} / {cost['进取'][1]} 万元")
        set_cell(r.cells[3], f"{cost['保守'][2]} / {cost['基准'][2]} / {cost['进取'][2]} 万元")
    elif lab.startswith("经营结果 "):
        k = lab.split(" ")[1]
        def s(v):
            return ("负 " + str(abs(v))) if v < 0 else ("正 " + str(v))
        set_cell(r.cells[1], s(res[k][0]) + " 万元")
        set_cell(r.cells[2], s(res[k][1]) + " 万元")
        set_cell(r.cells[3], s(res[k][2]) + " 万元")
doc.save(SRC)
d = Document(SRC)
for ri, r in enumerate(d.tables[20].rows):
    print(f"R{ri}: " + " | ".join(c.text.strip()[:52] for c in r.cells))