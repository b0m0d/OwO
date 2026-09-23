# -*- coding: utf-8 -*-
"""Rebuild 图 11 to match the recomputed three-scenario plan."""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(r"T:\创新创业\OwO-master")
spec = importlib.util.spec_from_file_location("cuttlebuild", ROOT / "tools" / "build_cuttle_competition_v5.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, GOLD = m.NAVY, m.TEAL, m.ORANGE, m.GOLD
INK, GRAY, LIGHT, GRID, WHITE = m.INK, m.GRAY, m.LIGHT, m.GRID, m.WHITE
rounded, center_text, label, font = m.rounded, m.center_text, m.label, m.font

im = Image.new("RGB", (1800, 920), "white")
d = ImageDraw.Draw(im)
label(d, (900, 34), "三年经营测算（保守 / 基准 / 进取三情景）", 42, True, NAVY, "ma")

years = ["第一年", "第二年", "第三年"]
rev  = [[7.2, 9.6, 14.4], [28.8, 48.8, 106.0], [63.2, 199.2, 352.0]]
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
        fv = font(21, True)
        tv = f"{rev[i][j]:g}"
        wv = d.textbbox((0, 0), tv, font=fv)[2]
        d.text((bx + 26 - wv / 2, base_y - rh - 27), tv, font=fv, fill="#" + scen_color[j])
        fc = font(21, False)
        tc = f"{cost[i][j]:g}"
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

label(d, (900, 226), "第一年为验证年，不计团队许可与私有部署收入；团队收入自第二年起确认",
      25, True, GRAY, "ma")
label(d, (900, 268), "单位 万元   数量与单价口径见表 19", 23, False, GRAY, "ma")
label(d, (160, 858), "基准情景第三年进入盈亏平衡上方   进取情景第三年转正 92.0 万元", 22, False, GRAY)
label(d, (1650, 858), "保守情景三年均未转正，用于观察下限", 22, True, GOLD, "ra")

p = ROOT / "docs" / "cuttle_v8_assets" / "fig10-financial-plan.png"
im.save(p, quality=95)
print("saved", p, im.size)