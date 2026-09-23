# -*- coding: utf-8 -*-
"""Rebuild 图 11 with the final numbers and sync chapter 6 capacity wording."""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw
from docx import Document

ROOT = Path(r"T:\创新创业\OwO-master")
spec = importlib.util.spec_from_file_location("cuttlebuild", ROOT / "tools" / "build_cuttle_competition_v5.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, GOLD = m.NAVY, m.TEAL, m.ORANGE, m.GOLD
INK, GRAY, LIGHT, WHITE = m.INK, m.GRAY, m.LIGHT, m.WHITE
rounded, center_text, label, font = m.rounded, m.center_text, m.label, m.font

im = Image.new("RGB", (1800, 920), "white")
d = ImageDraw.Draw(im)
label(d, (900, 34), "三年经营测算（保守 / 基准 / 进取三情景）", 42, True, NAVY, "ma")
years = ["第一年", "第二年", "第三年"]
rev  = [[1.9, 2.9, 4.3], [26.4, 48.8, 106.0], [63.2, 199.2, 352.0]]
cost = [[40, 50, 65], [55, 105, 190], [80, 190, 260]]
scen_color = [GOLD, TEAL, NAVY]
base_y, top, maxv = 760, 320, 360.0
scale = (base_y - top) / maxv
for i, year in enumerate(years):
    cx = 380 + i * 520
    for j in range(3):
        bx = cx - 190 + j * 130
        rh = max(3, int(rev[i][j] * scale))
        ch = int(cost[i][j] * scale)
        d.rectangle((bx, base_y - rh, bx + 52, base_y), fill="#" + scen_color[j])
        d.rectangle((bx + 66, base_y - ch, bx + 118, base_y), fill="#" + ORANGE)
        fv = font(21, True); tv = f"{rev[i][j]:g}"
        wv = d.textbbox((0, 0), tv, font=fv)[2]
        d.text((bx + 26 - wv / 2, base_y - rh - 27), tv, font=fv, fill="#" + scen_color[j])
        fc = font(21, False); tc = f"{cost[i][j]:g}"
        wc = d.textbbox((0, 0), tc, font=fc)[2]
        if ch <= 40:
            d.text((bx + 92 - wc / 2, base_y - ch - 27), tc, font=fc, fill="#" + ORANGE)
        else:
            d.text((bx + 92 - wc / 2, base_y - ch + 12), tc, font=fc, fill="#" + WHITE)
    label(d, (cx, 812), year, 28, True, INK, "ma")
d.line((150, base_y, 1650, base_y), fill="#" + INK, width=4)
d.rectangle((960, 140, 996, 166), fill="#" + GOLD)
d.rectangle((1042, 140, 1078, 166), fill="#" + TEAL)
d.rectangle((1124, 140, 1160, 166), fill="#" + NAVY)
label(d, (1004, 141), "保守", 23, False, INK)
label(d, (1086, 141), "基准", 23, False, INK)
label(d, (1168, 141), "进取", 23, False, INK)
d.rectangle((1250, 140, 1286, 166), fill="#" + ORANGE)
label(d, (1294, 141), "经营成本", 23, False, INK)
label(d, (900, 226), "第一年为验证年：个人付费用户上限 200 人，不计团队许可与私有部署收入",
      25, True, GRAY, "ma")
label(d, (900, 268), "单位 万元   数量与单价口径见表 19", 23, False, GRAY, "ma")
label(d, (160, 858), "基准情景第三年进入盈亏平衡上方   进取情景第三年转正 92.0 万元", 22, False, GRAY)
label(d, (1650, 858), "保守情景三年均未转正，用于观察下限", 22, True, GOLD, "ra")
p = ROOT / "docs" / "cuttle_v8_assets" / "fig10-financial-plan.png"
im.save(p, quality=95)
print("figure saved", im.size)

# ---- chapter 6 wording sync ----
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
def set_text(x, text):
    runs = x.runs
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)
def set_cell(cell, text):
    ps = cell.paragraphs
    set_text(ps[0], text)
    for extra in ps[1:]:
        extra._element.getparent().remove(extra._element)

t = doc.tables[12]
set_cell(t.rows[1].cells[3],
         "约 200 人。该容量同时约束第一年的个人付费用户规模与实验样本配置，"
         "因此第一年在付用户不超过 200 人")
for x in doc.paragraphs:
    st = x.text.strip()
    if st.startswith("试点容量与参考市场边界的用途不同"):
        set_text(x, "试点容量与参考市场边界的用途不同：前者用于配置研发与人力，并约束第一年的付费用户规模"
                    "（不超过 200 人）与实验样本配置；后者用于说明长期天花板。表中的比例均来自前期调研与访谈，"
                    "在正式试点开始后由真实转化数据替换；若试点转化率低于预设下限，项目将缩小场景范围，"
                    "而不是调整分母使数字成立。")
    if st.startswith("图 11 Cuttle 三年经营测算"):
        set_text(x, "图 11 Cuttle 三年经营测算（第一年付费用户上限 200 人）")
doc.save(SRC)
print("chapter 6 synced")