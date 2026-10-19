# -*- coding: utf-8 -*-
"""Rebuild the four figures whose internal text still carries defensive/teaching phrasing."""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(r"T:\创新创业\OwO-master")
spec = importlib.util.spec_from_file_location("cuttlebuild", ROOT / "tools" / "build_cuttle_competition_v5.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, GOLD = m.NAVY, m.TEAL, m.ORANGE, m.GOLD
INK, GRAY, LIGHT, LIGHT_BLUE, WHITE = m.INK, m.GRAY, m.LIGHT, m.LIGHT_BLUE, m.WHITE
rounded, center_text, arrow, label, font = m.rounded, m.center_text, m.arrow, m.label, m.font
OUT = ROOT / "docs" / "cuttle_v8_assets"

def save(im, name):
    p = OUT / name; im.save(p, quality=95); print("  ->", p.name, im.size)

# ---- 图 1：去掉“被消耗的并非…”式解释腔 ----
im = Image.new("RGB", (1800, 760), "white"); d = ImageDraw.Draw(im)
label(d, (900, 50), "知识工作者的真实矛盾：意图产生的位置与任务承接的位置不一致", 36, True, NAVY, "ma")
cols = [
    (100, 165, 560, 560, "任务现场", "资料分散在竞品网页\n客户 PDF  销售表格\n群聊结论与旧方案中", NAVY),
    (670, 165, 1130, 560, "用户承担的额外工作", "手工搬运上下文\n重新解释格式与口径\n判断风险并回填结果", ORANGE),
    (1240, 165, 1700, 560, "未转化为生产力的环节", "意图产生处与任务承接处\n之间存在距离\n应用一切换  理解即中断", TEAL),
]
for x1, y1, x2, y2, title, body, color in cols:
    rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 5)
    center_text(d, (x1, y1 + 22, x2, y1 + 140), title, 34, True, color)
    center_text(d, (x1 + 28, y1 + 140, x2 - 28, y2 - 25), body, 27, False, INK, 14)
arrow(d, (550, 360), (665, 360), GRAY, 7, 20)
arrow(d, (1120, 360), (1235, 360), GRAY, 7, 20)
rounded(d, (280, 615, 1520, 720), 30, NAVY, NAVY, 1)
center_text(d, (280, 615, 1520, 720),
            "Cuttle 把输入时刻作为任务生命周期的原生起点  并在同一位置结束", 33, True, WHITE)
save(im, "fig01-opportunity.png")

# ---- 图 5：去掉“会把任务判反”的解说腔 ----
im = Image.new("RGB", (1800, 820), "white"); d = ImageDraw.Draw(im)
label(d, (900, 46), "三维自治决策：自治程度 · 协作形态 · 执行位置", 42, True, NAVY, "ma")
dims = [
    (150, 125, 570, 240, "自治程度", "文本表达  ·  工具调用  ·  智能体执行", TEAL),
    (690, 125, 1110, 240, "协作形态", "单执行者  ·  多执行者（契约接力）", GOLD),
    (1230, 125, 1650, 240, "执行位置", "本地  ·  云端  ·  跨设备（扩展）", PURPLE if False else GOLD),
]
dims[2] = (1230, 125, 1650, 240, "执行位置", "本地  ·  云端  ·  跨设备（第二阶段验证）", m.PURPLE)
for x1, y1, x2, y2, title, body, color in dims:
    rounded(d, (x1, y1, x2, y2), 24, LIGHT, color, 4)
    center_text(d, (x1 + 8, y1 + 10, x2 - 8, y1 + 62), title, 31, True, color)
    center_text(d, (x1 + 8, y1 + 60, x2 - 8, y2 - 10), body, 22, False, INK, 6)
for x in [360, 900, 1440]:
    arrow(d, (x, 250), (x, 300), GRAY, 5, 14)
rounded(d, (330, 305, 1470, 430), 22, LIGHT_BLUE, NAVY, 3)
center_text(d, (350, 315, 1450, 360), "示例：修复报错并补测试", 27, True, NAVY)
center_text(d, (350, 360, 1450, 420),
            "自治程度=智能体执行   协作形态=单执行者   执行位置=本地加沙箱   审批=必须确认", 23, False, INK)
label(d, (900, 452), "维度组合的判定依据", 28, True, ORANGE, "ma")
cases = [
    (150, 495, 900, 700, "让手机读取一张照片",
     "自治需求低、单执行者，但受数据位置约束。\n按单一层级会被高估为最高档。"),
    (900, 495, 1650, 700, "分别阅读两篇论文并输出摘要",
     "多执行者但动作不可写、风险很低，\n其等级不应高于自动修改版本库的单执行者任务。"),
]
for x1, y1, x2, y2, title, body in cases:
    rounded(d, (x1, y1, x2, y2), 24, LIGHT, GRAY, 3)
    center_text(d, (x1 + 12, y1 + 12, x2 - 12, y1 + 62), title, 26, True, INK)
    center_text(d, (x1 + 18, y1 + 66, x2 - 18, y2 - 12), body, 22, False, INK, 8)
rounded(d, (150, 725, 1650, 795), 22, LIGHT, TEAL, 3)
center_text(d, (170, 725, 1630, 795),
            "决策输出 = 三维取值 + 策略引擎裁定的审批方式（无需确认 / 先预览 / 必须确认 / 直接拒绝）",
            25, True, TEAL)
save(im, "fig06-intent-escalation.png")

# ---- 图 6：标题去掉“而不是数学模型” ----
im = Image.new("RGB", (1700, 930), "white"); d = ImageDraw.Draw(im)
label(d, (850, 46), "意图胶囊（IC）数据结构与可信属性", 40, True, NAVY, "ma")
rounded(d, (470, 130, 1230, 740), 55, LIGHT_BLUE, TEAL, 6)
center_text(d, (470, 138, 1230, 196), "IC = { I, C, O, P, F, S }", 38, True, TEAL)
rows = [
    ("I", "Intent 任务意图", "用户此刻要做什么  涉及谁"),
    ("C", "Context 最小情境", "只取完成本任务所需的最少信息"),
    ("O", "Origin 来源位置", "应用  窗口  输入控件  对象"),
    ("P", "Provenance 证据来源", "每条结论的来源与获取方式"),
    ("F", "Freshness 时效", "有效期与失效条件"),
    ("S", "Scope 授权范围", "可读取范围与可执行范围"),
]
y = 212
for code, title, body in rows:
    d.rounded_rectangle((500, y, 566, y + 58), radius=16, fill="#" + m.WHITE, outline="#" + NAVY, width=3)
    center_text(d, (500, y, 566, y + 58), code, 31, True, NAVY)
    label(d, (588, y + 2), title, 26, True, NAVY)
    label(d, (588, y + 31), body, 22, False, INK)
    y += 86
side = [
    (70, 220, 430, 415, "可纠正", "用户一键查看来源\n修正错误理解", ORANGE),
    (70, 480, 430, 675, "可失效", "状态变化立即失效\n避免旧情境误用", GOLD),
    (1270, 220, 1630, 415, "可溯源", "每条结论绑定来源\n时间与作用域", m.PURPLE),
    (1270, 480, 1630, 675, "最小必要", "读取范围与意图绑定\n超出即需确认", NAVY),
]
for x1, y1, x2, y2, title, body, color in side:
    rounded(d, (x1, y1, x2, y2), 26, LIGHT, color, 4)
    center_text(d, (x1 + 10, y1 + 10, x2 - 10, y1 + 80), title, 29, True, color)
    center_text(d, (x1 + 15, y1 + 82, x2 - 15, y2 - 10), body, 23, False, INK, 8)
rounded(d, (70, 745, 1630, 890), 30, LIGHT_BLUE, TEAL, 4)
center_text(d, (100, 762, 1600, 818),
            "字段信度为元数据  动作风险由策略引擎按具体执行计划判定", 29, True, TEAL)
center_text(d, (110, 820, 1590, 878),
            "评价口径：Origin 绑定正确率 · Context Recall 与 Precision · 失效上下文误用率 · "
            "Provenance 覆盖率 · 情境过度读取率",
            23, False, INK, 8)
save(im, "fig05-intent-capsule.png")

# ---- 图 7：标题去掉“不是市场份额 而是…” ----
im = Image.new("RGB", (1800, 850), "white"); d = ImageDraw.Draw(im)
label(d, (900, 40), "首年可交付用户容量与验证样本配置", 40, True, NAVY, "ma")
rounded(d, (600, 132, 1200, 218), 26, LIGHT_BLUE, TEAL, 3)
center_text(d, (600, 132, 1200, 218), "按服务承载能力配置  不按市场规模推算", 28, True, TEAL)
layers = [
    (600, 246, 1200, 388, NAVY, "首年可交付容量  不超过 200 人",
     "来源为团队可直接触达的组织、合作单位与知识工作场景\n覆盖 Word 取数核验  报告交付  代码修复三类任务"),
    (440, 416, 1360, 572, TEAL, "验证与实验容量  50 至 80 人",
     "纵向核心组 15 至 25 人  ·  独立验证组 20 至 30 人\n合计 35 至 55 人进入正式实验"),
    (280, 600, 1520, 756, GOLD, "中长期扩展路径（第二阶段验证）",
     "以复核率达标、交付人力可复制、支持成本可核算为前提  每校可服务约 200 至 400 人"),
]
for x1, y1, x2, y2, color, title, note in layers:
    rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 4)
    center_text(d, (x1 + 20, y1 + 16, x2 - 20, y1 + 78), title, 27, True, color)
    center_text(d, (x1 + 20, y1 + 74, x2 - 20, y2 - 14), note, 21, False, INK, 6)
arrow(d, (900, 400), (900, 412), ORANGE, 7, 18)
arrow(d, (900, 582), (900, 594), ORANGE, 7, 18)
notes = [
    (70, 446, 400, 542, "用途", "配置试点人力\n与研发节奏"),
    (1400, 446, 1730, 542, "口径", "按服务能力配置\n由真实转化数据替换"),
    (70, 630, 400, 726, "不下结论", "可服务市场与\n潜在市场暂不测算"),
    (1400, 630, 1730, 726, "调整条件", "转化低于预设下限\n则缩小场景范围"),
]
for x1, y1, x2, y2, title, note in notes:
    rounded(d, (x1, y1, x2, y2), 20, LIGHT, GRAY, 3)
    center_text(d, (x1 + 8, y1 + 8, x2 - 8, y1 + 44), title, 23, True, GRAY)
    center_text(d, (x1 + 10, y1 + 44, x2 - 10, y2 - 8), note, 19, False, INK, 6)
label(d, (60, 796), "容量测算用于配置资源  不用于承诺收入规模", 25, True, GRAY)
save(im, "fig08-market-funnel.png")