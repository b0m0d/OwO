# -*- coding: utf-8 -*-
"""重绘 v6 需要更新的 5 张图（复用 tools/build_cuttle_competition_v5.py 的配色与绘制风格）。

输出：docs/cuttle_v6_assets/figures/*.png
画布尺寸与 v5 原图逐像素一致，可直接替换 docx 内 media，版式不会变形：
  图 2  fig02-closed-loop.png        1800x700
  图 6  fig05-intent-capsule.png     1700x930
  图 7  fig08-market-funnel.png      1800x850
  图 10 fig11-roadmap.png            1800x740
  图 11 fig10-financial-plan.png     1800x920
"""
from __future__ import annotations

import importlib.util
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(r"T:\创新创业\OwO-master")
BUILD = ROOT / "tools" / "build_cuttle_competition_v5.py"
OUT = ROOT / "docs" / "cuttle_v6_assets" / "figures"
OUT.mkdir(parents=True, exist_ok=True)

spec = importlib.util.spec_from_file_location("cuttlebuild", BUILD)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

NAVY, TEAL, ORANGE, PURPLE, GOLD = m.NAVY, m.TEAL, m.ORANGE, m.PURPLE, m.GOLD
INK, GRAY, LIGHT, LIGHT_BLUE, GRID, WHITE = m.INK, m.GRAY, m.LIGHT, m.LIGHT_BLUE, m.GRID, m.WHITE
rounded, center_text, arrow, label, font = m.rounded, m.center_text, m.arrow, m.label, m.font


def new(w, h):
    im = Image.new("RGB", (w, h), "white")
    return im, ImageDraw.Draw(im)


def save(im, name):
    p = OUT / name
    im.save(p, quality=95)
    print("  ->", p.name, im.size)


def block_center(draw, box, text, size, bold=True, color=INK, spacing=10):
    """在 box 内绘制垂直居中的多行文本，返回实际墨迹上下界。"""
    f = font(size, bold)
    lines = text.split("\n")
    hs = [draw.textbbox((0, 0), ln, font=f)[3] for ln in lines]
    total = sum(hs) + spacing * (len(lines) - 1)
    y = (box[1] + box[3] - total) / 2
    top = y
    for ln, h in zip(lines, hs):
        w = draw.textbbox((0, 0), ln, font=f)[2]
        draw.text(((box[0] + box[2] - w) / 2, y), ln, font=f, fill="#" + color)
        y += h + spacing
    return top, y - spacing


# ================================================================ 图 2 闭环（1800x700）
im, d = new(1800, 700)
label(d, (900, 52), "从输入到结果回到原处的闭环", 42, True, NAVY, "ma")
stages = [
    ("输入时刻", "文字 语音 选区"),
    ("意图胶囊", "对象 来源 有效期"),
    ("意图路由", "文本 工具 Agent"),
    ("受控执行", "权限 预算 审批"),
    ("成果验证", "证据 差异 验收"),
    ("返回原处", "Word 聊天 IDE"),
]
colors = [NAVY, TEAL, GOLD, ORANGE, PURPLE, NAVY]
x = 65
for i, ((title, body), color) in enumerate(zip(stages, colors)):
    box = (x, 180, x + 245, 490)
    rounded(d, box, 26, LIGHT, color, 5)
    block_center(d, (x + 10, 200, x + 235, 330), title, 31, True, color)
    block_center(d, (x + 16, 340, x + 229, 470), body, 23, False, INK, 8)
    if i < len(stages) - 1:
        arrow(d, (x + 250, 335), (x + 300, 335), GRAY, 5, 14)
    x += 290
arrow(d, (1645, 575), (155, 575), TEAL, 5, 16)
label(d, (900, 635), "任一环节失败都可回滚  证据与未解决问题随成果一起返回",
      26, True, GRAY, "ma")
save(im, "fig02-closed-loop.png")

# ================================================================ 图 6 意图胶囊（1700x930）
im, d = new(1700, 930)
label(d, (850, 46), "意图胶囊 IC 保存完成当前意图所需的最小状态", 40, True, NAVY, "ma")
rounded(d, (470, 140, 1230, 740), 55, LIGHT_BLUE, TEAL, 6)
center_text(d, (470, 148, 1230, 202), "IC = [ I, C, O, P, F, R ]", 38, True, TEAL)
rows = [
    ("I", "Intent 意图", "用户此刻要做什么  涉及谁"),
    ("C", "Context 最小情境", "只取完成本任务所需的最少信息"),
    ("O", "Origin 来源位置", "应用  窗口  输入控件  对象"),
    ("P", "Provenance 证据来源", "结构化接口  可访问性树  视觉"),
    ("F", "Freshness 有效期", "应用切换  对象修改  任务结束后失效"),
    ("R", "Risk 权限边界", "可见  可改  可执行范围  审批要求"),
]
y = 212
for code, title, body in rows:
    d.rounded_rectangle((500, y, 566, y + 58), radius=16, fill="#" + WHITE, outline="#" + NAVY, width=3)
    center_text(d, (500, y, 566, y + 58), code, 31, True, NAVY)
    label(d, (588, y + 2), title, 26, True, NAVY)
    label(d, (588, y + 31), body, 22, False, INK)
    y += 86
side = [
    (70, 220, 430, 415, "可纠正", "用户一键查看来源\n修正错误理解", ORANGE),
    (70, 480, 430, 675, "可失效", "状态变化立即失效\n避免旧情境误用", GOLD),
    (1270, 220, 1630, 415, "可溯源", "每条结论绑定来源\n时间与作用域", PURPLE),
    (1270, 480, 1630, 675, "最小必要", "读取范围与意图绑定\n超出即需确认", NAVY),
]
for x1, y1, x2, y2, title, body, color in side:
    rounded(d, (x1, y1, x2, y2), 26, LIGHT, color, 4)
    center_text(d, (x1 + 10, y1 + 10, x2 - 10, y1 + 80), title, 29, True, color)
    center_text(d, (x1 + 15, y1 + 82, x2 - 15, y2 - 10), body, 23, False, INK, 8)
# 底部通栏：路由输出与可验证口径（填掉两侧空白带）
rounded(d, (70, 745, 1630, 890), 30, LIGHT_BLUE, TEAL, 4)
center_text(d, (100, 762, 1600, 818), "意图胶囊是输入  路由输出才是可验证的授权结果",
            29, True, TEAL)
center_text(d, (110, 820, 1590, 878),
            "上下文暴露范围 · 可用工具集 · 是否强制审批 · 成果回填形式        "
            "评测口径：路由 Macro-F1 · 用户纠正率 · 情境过度读取率",
            24, False, INK, 8)
save(im, "fig05-intent-capsule.png")

# ================================================================ 图 7 市场漏斗（1800x850）
im, d = new(1800, 850)
label(d, (900, 42), "市场规模采用自下而上的三层测算", 42, True, NAVY, "ma")
rounded(d, (640, 134, 1160, 228), 26, LIGHT_BLUE, TEAL, 3)
center_text(d, (640, 134, 1160, 228), "从可直接影响的最小单元开始测算", 29, True, TEAL)
layers = [
    (150, 660, 1650, 790, NAVY,
     "TAM 潜在市场（仅作参照）  全国 PC 知识工作人口",
     "高等教育在学 4872.57 万人  高频知识工作者占比待调研确定"),
    (290, 470, 1510, 600, TEAL,
     "SAM 可服务市场  双一流与同类高校的实验室与学生团队",
     "每校 200 至 400 人 × 同类高校约 400 所 ≈ 8 万至 16 万人"),
    (450, 285, 1350, 415, GOLD,
     "SOM 可获取市场  西南大学第一年种子用户",
     "1500 人 × 40% 高频任务 × 50% 愿试用 × 2/3 转化 ≈ 200 人"),
]
notes = [
    (60, 495, 275, 585, "假设", "每校覆盖人数\n需本地试点负责人"),
    (1525, 495, 1740, 585, "假设", "占比待调研\n不下结论"),
    (60, 685, 275, 775, "校验", "逐层用访谈\n试点与付费数据校准"),
    (1525, 685, 1740, 775, "校验", "基线耗时\n与转化率实测"),
]
for x1, y1, x2, y2, color, title, note in layers:
    rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 4)
    center_text(d, (x1 + 20, y1 + 16, x2 - 20, y1 + 82), title, 29, True, color)
    center_text(d, (x1 + 20, y1 + 82, x2 - 20, y2 - 14), note, 23, False, INK)
for x1, y1, x2, y2, title, note in notes:
    rounded(d, (x1, y1, x2, y2), 20, LIGHT, GRAY, 3)
    center_text(d, (x1 + 8, y1 + 8, x2 - 8, y1 + 44), title, 23, True, GRAY)
    center_text(d, (x1 + 10, y1 + 44, x2 - 10, y2 - 8), note, 19, False, INK, 6)
arrow(d, (900, 655), (900, 425), ORANGE, 7, 22)
arrow(d, (900, 465), (900, 238), ORANGE, 7, 22)
label(d, (60, 806), "假设逐层标注  每一层都以真实用户研究与试点数据校准", 26, True, GRAY)
save(im, "fig08-market-funnel.png")

# ================================================================ 图 10 二十四个月路线（1800x740）
im, d = new(1800, 740)
label(d, (900, 42), "二十四个月研发与市场路线", 42, True, NAVY, "ma")
axis_y = 400
d.line((140, axis_y, 1660, axis_y), fill="#" + INK, width=7)
phases = [
    (220, "0 至 3 个月", "问题验证", "访谈 日记\n交互原型\n基线耗时", NAVY),
    (520, "4 至 6 个月", "闭环原型", "输入入口\n意图胶囊\n单 Agent 返回", TEAL),
    (850, "7 至 12 个月", "兼容矩阵与标准化", "Word 浏览器 VS Code\n微信 QQ 办公套件\nOrigin 与 Return 适配", GOLD),
    (1190, "13 至 18 个月", "团队版", "校园团队许可\n成果契约接口\n协同净收益", ORANGE),
    (1510, "19 至 24 个月", "规模复制", "工程加固\n合同与交付\n跨设备扩展验证", PURPLE),
]
for i, (x, span, title, body, color) in enumerate(phases):
    d.ellipse((x - 16, axis_y - 16, x + 16, axis_y + 16), fill="#" + color)
    up = i % 2 == 0
    box = (x - 150, 118 if up else 468, x + 150, 360 if up else 690)
    d.line((x, axis_y, x, box[3] if up else box[1]), fill="#" + color, width=4)
    rounded(d, box, 24, LIGHT, color, 4)
    block_center(d, (box[0] + 5, box[1] + 10, box[2] - 5, box[1] + 62), span, 22, True, color)
    block_center(d, (box[0] + 5, box[1] + 68, box[2] - 5, box[1] + 112), title, 26, True, INK)
    block_center(d, (box[0] + 12, box[1] + 116, box[2] - 12, box[3] - 18), body, 20, False, INK, 4)
label(d, (140, 700), "7 至 12 个月主线为输入法兼容矩阵与 Origin/Return 适配器标准化",
      21, True, GOLD)
save(im, "fig11-roadmap.png")

# ================================================================ 图 11 三情景财务（1800x920）
im, d = new(1800, 920)
label(d, (900, 36), "三年经营情景（保守 / 基准 / 进取）", 42, True, NAVY, "ma")
years = ["第一年", "第二年", "第三年"]
rev = [[53.4, 53.4, 82.8], [180, 320, 560], [640, 1280, 1980]]
cost = [[120, 120, 120], [220, 330, 430], [550, 1080, 1180]]
scen = ["保守", "基准", "进取"]
scen_color = [GOLD, TEAL, NAVY]

# 顶部横向图例：说明条形含义与三情景配色
rounded(d, (55, 92, 1745, 152), 16, LIGHT, GRID, 2)
d.rectangle((85, 108, 122, 136), fill="#" + TEAL)
label(d, (132, 110), "营业收入", 22, False, INK)
d.rectangle((250, 108, 287, 136), fill="#" + ORANGE)
label(d, (297, 110), "经营成本", 22, False, INK)
label(d, (420, 109), "柱色 = 情景", 23, True, GRAY)
for _k, (_col, _txt) in enumerate(zip(scen_color, scen)):
    _x = 650 + _k * 165
    d.rectangle((_x, 108, _x + 36, 136), fill="#" + _col)
    label(d, (_x + 46, 110), _txt, 22, False, INK)

base_y = 740
top = 190
maxv = 2000.0
for i, year in enumerate(years):
    cx = 380 + i * 520
    xs = [cx - 185, cx - 55, cx + 55]
    for j in range(3):
        ch = int(cost[i][j] / maxv * (base_y - top))
        d.rectangle((xs[j] - 12, base_y - ch, xs[j] + 12, base_y), fill="#" + ORANGE)
    for j in range(3):
        rh = int(rev[i][j] / maxv * (base_y - top))
        d.rectangle((xs[j] - 34, base_y - rh, xs[j] - 16, base_y), fill="#" + scen_color[j])
        label(d, (xs[j] - 25, base_y + 16), scen[j], 21, True, INK, "ma")
    d.line((cx - 240, base_y, cx + 240, base_y), fill="#" + GRID, width=3)
    label(d, (cx, 812), year, 28, True, INK, "ma")
d.line((150, base_y, 1650, base_y), fill="#" + INK, width=4)
label(d, (150, 862), "各年左中右三组自上而下为保守 基准 进取 实色条为营业收入 橙色条为经营成本",
      22, False, GRAY)
label(d, (1650, 862), "具体数值见表 19", 22, True, GRAY, "ra")
save(im, "fig10-financial-plan.png")

print("done ->", OUT)
