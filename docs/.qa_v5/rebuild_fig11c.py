# -*- coding: utf-8 -*-
"""Rebuild 图 11 with the recomputed three-scenario numbers."""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(r"T:\创新创业\OwO-master")
spec = importlib.util.spec_from_file_location("cuttlebuild", ROOT / "tools" / "build_cuttle_competition_v5.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, GOLD = m.NAVY, m.TEAL, m.ORANGE, m.GOLD
INK, GRAY, WHITE = m.INK, m.GRAY, m.WHITE
rounded, center_text, label, font = m.rounded, m.center_text, m.label, m.font

im = Image.new("RGB", (1800, 920), "white"); d = ImageDraw.Draw(im)
label(d, (900, 40), "三年经营测算（保守 / 基准 / 进取）", 48, True, NAVY, "ma")

years = ["第一年", "第二年", "第三年"]
rev  = [[1.0, 1.4, 2.2], [21.5, 40.5, 92.4], [52.6, 176.4, 311.2]]
cost = [[40, 50, 65], [55, 105, 190], [80, 190, 260]]
scen_color = [GOLD, TEAL, NAVY]
base_y, top, maxv = 770, 190, 330.0
scale = (base_y - top) / maxv
for i, year in enumerate(years):
    cx = 380 + i * 520
    for j in range(3):
        bx = cx - 190 + j * 130
        rh = max(4, int(rev[i][j] * scale)); ch = int(cost[i][j] * scale)
        d.rectangle((bx, base_y - rh, bx + 52, base_y), fill="#" + scen_color[j])
        d.rectangle((bx + 66, base_y - ch, bx + 118, base_y), fill="#" + ORANGE)
        fv = font(28, True); tv = f"{rev[i][j]:g}"
        d.text((bx + 26 - d.textbbox((0,0), tv, font=fv)[2]/2, base_y - rh - 36), tv, font=fv, fill="#" + scen_color[j])
        fc = font(26, False); tc = f"{cost[i][j]:g}"
        if ch <= 44:
            d.text((bx + 92 - d.textbbox((0,0), tc, font=fc)[2]/2, base_y - ch - 34), tc, font=fc, fill="#" + ORANGE)
        else:
            d.text((bx + 92 - d.textbbox((0,0), tc, font=fc)[2]/2, base_y - ch + 14), tc, font=fc, fill="#" + WHITE)
    label(d, (cx, 826), year, 34, True, INK, "ma")
d.line((140, base_y, 1660, base_y), fill="#" + INK, width=5)
for k, (col, txt) in enumerate(zip(scen_color, ["保守", "基准", "进取"])):
    x = 620 + k * 200
    d.rectangle((x, 108, x + 40, 144), fill="#" + col)
    label(d, (x + 52, 110), txt, 30, False, INK)
d.rectangle((1240, 108, 1280, 144), fill="#" + ORANGE)
label(d, (1292, 110), "经营成本", 30, False, INK)
label(d, (900, 176), "个人订阅收入按年均在付用户数 × 240 元测算", 26, True, GRAY, "ma")

p = ROOT / "docs" / "cuttle_v8_assets" / "fig10-financial-plan.png"
im.save(p, quality=95)
print("saved", p, im.size)
