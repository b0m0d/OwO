# -*- coding: utf-8 -*-
"""v7 -> v8 : content-only revision.
Bugs fixed: duplicate [12] reference pollution, empty 1.3/1.4 headings,
mis-styled Heading-2 body paragraphs, truncated sentences.
Content: 2 core innovations, IC term unification, knowledge-worker positioning,
input-focus justification, pricing evidence, team capability evidence.
"""
import io, sys, copy
from docx import Document
from docx.text.paragraph import Paragraph
from docx.oxml.ns import qn
from docx.shared import Pt

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"

doc = Document(SRC)

# ---------- helpers ----------
def set_text(p, text):
    runs = p.runs
    if not runs:
        p.add_run(text)
        return p
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""
    return p

def find_p(prefix, style=None):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix) and (style is None or p.style.name == style):
            return p
    raise KeyError(prefix)

def set_cell(cell, text):
    """Write text into a cell, keeping the first paragraph's formatting."""
    ps = cell.paragraphs
    set_text(ps[0], text)
    for extra in ps[1:]:
        extra._element.getparent().remove(extra._element)
    return cell

def set_row(table, ri, values):
    for ci, v in enumerate(values):
        set_cell(table.rows[ri].cells[ci], v)

def set_style(p, style):
    p.style = doc.styles[style]
    return p

def clone_body_after(ref_p, text, style='Normal'):
    """Insert a new paragraph after ref_p, cloning a known body paragraph's rPr."""
    new_el = copy.deepcopy(ref_p._element)
    ref_p._element.addnext(new_el)
    np = Paragraph(new_el, ref_p._parent)
    # strip extra runs
    for r in list(np.runs)[1:]:
        r._element.getparent().remove(r._element)
    set_style(np, style)
    set_text(np, text)
    return np

def para_after_tbl(tbl, model_p, text, style='Normal'):
    el = copy.deepcopy(model_p._element)
    tbl._element.addnext(el)
    p = Paragraph(el, tbl._parent)
    for r in list(p.runs)[1:]:
        r._element.getparent().remove(r._element)
    set_style(p, style)
    set_text(p, text)
    return p

BODY = None
for p in doc.paragraphs:
    if p.style.name == 'Normal' and len(p.text) > 100 and p.runs:
        BODY = p
        break

edits = 0

def E(prefix, text, style=None):
    global edits
    p = find_p(prefix)
    if style:
        set_style(p, style)
    set_text(p, text)
    edits += 1
    return p

# =====================================================================
# 1. 摘要
# =====================================================================
E("Cuttle 拟构建一套情境原生智能输入系统",
  "Cuttle 拟构建一套情境原生智能输入系统。用户在 Word、浏览器、聊天软件、开发工具等任意输入焦点表达需求时，"
  "系统只读取完成当前任务所需的最小情境，生成一个可校验、可失效、可追溯的意图胶囊（Intent Capsule, IC），"
  "再将请求路由为直接表达、工具调用、单智能体任务或多智能体协作。执行过程保留计划、审批、证据、差异与验收结果，"
  "并把可验证成果返回需求产生的原工作位置。")

E("项目的核心命题只有一句",
  "项目的核心命题是：AI 的执行能力正在快速增强，而执行所依赖的意图、情境与控制权，仍散落在用户手工搬运的环节里。"
  "现有 AI 助手多以独立对话框或工作台承接任务，用户需离开当前工作、重新交代背景并搬运结果；"
  "现有 AI 输入法已具备改写、续写、翻译与场景化表达，但任务通常停留在文本生成。"
  "Cuttle 选择这两者之间尚未贯通的位置：把输入焦点作为意图的原生起点，"
  "让意图进入系统后的每一次升级都有边界、有证据、可撤销。")

E("围绕这一命题，项目收敛出三个核心创新",
  "围绕这一命题，项目收敛出两个核心创新。"
  "核心创新一是意图胶囊（IC）：面向输入时刻、可校验、可失效、可追溯的最小情境结构，"
  "它把一次输入从一段自然语言变成有作用域、有生命周期、可供路由与执行共用的中间表示。"
  "核心创新二是从输入意图到受控行动的连续自治闭环：由复杂度、可逆性与数据范围等特征判定自治程度、协作形态与执行位置，"
  "并以 Return to Origin 把成果返回需求产生的原处。版本化成果契约、返回原处机制与策略、验证、恢复体系作为支撑上述机制落地的工程体系；"
  "自适应多智能体组队属于执行效率优化策略，不作为独立创新点主张；跨设备能力与技能生态为远期扩展，不是项目成立的必要条件。")

E("项目按照创意组从问题调研",
  "项目从问题调研、机制验证、产品试制到市场验证逐级推进。首个验证场景只保留一条主链（跨来源资料取数与口径核验到报告交付）"
  "与一条副链（代码问题到可验证差异）；关键验收指标覆盖情境识别、自治升级恰当性、情境过度读取与过度自治、任务成功、"
  "敏感操作拦截、失败恢复、时间节省与真实付费意愿。所有市场规模、用户增长与财务数据均为规划情景，"
  "并在每一阶段由真实任务日志、用户研究与合同证据校准。")

# 表 1
t = doc.tables[2]
set_row(t, 1, ["真实问题", "AI 入口、情境、行动与结果彼此割裂", "访谈、任务日记、跨应用流程观察"])
set_row(t, 2, ["项目方案", "在输入时刻理解情境并升级为受控行动", "交互原型、演示闭环、任务日志"])
set_row(t, 3, ["技术创新", "意图胶囊 IC；连续自治与返回原处闭环", "三组对照实验、消融实验、过度读取与过度自治测试"])
set_row(t, 4, ["产业路径", "知识工作场景切入、团队许可、私有部署", "试点、留存、付费验证、合作材料"])

# =====================================================================
# 2. 第一章
# =====================================================================
E("Cuttle 将输入时刻定义为 AI 协作的起点",
  "Cuttle 把输入时刻定义为 AI 协作的原生起点。用户无需先决定打开哪个模型、哪个 Agent 或哪个工作台，"
  "只需在当前输入焦点表达意图。系统先识别所处应用、当前对象与任务边界并形成意图胶囊，"
  "再判定这次请求应停留在文本层，还是升级为工具调用、智能体执行、多执行者协作，乃至跨设备任务。")

E("该思路把产品竞争从模型问答能力",
  "这一思路把产品竞争从模型问答能力转向意图承接与任务交付能力。项目不与基础模型厂商争夺模型能力本身，"
  "而是在模型与桌面应用之间构建一层可控、可验证、可替换的意图基础设施。模型能力越强，Cuttle 可调度的执行能力越丰富，"
  "而用户始终保有对情境范围、权限边界与最终结果的控制。判断项目是否成立的最终标准，"
  "不是调用了多少模型或 Agent，而是用户对系统的信任是否随使用加深。")

# 1.3 重构：标题 + 正文层级
E("1.3 输入焦点为什么是 AI 意图入口", "1.3 为什么必须做输入法", style="Heading 2")
E("在讨论产品之前，需要先回答一个更基础的问题",
  "1.3 为什么必须做输入法：把入口放在输入时刻的技术理由", style="Heading 2")
# 正文段落（原为 Heading 2）
for pref in ("第一，输入是显式意图", "第二，输入焦点天然带有 Origin",
             "第三，输入入口具有跨应用一致性", "第四，输入法不能承担无限权限"):
    E(pref, find_p(pref).text.strip(), style="Normal")

p4 = find_p("第四，输入法不能承担无限权限")
new1 = clone_body_after(p4,
  "上述四条说明了输入时刻在信号质量上的优势，但尚未回答一个更关键的质疑：")
new2 = clone_body_after(new1,
  "如果只需要一个全局快捷键加悬浮窗口，是否同样可以完成这些任务。项目认为存在实质差别，并把它作为核心命题必须被验证的部分。"
  "全局快捷入口可以做到随时唤起，但它拿不到“用户此刻在写什么、改什么、以什么身份写”这一层信息，"
  "因此每次会话仍需要用户手工交代背景，也只擅长输出一段文本，缺少把成果写回原对象的绑定关系。"
  "输入焦点的价值不在于“离键盘近”，而在于它同时提供了显式意图与原位置这两个结构化信号，"
  "使任务生命周期可以从意图产生处开始、并在同一位置结束。")
new3 = clone_body_after(new2,
  "因此项目把这条命题做成可证伪的实验，而不是设计主张：对照实验设置三组入口，"
  "独立 AI 工作台、全局快捷键 Agent、输入焦点原生的 Cuttle，在相同模型、工具与任务材料下比较"
  "上下文重复说明次数、来源绑定准确率、发起步骤数与结果返回成本。"
  "若三组之间没有可测差异，项目的入口选择即不成立，届时应转向通用 Agent 工作台路线。")
E("由此可以明确项目边界",
  "由此可以明确项目边界：Cuttle 的研究对象不是让输入法变得更会写字，而是研究用户意图如何从输入时刻进入系统，"
  "并在保留控制权的前提下安全、连续地升级为可验证行动。输入焦点不是项目的终点，"
  "而是意图质量与控制权兼顾的切入口；本计划书后续章节都服务于这一个命题。",
  style="Normal")

# 1.4 项目定义与价值主张
E("1.4 项目定义与价值主张", "1.4 项目定义与价值主张", style="Heading 2")
p143 = find_p("由此可以明确项目边界")
defn = clone_body_after(p143,
  "按上述边界，Cuttle 的定位可以概括为一句话：个人电脑上的意图层。它不替代办公软件、浏览器与开发工具，"
  "也不替代基础模型与 Agent 框架，而是在用户表达需求的那一刻承接意图，并以受控执行把成果送回原处。"
  "项目的价值主张与首批验证口径见表 2，全部指标由真实任务日志与用户研究记录支撑。")

t = doc.tables[3]
set_row(t, 0, ["主要任务", "现状与缺口", "Cuttle 的价值", "验证指标"])
set_row(t, 1, ["跨来源资料取数与口径核验（主链）",
               "数据分散在网页、PDF、表格、聊天记录与文档之间，来源与口径难以复核",
               "在目标文档中完成取数、核验、写作与来源标注，成果返回原处",
               "完成时间、返工次数、来源可追溯比例、四周留存"])
set_row(t, 2, ["代码问题定位与修复（副链）",
               "任务在 IDE、浏览器与终端之间切换，失败恢复成本高",
               "把请求升级为受控代码任务，返回可审阅差异、测试结果与回滚点",
               "任务成功率、回滚率、人工接管率"])
set_row(t, 3, ["报告与材料交付（扩展）",
               "格式规范、版本管理与审核链条割裂",
               "以版本化成果契约记录证据、修改与验收条件",
               "验收一次通过率、审核时间、证据覆盖率"])
set_row(t, 4, ["团队与机构部署（扩展）",
               "模型与工具分散，缺少权限治理与审计",
               "提供模型可替换、审批与审计统一的部署形态",
               "部署时间、越权拦截率、维护成本"])

doc.save(SRC)
print("stage1 ok edits=", edits)