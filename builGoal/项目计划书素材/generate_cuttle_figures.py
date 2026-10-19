from pathlib import Path
import math

from PIL import Image, ImageDraw, ImageFont


HERE = Path(__file__).resolve().parent
OUT = HERE / "cuttle-assets" / "figures"
OUT.mkdir(parents=True, exist_ok=True)
PAGE_BACKGROUND = HERE / "cuttle-assets" / "cuttle-page-background-color-v6-no-mascot.png"

W, H = 1800, 600
NAVY = "#123B6D"
TEAL = "#159AA6"
CORAL = "#F06B4E"
GOLD = "#E7A83E"
GREEN = "#4A9B72"
PURPLE = "#7767A8"
BLACK = "#111111"
MID = "#5F6873"
LINE = "#CBD5DF"
LIGHT = "#F4F7FA"
PALE_BLUE = "#EAF1F8"
PALE_TEAL = "#E8F5F5"
PALE_CORAL = "#FCEEEA"
WHITE = "#FFFFFF"


def font(size, bold=False):
    name = "msyhbd.ttc" if bold else "msyh.ttc"
    path = Path(r"C:\Windows\Fonts") / name
    return ImageFont.truetype(str(path), size)


def canvas():
    im = Image.new("RGB", (W, H), WHITE)
    d = ImageDraw.Draw(im)
    d.rounded_rectangle((8, 8, W - 8, H - 8), radius=26, outline=LINE, width=3, fill=WHITE)
    return im, d


def fit_text(d, box, text, max_size=40, min_size=20, bold=False, fill=BLACK, align="center", spacing=8):
    x1, y1, x2, y2 = box
    width = x2 - x1
    height = y2 - y1
    for size in range(max_size, min_size - 1, -1):
        f = font(size, bold)
        lines = []
        for para in str(text).split("\n"):
            current = ""
            for ch in para:
                test = current + ch
                if d.textlength(test, font=f) <= width or not current:
                    current = test
                else:
                    lines.append(current)
                    current = ch
            lines.append(current)
        total_h = sum(d.textbbox((0, 0), line or " ", font=f)[3] for line in lines) + spacing * (len(lines) - 1)
        if total_h <= height:
            y = y1 + (height - total_h) / 2
            for line in lines:
                bb = d.textbbox((0, 0), line or " ", font=f)
                tw = bb[2] - bb[0]
                if align == "left":
                    x = x1
                elif align == "right":
                    x = x2 - tw
                else:
                    x = x1 + (width - tw) / 2
                d.text((x, y), line, font=f, fill=fill)
                y += bb[3] + spacing
            return


def rounded(d, box, fill=WHITE, outline=LINE, radius=22, width=3):
    d.rounded_rectangle(box, radius=radius, fill=fill, outline=outline, width=width)


def arrow(d, start, end, color=NAVY, width=8, head=18):
    x1, y1 = start
    x2, y2 = end
    d.line((x1, y1, x2, y2), fill=color, width=width)
    ang = math.atan2(y2 - y1, x2 - x1)
    a1 = ang + math.pi * 0.82
    a2 = ang - math.pi * 0.82
    p1 = (x2 + head * math.cos(a1), y2 + head * math.sin(a1))
    p2 = (x2 + head * math.cos(a2), y2 + head * math.sin(a2))
    d.polygon([(x2, y2), p1, p2], fill=color)


def save(im, name):
    im.save(OUT / name, dpi=(220, 220), optimize=True)


def four_stage(name, stages, footers=None):
    im, d = canvas()
    colors = [(PALE_BLUE, NAVY), (PALE_TEAL, TEAL), (PALE_CORAL, CORAL), ("#F3EFF9", PURPLE)]
    x0, gap, bw = 90, 50, 365
    for i, (title, body) in enumerate(stages):
        x = x0 + i * (bw + gap)
        rounded(d, (x, 105, x + bw, 420), fill=colors[i][0], outline=colors[i][1], radius=28, width=4)
        d.ellipse((x + 24, 126, x + 86, 188), fill=colors[i][1])
        fit_text(d, (x + 24, 126, x + 86, 188), str(i + 1), 34, 24, True, WHITE)
        fit_text(d, (x + 95, 118, x + bw - 22, 195), title, 35, 23, True, BLACK, "left")
        d.line((x + 28, 213, x + bw - 28, 213), fill=colors[i][1], width=3)
        fit_text(d, (x + 34, 235, x + bw - 34, 390), body, 30, 21, False, BLACK)
        if i < 3:
            arrow(d, (x + bw + 8, 265), (x + bw + gap - 8, 265), colors[i + 1][1], 7, 17)
    if footers:
        fit_text(d, (95, 460, W - 95, 555), "   ·   ".join(footers), 29, 20, True, MID)
    save(im, name)


def fig_1_1():
    four_stage(
        "fig1-1-background-evolution.png",
        [
            ("传统输入法", "以词频、词库与联想提升录入速度"),
            ("大模型助手", "从文本补全扩展到对话式内容生成"),
            ("情境感知", "理解当前窗口、选区、任务与用户偏好"),
            ("Cuttle协同", "把表达辅助连接到可审阅的任务执行"),
        ],
        ["更快输入", "减少上下文搬运", "过程可见", "成果可复用"],
    )


def loop_figure(name, nodes, center_title, center_sub=""):
    im, d = canvas()
    cx, cy = 900, 305
    rx, ry = 610, 205
    colors = [NAVY, TEAL, CORAL, GOLD, GREEN, PURPLE]
    centers = []
    n = len(nodes)
    for i in range(n):
        ang = -math.pi / 2 + i * 2 * math.pi / n
        centers.append((cx + rx * math.cos(ang), cy + ry * math.sin(ang)))
    for i in range(n):
        a = centers[i]
        b = centers[(i + 1) % n]
        vx, vy = b[0] - a[0], b[1] - a[1]
        ln = max(math.hypot(vx, vy), 1)
        start = (a[0] + vx / ln * 105, a[1] + vy / ln * 52)
        end = (b[0] - vx / ln * 105, b[1] - vy / ln * 52)
        arrow(d, start, end, colors[(i + 1) % len(colors)], 7, 16)
    rounded(d, (cx - 205, cy - 75, cx + 205, cy + 75), fill=LIGHT, outline=NAVY, radius=38, width=4)
    fit_text(d, (cx - 190, cy - 62, cx + 190, cy + 8), center_title, 40, 25, True)
    if center_sub:
        fit_text(d, (cx - 180, cy + 5, cx + 180, cy + 55), center_sub, 24, 18, False, MID)
    for i, (title, body) in enumerate(nodes):
        x, y = centers[i]
        rounded(d, (x - 170, y - 58, x + 170, y + 58), fill=WHITE, outline=colors[i % len(colors)], radius=26, width=4)
        fit_text(d, (x - 152, y - 46, x + 152, y - 4), title, 30, 21, True)
        fit_text(d, (x - 152, y + 0, x + 152, y + 45), body, 22, 17, False, MID)
    save(im, name)


def fig_1_2():
    loop_figure(
        "fig1-2-value-loop.png",
        [
            ("情境感知", "识别窗口、选区与任务"),
            ("自然表达", "生成符合语境的内容"),
            ("计划审批", "高风险步骤先确认"),
            ("受控执行", "在权限边界内行动"),
            ("审阅复用", "沉淀结果、偏好与技能"),
        ],
        "Cuttle价值闭环",
        "从输入增强到任务协同",
    )


def fig_2_1():
    im, d = canvas()
    stages = [
        ("输入效率工具", "词库 / 联想", 1),
        ("AI表达工具", "补写 / 改写", 2),
        ("系统级助手", "上下文 / 跨应用", 3),
        ("情境协同平台", "计划 / 审批 / 执行", 4),
    ]
    colors = [NAVY, TEAL, CORAL, PURPLE]
    for i, (title, sub, level) in enumerate(stages):
        x = 115 + i * 410
        y = 450 - level * 72
        d.rounded_rectangle((x, y, x + 310, 485), radius=20, fill=colors[i])
        fit_text(d, (x + 15, y + 12, x + 295, y + 65), title, 32, 22, True, WHITE)
        fit_text(d, (x + 15, y + 67, x + 295, 472), sub, 24, 18, False, WHITE)
        if i < 3:
            arrow(d, (x + 320, y + 70), (x + 395, y - 28), colors[i + 1], 8, 18)
    d.line((100, 505, 1700, 505), fill=BLACK, width=4)
    fit_text(d, (100, 515, 1700, 565), "理解深度、跨应用连续性与任务可控性持续提升", 28, 22, True, MID)
    save(im, "fig2-1-industry-trend.png")


def quadrant(name, items, center=None):
    im, d = canvas()
    boxes = [(75, 55, 865, 280), (935, 55, 1725, 280), (75, 320, 865, 545), (935, 320, 1725, 545)]
    palette = [(PALE_BLUE, NAVY), (PALE_TEAL, TEAL), (PALE_CORAL, CORAL), ("#F3EFF9", PURPLE)]
    for box, item, color in zip(boxes, items, palette):
        title, body = item
        rounded(d, box, fill=color[0], outline=color[1], radius=28, width=4)
        d.rounded_rectangle((box[0] + 24, box[1] + 24, box[0] + 180, box[1] + 80), radius=18, fill=color[1])
        fit_text(d, (box[0] + 34, box[1] + 28, box[0] + 170, box[1] + 75), title, 29, 21, True, WHITE)
        fit_text(d, (box[0] + 210, box[1] + 25, box[2] - 30, box[3] - 25), body, 29, 20, False, BLACK, "left")
    if center:
        rounded(d, (760, 255, 1040, 345), fill=WHITE, outline=BLACK, radius=30, width=4)
        fit_text(d, (780, 270, 1020, 330), center, 32, 22, True)
    save(im, name)


def fig_2_2():
    quadrant(
        "fig2-2-pain-points.png",
        [
            ("痛点一", "上下文反复丢失\n用户频繁复制、切换与重新解释"),
            ("痛点二", "表达与场景不匹配\n同一内容需针对对象反复改写"),
            ("痛点三", "复杂任务过程不可见\n从意图到交付缺少连续工作流"),
            ("痛点四", "自动化信任不足\n权限、审批、回退与审计不清晰"),
        ],
        "核心矛盾",
    )


def fig_2_3():
    quadrant(
        "fig2-3-pest.png",
        [
            ("P 政策", "人工智能与数字经济持续推进\n数据安全与合规要求同步提高"),
            ("E 经济", "模型与算力成本逐步下降\n商业化需聚焦高频刚需场景"),
            ("S 社会", "用户接受度快速提升\n同时关注隐私、误操作与可信度"),
            ("T 技术", "多模态、UI自动化与Agent加速成熟\n兼容性和可靠性仍是落地难点"),
        ],
        "外部环境",
    )


def fig_2_4():
    quadrant(
        "fig2-4-swot.png",
        [
            ("S 优势", "统一入口与情境感知\n输入、桌宠、Agent协同设计\n强调权限、审计与可回退"),
            ("W 劣势", "早期品牌与样本规模有限\n跨应用兼容成本较高\n商业数据仍需持续验证"),
            ("O 机会", "系统级AI助手需求增长\n高校与知识工作者试点空间大\n国产化与私有部署需求明确"),
            ("T 威胁", "平台厂商快速下沉\n隐私合规与模型成本波动\n同类产品竞争加剧"),
        ],
        "战略研判",
    )


def bar_chart(name, categories, series, y_max, unit, note=""):
    im, d = canvas()
    left, top, right, bottom = 170, 70, 1640, 500
    all_values = [v for _, vals in series for v in vals]
    plot_min = -100 if min(all_values) < 0 else 0
    plot_max = y_max
    span = plot_max - plot_min
    def y_of(value):
        return bottom - (value - plot_min) / span * (bottom - top)
    d.line((left, top, left, bottom), fill=BLACK, width=4)
    baseline = y_of(0)
    d.line((left, baseline, right, baseline), fill=BLACK, width=4)
    ticks = 5
    for i in range(ticks + 1):
        y = bottom - (bottom - top) * i / ticks
        d.line((left, y, right, y), fill=LINE, width=2)
        val = plot_min + span * i / ticks
        fit_text(d, (30, y - 22, left - 20, y + 22), f"{val:g}", 23, 18, False, MID, "right")
    n = len(categories)
    group_w = (right - left) / n
    colors = [NAVY, TEAL, CORAL]
    for i, cat in enumerate(categories):
        gx = left + group_w * i
        bar_w = min(90, group_w / (len(series) + 1))
        total_w = bar_w * len(series) + 18 * (len(series) - 1)
        start_x = gx + (group_w - total_w) / 2
        for j, (label, vals) in enumerate(series):
            val = vals[i]
            x1 = start_x + j * (bar_w + 18)
            y1 = y_of(val)
            y_top, y_bottom = min(y1, baseline), max(y1, baseline)
            d.rounded_rectangle((x1, y_top, x1 + bar_w, y_bottom), radius=10, fill=colors[j], outline=colors[j])
            if val >= 0:
                label_box = (x1 - 20, y_top - 38, x1 + bar_w + 20, y_top - 2)
            else:
                label_box = (x1 - 20, y_bottom + 2, x1 + bar_w + 20, y_bottom + 38)
            fit_text(d, label_box, f"{val:g}", 23, 17, True, BLACK)
        fit_text(d, (gx, bottom + 10, gx + group_w, bottom + 53), cat, 25, 19, True)
    # Keep the legend in the upper-left plotting area, away from the tallest third-year bars.
    lx = 250
    for j, (label, _) in enumerate(series):
        d.rounded_rectangle((lx, 22 + j * 42, lx + 28, 50 + j * 42), radius=6, fill=colors[j])
        fit_text(d, (lx + 40, 18 + j * 42, lx + 240, 55 + j * 42), label, 22, 18, False, BLACK, "left")
    fit_text(d, (30, 15, 260, 55), unit, 22, 17, False, MID, "left")
    if note:
        fit_text(d, (250, 548, 1550, 585), note, 21, 17, False, MID)
    save(im, name)


def make_page_background():
    w, h = 1654, 2339
    im = Image.new("RGB", (w, h), WHITE)
    d = ImageDraw.Draw(im)
    # Header decoration stays within the page-margin band and leaves body content untouched.
    for y in range(0, 150):
        t = y / 150
        color = (int(238 + 17 * t), int(245 + 10 * t), int(250 + 5 * t))
        d.line((0, y, w, y), fill=color)
    d.line((0, 132, w, 132), fill=NAVY, width=4)
    d.line((170, 132, 710, 132), fill=TEAL, width=6)
    d.line((710, 132, 835, 132), fill=CORAL, width=6)
    d.arc((-170, -165, 260, 220), 5, 145, fill=TEAL, width=8)
    d.arc((-125, -120, 215, 185), 8, 145, fill=CORAL, width=5)
    for x, y, r, color in [(1500, 38, 14, TEAL), (1545, 72, 8, CORAL), (1590, 40, 5, NAVY)]:
        d.ellipse((x - r, y - r, x + r, y + r), fill=color)
    # Footer decoration uses layered curves, with a clean center for page numbering.
    footer_top = h - 110
    for y in range(footer_top, h):
        t = (y - footer_top) / 175
        color = (int(255 - 13 * t), int(255 - 8 * t), int(255 - 3 * t))
        d.line((0, y, w, y), fill=color)
    d.arc((-420, h - 150, 980, h + 270), 190, 340, fill=NAVY, width=7)
    d.arc((-300, h - 132, 1070, h + 255), 190, 340, fill=TEAL, width=5)
    d.arc((720, h - 142, 2010, h + 250), 200, 350, fill=CORAL, width=6)
    d.line((0, h - 45, 600, h - 45), fill=TEAL, width=5)
    d.line((1050, h - 45, w, h - 45), fill=NAVY, width=5)
    for x, y, r, color in [(80, h - 75, 9, CORAL), (130, h - 92, 5, TEAL), (1530, h - 72, 10, TEAL)]:
        d.ellipse((x - r, y - r, x + r, y + r), fill=color)
    im.save(PAGE_BACKGROUND, dpi=(200, 200), optimize=True)


def fig_2_5():
    bar_chart(
        "fig2-5-market-growth.png",
        ["第一年", "第二年", "第三年"],
        [("注册用户（万人）", [1, 8, 30]), ("付费用户（万人）", [0.05, 0.64, 3.6])],
        32,
        "单位：万人",
        "按项目分阶段经营目标测算，实际结果取决于产品验证与渠道转化",
    )


def fig_3_1():
    im, d = canvas()
    layers = [
        ("智能输入入口", "补全、改写、回复、术语与快捷指令", PALE_BLUE, NAVY),
        ("情境感知引擎", "窗口、选区、文件、任务状态与个人偏好", PALE_TEAL, TEAL),
        ("桌面伙伴与工作台", "提示、计划、审批、进度、结果与回退", PALE_CORAL, CORAL),
        ("任务智能体与技能生态", "跨应用执行、专用技能、团队模板与复用", "#F3EFF9", PURPLE),
    ]
    for i, (title, body, fill, color) in enumerate(layers):
        y = 50 + i * 132
        rounded(d, (140, y, 1500, y + 102), fill=fill, outline=color, radius=24, width=4)
        d.rounded_rectangle((165, y + 20, 500, y + 82), radius=18, fill=color)
        fit_text(d, (180, y + 24, 485, y + 78), title, 31, 22, True, WHITE)
        fit_text(d, (540, y + 18, 1460, y + 86), body, 28, 20, False, BLACK, "left")
        if i < 3:
            arrow(d, (820, y + 110), (820, y + 128), color, 6, 14)
    rounded(d, (1545, 50, 1690, 548), fill=LIGHT, outline=BLACK, radius=28, width=4)
    fit_text(d, (1575, 80, 1660, 520), "安全\n权限\n审计\n回退", 31, 22, True)
    save(im, "fig3-1-product-layers.png")


def fig_3_2():
    im, d = canvas()
    steps = [
        ("情境建立", "自动识别当前工作内容"),
        ("即时表达", "在输入处直接补写改写"),
        ("任务升级", "将复杂意图转为任务"),
        ("计划审批", "确认步骤、权限与范围"),
        ("受控执行", "跨应用行动并持续反馈"),
        ("交付复用", "审阅成果并沉淀模板"),
    ]
    colors = [NAVY, TEAL, CORAL, GOLD, GREEN, PURPLE]
    for i, (title, body) in enumerate(steps):
        x = 65 + i * 285
        d.ellipse((x + 85, 55, x + 155, 125), fill=colors[i])
        fit_text(d, (x + 90, 62, x + 150, 117), str(i + 1), 32, 24, True, WHITE)
        if i < 5:
            arrow(d, (x + 160, 90), (x + 275, 90), colors[i + 1], 7, 16)
        rounded(d, (x, 155, x + 240, 485), fill=WHITE, outline=colors[i], radius=26, width=4)
        fit_text(d, (x + 18, 178, x + 222, 245), title, 30, 22, True)
        d.line((x + 22, 260, x + 218, 260), fill=colors[i], width=3)
        fit_text(d, (x + 24, 285, x + 216, 448), body, 25, 19, False, BLACK)
    fit_text(d, (180, 520, 1620, 570), "用户始终可见：当前情境  →  建议内容  →  执行计划  →  任务状态  →  最终成果", 25, 19, True, MID)
    save(im, "fig3-2-user-journey.png")


def fig_3_3():
    im, d = canvas()
    layers = [
        ("交互层", "输入法入口｜桌面伙伴｜任务工作台", NAVY),
        ("情境层", "窗口与选区｜文件与剪贴板｜用户偏好", TEAL),
        ("协同层", "意图识别｜计划生成｜审批与状态机", CORAL),
        ("执行层", "受控工具｜应用适配器｜失败回退", GOLD),
        ("资产层", "技能模板｜审计记录｜可复用成果", GREEN),
        ("模型层", "云端/本地模型路由｜提示与上下文治理", PURPLE),
    ]
    for i, (title, body, color) in enumerate(layers):
        y = 45 + i * 84
        rounded(d, (105, y, 1390, y + 66), fill=WHITE, outline=color, radius=18, width=4)
        d.rounded_rectangle((125, y + 10, 320, y + 56), radius=13, fill=color)
        fit_text(d, (135, y + 11, 310, y + 54), title, 26, 20, True, WHITE)
        fit_text(d, (350, y + 8, 1360, y + 58), body, 26, 18, False, BLACK, "left")
    rounded(d, (1440, 45, 1695, 486), fill=LIGHT, outline=BLACK, radius=26, width=4)
    fit_text(d, (1470, 70, 1665, 130), "安全边界", 31, 22, True)
    for i, text in enumerate(["最小权限", "敏感操作审批", "凭据隔离", "全程审计", "可中断可回退"]):
        y = 145 + i * 60
        d.ellipse((1470, y + 7, 1490, y + 27), fill=CORAL)
        fit_text(d, (1505, y, 1665, y + 42), text, 23, 18, False, BLACK, "left")
    fit_text(d, (200, 530, 1600, 570), "架构原则：模型负责理解与建议，权限策略决定能否执行，审计记录支撑责任追溯", 24, 18, True, MID)
    save(im, "fig3-3-tech-architecture.png")


def fig_4_1():
    im, d = canvas()
    rounded(d, (640, 190, 1160, 410), fill=LIGHT, outline=NAVY, radius=40, width=5)
    fit_text(d, (680, 220, 1120, 300), "Cuttle价值主张", 38, 26, True)
    fit_text(d, (700, 305, 1100, 380), "以统一入口降低表达与任务协同成本", 26, 19, False, MID)
    cards = [
        ("个人订阅", "高级表达与任务额度", 90, 75, NAVY),
        ("团队许可", "协作模板与管理能力", 90, 390, TEAL),
        ("私有部署", "本地模型与合规交付", 1330, 75, CORAL),
        ("技能生态", "模板、插件与分成", 1330, 390, PURPLE),
        ("联合方案", "高校与行业伙伴共建", 640, 30, GOLD),
    ]
    for title, body, x, y, color in cards:
        rounded(d, (x, y, x + 380, y + 125), fill=WHITE, outline=color, radius=24, width=4)
        fit_text(d, (x + 20, y + 15, x + 360, y + 58), title, 30, 21, True)
        fit_text(d, (x + 25, y + 64, x + 355, y + 110), body, 23, 18, False, MID)
        sx = x + 380 if x < 640 else (x if x > 1160 else x + 190)
        sy = y + 62 if x != 640 else y + 125
        ex = 640 if x < 640 else (1160 if x > 1160 else 900)
        ey = 300 if x != 640 else 190
        arrow(d, (sx, sy), (ex, ey), color, 6, 15)
    fit_text(d, (430, 515, 1370, 565), "目标客户：个人知识工作者｜高校师生｜创新团队｜重视数据安全的组织", 25, 18, True, MID)
    save(im, "fig4-1-business-model.png")


def fig_5_1():
    loop_figure(
        "fig5-1-growth-loop.png",
        [
            ("真实任务试点", "选择高频可量化场景"),
            ("可复现案例", "沉淀前后对比与证据"),
            ("内容与赛事传播", "形成可信产品叙事"),
            ("新用户试用", "低门槛体验核心价值"),
            ("留存与技能复用", "模板越用越贴合"),
            ("口碑推荐", "由成果带动自然增长"),
        ],
        "增长飞轮",
        "产品价值驱动传播",
    )


def fig_6_1():
    im, d = canvas()
    cx, cy, r = 410, 300, 220
    values = [45, 20, 15, 10, 10]
    colors = [NAVY, TEAL, CORAL, GOLD, PURPLE]
    labels = ["产品研发", "市场与试点", "模型与基础设施", "合规与知识产权", "运营与预备金"]
    start = -90
    for val, color in zip(values, colors):
        end = start + val / 100 * 360
        d.pieslice((cx - r, cy - r, cx + r, cy + r), start, end, fill=color, outline=WHITE, width=4)
        start = end
    d.ellipse((cx - 92, cy - 92, cx + 92, cy + 92), fill=WHITE)
    fit_text(d, (cx - 75, cy - 55, cx + 75, cy + 55), "首轮资金\n100%", 31, 22, True)
    for i, (label, val, color) in enumerate(zip(labels, values, colors)):
        y = 95 + i * 92
        d.rounded_rectangle((800, y, 845, y + 45), radius=8, fill=color)
        fit_text(d, (875, y - 5, 1360, y + 50), label, 29, 21, True, BLACK, "left")
        fit_text(d, (1380, y - 5, 1580, y + 50), f"{val}%", 31, 22, True, color, "right")
        d.line((875, y + 58, 1580, y + 58), fill=LINE, width=2)
    fit_text(d, (850, 545, 1600, 580), "资金使用以完成核心产品验证和可复制试点为优先", 22, 17, False, MID)
    save(im, "fig6-1-fund-allocation.png")


def fig_6_2():
    bar_chart(
        "fig6-2-financial-forecast.png",
        ["第一年", "第二年", "第三年"],
        [("营业收入", [24, 180, 650]), ("总成本", [52, 150, 380]), ("经营结果", [-28, 30, 270])],
        700,
        "单位：万元",
        "经营结果＝营业收入－总成本；第一年投入期亏损，第二年进入盈亏平衡区间",
    )


def fig_7_1():
    im, d = canvas()
    rounded(d, (660, 45, 1140, 145), fill=PALE_BLUE, outline=NAVY, radius=26, width=4)
    fit_text(d, (690, 62, 1110, 128), "项目负责人 / 项目管理委员会", 31, 22, True)
    d.line((900, 145, 900, 220), fill=BLACK, width=5)
    d.line((280, 220, 1520, 220), fill=BLACK, width=5)
    teams = [
        ("技术研发组", "输入法、情境感知\nAgent与安全架构", 100, NAVY),
        ("产品验证组", "需求调研、原型设计\n测试与数据复盘", 530, TEAL),
        ("运营与竞赛组", "材料、展示、传播\n渠道与合作维护", 960, CORAL),
    ]
    for title, body, x, color in teams:
        center = x + 170
        d.line((center, 220, center, 260), fill=BLACK, width=5)
        rounded(d, (x, 260, x + 340, 465), fill=WHITE, outline=color, radius=26, width=4)
        fit_text(d, (x + 25, 282, x + 315, 345), title, 30, 22, True)
        fit_text(d, (x + 28, 360, x + 312, 445), body, 24, 18, False, MID)
    rounded(d, (1390, 260, 1700, 465), fill=LIGHT, outline=PURPLE, radius=26, width=4)
    fit_text(d, (1415, 282, 1675, 345), "指导与专家组", 29, 21, True)
    fit_text(d, (1420, 360, 1670, 445), "赵恒军副教授\n技术、研究与风险把关", 23, 17, False, MID)
    arrow(d, (1390, 365), (1310, 365), PURPLE, 6, 15)
    fit_text(d, (300, 510, 1500, 560), "重大决策集体评审｜需求与研发双向闭环｜周推进、月复盘、阶段里程碑验收", 24, 18, True, MID)
    save(im, "fig7-1-organization.png")


def fig_8_1():
    im, d = canvas()
    left, top, size = 95, 80, 400
    cell = size / 3
    fills = [["#FFF7E7", "#FCE9DE", "#F8D9D2"], ["#EEF7EA", "#FFF1D7", "#FBE1D8"], ["#E8F4ED", "#EDF4E7", "#FFF3D9"]]
    for row in range(3):
        for col in range(3):
            x1 = left + col * cell
            y1 = top + row * cell
            d.rectangle((x1, y1, x1 + cell, y1 + cell), fill=fills[row][col], outline=WHITE, width=4)
    labels = [
        ("输入兼容", 2, 0, CORAL), ("隐私安全", 2, 1, CORAL),
        ("技术可靠性", 1, 0, GOLD), ("团队持续性", 1, 1, GOLD),
        ("市场竞争", 1, 2, TEAL), ("成本波动", 0, 2, NAVY),
        ("知识产权", 2, 1, PURPLE),
    ]
    offsets = {}
    for label, col, row, color in labels:
        key = (col, row)
        idx = offsets.get(key, 0)
        offsets[key] = idx + 1
        x = left + col * cell + 12
        y = top + row * cell + 12 + idx * 48
        d.rounded_rectangle((x, y, x + cell - 24, y + 40), radius=12, fill=WHITE, outline=color, width=3)
        fit_text(d, (x + 8, y + 3, x + cell - 32, y + 37), label, 20, 16, True)
    fit_text(d, (left - 65, top, left - 10, top + size), "影响程度\n高\n中\n低", 21, 16, True, MID)
    fit_text(d, (left, top + size + 10, left + size, top + size + 48), "发生概率：低  →  中  →  高", 22, 17, True, MID)
    loop = [("识别", NAVY), ("评估", TEAL), ("预警", GOLD), ("应对", CORAL), ("复盘", PURPLE)]
    x = 750
    for i, (label, color) in enumerate(loop):
        rounded(d, (x, 120 + i * 82, x + 300, 178 + i * 82), fill=WHITE, outline=color, radius=20, width=4)
        fit_text(d, (x + 20, 128 + i * 82, x + 280, 170 + i * 82), label, 28, 21, True)
        if i < len(loop) - 1:
            arrow(d, (x + 150, 182 + i * 82), (x + 150, 194 + i * 82), color, 5, 12)
    rounded(d, (1160, 95, 1690, 500), fill=LIGHT, outline=BLACK, radius=28, width=4)
    fit_text(d, (1190, 115, 1660, 170), "预警触发条件", 30, 22, True)
    fit_text(d, (1200, 180, 1650, 470), "• 关键应用失败率持续上升\n• 未授权数据访问或敏感信息外泄\n• 任务回退率超过阈值\n• 单次任务成本明显偏离预算\n• 试点留存与付费转化连续下降", 25, 18, False, BLACK, "left")
    save(im, "fig8-1-risk-matrix.png")


def fig_9_1():
    im, d = canvas()
    y = 290
    d.line((110, y, 1690, y), fill=BLACK, width=6)
    phases = [
        ("0—3个月", "M1 核心闭环", "输入增强\n权限与审计基线", NAVY),
        ("4—8个月", "M2 可用原型", "典型场景打通\n完成首轮试点", TEAL),
        ("9—15个月", "M3 产品验证", "稳定性提升\n形成付费样本", CORAL),
        ("16—24个月", "M4 规模复制", "团队版本\n渠道与生态合作", GOLD),
        ("24个月后", "M5 平台演进", "技能市场\n行业解决方案", PURPLE),
    ]
    gap = 310
    for i, (period, milestone, body, color) in enumerate(phases):
        x = 130 + i * gap
        d.ellipse((x - 18, y - 18, x + 18, y + 18), fill=color)
        box_y = 70 if i % 2 == 0 else 350
        rounded(d, (x - 105, box_y, x + 210, box_y + 170), fill=WHITE, outline=color, radius=24, width=4)
        fit_text(d, (x - 90, box_y + 12, x + 195, box_y + 50), period, 24, 18, True, color)
        fit_text(d, (x - 90, box_y + 50, x + 195, box_y + 92), milestone, 27, 20, True)
        fit_text(d, (x - 85, box_y + 96, x + 190, box_y + 160), body, 20, 14, False, MID)
        d.line((x, y - 18 if box_y < y else y + 18, x, box_y + 170 if box_y < y else box_y), fill=color, width=4)
    fit_text(d, (200, 535, 1600, 580), "每一阶段均以可验证里程碑为进入下一阶段的决策门槛", 25, 18, True, MID)
    save(im, "fig9-1-roadmap.png")


def fig_10_1():
    im, d = canvas()
    rounded(d, (655, 205, 1145, 395), fill=LIGHT, outline=NAVY, radius=42, width=5)
    fit_text(d, (700, 230, 1100, 305), "Cuttle社会价值", 38, 26, True)
    fit_text(d, (715, 315, 1085, 370), "可信、普惠、可持续的人机协同", 24, 18, False, MID)
    nodes = [
        ("学习与工作效率", "减少机械搬运\n缩短从想法到交付的路径", 70, 70, NAVY),
        ("数字包容", "降低表达门槛\n支持不同能力与语言习惯", 70, 390, TEAL),
        ("可信人工智能", "权限清晰、过程可审计\n推动负责任应用", 1330, 70, CORAL),
        ("创新与就业", "带动技能开发、适配服务\n与行业解决方案岗位", 1330, 390, PURPLE),
    ]
    for title, body, x, y, color in nodes:
        rounded(d, (x, y, x + 400, y + 145), fill=WHITE, outline=color, radius=26, width=4)
        fit_text(d, (x + 25, y + 18, x + 375, y + 62), title, 29, 21, True)
        fit_text(d, (x + 28, y + 70, x + 372, y + 130), body, 22, 17, False, MID)
        start = (x + 400, y + 72) if x < 655 else (x, y + 72)
        end = (655, 260 if y < 205 else 340) if x < 655 else (1145, 260 if y < 205 else 340)
        arrow(d, start, end, color, 6, 15)
    fit_text(d, (450, 520, 1350, 565), "以真实任务成效为评价依据，以安全边界保障长期社会价值", 24, 18, True, MID)
    save(im, "fig10-1-social-impact.png")


def main():
    make_page_background()
    fig_1_1()
    fig_1_2()
    fig_2_1()
    fig_2_2()
    fig_2_3()
    fig_2_4()
    fig_2_5()
    fig_3_1()
    fig_3_2()
    fig_3_3()
    fig_4_1()
    fig_5_1()
    fig_6_1()
    fig_6_2()
    fig_7_1()
    fig_8_1()
    fig_9_1()
    fig_10_1()
    print(f"generated={len(list(OUT.glob('*.png')))} out={OUT}")


if __name__ == "__main__":
    main()
