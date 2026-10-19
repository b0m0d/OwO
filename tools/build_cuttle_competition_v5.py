from __future__ import annotations

import math
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont
from docx import Document
from docx.enum.section import WD_SECTION
from docx.enum.table import WD_CELL_VERTICAL_ALIGNMENT, WD_TABLE_ALIGNMENT
from docx.enum.text import WD_ALIGN_PARAGRAPH, WD_BREAK, WD_LINE_SPACING
from docx.oxml import OxmlElement
from docx.oxml.ns import qn
from docx.shared import Cm, Inches, Pt, RGBColor


ROOT = Path(r"T:\创新创业\OwO-master")
DOCS = ROOT / "docs"
ASSETS = DOCS / "cuttle_v5_assets"
FIGS = ASSETS / "figures"
OUTPUT = DOCS / "Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v5.docx"
LOGO = ROOT / "builGoal" / "项目计划书素材" / "cuttle-assets" / "cuttle-logo.png"

NAVY = "163B65"
TEAL = "1C9AA5"
ORANGE = "EF6A4A"
PURPLE = "735FA7"
GOLD = "E3A62F"
INK = "1D2733"
GRAY = "66717E"
LIGHT = "F3F6F8"
LIGHT_BLUE = "EAF3F7"
GRID = "D9D9D9"
WHITE = "FFFFFF"
BLACK = "000000"

FONT_REG = r"C:\Windows\Fonts\msyh.ttc"
FONT_BOLD = r"C:\Windows\Fonts\msyhbd.ttc"


def ensure_dirs() -> None:
    FIGS.mkdir(parents=True, exist_ok=True)


def font(size: int, bold: bool = False) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(FONT_BOLD if bold else FONT_REG, size)


def rounded(draw: ImageDraw.ImageDraw, box, radius=22, fill=WHITE, outline=NAVY, width=3):
    draw.rounded_rectangle(box, radius=radius, fill="#" + fill, outline="#" + outline, width=width)


def center_text(draw, box, text, size=34, bold=False, color=INK, spacing=8):
    f = font(size, bold)
    lines = text.split("\n")
    heights = [draw.textbbox((0, 0), line, font=f)[3] for line in lines]
    total = sum(heights) + spacing * (len(lines) - 1)
    y = (box[1] + box[3] - total) / 2
    for line, h in zip(lines, heights):
        w = draw.textbbox((0, 0), line, font=f)[2]
        draw.text(((box[0] + box[2] - w) / 2, y), line, font=f, fill="#" + color)
        y += h + spacing


def arrow(draw, start, end, color=GRAY, width=6, head=16):
    draw.line([start, end], fill="#" + color, width=width)
    angle = math.atan2(end[1] - start[1], end[0] - start[0])
    p1 = (
        end[0] - head * math.cos(angle - math.pi / 6),
        end[1] - head * math.sin(angle - math.pi / 6),
    )
    p2 = (
        end[0] - head * math.cos(angle + math.pi / 6),
        end[1] - head * math.sin(angle + math.pi / 6),
    )
    draw.polygon([end, p1, p2], fill="#" + color)


def label(draw, xy, text, size=28, bold=False, color=INK, anchor="la"):
    draw.text(xy, text, font=font(size, bold), fill="#" + color, anchor=anchor)


def save_fig(name: str, image: Image.Image) -> Path:
    path = FIGS / name
    image.save(path, quality=94)
    return path


def build_figures() -> dict[str, Path]:
    figs: dict[str, Path] = {}

    im = Image.new("RGB", (1800, 760), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 50), "现有 AI 工作流的三个断点", 42, True, NAVY, "ma")
    cols = [
        (120, 170, 540, 520, "入口割裂", "用户离开当前应用\n打开对话框重新描述需求", NAVY),
        (690, 170, 1110, 520, "情境割裂", "应用 对象 关系和历史成果\n需要手工搬运", TEAL),
        (1260, 170, 1680, 520, "行动割裂", "生成结果留在 AI 窗口\n用户再复制 审核和回填", ORANGE),
    ]
    for x1, y1, x2, y2, title, body, color in cols:
        rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 5)
        center_text(d, (x1, y1 + 20, x2, y1 + 135), title, 38, True, color)
        center_text(d, (x1 + 30, y1 + 130, x2 - 30, y2 - 25), body, 28, False, INK, 14)
    arrow(d, (530, 345), (675, 345), GOLD, 7, 20)
    arrow(d, (1100, 345), (1245, 345), GOLD, 7, 20)
    rounded(d, (310, 610, 1490, 715), 30, NAVY, NAVY, 1)
    center_text(d, (310, 610, 1490, 715), "Cuttle 把输入时刻转化为可验证行动的起点", 34, True, WHITE)
    figs["opportunity"] = save_fig("fig01-opportunity.png", im)

    im = Image.new("RGB", (1800, 700), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 48), "从输入到结果回到原处的闭环", 42, True, NAVY, "ma")
    stages = [
        ("输入时刻", "文字 语音 选区"),
        ("情境胶囊", "对象 来源 有效期"),
        ("意图路由", "文本 工具 Agent"),
        ("受控执行", "权限 预算 审批"),
        ("成果验证", "证据 差异 验收"),
        ("返回原处", "Word 聊天 IDE"),
    ]
    colors = [NAVY, TEAL, GOLD, ORANGE, PURPLE, NAVY]
    x = 65
    for i, ((title, body), color) in enumerate(zip(stages, colors)):
        box = (x, 210, x + 245, 480)
        rounded(d, box, 26, LIGHT, color, 5)
        center_text(d, (x + 10, 220, x + 235, 330), title, 31, True, color)
        center_text(d, (x + 16, 335, x + 229, 460), body, 23, False, INK, 8)
        if i < len(stages) - 1:
            arrow(d, (x + 250, 345), (x + 300, 345), GRAY, 5, 14)
        x += 290
    arrow(d, (1645, 530), (155, 530), TEAL, 5, 16)
    label(d, (900, 580), "任务在哪里产生  验证后的成果就回到哪里", 29, True, TEAL, "ma")
    figs["closed_loop"] = save_fig("fig02-closed-loop.png", im)

    im = Image.new("RGB", (1600, 1000), "white")
    d = ImageDraw.Draw(im)
    margin = 150
    d.line((margin, 850, 1450, 850), fill="#" + INK, width=5)
    d.line((margin, 850, margin, 120), fill="#" + INK, width=5)
    arrow(d, (1450, 850), (1510, 850), INK, 5, 18)
    arrow(d, (margin, 120), (margin, 65), INK, 5, 18)
    label(d, (830, 945), "更接近用户意图产生的输入时刻", 30, True, INK, "ma")
    label(d, (45, 450), "任务闭环执行能力", 30, True, INK, "mm")
    points = [
        (380, 735, "搜狗输入法", TEAL),
        (470, 670, "讯飞输入法", TEAL),
        (950, 640, "Wispr Flow", GOLD),
        (430, 280, "Codex", NAVY),
        (560, 220, "MiMo Desktop", PURPLE),
        (1280, 170, "Cuttle", ORANGE),
    ]
    for x, y, name, color in points:
        d.ellipse((x - 14, y - 14, x + 14, y + 14), fill="#" + color)
        rounded(d, (x + 20, y - 38, x + 245, y + 38), 18, LIGHT, color, 3)
        center_text(d, (x + 20, y - 38, x + 245, y + 38), name, 24, name == "Cuttle", color)
    d.rounded_rectangle((1020, 70, 1530, 330), radius=28, fill="#FFF5F0", outline="#" + ORANGE, width=4)
    label(d, (1060, 105), "项目占位", 30, True, ORANGE)
    label(d, (1060, 165), "任意输入焦点", 25, False, INK)
    label(d, (1060, 210), "连续意图升级", 25, False, INK)
    label(d, (1060, 255), "验证后返回原处", 25, False, INK)
    figs["competition"] = save_fig("fig03-competition-map.png", im)

    im = Image.new("RGB", (1800, 950), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 50), "一个统一入口和四个核心系统", 42, True, NAVY, "ma")
    rounded(d, (520, 130, 1280, 250), 30, NAVY, NAVY, 1)
    center_text(d, (520, 130, 1280, 250), "Cuttle Input  键盘 拼音 语音 选区 快捷指令", 33, True, WHITE)
    systems = [
        (180, 360, 540, 680, "Context Engine", "读取最小必要情境\n生成可纠正的情境胶囊", TEAL),
        (590, 360, 950, 680, "Intent Router", "判断表达 工具 单 Agent\nSwarm 或跨设备任务", GOLD),
        (1000, 360, 1360, 680, "Agent Fabric", "拆解任务 分配能力\n执行 审批与协作", ORANGE),
        (1410, 360, 1770, 680, "Verification Return", "绑定证据 验证成果\n返回原始工作位置", PURPLE),
    ]
    for x1, y1, x2, y2, title, body, color in systems:
        rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 5)
        center_text(d, (x1 + 15, y1 + 25, x2 - 15, y1 + 135), title, 28, True, color)
        center_text(d, (x1 + 22, y1 + 145, x2 - 22, y2 - 25), body, 25, False, INK, 12)
    for x in [360, 770, 1180, 1590]:
        arrow(d, (900, 260), (x, 345), GRAY, 5, 14)
    rounded(d, (270, 790, 1530, 890), 26, LIGHT_BLUE, TEAL, 3)
    center_text(d, (270, 790, 1530, 890), "共同底座  最小权限  本地优先  审批审计  版本化成果  模型可替换", 28, True, TEAL)
    figs["architecture"] = save_fig("fig04-architecture.png", im)

    im = Image.new("RGB", (1700, 930), "white")
    d = ImageDraw.Draw(im)
    label(d, (850, 50), "情境胶囊保存完成当前意图所需的最小状态", 40, True, NAVY, "ma")
    rounded(d, (520, 180, 1180, 770), 55, LIGHT_BLUE, TEAL, 6)
    center_text(d, (520, 190, 1180, 300), "Intent Capsule", 40, True, TEAL)
    fields = [
        ("应用与输入焦点", "当前应用  窗口  光标位置"),
        ("任务对象", "文档  选区  文件  会话"),
        ("关系与意图", "用户要做什么  涉及谁"),
        ("证据来源", "结构化接口  可访问性树  视觉"),
        ("有效期", "应用切换  文件关闭  用户纠正后失效"),
        ("置信度与权限", "可见  可改  可分享  可执行范围"),
    ]
    y = 320
    for title, body in fields:
        label(d, (585, y), title, 25, True, NAVY)
        label(d, (870, y), body, 23, False, INK)
        y += 68
    side = [
        (80, 240, 430, 430, "可纠正", "用户一键查看来源\n修正错误理解", ORANGE),
        (80, 500, 430, 690, "可过期", "状态变化立即失效\n避免旧情境误用", GOLD),
        (1270, 240, 1620, 430, "可溯源", "每条结论绑定来源\n时间与作用域", PURPLE),
        (1270, 500, 1620, 690, "最小必要", "只读取完成任务\n所需的信息", NAVY),
    ]
    for x1, y1, x2, y2, title, body, color in side:
        rounded(d, (x1, y1, x2, y2), 26, LIGHT, color, 4)
        center_text(d, (x1 + 10, y1 + 12, x2 - 10, y1 + 85), title, 29, True, color)
        center_text(d, (x1 + 15, y1 + 85, x2 - 15, y2 - 12), body, 23, False, INK, 8)
    figs["capsule"] = save_fig("fig05-intent-capsule.png", im)

    im = Image.new("RGB", (1800, 820), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 48), "意图升级由任务复杂度和风险共同决定", 42, True, NAVY, "ma")
    levels = [
        (90, 540, 440, 690, "L0 直接表达", "补全 改写 翻译", TEAL),
        (470, 440, 820, 690, "L1 工具调用", "检索 计算 格式转换", GOLD),
        (850, 320, 1200, 690, "L2 单 Agent", "多步骤 可验证任务", ORANGE),
        (1230, 180, 1680, 690, "L3 Swarm 或设备网格", "并行分工 跨设备 长任务", PURPLE),
    ]
    for x1, y1, x2, y2, title, body, color in levels:
        rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 5)
        center_text(d, (x1 + 10, y1 + 15, x2 - 10, y1 + 105), title, 29, True, color)
        center_text(d, (x1 + 20, y1 + 105, x2 - 20, y2 - 20), body, 24, False, INK, 8)
    label(d, (90, 760), "风险上升  读取范围扩大  执行影响增强  人工审批增多", 28, True, ORANGE)
    arrow(d, (820, 748), (1680, 748), ORANGE, 6, 18)
    figs["escalation"] = save_fig("fig06-intent-escalation.png", im)

    im = Image.new("RGB", (1800, 850), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 45), "三阶段验证把创意变成可复现证据", 42, True, NAVY, "ma")
    phases = [
        (100, 180, 560, 680, "发现问题", "30 人深度访谈\n14 天任务日记\n跨应用流程观察", NAVY, "输出\n高频任务清单\n敏感边界清单"),
        (670, 180, 1130, 680, "验证机制", "60 人可用性测试\n200 个真实任务\n对照与消融实验", TEAL, "输出\n交互原型\n核心指标基线"),
        (1240, 180, 1700, 680, "验证价值", "120 人四周试点\n1000 次任务记录\n校园团队联合评估", ORANGE, "输出\n留存与付费意愿\n机构采购证据"),
    ]
    for i, (x1, y1, x2, y2, title, body, color, out) in enumerate(phases):
        rounded(d, (x1, y1, x2, y2), 30, LIGHT, color, 5)
        center_text(d, (x1 + 15, y1 + 20, x2 - 15, y1 + 120), title, 34, True, color)
        center_text(d, (x1 + 25, y1 + 135, x2 - 25, y1 + 350), body, 27, False, INK, 14)
        d.line((x1 + 50, y1 + 380, x2 - 50, y1 + 380), fill="#" + GRID, width=3)
        center_text(d, (x1 + 25, y1 + 395, x2 - 25, y2 - 20), out, 25, True, color, 12)
        if i < 2:
            arrow(d, (x2 + 12, 430), (x2 + 95, 430), GRAY, 6, 17)
    figs["research"] = save_fig("fig07-validation-plan.png", im)

    im = Image.new("RGB", (1800, 850), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 40), "市场规模采用自上而下校验和自下而上获客两种口径", 39, True, NAVY, "ma")
    funnel = [
        (180, 140, 1620, 270, "潜在环境  6.02 亿生成式人工智能用户", NAVY),
        (310, 300, 1490, 430, "首个可服务市场  4872.57 万高等教育在学规模", TEAL),
        (450, 460, 1350, 590, "重点细分人群  约 390 万至 585 万高频 PC 学习与知识工作者", GOLD),
        (650, 620, 1150, 750, "三年目标  25 万注册用户  3.5 万付费用户", ORANGE),
    ]
    for x1, y1, x2, y2, text, color in funnel:
        rounded(d, (x1, y1, x2, y2), 28, LIGHT, color, 4)
        center_text(d, (x1 + 20, y1 + 10, x2 - 20, y2 - 10), text, 28, True, color)
    figs["market"] = save_fig("fig08-market-funnel.png", im)

    im = Image.new("RGB", (1800, 760), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 45), "以校园高频任务切入 再扩展到专业知识工作", 42, True, NAVY, "ma")
    stages = [
        (100, 180, 560, 620, "第一阶段", "高校学生和科研团队", "课程报告\n文献整理\n代码与材料任务", NAVY),
        (670, 180, 1130, 620, "第二阶段", "开发者和内容团队", "技能市场\n团队许可证\n可复用流程", TEAL),
        (1240, 180, 1700, 620, "第三阶段", "机构与私有部署", "内网模型\n审计治理\n行业解决方案", ORANGE),
    ]
    for i, (x1, y1, x2, y2, phase, who, tasks, color) in enumerate(stages):
        rounded(d, (x1, y1, x2, y2), 32, LIGHT, color, 5)
        center_text(d, (x1 + 10, y1 + 20, x2 - 10, y1 + 100), phase, 28, True, color)
        center_text(d, (x1 + 20, y1 + 110, x2 - 20, y1 + 230), who, 30, True, INK)
        center_text(d, (x1 + 25, y1 + 250, x2 - 25, y2 - 25), tasks, 25, False, INK, 12)
        if i < 2:
            arrow(d, (x2 + 15, 400), (x2 + 95, 400), GRAY, 6, 17)
    figs["gtm"] = save_fig("fig09-go-to-market.png", im)

    im = Image.new("RGB", (1800, 920), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 45), "三年财务规划情景", 42, True, NAVY, "ma")
    years = ["第一年", "第二年", "第三年"]
    rev = [47.4, 403, 1740]
    cost = [120, 330, 1080]
    maxv = 1800
    base_y = 790
    chart_top = 150
    for i, year in enumerate(years):
        cx = 400 + i * 500
        rh = (rev[i] / maxv) * (base_y - chart_top)
        ch = (cost[i] / maxv) * (base_y - chart_top)
        d.rectangle((cx - 110, base_y - rh, cx - 15, base_y), fill="#" + TEAL)
        d.rectangle((cx + 25, base_y - ch, cx + 120, base_y), fill="#" + ORANGE)
        label(d, (cx - 62, base_y - rh - 18), f"{rev[i]:g}", 24, True, TEAL, "ms")
        label(d, (cx + 72, base_y - ch - 18), f"{cost[i]:g}", 24, True, ORANGE, "ms")
        label(d, (cx, 840), year, 27, True, INK, "ma")
    d.line((180, base_y, 1620, base_y), fill="#" + INK, width=4)
    rounded(d, (1230, 95, 1650, 205), 18, LIGHT, GRID, 2)
    d.rectangle((1270, 125, 1315, 160), fill="#" + TEAL)
    label(d, (1335, 142), "营业收入", 23, False, INK, "lm")
    d.rectangle((1480, 125, 1525, 160), fill="#" + ORANGE)
    label(d, (1545, 142), "经营成本", 23, False, INK, "lm")
    label(d, (210, 860), "单位 万元", 22, False, GRAY)
    figs["finance"] = save_fig("fig10-financial-plan.png", im)

    im = Image.new("RGB", (1800, 740), "white")
    d = ImageDraw.Draw(im)
    label(d, (900, 45), "二十四个月产品与市场路线", 42, True, NAVY, "ma")
    y = 400
    d.line((140, y, 1660, y), fill="#" + INK, width=7)
    phases = [
        (220, "0 至 3 个月", "问题验证", "访谈 日记\n交互原型", NAVY),
        (520, "4 至 6 个月", "闭环原型", "输入 胶囊\n单 Agent 返回", TEAL),
        (850, "7 至 12 个月", "校园试点", "对照实验\n安全评测", GOLD),
        (1190, "13 至 18 个月", "产品化", "团队许可证\n技能市场", ORANGE),
        (1510, "19 至 24 个月", "规模验证", "机构合作\n私有部署", PURPLE),
    ]
    for i, (x, span, title, body, color) in enumerate(phases):
        d.ellipse((x - 16, y - 16, x + 16, y + 16), fill="#" + color)
        up = i % 2 == 0
        box = (x - 125, 120 if up else 470, x + 125, 340 if up else 690)
        d.line((x, y, x, box[3] if up else box[1]), fill="#" + color, width=4)
        rounded(d, box, 24, LIGHT, color, 4)
        center_text(d, (box[0] + 5, box[1] + 10, box[2] - 5, box[1] + 70), span, 22, True, color)
        center_text(d, (box[0] + 5, box[1] + 75, box[2] - 5, box[1] + 135), title, 27, True, INK)
        center_text(d, (box[0] + 10, box[1] + 140, box[2] - 10, box[3] - 10), body, 22, False, INK, 6)
    figs["roadmap"] = save_fig("fig11-roadmap.png", im)

    return figs


def set_cell_shading(cell, fill: str):
    tc_pr = cell._tc.get_or_add_tcPr()
    shd = tc_pr.find(qn("w:shd"))
    if shd is None:
        shd = OxmlElement("w:shd")
        tc_pr.append(shd)
    shd.set(qn("w:fill"), fill)


def set_cell_border(cell, color=GRID, size="6"):
    tc_pr = cell._tc.get_or_add_tcPr()
    borders = tc_pr.first_child_found_in("w:tcBorders")
    if borders is None:
        borders = OxmlElement("w:tcBorders")
        tc_pr.append(borders)
    for edge in ("top", "left", "bottom", "right", "insideH", "insideV"):
        tag = "w:" + edge
        el = borders.find(qn(tag))
        if el is None:
            el = OxmlElement(tag)
            borders.append(el)
        el.set(qn("w:val"), "single")
        el.set(qn("w:sz"), size)
        el.set(qn("w:color"), color)


def set_cell_margins(cell, top=100, start=120, bottom=100, end=120):
    tc = cell._tc
    tc_pr = tc.get_or_add_tcPr()
    tc_mar = tc_pr.first_child_found_in("w:tcMar")
    if tc_mar is None:
        tc_mar = OxmlElement("w:tcMar")
        tc_pr.append(tc_mar)
    for m, value in (("top", top), ("start", start), ("bottom", bottom), ("end", end)):
        node = tc_mar.find(qn(f"w:{m}"))
        if node is None:
            node = OxmlElement(f"w:{m}")
            tc_mar.append(node)
        node.set(qn("w:w"), str(value))
        node.set(qn("w:type"), "dxa")


def set_repeat_table_header(row):
    tr_pr = row._tr.get_or_add_trPr()
    tbl_header = OxmlElement("w:tblHeader")
    tbl_header.set(qn("w:val"), "true")
    tr_pr.append(tbl_header)


def set_width(cell, cm_value):
    tc_pr = cell._tc.get_or_add_tcPr()
    tc_w = tc_pr.find(qn("w:tcW"))
    if tc_w is None:
        tc_w = OxmlElement("w:tcW")
        tc_pr.append(tc_w)
    tc_w.set(qn("w:w"), str(int(Cm(cm_value))))
    tc_w.set(qn("w:type"), "dxa")


def set_run_font(run, name="Microsoft YaHei", size=10.5, bold=None, color=BLACK):
    run.font.name = name
    run.font.size = Pt(size)
    if bold is not None:
        run.bold = bold
    run.font.color.rgb = RGBColor.from_string(color)
    rpr = run._element.get_or_add_rPr()
    rfonts = rpr.rFonts
    if rfonts is None:
        rfonts = OxmlElement("w:rFonts")
        rpr.append(rfonts)
    for key in ("ascii", "hAnsi", "eastAsia", "cs"):
        rfonts.set(qn(f"w:{key}"), name)


def set_picture_alt(run, title: str, description: str):
    for doc_pr in run._r.xpath(".//wp:docPr"):
        doc_pr.set("title", title)
        doc_pr.set("descr", description)


def add_hyperlink(paragraph, text, url):
    part = paragraph.part
    r_id = part.relate_to(url, "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink", is_external=True)
    hyperlink = OxmlElement("w:hyperlink")
    hyperlink.set(qn("r:id"), r_id)
    new_run = OxmlElement("w:r")
    rpr = OxmlElement("w:rPr")
    color = OxmlElement("w:color")
    color.set(qn("w:val"), NAVY)
    rpr.append(color)
    underline = OxmlElement("w:u")
    underline.set(qn("w:val"), "single")
    rpr.append(underline)
    rfonts = OxmlElement("w:rFonts")
    for key in ("ascii", "hAnsi", "eastAsia", "cs"):
        rfonts.set(qn(f"w:{key}"), "Microsoft YaHei")
    rpr.append(rfonts)
    size = OxmlElement("w:sz")
    size.set(qn("w:val"), "18")
    rpr.append(size)
    new_run.append(rpr)
    text_el = OxmlElement("w:t")
    text_el.text = text
    new_run.append(text_el)
    hyperlink.append(new_run)
    paragraph._p.append(hyperlink)


def add_page_field(paragraph):
    run = paragraph.add_run()
    fld_char1 = OxmlElement("w:fldChar")
    fld_char1.set(qn("w:fldCharType"), "begin")
    instr = OxmlElement("w:instrText")
    instr.set(qn("xml:space"), "preserve")
    instr.text = " PAGE "
    fld_char2 = OxmlElement("w:fldChar")
    fld_char2.set(qn("w:fldCharType"), "end")
    run._r.extend([fld_char1, instr, fld_char2])
    set_run_font(run, size=9, color=GRAY)


def add_toc_field(paragraph):
    run = paragraph.add_run()
    begin = OxmlElement("w:fldChar")
    begin.set(qn("w:fldCharType"), "begin")
    instr = OxmlElement("w:instrText")
    instr.set(qn("xml:space"), "preserve")
    instr.text = ' TOC \\o "1-2" \\h \\z \\u '
    sep = OxmlElement("w:fldChar")
    sep.set(qn("w:fldCharType"), "separate")
    txt_run = OxmlElement("w:r")
    txt = OxmlElement("w:t")
    txt.text = "目录将在 Word 打开时自动更新"
    txt_run.append(txt)
    end = OxmlElement("w:fldChar")
    end.set(qn("w:fldCharType"), "end")
    run._r.extend([begin, instr, sep])
    paragraph._p.append(txt_run)
    paragraph._p.append(end)


def style_document(doc: Document):
    section = doc.sections[0]
    section.page_width = Cm(21.0)
    section.page_height = Cm(29.7)
    section.top_margin = Cm(1.8)
    section.bottom_margin = Cm(1.7)
    section.left_margin = Cm(2.0)
    section.right_margin = Cm(2.0)
    section.different_first_page_header_footer = True

    normal = doc.styles["Normal"]
    normal.font.name = "Microsoft YaHei"
    normal.font.size = Pt(10.5)
    normal.font.color.rgb = RGBColor.from_string(INK)
    normal._element.rPr.rFonts.set(qn("w:eastAsia"), "Microsoft YaHei")
    pf = normal.paragraph_format
    pf.line_spacing = 1.42
    pf.space_after = Pt(6)
    pf.first_line_indent = Cm(0.74)

    title = doc.styles["Title"]
    title.font.name = "Microsoft YaHei"
    title.font.size = Pt(25)
    title.font.bold = True
    title.font.color.rgb = RGBColor.from_string(BLACK)
    title._element.rPr.rFonts.set(qn("w:eastAsia"), "Microsoft YaHei")
    title.paragraph_format.space_after = Pt(10)
    # Remove the blue bottom rule inherited from Word's built-in Title style.
    title_ppr = title._element.get_or_add_pPr()
    title_border = title_ppr.find(qn("w:pBdr"))
    if title_border is not None:
        title_ppr.remove(title_border)

    for style_name, size, before, after in [
        ("Heading 1", 16, 18, 10),
        ("Heading 2", 13, 14, 7),
        ("Heading 3", 11.5, 10, 5),
    ]:
        st = doc.styles[style_name]
        st.font.name = "Microsoft YaHei"
        st.font.size = Pt(size)
        st.font.bold = True
        st.font.color.rgb = RGBColor.from_string(BLACK)
        st._element.rPr.rFonts.set(qn("w:eastAsia"), "Microsoft YaHei")
        st.paragraph_format.space_before = Pt(before)
        st.paragraph_format.space_after = Pt(after)
        st.paragraph_format.keep_with_next = True
        st.paragraph_format.keep_together = True

    if "List Bullet" in doc.styles:
        st = doc.styles["List Bullet"]
        st.font.name = "Microsoft YaHei"
        st.font.size = Pt(10.5)
        st._element.rPr.rFonts.set(qn("w:eastAsia"), "Microsoft YaHei")
        st.paragraph_format.space_after = Pt(4)
        st.paragraph_format.left_indent = Cm(0.75)
        st.paragraph_format.first_line_indent = Cm(-0.35)


def configure_headers_footers(doc: Document):
    sec = doc.sections[0]
    header = sec.header
    p = header.paragraphs[0]
    p.alignment = WD_ALIGN_PARAGRAPH.RIGHT
    run = p.add_run("Cuttle 情境原生智能输入系统")
    set_run_font(run, size=8.5, color=GRAY)
    footer = sec.footer
    p = footer.paragraphs[0]
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    add_page_field(p)


def add_cover(doc: Document):
    p = doc.add_paragraph()
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    p.paragraph_format.space_after = Pt(38)
    r = p.add_run("中国国际大学生创新大赛 2026")
    set_run_font(r, size=15, bold=True, color=NAVY)

    p = doc.add_paragraph()
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    r = p.add_run()
    r.add_picture(str(LOGO), width=Cm(4.2))
    set_picture_alt(r, "Cuttle 品牌标识", "Cuttle 章鱼形品牌标识")

    p = doc.add_paragraph(style="Title")
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    p.paragraph_format.space_before = Pt(15)
    p.add_run("Cuttle 情境原生智能输入系统")

    p = doc.add_paragraph()
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    p.paragraph_format.space_after = Pt(18)
    r = p.add_run("面向 AI Agent 时代的新一代智能输入法")
    set_run_font(r, size=14, bold=True, color=TEAL)

    p = doc.add_paragraph()
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    p.paragraph_format.left_indent = Cm(2.0)
    p.paragraph_format.right_indent = Cm(2.0)
    p.paragraph_format.space_after = Pt(36)
    r = p.add_run("把用户在任意输入框中的自然语言连续升级为可验证的智能体行动 并将结果返回原工作位置")
    set_run_font(r, size=11.5, color=INK)

    rows = [
        ("参赛组别", "高教主赛道  本科生创意组"),
        ("项目类型", "人工智能+  新工科"),
        ("参赛学校", "西南大学"),
        ("项目负责人", "杨俊熙"),
        ("指导教师", "赵恒军"),
    ]
    table = doc.add_table(rows=len(rows), cols=2)
    table.alignment = WD_TABLE_ALIGNMENT.CENTER
    table.autofit = False
    for i, (k, v) in enumerate(rows):
        set_width(table.cell(i, 0), 3.7)
        set_width(table.cell(i, 1), 8.3)
        set_cell_shading(table.cell(i, 0), LIGHT_BLUE)
        for j, text in enumerate((k, v)):
            cell = table.cell(i, j)
            cell.vertical_alignment = WD_CELL_VERTICAL_ALIGNMENT.CENTER
            set_cell_margins(cell, 120, 150, 120, 150)
            set_cell_border(cell, WHITE, "0")
            p = cell.paragraphs[0]
            p.alignment = WD_ALIGN_PARAGRAPH.CENTER if j == 0 else WD_ALIGN_PARAGRAPH.LEFT
            p.paragraph_format.first_line_indent = Cm(0)
            p.paragraph_format.space_after = Pt(0)
            r = p.add_run(text)
            set_run_font(r, size=10.5, bold=(j == 0), color=NAVY if j == 0 else INK)
    doc.add_paragraph().add_run().add_break(WD_BREAK.PAGE)


def add_contents(doc: Document):
    p = doc.add_paragraph("目录", style="Heading 1")
    p.paragraph_format.page_break_before = False
    p.paragraph_format.space_after = Pt(14)

    entries = [
        ("项目摘要", "3"),
        ("第一章  真实问题与项目机会", "4"),
        ("第二章  用户需求与验证设计", "6"),
        ("第三章  产品方案与体验闭环", "8"),
        ("第四章  核心技术与创新机制", "10"),
        ("第五章  安全伦理与数据治理", "13"),
        ("第六章  市场空间与竞争定位", "14"),
        ("第七章  商业模式与市场进入", "17"),
        ("第八章  研发验证与实施路径", "19"),
        ("第九章  团队协作与人才培养", "21"),
        ("第十章  财务规划与资源配置", "22"),
        ("第十一章  风险管理", "24"),
        ("第十二章  社会价值与发展愿景", "25"),
        ("参考资料", "26"),
    ]
    table = doc.add_table(rows=len(entries), cols=2)
    table.alignment = WD_TABLE_ALIGNMENT.CENTER
    table.autofit = False
    for i, (title_text, page_text) in enumerate(entries):
        left, right = table.rows[i].cells
        set_width(left, 14.0)
        set_width(right, 1.4)
        for cell in (left, right):
            set_cell_margins(cell, 115, 80, 115, 80)
            set_cell_border(cell, WHITE, "0")
            if i % 2 == 0:
                set_cell_shading(cell, LIGHT_BLUE)
        lp = left.paragraphs[0]
        lp.paragraph_format.first_line_indent = Cm(0)
        lp.paragraph_format.space_after = Pt(0)
        lr = lp.add_run(title_text)
        set_run_font(lr, size=10.5, bold=i in (0, 1, 4, 7, 10, 13), color=NAVY if i in (0, 1, 4, 7, 10, 13) else INK)
        rp = right.paragraphs[0]
        rp.alignment = WD_ALIGN_PARAGRAPH.RIGHT
        rp.paragraph_format.first_line_indent = Cm(0)
        rp.paragraph_format.space_after = Pt(0)
        rr = rp.add_run(page_text)
        set_run_font(rr, size=10.5, bold=True, color=TEAL)

    note = doc.add_paragraph()
    note.paragraph_format.first_line_indent = Cm(0)
    note.paragraph_format.space_before = Pt(12)
    note.alignment = WD_ALIGN_PARAGRAPH.CENTER
    r = note.add_run("问题牵引  机制创新  证据验证  产业闭环  成长育人")
    set_run_font(r, size=9.5, bold=True, color=GRAY)
    doc.add_paragraph().add_run().add_break(WD_BREAK.PAGE)


def add_para(doc: Document, text: str, bold_lead: str | None = None, align=None, no_indent=False):
    p = doc.add_paragraph()
    if no_indent:
        p.paragraph_format.first_line_indent = Cm(0)
    if align is not None:
        p.alignment = align
    if bold_lead and text.startswith(bold_lead):
        r = p.add_run(bold_lead)
        set_run_font(r, bold=True, color=INK)
        r2 = p.add_run(text[len(bold_lead):])
        set_run_font(r2, color=INK)
    else:
        r = p.add_run(text)
        set_run_font(r, color=INK)
    return p


def add_bullets(doc: Document, items):
    for item in items:
        p = doc.add_paragraph(style="List Bullet")
        p.paragraph_format.first_line_indent = Cm(-0.35)
        r = p.add_run(item)
        set_run_font(r, color=INK)


def add_table(doc: Document, headers, rows, widths=None, font_size=9.2, caption=None, alignments=None):
    if caption:
        p = doc.add_paragraph()
        p.alignment = WD_ALIGN_PARAGRAPH.CENTER
        p.paragraph_format.first_line_indent = Cm(0)
        p.paragraph_format.space_before = Pt(6)
        p.paragraph_format.space_after = Pt(4)
        p.paragraph_format.keep_with_next = True
        r = p.add_run(caption)
        set_run_font(r, size=9.5, bold=True, color=INK)
    table = doc.add_table(rows=1, cols=len(headers))
    table.alignment = WD_TABLE_ALIGNMENT.CENTER
    table.autofit = False
    hdr = table.rows[0]
    set_repeat_table_header(hdr)
    hdr_pr = hdr._tr.get_or_add_trPr()
    hdr_pr.append(OxmlElement("w:cantSplit"))
    for j, h in enumerate(headers):
        cell = hdr.cells[j]
        if widths:
            set_width(cell, widths[j])
        set_cell_shading(cell, NAVY)
        set_cell_border(cell)
        set_cell_margins(cell, 110, 110, 110, 110)
        cell.vertical_alignment = WD_CELL_VERTICAL_ALIGNMENT.CENTER
        p = cell.paragraphs[0]
        p.alignment = WD_ALIGN_PARAGRAPH.CENTER
        p.paragraph_format.first_line_indent = Cm(0)
        p.paragraph_format.space_after = Pt(0)
        r = p.add_run(str(h))
        set_run_font(r, size=font_size, bold=True, color=WHITE)
    for i, row in enumerate(rows):
        added_row = table.add_row()
        row_pr = added_row._tr.get_or_add_trPr()
        row_pr.append(OxmlElement("w:cantSplit"))
        cells = added_row.cells
        for j, value in enumerate(row):
            cell = cells[j]
            if widths:
                set_width(cell, widths[j])
            set_cell_shading(cell, WHITE if i % 2 == 0 else LIGHT_BLUE)
            set_cell_border(cell)
            set_cell_margins(cell, 105, 115, 105, 115)
            cell.vertical_alignment = WD_CELL_VERTICAL_ALIGNMENT.CENTER
            p = cell.paragraphs[0]
            p.paragraph_format.first_line_indent = Cm(0)
            p.paragraph_format.space_after = Pt(0)
            if alignments:
                p.alignment = alignments[j]
            else:
                p.alignment = WD_ALIGN_PARAGRAPH.CENTER if j == 0 else WD_ALIGN_PARAGRAPH.LEFT
            r = p.add_run(str(value))
            set_run_font(r, size=font_size, color=INK)
    doc.add_paragraph().paragraph_format.space_after = Pt(1)
    return table


def add_figure(doc: Document, path: Path, caption: str, width_cm=15.8):
    p = doc.add_paragraph()
    p.alignment = WD_ALIGN_PARAGRAPH.CENTER
    p.paragraph_format.first_line_indent = Cm(0)
    p.paragraph_format.space_before = Pt(7)
    p.paragraph_format.space_after = Pt(3)
    p.paragraph_format.keep_with_next = True
    r = p.add_run()
    r.add_picture(str(path), width=Cm(width_cm))
    set_picture_alt(r, caption, caption)
    c = doc.add_paragraph()
    c.alignment = WD_ALIGN_PARAGRAPH.CENTER
    c.paragraph_format.first_line_indent = Cm(0)
    c.paragraph_format.space_after = Pt(8)
    c.paragraph_format.keep_with_next = True
    r = c.add_run(caption)
    set_run_font(r, size=9.2, bold=True, color=INK)


def add_chapter(doc: Document, text: str, page_break=True):
    p = doc.add_paragraph(text, style="Heading 1")
    p.paragraph_format.page_break_before = page_break
    return p


def build_doc(figs: dict[str, Path]):
    doc = Document()
    style_document(doc)
    configure_headers_footers(doc)
    add_cover(doc)
    add_contents(doc)

    p = doc.add_paragraph("项目摘要", style="Heading 1")
    p.paragraph_format.page_break_before = False
    add_para(
        doc,
        "Cuttle 拟构建一套情境原生智能输入系统。用户在 Word、浏览器、聊天软件、开发工具等任意输入焦点表达需求时，系统读取完成当前任务所需的最小情境，形成带来源、有效期、置信度和权限边界的意图胶囊，再将请求路由为直接表达、工具调用、单智能体任务或多智能体协作。执行过程保留计划、审批、证据、差异和验收结果，最终把可验证成果返回需求产生的应用。",
    )
    add_para(
        doc,
        "项目抓住了 AI Agent 能力快速增强之后仍未解决的入口问题。现有 AI 助手多以独立对话框或工作台承接任务，用户仍需离开当前工作、重新描述背景并搬运结果；现有 AI 输入法已经具备改写、续写、翻译和场景化表达，但任务通常停留在文本生成。Cuttle 选择二者之间尚未贯通的位置，把输入法从字符工具升级为个人电脑的意图入口。",
    )
    add_para(
        doc,
        "核心创新包括可纠正和可过期的情境胶囊、按复杂度与风险连续升级的意图协议、基于收益成本风险判断是否组队的自适应多智能体机制，以及基于版本化成果和任务契约的接力与返回机制。首个市场聚焦高校学生、教师与科研团队的报告、文献、代码和项目材料任务，通过校园试点形成可复现案例，再向开发者团队和机构私有部署扩展。",
    )
    add_para(
        doc,
        "项目按照创意组从问题调研、机制验证、产品试制到市场验证的逻辑推进。关键验收指标覆盖情境识别、任务成功、人工干预、敏感操作拦截、失败恢复、时间节省、连续留存和付费意愿。所有市场规模、用户增长与财务数据均作为规划情景，并在每一阶段由真实任务日志、用户研究和合同证据校准。",
    )

    add_table(
        doc,
        ["项目要素", "核心内容", "评委可判断的证据"],
        [
            ["真实问题", "AI 入口 情境 行动和结果彼此割裂", "访谈 任务日记 跨应用流程观察"],
            ["项目方案", "输入时刻理解情境并升级为受控行动", "交互原型 演示闭环 任务日志"],
            ["技术创新", "情境胶囊 意图升级 自适应协同 成果接力", "对照实验 消融实验 安全测试"],
            ["产业路径", "校园任务切入 团队许可 私有部署", "试点 留存 付费验证 合作材料"],
            ["人才培养", "学生主导调研 研发 测试 运营和复盘", "过程记录 版本贡献 成果归属"],
        ],
        widths=[3.0, 6.2, 6.6],
        caption="表 1 项目评审证据链",
    )

    add_chapter(doc, "第一章 真实问题与项目机会")
    doc.add_paragraph("1.1 一个发生在输入时刻的真实矛盾", style="Heading 2")
    add_para(
        doc,
        "一名学生在 Word 中写课程报告时，需要核对资料、提炼证据、生成图表并修改表达。今天的典型做法是打开浏览器或 AI 对话框，复制题目和正文，重新解释格式要求，等待生成，再把结果粘回 Word。任务越复杂，背景搬运越多；应用一切换，AI 对当前对象、关系和进度的理解就被打断。",
    )
    add_para(
        doc,
        "Agent 产品正在提升规划、工具调用和多智能体协作能力，输入法产品也在进入大模型时代，但二者大多从不同入口服务用户。工作台擅长执行已经表达清楚的任务，输入法接近意图产生的现场，却主要负责文字生成。用户因此承担了上下文整理、任务升级、风险判断和结果回填四项额外工作。",
    )
    add_figure(doc, figs["opportunity"], "图 1 现有 AI 工作流的断点与 Cuttle 的项目机会")

    doc.add_paragraph("1.2 项目洞察", style="Heading 2")
    add_para(
        doc,
        "Cuttle 将输入时刻定义为 AI 协作的起点。用户无需先决定应该打开哪个模型、哪个 Agent 或哪个工作台，只需在当前输入焦点表达意图。系统先识别所处应用、当前对象和任务边界，再判断这次请求应该停留在文本层，还是升级为工具、Agent、Swarm 或跨设备任务。",
    )
    add_para(
        doc,
        "该思路把产品竞争从模型问答能力转向意图进入和任务交付能力。项目不与基础模型厂商争夺模型本身，而是在模型和桌面应用之间构建一层可控、可验证、可替换的意图基础设施。模型能力越强，Cuttle 可调度的执行能力越丰富；用户仍保有对情境、权限和最终结果的控制。",
    )
    add_figure(doc, figs["closed_loop"], "图 2 Cuttle 从输入到成果返回原处的完整闭环")

    doc.add_paragraph("1.3 项目定义与价值主张", style="Heading 2")
    add_table(
        doc,
        ["对象", "当前成本", "Cuttle 的价值", "验证指标"],
        [
            ["高校学生", "资料搬运 重复提示 格式返工", "在当前文档中完成检索 整理 写作和交付", "完成时间 返工次数 四周留存"],
            ["教师科研人员", "文献 证据与版本分散", "将来源 中间证据和成果绑定", "引用准确率 审核时间 可追溯率"],
            ["开发者", "任务在 IDE 浏览器和终端间切换", "把请求升级为受控代码任务并返回差异", "任务成功率 回滚率 人工接管率"],
            ["高校实验室", "模型与工具分散 缺少权限治理", "提供可替换模型 统一审批和审计", "部署时间 越权拦截 维护成本"],
        ],
        widths=[2.8, 4.2, 5.5, 3.5],
        caption="表 2 目标用户价值与验证口径",
    )

    add_chapter(doc, "第二章 用户需求与验证设计")
    doc.add_paragraph("2.1 首批用户与高频任务", style="Heading 2")
    add_para(
        doc,
        "项目采用窄场景切入。首批用户选择高频使用个人电脑、任务边界清晰、结果可以验收的高校学生、教师科研人员和学生开发者。首批任务集中在课程资料到报告、文献到综述、代码问题到可验证修复、会议记录到正式材料四类闭环。每类任务都能记录输入、过程、权限、结果和人工修订，适合形成比赛所需的真实证据。",
    )
    add_table(
        doc,
        ["用户", "高频任务", "现有障碍", "首个验证场景"],
        [
            ["本科生", "课程报告 项目申报 文档整理", "反复复制背景 不清楚工具边界", "课程资料生成结构化报告"],
            ["研究生", "文献梳理 数据解释 论文材料", "来源分散 证据难回溯", "多来源研究任务与证据表"],
            ["教师", "教案 课程材料 项目管理", "重复格式工作 审核链条长", "教学材料规范化与审核"],
            ["学生开发者", "代码修复 测试 文档 发布", "工具切换多 失败恢复难", "代码差异 测试与回滚闭环"],
        ],
        widths=[2.7, 4.4, 4.5, 4.4],
        caption="表 3 首批用户任务地图",
    )

    doc.add_paragraph("2.2 用户研究方法", style="Heading 2")
    add_para(
        doc,
        "用户研究分为发现、机制验证和价值验证三个阶段。团队先通过半结构化访谈、任务日记与情境观察识别最频繁的跨应用任务和最敏感的数据边界；随后使用可交互原型和 Wizard of Oz 方法验证用户是否理解意图升级、审批和结果返回；最后用真实版本开展四周连续试点，观察任务成功、留存、信任与付费意愿。",
    )
    add_figure(doc, figs["research"], "图 3 从问题发现到价值验证的研究路径")

    doc.add_paragraph("2.3 核心假设与证伪条件", style="Heading 2")
    add_table(
        doc,
        ["假设", "验证方法", "通过门槛", "证伪后的决策"],
        [
            ["输入时刻是更自然的 AI 入口", "原型 A B 测试", "70% 以上用户优先选择原地发起", "缩小入口范围 保留快捷指令"],
            ["情境胶囊减少重复说明", "对照任务", "上下文重复输入减少 30%", "减少自动感知 增强手动选区"],
            ["连续升级比独立工作台更高效", "同任务计时", "中位完成时间降低 25%", "只保留高价值任务升级"],
            ["可见审批能够建立信任", "敏感任务可用性测试", "误批准率低于 3%", "增加解释与二次确认"],
            ["校园团队愿意付费", "试点与报价测试", "至少 3 个付费意向或采购流程", "转向个人订阅或开发者生态"],
        ],
        widths=[4.2, 3.2, 4.2, 4.4],
        caption="表 4 核心商业与产品假设",
        font_size=8.9,
    )

    add_chapter(doc, "第三章 产品方案与体验闭环")
    doc.add_paragraph("3.1 产品总体形态", style="Heading 2")
    add_para(
        doc,
        "Cuttle 由一个统一入口和四个核心系统组成。统一入口承接键盘、拼音、自然语言、语音、选区和快捷指令；Context Engine 生成最小必要情境；Intent Router 决定任务层级；Agent Fabric 负责受控执行与协作；Verification and Return 绑定证据、验收成果并把结果返回原始应用。",
    )
    add_figure(doc, figs["architecture"], "图 4 Cuttle 一个入口与四个核心系统")

    doc.add_paragraph("3.2 典型体验场景", style="Heading 2")
    add_table(
        doc,
        ["场景", "用户输入", "系统处理", "最终交付"],
        [
            ["课程报告", "在 Word 输入 根据这些资料完成报告", "识别文档要求 调用资料检索与写作流程", "带引用与修改痕迹的 Word 内容"],
            ["科研综述", "在文献管理页面输入 比较这些方法", "读取选中文献 建立证据矩阵 并请求必要确认", "论点 证据 来源与局限表"],
            ["代码修复", "在 IDE 输入 修复这个报错并测试", "读取错误与相关文件 生成计划 执行测试", "可审阅差异 测试结果与回滚点"],
            ["沟通表达", "在聊天框输入 礼貌拒绝并给替代时间", "结合关系与语气生成候选 不直接发送", "可编辑的回复候选"],
        ],
        widths=[2.6, 4.1, 5.3, 4.0],
        caption="表 5 典型场景的输入到交付闭环",
        font_size=8.9,
    )

    doc.add_paragraph("3.3 分级交互", style="Heading 2")
    add_para(
        doc,
        "产品保持四级渐进交互。L0 只生成可编辑文本；L1 调用范围明确、结果可预览的工具；L2 进入单 Agent 工作流并显示计划、权限和验收条件；L3 仅在并行收益明确或任务跨设备时启用 Swarm 或设备网格。层级越高，系统展示越多的计划、成本、风险和审批信息。",
    )
    add_figure(doc, figs["escalation"], "图 5 从直接表达至多智能体协作的意图升级阶梯")

    doc.add_paragraph("3.4 返回原处机制", style="Heading 2")
    add_para(
        doc,
        "Return to Origin 是产品完成闭环的关键。Word 中产生的任务返回为可审阅段落、批注或文档副本；聊天中产生的任务返回为候选回复，发送权仍由用户掌握；IDE 中产生的任务返回为差异、测试结果和回滚点；手机发起的跨设备任务返回为带状态与证据的任务卡。结果格式由原始应用、任务契约和风险等级共同决定。",
    )

    add_chapter(doc, "第四章 核心技术与创新机制")
    doc.add_paragraph("4.1 创新一 面向输入时刻的情境胶囊", style="Heading 2")
    add_para(
        doc,
        "情境胶囊不是一段不可控的屏幕抓取，而是一组有结构、有作用域和有生命周期的最小状态。每条情境结论记录来源、观察时间、适用应用、对象版本、有效期、置信度和可能使其失效的事件。系统优先读取应用提供的结构化接口，其次使用可访问性树与界面语义，视觉证据只在必要时作为较弱信号。",
    )
    add_figure(doc, figs["capsule"], "图 6 意图胶囊的数据结构与可信属性")
    add_table(
        doc,
        ["字段", "作用", "失效事件", "用户控制"],
        [
            ["Application Focus", "标识当前应用 窗口和输入焦点", "切换应用 关闭窗口", "查看 关闭感知"],
            ["Task Object", "标识正在处理的文档 选区或文件", "对象修改 文件关闭", "手动选择或排除"],
            ["Relation Intent", "记录用户目标和对象关系", "用户纠正 任务结束", "直接修改"],
            ["Provenance TTL", "记录来源 时间和有效期", "超过期限 依赖变化", "查看来源"],
            ["Confidence Scope", "决定是否可自动使用", "置信度下降 权限变化", "确认或拒绝"],
        ],
        widths=[3.4, 5.0, 3.7, 3.9],
        caption="表 6 情境胶囊关键字段",
        font_size=8.8,
    )

    doc.add_paragraph("4.2 创新二 连续意图升级协议", style="Heading 2")
    add_para(
        doc,
        "系统根据任务复杂度、工具范围、数据敏感度、动作可逆性、预计收益与成本决定升级层级。普通补全不进入 Agent；需要读取文件但不修改外部状态的任务进入工具或单 Agent；涉及消息发送、文件覆盖、命令执行、支付或跨设备控制时，系统必须展示目标、影响范围与恢复方式，并由独立审批节点授权。",
    )
    add_para(
        doc,
        "升级协议解决了输入法与 Agent 之间的责任断层。输入入口只负责理解和路由，不直接持有高风险工具；Agent Core 负责规划与执行，也不能为自己扩大权限。审批系统独立判断，最终验证器依据任务契约检查结果。",
    )

    doc.add_paragraph("4.3 创新三 自适应多智能体协同", style="Heading 2")
    add_para(
        doc,
        "Cuttle 不默认组建多 Agent。路由器先估计并行收益、协调成本、结果合并难度和风险暴露，只有当分工能够显著提高成功率或缩短关键路径时才启用协作。Coordinator 明确目标、预算和验收条件，Worker 只获得完成子任务所需的最小上下文与工具，Verifier 独立检查结果，Human 节点处理信息不足、冲突和高风险决策。",
    )
    add_table(
        doc,
        ["任务特征", "优先形态", "原因", "验证指标"],
        [
            ["单步表达或低风险改写", "L0 直接表达", "无需执行系统", "接受率 修改量"],
            ["边界清晰的单工具任务", "L1 工具", "减少模型规划开销", "成功率 时延"],
            ["串行多步骤任务", "L2 单 Agent", "责任集中 易于恢复", "完成率 接管率"],
            ["可并行且成果可合并", "L3 Swarm", "缩短关键路径并交叉验证", "净收益 冲突率"],
            ["数据或能力分布在多设备", "L3 Device Mesh", "在数据所在处执行", "传输量 成功率"],
        ],
        widths=[4.2, 3.2, 4.6, 4.0],
        caption="表 7 意图路由与协作形态",
    )

    doc.add_paragraph("4.4 创新四 版本化成果接力与返回", style="Heading 2")
    add_para(
        doc,
        "多 Agent 之间不以自由对话作为主要交接方式，而以版本化 Artifact 和任务契约接力。每个 Worker 的输出包含来源、修改、证据、验证、未解决问题和恢复点；下一个 Worker 只在契约满足后接收成果。最终交付物保留从原始对象、中间证据到最终结果的链路，并由 Return to Origin 适配器返回原应用。",
    )

    doc.add_paragraph("4.5 创新五 基于能力与信任的设备网格", style="Heading 2")
    add_para(
        doc,
        "跨设备能力作为第二阶段扩展。设备不只是远程控制终端，而是带能力、数据位置、可信等级和在线状态的执行节点。调度器优先让任务在数据所在设备执行，只传递完成任务所需的摘要或成果；高风险动作需要目标设备本地确认。该设计减少敏感数据跨设备流动，也让手机、个人电脑和实验室工作站能够按能力分工。",
    )

    doc.add_paragraph("4.6 创新评价与对照实验", style="Heading 2")
    add_table(
        doc,
        ["实验", "对照组", "处理组", "主指标"],
        [
            ["入口效率", "独立 AI 对话框", "Cuttle 输入入口", "完成时间 上下文重复输入"],
            ["情境贡献", "无情境胶囊", "完整情境胶囊", "意图识别 F1 用户纠正率"],
            ["路由贡献", "所有任务单 Agent", "自适应路由", "成功率 成本 时延"],
            ["协同贡献", "固定 Swarm", "自适应 Swarm", "净收益 冲突率 合并失败率"],
            ["验证贡献", "无 Artifact 验证", "版本化成果与验证器", "正确率 恢复率 返工次数"],
            ["安全贡献", "Agent 自主授权", "独立审批与最小权限", "越权拦截率 误审批率"],
        ],
        widths=[3.0, 4.0, 4.5, 4.5],
        caption="表 8 核心创新的对照与消融设计",
        font_size=8.8,
    )

    add_chapter(doc, "第五章 安全伦理与数据治理")
    doc.add_paragraph("5.1 输入法场景的安全原则", style="Heading 2")
    add_para(
        doc,
        "输入法位于高频、全局且敏感的系统位置。Cuttle 将安全边界写入产品结构：默认关闭不必要的情境读取；敏感窗口和密码字段自动停用；数据处理遵循最小必要、本地优先和目的限定；高风险动作必须经过独立审批；所有外部动作保留审计与恢复信息。个人信息处理遵循告知同意、最小范围和影响最小的原则，并为用户提供查看、更正、删除和关闭能力。[9][10][11]",
    )
    add_table(
        doc,
        ["风险层级", "示例", "默认策略", "恢复与证据"],
        [
            ["低", "补全 翻译 语气调整", "本地候选 可忽略", "保留用户修改量"],
            ["中", "读取选区 生成文档副本", "显示范围 先预览", "原件不覆盖 可撤销"],
            ["高", "写文件 执行命令 访问网络", "独立审批 限定工具与路径", "差异 日志 回滚点"],
            ["极高", "发送消息 删除 支付 敏感数据外发", "默认拒绝 二次确认或禁止", "完整审计 人工处置"],
        ],
        widths=[2.2, 4.0, 5.1, 4.7],
        caption="表 9 风险分级与控制策略",
    )

    doc.add_paragraph("5.2 数据生命周期", style="Heading 2")
    add_bullets(
        doc,
        [
            "采集前说明对象、用途、范围和保留时间，用户可按应用、字段和任务类型关闭感知。",
            "处理时优先在本地完成；确需云模型时，只发送完成任务所需的最小片段，并在发送前展示范围。",
            "存储时区分临时情境、任务证据和长期技能，分别设置短时有效期、项目保留期和用户主动保存。",
            "任务结束后自动让短时情境失效；用户可以导出、删除、清空和撤回授权。",
            "训练与分析默认不使用个人内容；匿名化统计与改进计划需单独授权。",
        ],
    )

    doc.add_paragraph("5.3 安全验收门槛", style="Heading 2")
    add_table(
        doc,
        ["安全目标", "指标", "阶段门槛", "不达标处理"],
        [
            ["未经授权不执行", "高风险未授权动作数", "必须为 0", "冻结发布并复盘"],
            ["敏感场景不感知", "密码 支付 金融窗口误读取率", "必须为 0", "扩大黑名单与系统级屏蔽"],
            ["审批可理解", "用户能正确判断影响范围", "正确率不低于 95%", "重写说明与交互"],
            ["失败可恢复", "可逆任务恢复成功率", "不低于 90%", "限制能力并补偿测试"],
            ["证据可追溯", "外部动作带来源与审计记录", "覆盖率 100%", "阻止交付"],
        ],
        widths=[3.1, 4.3, 3.3, 5.3],
        caption="表 10 安全发布门槛",
    )

    add_chapter(doc, "第六章 市场空间与竞争定位", page_break=False)
    doc.add_paragraph("6.1 市场环境", style="Heading 2")
    add_para(
        doc,
        "中国互联网络信息中心第 57 次报告显示，截至 2025 年 12 月，我国生成式人工智能用户达到 6.02 亿人，普及率 42.8%；全国网民规模达到 11.25 亿人。[3] 工业和信息化部数据显示，2025 年我国软件业务收入 15.48 万亿元，信息技术服务收入同比增长 14.7%。[5] 这些数据说明 AI 使用已进入大规模扩散阶段，市场问题正在从是否使用 AI 转向如何把 AI 安全地接入日常工作。",
    )
    add_para(
        doc,
        "教育部 2025 年统计公报显示，全国各种形式高等教育在学总规模为 4872.57 万人。[4] 高校人群拥有集中、重复、可验收的文档、研究和代码任务，且校园试点便于持续观察，因此构成 Cuttle 的首个可服务市场。按其中 8% 至 12% 属于高频 PC 学习和知识工作用户的规划假设，重点细分人群约为 390 万至 585 万。该比例必须通过团队的用户研究逐步校准。",
    )
    add_figure(doc, figs["market"], "图 7 Cuttle 市场规模与三年目标漏斗")

    doc.add_paragraph("6.2 竞争格局", style="Heading 2")
    add_para(
        doc,
        "输入法产品正在增加续写、帮写、润色、翻译和窗口跟随能力。搜狗输入法 2026 年更新已提供 AI 光标助手和多项 AI 帮写服务。[6] Wispr Flow 的 Context Awareness 会读取光标附近文本与活动应用，按场景调整转写和风格。[7] 另一侧，Codex 和 MiMo Desktop 等工作台型产品已经提供长任务、多 Agent、技能和桌面执行能力。[8][12] Cuttle 的差异不在于单独拥有某一项能力，而在于把输入焦点、持续情境、分级执行、成果验证和返回原处组成一条系统链路。",
    )
    add_figure(doc, figs["competition"], "图 8 主要产品的入口距离与任务闭环能力定位")
    add_table(
        doc,
        ["类别", "代表产品", "强项", "未覆盖的关键环节", "Cuttle 应对"],
        [
            ["AI 输入法", "搜狗 讯飞 百度", "高频入口 补全改写 语音", "复杂任务执行与成果验证", "把输入连续升级为受控任务"],
            ["全局语音输入", "Wispr Flow", "跨应用输入 情境化表达", "工具与 Agent 交付闭环", "扩展至任务路由与返回"],
            ["通用 AI 助手", "ChatGPT Claude", "模型能力与知识服务", "需要用户显式搬运上下文", "用情境胶囊减少重复说明"],
            ["Agent 工作台", "Codex MiMo Desktop", "长任务 多 Agent 桌面执行", "远离多数用户的输入时刻", "作为所有 Agent 前的意图入口"],
            ["自动化平台", "RPA 工作流工具", "确定性流程和企业集成", "自然表达与开放任务适应性弱", "自然语言路由与可验证执行"],
        ],
        widths=[2.5, 3.0, 3.7, 4.2, 4.6],
        caption="表 11 竞争类别与差异化策略",
        font_size=8.5,
    )

    doc.add_paragraph("6.3 竞争壁垒", style="Heading 2")
    add_bullets(
        doc,
        [
            "交互壁垒：围绕输入时刻形成跨应用的一致意图协议和返回适配器。",
            "数据壁垒：在用户授权前提下积累匿名化的任务类型、路由效果和失败模式，不沉淀个人内容。",
            "工程壁垒：把权限、审计、版本化成果、验证和恢复作为统一运行时能力。",
            "生态壁垒：通过技能契约和模型可替换接口，让高校、开发者和行业伙伴复用流程。",
            "信任壁垒：以最小权限、来源可见和失败可恢复建立长期使用习惯。",
        ],
    )

    add_chapter(doc, "第七章 商业模式与市场进入", page_break=False)
    doc.add_paragraph("7.1 商业模式", style="Heading 2")
    add_para(
        doc,
        "Cuttle 采用个人订阅、团队许可证、私有部署与技能生态四类收入。免费版承担体验和传播，只提供低风险表达与少量任务；个人专业版面向高频学生、研究者和开发者；团队版提供成员管理、共享技能、预算、审计和支持；私有部署面向有数据合规要求的高校实验室和机构。技能市场在核心产品稳定后开放，避免过早分散研发资源。",
    )
    add_table(
        doc,
        ["产品", "目标用户", "核心权益", "规划定价", "收入方式"],
        [
            ["个人免费版", "学生与轻度用户", "基础表达 本地情境 少量任务", "免费", "获客与验证"],
            ["个人专业版", "研究者 开发者 高频用户", "高额度任务 多模型 技能和恢复", "29 元每月或 240 元每年", "订阅"],
            ["校园团队版", "实验室 课程组 学生团队", "成员 策略 审计 共享技能", "4 万元每年起", "许可证与服务"],
            ["私有部署版", "高校与机构", "内网模型 定制策略 运维支持", "20 万元每项目起", "实施与维护"],
            ["技能生态", "开发者与行业伙伴", "技能发布 交易 质量认证", "交易分成 15%", "平台服务"],
        ],
        widths=[2.8, 3.4, 4.6, 3.4, 2.8],
        caption="表 12 产品版本与收入结构",
        font_size=8.7,
    )

    doc.add_paragraph("7.2 市场进入路径", style="Heading 2")
    add_figure(doc, figs["gtm"], "图 9 Cuttle 从校园任务到机构服务的市场进入路径")
    add_para(
        doc,
        "第一阶段选择三类可公开、可验收的校园任务形成样板：课程报告、科研资料与学生开发项目。每个试点围绕任务完成率、时间节省、安全理解和连续使用记录证据。第二阶段以开源 SDK、技能模板和开发者案例扩大高价值用户。第三阶段依据真实采购需求提供团队许可证和私有部署，合同范围绑定可交付能力、数据边界与支持成本。",
    )

    doc.add_paragraph("7.3 增长与留存", style="Heading 2")
    add_bullets(
        doc,
        [
            "首用价值：用户在十分钟内完成一个低风险任务，理解系统能做什么和不能做什么。",
            "习惯形成：任务结束后把验证过的流程沉淀为个人技能，减少下一次重复说明。",
            "自然传播：只允许分享已脱敏的技能模板、方法和时间节省，不以用户内容换取奖励。",
            "团队扩散：个人用户将稳定流程带入课程组、实验室和学生团队，形成团队许可证机会。",
            "留存诊断：区分新鲜感流失、任务价值不足、信任不足和兼容性问题，分别调整产品。",
        ],
    )

    doc.add_paragraph("7.4 单位经济模型", style="Heading 2")
    add_table(
        doc,
        ["收入单元", "年均收入假设", "直接成本假设", "毛利率情景", "验证重点"],
        [
            ["个人专业版", "240 元每人", "60 元模型与支持", "75%", "付费留存 模型成本"],
            ["校园团队版", "4 万元每团队", "1.2 万元交付支持", "70%", "使用深度 续约"],
            ["私有部署", "20 万元每项目", "9 万元实施与维护", "55%", "交付周期 定制边界"],
            ["技能生态", "按交易额分成 15%", "审核和结算成本", "成熟后测算", "供给质量与纠纷率"],
        ],
        widths=[3.0, 3.4, 3.8, 2.7, 3.1],
        caption="表 13 单位经济模型的规划假设",
    )

    add_chapter(doc, "第八章 研发验证与实施路径", page_break=False)
    doc.add_paragraph("8.1 研发原则", style="Heading 2")
    add_para(
        doc,
        "研发以可验证闭环为最小单位。每个版本必须同时交付输入入口、情境来源、权限策略、执行结果、验证证据和恢复路径，不以功能列表作为完成标准。产品先覆盖少数高频应用和固定任务，再根据试点数据扩大兼容性。模型层保持可替换，避免项目价值依赖单一模型。",
    )

    doc.add_paragraph("8.2 二十四个月路线", style="Heading 2")
    add_figure(doc, figs["roadmap"], "图 10 Cuttle 二十四个月产品与市场路线")
    add_table(
        doc,
        ["阶段", "产品目标", "研究目标", "市场目标", "进入下一阶段门槛"],
        [
            ["0 至 3 个月", "交互原型与情境边界", "完成访谈与任务日记", "确定三个高频任务", "需求重复出现且可验收"],
            ["4 至 6 个月", "输入 胶囊 单 Agent 返回闭环", "完成可用性与安全测试", "30 名深度用户", "任务成功率达到 80%"],
            ["7 至 12 个月", "三至五个应用稳定演示", "完成对照和消融", "三个校园试点", "连续四周留存和安全门槛"],
            ["13 至 18 个月", "团队版与技能契约", "验证协同净收益", "首批付费团队", "续用意愿和可控交付成本"],
            ["19 至 24 个月", "私有部署与设备网格试验", "验证跨设备最小数据原则", "机构合作与复制", "合同 回款 支持成本可控"],
        ],
        widths=[2.4, 4.0, 3.7, 2.8, 3.1],
        caption="表 14 分阶段研发与市场决策门",
        font_size=8.4,
    )

    doc.add_paragraph("8.3 关键指标体系", style="Heading 2")
    add_table(
        doc,
        ["维度", "主指标", "十二个月目标", "数据来源"],
        [
            ["需求", "任务重复出现率 首用意愿", "高频任务覆盖 70% 研究样本", "访谈 日记 观察"],
            ["情境", "意图识别 F1 用户纠正率", "F1 不低于 0.90 纠正率低于 10%", "标注集与任务日志"],
            ["执行", "端到端任务成功率", "不低于 85%", "固定任务套件与真实任务"],
            ["效率", "完成时间 重复提示次数", "时间降低 25% 提示减少 30%", "对照实验"],
            ["安全", "未授权高风险动作 恢复率", "未授权为 0 恢复不低于 90%", "红队与故障注入"],
            ["市场", "四周留存 付费意愿 试点数", "留存 40% 三个试点", "产品分析 合作记录"],
        ],
        widths=[2.5, 4.2, 4.8, 4.5],
        caption="表 15 项目十二个月核心指标",
        font_size=8.7,
    )

    doc.add_paragraph("8.4 知识产权与成果计划", style="Heading 2")
    add_para(
        doc,
        "知识产权围绕真正形成差异的机制布局。近期完成客户端与运行时软件著作权登记，沉淀意图胶囊和连续升级协议的发明交底，建立商标与开源依赖清单；中期依据新颖性检索和实验结果决定专利申请，形成用户研究报告、评测数据集、技术白皮书和论文。所有成果明确学生贡献、数据授权和第三方许可，不以申请数量替代创新质量。",
    )

    add_chapter(doc, "第九章 团队协作与人才培养", page_break=False)
    doc.add_paragraph("9.1 团队结构", style="Heading 2")
    add_para(
        doc,
        "项目采用学生主导、教师指导、用户共同验证的协作方式。团队成员分别负责产品与架构、智能体工程与评测、用户研究与商业验证，每个里程碑都保留任务单、版本记录、测试报告和用户证据。指导教师负责方法、伦理、安全和阶段评审，不代替学生完成核心研发与答辩。",
    )
    add_table(
        doc,
        ["成员", "角色", "主要职责", "过程证据"],
        [
            ["杨俊熙", "项目负责人", "项目定位 产品设计 架构统筹 赛事整合", "需求文档 设计决策 版本里程碑"],
            ["张子豪", "技术研发", "Agent Runtime 工具接入 评测与故障恢复", "代码提交 测试记录 评测报告"],
            ["吴柳彤", "研究与运营", "用户研究 交互验证 内容与商业试点", "访谈记录 原型反馈 试点材料"],
            ["赵恒军", "指导教师", "研究方法 安全边界 学术与赛事指导", "评审意见 指导记录 资源协调"],
        ],
        widths=[2.5, 3.0, 6.2, 4.3],
        caption="表 16 团队分工与贡献证据",
        font_size=8.8,
    )

    doc.add_paragraph("9.2 学生能力成长路径", style="Heading 2")
    add_table(
        doc,
        ["阶段", "学习任务", "实践任务", "可核验成长成果"],
        [
            ["需求发现", "用户研究 创新方法", "访谈 观察 任务建模", "问题定义与研究材料"],
            ["产品试制", "系统设计 交互与安全", "原型 兼容性 权限设计", "设计决策与版本记录"],
            ["实验论证", "实验设计 数据分析", "对照 消融 红队 故障注入", "评测报告与复现实验"],
            ["市场验证", "商业模式 财务与合规", "校园试点 报价 合作复盘", "意向 反馈与经营口径"],
            ["成果传播", "知识产权 科学表达", "软著 专利交底 论文 路演", "成果归属与公开材料"],
        ],
        widths=[2.8, 4.0, 4.4, 4.8],
        caption="表 17 项目驱动的人才培养路径",
    )

    doc.add_paragraph("9.3 协作与质量机制", style="Heading 2")
    add_bullets(
        doc,
        [
            "每周研发站会检查目标、阻塞、风险和下一项可验收任务。",
            "每两周产品评审由用户价值、技术可行性、安全边界和证据质量四项共同决定优先级。",
            "重大范围、数据、外部合作和财务事项由项目负责人、相关成员和指导教师共同审查。",
            "公开材料由技术负责人、用户研究负责人和指导教师交叉复核，确保能力、数据与成员贡献真实。",
            "成员贡献以任务、代码、文档、测试和运营证据归档，不以署名顺序替代实际投入。",
        ],
    )

    add_chapter(doc, "第十章 财务规划与资源配置", page_break=False)
    doc.add_paragraph("10.1 财务测算边界", style="Heading 2")
    add_para(
        doc,
        "本章用于检验商业模型能否形成可持续经营，不代表项目已经取得收入、融资或用户规模。测算以个人年费 240 元、校园团队年费 4 万元、私有部署单项目 20 万元为基准，收入确认以实际付费和验收为前提；模型调用、支持、交付和合规成本随任务增长动态调整。",
    )

    doc.add_paragraph("10.2 三年经营情景", style="Heading 2")
    add_figure(doc, figs["finance"], "图 11 Cuttle 三年收入与成本规划情景")
    add_table(
        doc,
        ["指标", "第一年", "第二年", "第三年", "主要假设"],
        [
            ["付费个人", "600 人", "7000 人", "3.5 万人", "年均收入 240 元"],
            ["校园团队", "6 个", "30 个", "100 个", "平均 4 万至 5 万元"],
            ["私有部署", "1 个", "5 个", "15 个", "平均 15 万至 20 万元"],
            ["营业收入", "47.4 万元", "403 万元", "1740 万元", "含技能生态收入"],
            ["经营成本", "120 万元", "330 万元", "1080 万元", "研发 模型 市场与交付"],
            ["经营结果", "负 72.6 万元", "73 万元", "660 万元", "第二年进入盈亏平衡区间"],
        ],
        widths=[2.8, 2.8, 2.8, 2.8, 4.8],
        caption="表 18 三年经营情景测算",
        font_size=8.8,
    )

    doc.add_paragraph("10.3 首轮资源需求", style="Heading 2")
    add_para(
        doc,
        "首轮拟配置 120 万元资源，用于完成十二个月的产品、研究和市场验证。资金分三批释放：原型与研究通过后投入兼容性和安全测试；对照实验达到门槛后投入校园试点；留存和付费意愿达到门槛后扩大市场与交付。项目将优先使用高校实验室、开源模型和本地推理资源降低固定成本。",
    )
    add_table(
        doc,
        ["用途", "比例", "金额", "对应交付"],
        [
            ["产品与研发", "42%", "50.4 万元", "输入入口 情境引擎 执行与验证"],
            ["评测与安全", "18%", "21.6 万元", "数据集 对照实验 红队与恢复"],
            ["市场与试点", "18%", "21.6 万元", "校园研究 试点与用户支持"],
            ["模型与基础设施", "12%", "14.4 万元", "本地推理 云额度 监测"],
            ["知识产权与预备金", "10%", "12 万元", "软著 专利 合规与不确定性"],
        ],
        widths=[4.0, 2.5, 3.4, 6.1],
        caption="表 19 首轮资源配置",
    )

    add_chapter(doc, "第十一章 风险管理", page_break=False)
    doc.add_paragraph("11.1 关键风险与应对", style="Heading 2")
    add_table(
        doc,
        ["风险", "概率", "影响", "预警信号", "应对措施"],
        [
            ["输入法与应用兼容性", "高", "高", "崩溃 焦点丢失 延迟上升", "缩小应用范围 自动化兼容测试 降级为快捷入口"],
            ["情境误判与隐私", "中", "极高", "敏感字段被读取 用户纠正上升", "默认关闭 敏感屏蔽 最小读取 独立审计"],
            ["Agent 误操作", "中", "极高", "越权 外发 覆盖或不可逆动作", "最小权限 审批 预览 沙箱与回滚"],
            ["模型能力波动", "中", "高", "成功率下降 成本或时延异常", "模型替换 固定评测 预算上限和降级"],
            ["用户留存不足", "中", "高", "四周留存低 高频任务不足", "聚焦单一高价值任务 改善首用与技能复用"],
            ["商业范围失控", "中", "高", "定制需求占用研发 团队交付亏损", "标准合同 边界报价 里程碑验收"],
            ["知识产权争议", "低", "高", "依赖许可不清 数据授权缺失", "依赖清单 原创记录 授权和合规审查"],
            ["团队持续性", "中", "中", "核心模块单人掌握 里程碑延期", "双人复核 文档化 模块轮换与交接演练"],
        ],
        widths=[3.0, 1.5, 1.6, 4.4, 5.5],
        caption="表 20 项目风险登记表",
        font_size=8.1,
    )

    doc.add_paragraph("11.2 停止与转向条件", style="Heading 2")
    add_para(
        doc,
        "项目在每一阶段设置明确的停止或转向条件。若用户不愿在输入时刻授权情境读取，则转向用户主动选区与快捷指令；若连续升级无法显著降低时间和重复提示，则将 Agent 能力限制在少数高价值任务；若固定 Swarm 优于自适应路由，则收缩路由复杂度；若校园团队缺乏付费意愿，则优先验证个人订阅和开发者生态。项目不以完成既定功能为唯一目标，而以问题是否真实、机制是否有效、安全是否达标和经营是否可持续决定投入。",
    )

    add_chapter(doc, "第十二章 社会价值与发展愿景")
    doc.add_paragraph("12.1 学习与知识工作的直接价值", style="Heading 2")
    add_para(
        doc,
        "Cuttle 计划把资料搬运、格式返工和重复提示交给受控工具，让学生和教师把时间用于理解、判断与创造。通过来源、证据、修改和验收记录，项目鼓励用户对 AI 结果保持审阅责任，避免把生成内容直接当作答案。对于科研与项目材料，版本化成果有助于团队复盘过程、发现错误并积累可复用方法。",
    )

    doc.add_paragraph("12.2 可信人工智能与数字包容", style="Heading 2")
    add_para(
        doc,
        "项目把权限、审批、数据最小化和失败恢复放在效率之前，使普通用户也能理解系统正在读取什么、准备做什么、完成了什么以及如何撤销。语音、自然语言和界面解释能够降低复杂 Agent 的使用门槛；本地优先和模型可替换架构则为高校、实验室和对数据敏感的机构提供更可控的应用路径。",
    )

    doc.add_paragraph("12.3 发展愿景", style="Heading 2")
    add_para(
        doc,
        "Cuttle 的长期目标是成为个人电脑上的意图层。用户在任何应用表达需求时，系统都能以最小必要情境理解任务，以适当的工具或智能体完成工作，以清晰证据说明结果，并把成果送回用户原本工作的地方。项目最终衡量的不是调用了多少模型或 Agent，而是用户是否减少了无意义搬运，任务是否更可靠，权限是否仍在用户手中。",
    )

    add_chapter(doc, "参考资料")
    refs = [
        ("[1] 教育部关于举办中国国际大学生创新大赛 2026 的通知", "https://www.moe.gov.cn/srcsite/A08/s5672/202607/t20260731_1445670.html"),
        ("[2] 中国国际大学生创新大赛 2025 高教主赛道创意组评审规则", "https://www.xit.edu.cn/stjq/2025/0610/c335a62225/page.htm"),
        ("[3] 中国互联网络信息中心 第 57 次中国互联网络发展状况统计报告", "https://www3.cnnic.cn/n4/2026/0304/c88-11549.html"),
        ("[4] 教育部 2025 年全国教育事业发展统计公报", "https://hudong.moe.gov.cn/jyb_sjzl/sjzl_fztjgb/202607/t20260706_1442870.html"),
        ("[5] 工业和信息化部 2025 年软件业运行情况", "https://www.miit.gov.cn/gxsj/tjfx/rjy/art/2026/art_65a12a560865432bb1548fdddc74f19c.html"),
        ("[6] 搜狗输入法升级日志", "https://pinyin.sogou.com/changelog.php"),
        ("[7] Wispr Flow Context Awareness", "https://docs.wisprflow.ai/articles/4678293671-Context-Awareness"),
        ("[8] OpenAI Introducing the Codex app", "https://openai.com/index/introducing-the-codex-app/"),
        ("[9] 中华人民共和国个人信息保护法", "https://www.npc.gov.cn/npc/c2/c30834/202108/t20210820_313088.html"),
        ("[10] 中华人民共和国数据安全法", "https://www.npc.gov.cn/npc/c2/c30834/202106/t20210610_311888.html"),
        ("[11] 生成式人工智能服务管理暂行办法", "https://www.miit.gov.cn/zcfg/qtl/art/2023/art_f4e8f71ae1dc43b0980b962907b7738f.html"),
        ("[12] Xiaomi MiMo Desktop", "https://mimo.mi.com/docs/en-US/news/latest/mimo-desktop"),
    ]
    for text, url in refs:
        p = doc.add_paragraph()
        p.paragraph_format.first_line_indent = Cm(0)
        p.paragraph_format.space_after = Pt(7)
        add_hyperlink(p, text, url)

    p = doc.add_paragraph()
    p.paragraph_format.first_line_indent = Cm(0)
    p.paragraph_format.space_before = Pt(12)
    r = p.add_run("说明  本计划书中的用户增长 市场占比 定价 财务与里程碑为项目规划情景 需要以真实访谈 试点 付费 合同和经营数据持续校准")
    set_run_font(r, size=9, bold=True, color=GRAY)

    settings = doc.settings.element
    update_fields = settings.find(qn("w:updateFields"))
    if update_fields is None:
        update_fields = OxmlElement("w:updateFields")
        settings.append(update_fields)
    update_fields.set(qn("w:val"), "true")

    core = doc.core_properties
    core.title = "Cuttle 情境原生智能输入系统参赛商业计划书"
    core.subject = "中国国际大学生创新大赛 2026 高教主赛道本科生创意组"
    core.author = "Cuttle 项目团队"
    core.keywords = "Cuttle, 情境原生智能输入, AI Agent, 商业计划书"
    core.comments = ""

    doc.save(OUTPUT)


if __name__ == "__main__":
    ensure_dirs()
    figures = build_figures()
    build_doc(figures)
    print(OUTPUT)
