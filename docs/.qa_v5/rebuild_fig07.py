# -*- coding: utf-8 -*-
"""Fix 图 7 (试点容量): move the flow arrows into the gaps so they never cross card text."""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(r"T:\创新创业\OwO-master")
spec = importlib.util.spec_from_file_location("cuttlebuild", ROOT / "tools" / "build_cuttle_competition_v5.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, GOLD = m.NAVY, m.TEAL, m.ORANGE, m.GOLD
INK, GRAY, LIGHT, LIGHT_BLUE = m.INK, m.GRAY, m.LIGHT, m.LIGHT_BLUE
rounded, center_text, arrow, label = m.rounded, m.center_text, m.arrow, m.label

im = Image.new("RGB", (1800, 850), "white")
d = ImageDraw.Draw(im)
label(d, (900, 40), "第一年不是市场份额  而是团队能够真实服务的容量", 40, True, NAVY, "ma")
rounded(d, (600, 132, 1200, 218), 26, LIGHT_BLUE, TEAL, 3)
center_text(d, (600, 132, 1200, 218), "从可直接影响的最小样本单元出发", 28, True, TEAL)

# 三层：自上而下为可交付容量 -> 深度与对照容量 -> 后续外推路径
top_box = (600, 246, 1200, 388, NAVY,
           "第一年可交付容量  约 200 人",
           "相关人群 3000 人 × 高频任务约 40%\n× 愿试用约 50% × 稳定观察约三分之二")
mid_box = (440, 416, 1360, 572, TEAL,
           "深度与对照容量  50 至 80 人形成深度数据",
           "纵向核心组 15 至 25 人  ·  独立验证组 20 至 30 人\n合计 35 至 55 人进入正式实验")
bot_box = (280, 600, 1520, 756, GOLD,
           "后续外推路径（待验证）  首校复制率验证之后的可服务规模",
           "以复核率达标、交付人力可复制、支持成本可核算为前提  每校可服务约 200 至 400 人")

for x1, y1, x2, y2, color, title, note in (top_box, mid_box, bot_box):
    rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 4)
    center_text(d, (x1 + 20, y1 + 16, x2 - 20, y1 + 78), title, 27, True, color)
    h = (y2 - 14) - (y1 + 78)
    if "\n" in note:
        center_text(d, (x1 + 20, y1 + 74, x2 - 20, y2 - 14), note, 21, False, INK, 6)
    else:
        center_text(d, (x1 + 16, y1 + 78, x2 - 16, y2 - 14), note, 21, False, INK)

# 箭头放在层间空隙中
arrow(d, (900, 400), (900, 412), ORANGE, 7, 18)
arrow(d, (900, 582), (900, 594), ORANGE, 7, 18)

notes = [
    (70, 446, 400, 542, "用途", "配置试点人力\n与研发节奏"),
    (1400, 446, 1730, 542, "口径", "比例来自前期访谈\n由真实转化数据替换"),
    (70, 630, 400, 726, "不下结论", "可服务市场与\n潜在市场暂不测算"),
    (1400, 630, 1730, 726, "替换条件", "转化低于预设下限\n则缩小场景范围"),
]
for x1, y1, x2, y2, title, note in notes:
    rounded(d, (x1, y1, x2, y2), 20, LIGHT, GRAY, 3)
    center_text(d, (x1 + 8, y1 + 8, x2 - 8, y1 + 44), title, 23, True, GRAY)
    center_text(d, (x1 + 10, y1 + 44, x2 - 10, y2 - 8), note, 19, False, INK, 6)
label(d, (60, 796), "容量测算用于配置资源  不用于承诺收入规模", 25, True, GRAY)
p = ROOT / "docs" / "cuttle_v8_assets" / "fig08-market-funnel.png"
im.save(p, quality=95)
print("saved", p, im.size)