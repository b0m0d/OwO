# -*- coding: utf-8 -*-
"""v8 figures: redraw every figure whose internal text no longer matches the v8 narrative.
Canvas sizes are kept identical to the embedded originals so the docx layout is unchanged.
"""
from __future__ import annotations
import importlib.util
from pathlib import Path
from PIL import Image, ImageDraw

ROOT = Path(r"T:\创新创业\OwO-master")
BUILD = ROOT / "tools" / "build_cuttle_competition_v5.py"
OUT = ROOT / "docs" / "cuttle_v8_assets"
OUT.mkdir(parents=True, exist_ok=True)

spec = importlib.util.spec_from_file_location("cuttlebuild", BUILD)
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
NAVY, TEAL, ORANGE, PURPLE, GOLD = m.NAVY, m.TEAL, m.ORANGE, m.PURPLE, m.GOLD
INK, GRAY, LIGHT, LIGHT_BLUE, GRID, WHITE = m.INK, m.GRAY, m.LIGHT, m.LIGHT_BLUE, m.GRID, m.WHITE
rounded, center_text, arrow, label, font = m.rounded, m.center_text, m.arrow, m.label, m.font

def new(w, h):
    im = Image.new("RGB", (w, h), "white"); return im, ImageDraw.Draw(im)

def save(im, name):
    p = OUT / name; im.save(p, quality=95); print("  ->", p.name, im.size)

def block_center(draw, box, text, size, bold=True, color=INK, spacing=10):
    f = font(size, bold); lines = text.split("\n")
    hs = [draw.textbbox((0, 0), ln, font=f)[3] for ln in lines]
    total = sum(hs) + spacing * (len(lines) - 1)
    y = (box[1] + box[3] - total) / 2
    for ln, h in zip(lines, hs):
        w = draw.textbbox((0, 0), ln, font=f)[2]
        draw.text(((box[0] + box[2] - w) / 2, y), ln, font=f, fill="#" + color)
        y += h + spacing

# ================================================================ 图 1 真实问题 (1800x760)
im, d = new(1800, 760)
label(d, (900, 50), "知识工作者的真实矛盾：意图产生的位置与任务承接的位置不一致", 36, True, NAVY, "ma")
cols = [
    (100, 165, 560, 560, "任务现场",
     "资料分散在竞品网页\n客户 PDF  销售表格\n群聊结论与旧方案中", NAVY),
    (670, 165, 1130, 560, "用户承担的额外工作",
     "手工搬运上下文\n重新解释格式与口径\n判断风险并回填结果", ORANGE),
    (1240, 165, 1700, 560, "被消耗的并非模型能力",
     "而是意图产生处与任务\n承接处之间的这段距离\n应用一切换  理解即中断", TEAL),
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

# ================================================================ 图 3 竞争定位 (1600x1000)
im, d = new(1600, 1000)
margin = 150
d.line((margin, 850, 1450, 850), fill="#" + INK, width=5)
d.line((margin, 850, margin, 120), fill="#" + INK, width=5)
arrow(d, (1450, 850), (1510, 850), INK, 5, 18)
arrow(d, (margin, 120), (margin, 65), INK, 5, 18)
label(d, (830, 945), "任务生命周期的覆盖程度  从输入焦点到成果返回原处", 28, True, INK, "ma")
label(d, (45, 450), "治理与可靠性  证据  审批  恢复", 28, True, INK, "mm")
points = [
    (380, 720, "搜狗输入法", TEAL), (470, 655, "讯飞输入法", TEAL),
    (950, 625, "Wispr Flow", GOLD),
    (430, 285, "Codex", NAVY), (560, 225, "MiMo Desktop", PURPLE),
    (1120, 300, "Claude Code", PURPLE),
    (1290, 160, "Cuttle", ORANGE),
]
for x, y, name, color in points:
    d.ellipse((x - 14, y - 14, x + 14, y + 14), fill="#" + color)
    rounded(d, (x + 20, y - 38, x + 245, y + 38), 18, LIGHT, color, 3)
    center_text(d, (x + 20, y - 38, x + 245, y + 38), name, 24, name == "Cuttle", color)
d.rounded_rectangle((980, 60, 1530, 285), radius=28, fill="#FFF5F0", outline="#" + ORANGE, width=4)
label(d, (1010, 85), "Cuttle 的目标位置", 28, True, ORANGE)
for k, txt in enumerate(["输入焦点原生的意图入口", "最小情境与来源可追溯", "受控执行与独立审批", "成果返回需求原处"]):
    label(d, (1010, 135 + k * 36), txt, 23, False, INK)
label(d, (150, 70), "判断依据为公开文档  能力边界随版本变化  不对任何产品作能力否定断言",
      22, False, GRAY)
save(im, "fig03-competition-map.png")

# ================================================================ 图 4 架构 (1800x950)
im, d = new(1800, 950)
label(d, (900, 50), "一个统一入口与四个核心系统", 42, True, NAVY, "ma")
rounded(d, (500, 130, 1300, 250), 30, NAVY, NAVY, 1)
center_text(d, (500, 130, 1300, 250), "统一输入入口  键盘 拼音 语音 选区 快捷指令", 32, True, WHITE)
systems = [
    (180, 360, 540, 680, "Context Engine", "读取完成本任务\n所需的最小情境\n生成意图胶囊 IC", TEAL),
    (590, 360, 950, 680, "Intent Router", "判定自治程度\n协作形态与执行位置\n输出审批要求", GOLD),
    (1000, 360, 1360, 680, "Agent Fabric", "按契约拆分与执行\n独立审批与最小权限\n可回滚  可审计", ORANGE),
    (1410, 360, 1770, 680, "Verification Return", "绑定证据与验收条件\n返回需求产生的\n应用与输入位置", PURPLE),
]
for x1, y1, x2, y2, title, body, color in systems:
    rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 5)
    center_text(d, (x1 + 15, y1 + 25, x2 - 15, y1 + 135), title, 27, True, color)
    center_text(d, (x1 + 20, y1 + 145, x2 - 20, y2 - 25), body, 24, False, INK, 12)
for x in [360, 770, 1180, 1590]:
    arrow(d, (900, 260), (x, 345), GRAY, 5, 14)
rounded(d, (240, 780, 1560, 890), 26, LIGHT_BLUE, TEAL, 3)
center_text(d, (240, 780, 1560, 890),
            "共同底座  最小权限  本地优先  独立审批  审计与回滚  版本化成果契约  模型可替换",
            27, True, TEAL)
save(im, "fig04-architecture.png")

# ================================================================ 图 5 三维决策 (1800x820)
im, d = new(1800, 820)
label(d, (900, 46), "每一次输入都要回答三个相互独立的问题", 42, True, NAVY, "ma")
dims = [
    (150, 125, 570, 240, "自治程度", "文本表达  ·  工具调用  ·  智能体执行", TEAL),
    (690, 125, 1110, 240, "协作形态", "单执行者  ·  多执行者（契约接力）", GOLD),
    (1230, 125, 1650, 240, "执行位置", "本地  ·  云端  ·  跨设备", PURPLE),
]
for x1, y1, x2, y2, title, body, color in dims:
    rounded(d, (x1, y1, x2, y2), 24, LIGHT, color, 4)
    center_text(d, (x1 + 8, y1 + 10, x2 - 8, y1 + 62), title, 31, True, color)
    center_text(d, (x1 + 8, y1 + 60, x2 - 8, y2 - 10), body, 23, False, INK, 6)
for x in [360, 900, 1440]:
    arrow(d, (x, 250), (x, 300), GRAY, 5, 14)
rounded(d, (330, 305, 1470, 430), 22, LIGHT_BLUE, NAVY, 3)
center_text(d, (350, 315, 1450, 360), "例：修复报错并补测试", 27, True, NAVY)
center_text(d, (350, 360, 1450, 420),
            "自治程度=智能体执行   协作形态=单执行者   执行位置=本地加沙箱   审批=必须确认", 23, False, INK)
label(d, (900, 452), "拆分维度的意义：单一层级会把不同性质的任务判反", 28, True, ORANGE, "ma")
cases = [
    (150, 495, 900, 700, "反例一  让手机读取一张照片",
     "自治需求低  单执行者  但跨设备。若按单轴层级，\n会被误判为最高档，并错误地提高权限与审批强度。"),
    (900, 495, 1650, 700, "反例二  分别阅读两篇论文并只输出摘要",
     "多执行者但动作不可写、风险很低。它不应比\n自动修改版本库的单执行者任务层级更高。"),
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

# ================================================================ 图 6 IC 结构 (1700x930) —— R 改为 S
im, d = new(1700, 930)
label(d, (850, 46), "意图胶囊 IC 是数据结构而不是数学模型", 40, True, NAVY, "ma")
rounded(d, (470, 130, 1230, 740), 55, LIGHT_BLUE, TEAL, 6)
center_text(d, (470, 138, 1230, 196), "IC = { I, C, O, P, F, S }", 38, True, TEAL)
rows = [
    ("I", "Intent 意图", "用户此刻要做什么  涉及谁"),
    ("C", "Context 最小情境", "只取完成本任务所需的最少信息"),
    ("O", "Origin 来源位置", "应用  窗口  输入控件  对象"),
    ("P", "Provenance 证据来源", "每条结论的来源与获取方式"),
    ("F", "Freshness 时效", "有效期与失效条件"),
    ("S", "Scope 范围与同意边界", "可读取范围与可执行范围"),
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
rounded(d, (70, 745, 1630, 890), 30, LIGHT_BLUE, TEAL, 4)
center_text(d, (100, 762, 1600, 818),
            "字段信度是元数据  动作风险由策略引擎按具体动作裁定  不进入 IC", 29, True, TEAL)
center_text(d, (110, 820, 1590, 878),
            "评价口径：Origin 绑定正确率 · Context Recall 与 Precision · 失效上下文误用率 · "
            "Provenance 覆盖率 · 情境过度读取率",
            23, False, INK, 8)
save(im, "fig05-intent-capsule.png")

# ================================================================ 图 7 研究路径 (1800x850)
im, d = new(1800, 850)
label(d, (900, 45), "从问题发现到价值验证的研究路径", 42, True, NAVY, "ma")
phases = [
    (100, 175, 560, 520, "发现问题",
     "半结构化访谈\n任务日记与情境观察\n基线耗时测量", NAVY),
    (670, 175, 1130, 520, "验证机制",
     "可交互原型与 Wizard of Oz\n三组入口对照实验\n机制消融实验", TEAL),
    (1240, 175, 1700, 520, "验证价值",
     "纵向核心组 15 至 25 人\n独立验证组 20 至 30 人\n同组内交叉设计", ORANGE),
]
for i, (x1, y1, x2, y2, title, body, color) in enumerate(phases):
    rounded(d, (x1, y1, x2, y2), 30, LIGHT, color, 5)
    center_text(d, (x1 + 15, y1 + 22, x2 - 15, y1 + 120), title, 34, True, color)
    center_text(d, (x1 + 22, y1 + 130, x2 - 22, y2 - 20), body, 26, False, INK, 14)
    if i < 2:
        arrow(d, (x2 + 12, 340), (x2 + 95, 340), GRAY, 6, 17)
outputs = [
    (100, 555, 1700 - 1140 - 0 + 0, 700, NAVY, "输出", "高频任务清单与敏感边界清单"),
    (670, 555, 1130, 700, TEAL, "输出", "交互原型与核心指标基线"),
    (1240, 555, 1700, 700, ORANGE, "输出", "留存  付费意愿与采购证据"),
]
outputs[0] = (100, 555, 560, 700, NAVY, "输出", "高频任务清单与敏感边界清单")
for x1, y1, x2, y2, color, tag, body in outputs:
    rounded(d, (x1, y1, x2, y2), 22, LIGHT_BLUE, color, 3)
    center_text(d, (x1 + 12, y1 + 8, x2 - 12, y1 + 52), tag, 24, True, color)
    center_text(d, (x1 + 14, y1 + 54, x2 - 14, y2 - 10), body, 22, False, INK, 6)
rounded(d, (100, 735, 1700, 805), 20, LIGHT, GRAY, 3)
center_text(d, (120, 735, 1680, 805),
            "统计口径：阈值由预实验确定  样本量依据效应量做统计功效分析  结果报告效应量与置信区间",
            24, True, INK)
save(im, "fig07-validation-plan.png")

# ================================================================ 图 8 试点容量 (1800x850)
im, d = new(1800, 850)
label(d, (900, 42), "第一年不是市场份额  而是团队能够真实服务的容量", 40, True, NAVY, "ma")
rounded(d, (600, 138, 1200, 232), 26, LIGHT_BLUE, TEAL, 3)
center_text(d, (600, 138, 1200, 232), "从可直接影响的最小样本单元出发", 29, True, TEAL)
layers = [
    (300, 665, 1500, 795, GOLD,
     "后续外推路径（待验证）  首校复制率验证之后的可服务规模",
     "以复核率达标、交付人力可复制、支持成本可核算为前提  每校可服务约 200 至 400 人"),
    (450, 470, 1350, 600, TEAL,
     "深度与对照容量  50 至 80 人形成深度数据",
     "纵向核心组 15 至 25 人  ·  独立验证组 20 至 30 人  ·  合计 35 至 55 人进入正式实验"),
    (600, 285, 1200, 415, NAVY,
     "第一年可交付容量  约 200 人",
     "相关人群 3000 人 × 高频任务约 40% × 愿试用约 50% × 稳定观察约三分之二"),
]
for x1, y1, x2, y2, color, title, note in layers:
    rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 4)
    center_text(d, (x1 + 20, y1 + 16, x2 - 20, y1 + 82), title, 27, True, color)
    center_text(d, (x1 + 20, y1 + 82, x2 - 20, y2 - 14), note, 22, False, INK)
notes = [
    (70, 500, 380, 590, "用途", "配置试点人力\n与研发节奏"),
    (1420, 500, 1730, 590, "口径", "比例来自前期访谈\n由真实转化替换"),
    (70, 690, 380, 780, "不下结论", "可服务市场与\n潜在市场暂不测算"),
    (1420, 690, 1730, 780, "替换条件", "转化低于预设下限\n则缩小场景范围"),
]
for x1, y1, x2, y2, title, note in notes:
    rounded(d, (x1, y1, x2, y2), 20, LIGHT, GRAY, 3)
    center_text(d, (x1 + 8, y1 + 8, x2 - 8, y1 + 44), title, 23, True, GRAY)
    center_text(d, (x1 + 10, y1 + 44, x2 - 10, y2 - 8), note, 19, False, INK, 6)
arrow(d, (900, 660), (900, 425), ORANGE, 7, 22)
arrow(d, (900, 465), (900, 245), ORANGE, 7, 22)
label(d, (70, 812), "容量测算用于配置资源  不用于承诺收入规模", 25, True, GRAY)
save(im, "fig08-market-funnel.png")

# ================================================================ 图 9 市场进入 (1800x760)
im, d = new(1800, 760)
label(d, (900, 45), "从知识工作场景样板到团队许可与私有部署", 42, True, NAVY, "ma")
stages = [
    (100, 170, 560, 620, "第一阶段",
     "高频 PC 知识工作场景",
     "跨来源取数与核验\n报告与材料交付\n代码修复与测试", NAVY),
    (670, 170, 1130, 620, "第二阶段",
     "团队许可与开发者生态",
     "团队版与统一策略\n开源 SDK 与技能模板\n跨应用协议标准化", TEAL),
    (1240, 170, 1700, 620, "第三阶段",
     "机构与私有部署",
     "内网模型与定制策略\n审计治理与合规\n规模化复制", ORANGE),
]
for i, (x1, y1, x2, y2, phase, who, tasks, color) in enumerate(stages):
    rounded(d, (x1, y1, x2, y2), 32, LIGHT, color, 5)
    center_text(d, (x1 + 10, y1 + 20, x2 - 10, y1 + 100), phase, 28, True, color)
    center_text(d, (x1 + 20, y1 + 110, x2 - 20, y1 + 230), who, 28, True, INK)
    center_text(d, (x1 + 25, y1 + 250, x2 - 25, y2 - 25), tasks, 24, False, INK, 12)
    if i < 2:
        arrow(d, (x2 + 15, 395), (x2 + 95, 395), GRAY, 6, 17)
label(d, (900, 680), "每个阶段以决策门结算：达标才进入下一阶段  未达标则缩小场景或暂停投入",
      26, True, GRAY, "ma")
save(im, "fig09-go-to-market.png")

# ================================================================ 图 11 三年三情景 (1800x920)
im, d = new(1800, 920)
label(d, (900, 40), "三年经营测算（保守 / 基准 / 进取三情景）", 42, True, NAVY, "ma")
years = ["第一年", "第二年", "第三年"]
rev  = [[14.4, 60.6, 104], [146, 272, 482], [372, 836, 1464]]
cost = [[90, 110, 120], [180, 282, 486], [390, 636, 1320]]
scen = ["保守", "基准", "进取"]
scen_color = [GOLD, TEAL, NAVY]
base_y, top, maxv = 745, 200, 1480.0
scale = (base_y - top) / maxv
for i, year in enumerate(years):
    cx = 380 + i * 520
    for j in range(3):
        bx = cx - 190 + j * 130
        rh = int(rev[i][j] * scale)
        ch = int(cost[i][j] * scale)
        d.rectangle((bx, base_y - rh, bx + 52, base_y), fill="#" + scen_color[j])
        d.rectangle((bx + 66, base_y - ch, bx + 118, base_y), fill="#" + ORANGE)
        fv = font(21, True)
        wv = d.textbbox((0, 0), f"{rev[i][j]:g}", font=fv)[2]
        d.text((bx + 26 - wv / 2, base_y - rh - 26), f"{rev[i][j]:g}", font=fv, fill="#" + scen_color[j])
        fc = font(21, False)
        wc = d.textbbox((0, 0), f"{cost[i][j]:g}", font=fc)[2]
        if ch <= 46:
            d.text((bx + 92 - wc / 2, base_y - ch - 26), f"{cost[i][j]:g}", font=fc, fill="#" + ORANGE)
        else:
            d.text((bx + 92 - wc / 2, base_y - ch + 10), f"{cost[i][j]:g}", font=fc, fill="#" + WHITE)
    label(d, (cx, 812), year, 28, True, INK, "ma")
d.line((150, base_y, 1650, base_y), fill="#" + INK, width=4)
# 图例
rounded(d, (1230, 95, 1740, 178), 18, LIGHT, GRID, 2)
d.rectangle((1262, 120, 1298, 146), fill="#" + GOLD)
d.rectangle((1340, 120, 1376, 146), fill="#" + TEAL)
d.rectangle((1418, 120, 1454, 146), fill="#" + NAVY)
label(d, (1306, 121), "保守", 22, False, INK)
label(d, (1384, 121), "基准", 22, False, INK)
label(d, (1462, 121), "进取", 22, False, INK)
label(d, (1262, 152), "柱色 = 情景   橙柱 = 经营成本", 22, True, GRAY)
label(d, (210, 862), "单位 万元   数值取自表 19 三个情景的营业收入与经营成本", 22, False, GRAY)
label(d, (1650, 862), "基准情景第三年进入盈亏平衡上方", 22, True, TEAL, "ra")
label(d, (900, 130), "第一年基准与保守收入相同（60.6 万元）  进取情景为 104 万元", 24, True, GRAY, "ma")
save(im, "fig10-financial-plan.png")

print("done ->", OUT)