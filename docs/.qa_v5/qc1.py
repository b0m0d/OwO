# -*- coding: utf-8 -*-
"""Final QC typesetting pass. Step 1: figure 7 rebuild, references format, 6.2 residual text, remove education figure."""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw
from docx import Document

ROOT = Path(r"T:\创新创业\OwO-master")
spec = importlib.util.spec_from_file_location("cuttlebuild", ROOT / "tools" / "build_cuttle_competition_v5.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, GOLD, PURPLE = m.NAVY, m.TEAL, m.ORANGE, m.GOLD, m.PURPLE
INK, GRAY, LIGHT, LIGHT_BLUE, WHITE = m.INK, m.GRAY, m.LIGHT, m.LIGHT_BLUE, m.WHITE
rounded, center_text, arrow, label, font = m.rounded, m.center_text, m.arrow, m.label, m.font

# ---------- 图 7：三层数字图，大字号，无旁注 ----------
im = Image.new("RGB", (1800, 850), "white"); d = ImageDraw.Draw(im)
label(d, (900, 60), "首年可交付容量与验证样本配置", 54, True, NAVY, "ma")
bands = [
    (150, 170, 1650, 360, NAVY, "首年可交付容量", "不超过 200 人", "按服务承载能力配置"),
    (300, 420, 1500, 610, TEAL, "深度观察容量", "50 – 80 人", "纵向核心组 15–25 人　独立验证组 20–30 人"),
    (450, 670, 1350, 800, GOLD, "正式实验样本", "35 – 55 人", "进入对照与消融实验"),
]
for x1, y1, x2, y2, color, title, number, note in bands:
    rounded(d, (x1, y1, x2, y2), 30, LIGHT, color, 6)
    center_text(d, (x1 + 20, y1 + 14, x2 - 20, y1 + 74), title, 38, True, color)
    center_text(d, (x1 + 20, y1 + 74, x2 - 20, y1 + 140), number, 52, True, INK)
    center_text(d, (x1 + 20, y1 + 136, x2 - 20, y2 - 12), note, 28, False, GRAY)
arrow(d, (900, 372), (900, 408), ORANGE, 9, 22)
arrow(d, (900, 622), (900, 658), ORANGE, 9, 22)
p = ROOT / "docs" / "cuttle_v8_assets" / "fig08-market-funnel.png"
im.save(p, quality=95); print("  fig08 rebuilt", im.size)

# ---------- 图 8：去掉解释框 ----------
im = Image.new("RGB", (1600, 1000), "white"); d = ImageDraw.Draw(im)
L, R, B, T = 210, 1470, 860, 150
d.line((L, B, R, B), fill="#" + INK, width=5)
d.line((L, B, L, T), fill="#" + INK, width=5)
arrow(d, (R, B), (1545, B), INK, 5, 18)
arrow(d, (L, T), (L, 92), INK, 5, 18)
label(d, (840, 940), "任务生命周期覆盖程度", 34, True, INK, "ma")
d.text((145, 520), "执行与治理深度", font=font(34, True), fill="#" + INK, anchor="mm")
boxes = [
    (330, 760, "搜狗输入法", TEAL), (430, 690, "讯飞输入法", TEAL),
    (300, 605, "Wispr Flow", GOLD), (470, 530, "ChatGPT", NAVY),
    (390, 440, "Codex", NAVY), (520, 350, "MiMo Desktop", PURPLE),
    (630, 265, "Claude Code", PURPLE),
    (1180, 300, "Cuttle", ORANGE),
]
for x, y, name, color in boxes:
    d.ellipse((x - 16, y - 16, x + 16, y + 16), fill="#" + color)
    if name == "Cuttle":
        rounded(d, (x - 110, y - 40, x + 110, y + 40), 18, "FFF5F0", color, 5)
        center_text(d, (x - 110, y - 40, x + 110, y + 40), name, 32, True, color)
    else:
        rounded(d, (x + 24, y - 32, x + 258, y + 32), 18, LIGHT, color, 4)
        center_text(d, (x + 24, y - 32, x + 258, y + 32), name, 26, False, color)
p = ROOT / "docs" / "cuttle_v8_assets" / "fig03-competition-map.png"
im.save(p, quality=95); print("  fig03 rebuilt", im.size)

# ---------- 图 11：放大数值与图例，缩小画布信息密度 ----------
im = Image.new("RGB", (1800, 920), "white"); d = ImageDraw.Draw(im)
label(d, (900, 40), "三年经营测算（保守 / 基准 / 进取）", 48, True, NAVY, "ma")
years = ["第一年", "第二年", "第三年"]
rev  = [[1.9, 2.9, 4.3], [26.4, 48.8, 106.0], [63.2, 199.2, 352.0]]
cost = [[40, 50, 65], [55, 105, 190], [80, 190, 260]]
scen_color = [GOLD, TEAL, NAVY]
base_y, top, maxv = 770, 190, 360.0
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
    x = 640 + k * 200
    d.rectangle((x, 108, x + 40, 144), fill="#" + col)
    label(d, (x + 52, 110), txt, 30, False, INK)
d.rectangle((1240, 108, 1280, 144), fill="#" + ORANGE)
label(d, (1292, 110), "经营成本", 30, False, INK)
p = ROOT / "docs" / "cuttle_v8_assets" / "fig10-financial-plan.png"
im.save(p, quality=95); print("  fig10 rebuilt", im.size)

# ---------- 文档内文本修正 ----------
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
def set_text(p, t):
    runs = p.runs
    if not runs:
        p.add_run(t); return
    runs[0].text = t
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)
def set_cell(cell, text):
    ps = cell.paragraphs
    set_text(ps[0], text)
    for extra in ps[1:]:
        extra._element.getparent().remove(extra._element)

# 6.2 残留口径句
for p in doc.paragraphs:
    if p.text.strip().startswith("首年测算用于配置研发与人力"):
        set_text(p, "首年容量依据研发、试点与服务承载能力配置，正式试点开始后由真实使用数据持续校准；"
                    "若转化率低于预设下限，项目将缩小场景范围并重新测算。")
        print("  6.2 残留句改写")

# 表 6 中长期市场空间：删掉高等教育在学总规模与 [4]
for t in doc.tables:
    fc = [c.text.strip() for c in t.rows[0].cells]
    if fc[:2] == ["口径", "测算对象"]:
        for row in t.rows:
            if row.cells[0].text.strip().startswith("中长期市场空间"):
                set_cell(row.cells[2], "引用我国生成式人工智能用户规模、软件业务收入等公开统计数据作为行业背景")
                print("  表 6 删除教育人口引用")
        break

doc.save(SRC)
print("step 1 done")