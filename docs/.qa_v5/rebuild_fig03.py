# -*- coding: utf-8 -*-
"""Rebuild 图 3 (competitive map) layout to remove label collisions."""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(r"T:\创新创业\OwO-master")
spec = importlib.util.spec_from_file_location("cuttlebuild", ROOT / "tools" / "build_cuttle_competition_v5.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, PURPLE, GOLD = m.NAVY, m.TEAL, m.ORANGE, m.PURPLE, m.GOLD
INK, GRAY, LIGHT = m.INK, m.GRAY, m.LIGHT
rounded, center_text, arrow, label = m.rounded, m.center_text, m.arrow, m.label

im = Image.new("RGB", (1600, 1000), "white")
d = ImageDraw.Draw(im)
L, R, B, T = 210, 1470, 860, 150
d.line((L, B, R, B), fill="#" + INK, width=5)
d.line((L, B, L, T), fill="#" + INK, width=5)
arrow(d, (R, B), (1545, B), INK, 5, 18)
arrow(d, (L, T), (L, 92), INK, 5, 18)
label(d, (840, 940), "横轴：任务生命周期的覆盖程度  纵轴：执行与治理深度（内容生成 · 工具执行 · 受控执行 · 审批与回滚）", 26, True, INK, "ma")
d.text((145, 520), "执行与治理深度", font=m.font(29, True), fill="#" + INK, anchor="mm")

boxes = [
    (330, 760, "搜狗输入法", TEAL), (430, 690, "讯飞输入法", TEAL),
    (300, 605, "Wispr Flow", GOLD), (470, 530, "ChatGPT", NAVY),
    (390, 440, "Codex", NAVY), (520, 350, "MiMo Desktop", PURPLE),
    (630, 265, "Claude Code", PURPLE),
    (1330, 175, "Cuttle", ORANGE),
]
for x, y, name, color in boxes:
    d.ellipse((x - 15, y - 15, x + 15, y + 15), fill="#" + color)
    if name == "Cuttle":
        rounded(d, (x - 96, y + 24, x + 96, y + 88), 18, "FFF5F0", color, 4)
        center_text(d, (x - 96, y + 24, x + 96, y + 88), name, 27, True, color)
    else:
        rounded(d, (x + 22, y - 30, x + 232, y + 30), 18, LIGHT, color, 3)
        center_text(d, (x + 22, y - 30, x + 232, y + 30), name, 23, False, color)

# 落点说明卡片（置于右下空白区，不与任何标签重叠）
rounded(d, (880, 640, 1450, 810), 24, "F7FAFB", GRAY, 3)
label(d, (912, 660), "Cuttle 不主张任何产品缺少某项能力", 25, True, INK)
for k, txt in enumerate(["产品能力边界随版本快速变化，判断依据为公开文档",
                         "坐标仅表示各产品公开目标下的重心位置",
                         "差异来源是设计目标：独立工作空间与输入焦点原生入口"]):
    label(d, (912, 702 + k * 34), "· " + txt, 20, False, GRAY)
p = ROOT / "docs" / "cuttle_v8_assets" / "fig03-competition-map.png"
im.save(p, quality=95)
print("saved", p, im.size)