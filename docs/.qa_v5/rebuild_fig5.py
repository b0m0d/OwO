# -*- coding: utf-8 -*-
"""Rebuild 图 5: mark cross-device as an extension (dashed), rename to 三维任务决策."""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(r"T:\创新创业\OwO-master")
spec = importlib.util.spec_from_file_location("cuttlebuild", ROOT / "tools" / "build_cuttle_competition_v5.py")
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, GOLD, PURPLE = m.NAVY, m.TEAL, m.ORANGE, m.GOLD, m.PURPLE
INK, GRAY, LIGHT, LIGHT_BLUE, WHITE = m.INK, m.GRAY, m.LIGHT, m.LIGHT_BLUE, m.WHITE
rounded, center_text, arrow, label = m.rounded, m.center_text, m.arrow, m.label

im = Image.new("RGB", (1800, 820), "white"); d = ImageDraw.Draw(im)
label(d, (900, 46), "三维任务决策：自治程度 · 协作形态 · 执行位置", 42, True, NAVY, "ma")
dims = [
    (110, 125, 570, 250, "自治程度", "文本表达  ·  工具调用  ·  智能体执行", TEAL, None),
    (670, 125, 1130, 250, "协作形态", "单执行者  ·  多执行者（契约接力）", GOLD, None),
    (1230, 125, 1690, 250, "执行位置", "本地  ·  云端  ·  跨设备（扩展）", PURPLE, "跨设备为第二阶段验证"),
]
for x1, y1, x2, y2, title, body, color, note in dims:
    rounded(d, (x1, y1, x2, y2), 24, LIGHT, color, 4)
    center_text(d, (x1 + 8, y1 + 12, x2 - 8, y1 + 62), title, 31, True, color)
    center_text(d, (x1 + 8, y1 + 60, x2 - 8, y1 + 104), body, 22, False, INK, 6)
    if note:
        center_text(d, (x1 + 8, y1 + 100, x2 - 8, y2 - 8), note, 19, True, ORANGE, 4)
for x in [340, 900, 1460]:
    arrow(d, (x, 258), (x, 300), GRAY, 5, 14)
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
p = ROOT / "docs" / "cuttle_v8_assets" / "fig06-intent-escalation.png"
im.save(p, quality=95)
print("saved", p, im.size)