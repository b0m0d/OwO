# -*- coding: utf-8 -*-
"""Content refinements: centered table notes, ch.9 de-templated, ch.8 trim,
ch.1 precision, ch.6/7 wording, term variety."""
import copy, glob, os, re
from docx import Document
from docx.text.paragraph import Paragraph
from docx.oxml.ns import qn
from docx.shared import Pt
from docx.enum.text import WD_ALIGN_PARAGRAPH

SRC = [x for x in glob.glob(os.path.join(r"T:\创新创业\OwO-master\docs", "Cuttle*v8.docx"))
       if "预览" not in x][0]
doc = Document(SRC)

def ptxt(p):
    return "".join(t.text or "" for t in p._element.iter(qn('w:t')))

def set_text(p, text):
    runs = p.runs
    if not runs:
        p.add_run(text); return
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

def find(prefix):
    for p in doc.paragraphs:
        if ptxt(p).strip().startswith(prefix):
            return p
    raise KeyError(prefix)

def E(prefix, text):
    set_text(find(prefix), text)

# ---------- 1) 表注居中、字号 9pt、灰色 ----------
n_note = 0
for p in doc.paragraphs:
    t = p.text.strip()
    if t.startswith("注："):
        p.alignment = WD_ALIGN_PARAGRAPH.CENTER
        p.paragraph_format.first_line_indent = Pt(0)
        p.paragraph_format.space_before = Pt(4)
        p.paragraph_format.space_after = Pt(8)
        for r in p.runs:
            r.font.size = Pt(9)
            r.font.name = "Microsoft YaHei"
        n_note += 1
print("centered table notes:", n_note)

# ---------- 2) 第九章：打散五阶段工整句式 ----------
E("项目按需求发现、产品试制、实验论证、市场验证与成果传播五个环节推进",
  "成员能力在真实的研发与验证过程中形成。项目启动阶段完成用户调研与需求分析，"
  "训练的是把模糊需求转化为可检验问题的能力；原型阶段完成系统设计、交互设计与权限方案，"
  "团队由此建立起对安全边界与工程约束的判断。"
  "进入实验与评测阶段后，成员独立完成实验设计、数据分析、对照消融与红队故障注入，"
  "并输出可复现的评测报告；这一环节的产出直接进入产品迭代，而不是停留在文档层面。"
  "在市场与成果转化方面，成员通过场景试点、报价谈判与合作复盘了解真实商业约束，"
  "并完成软件著作权、专利交底与论文写作。"
  "全部成果以任务、代码、文档、测试与运营记录归档，贡献归属与数据授权同步明确。")

# ---------- 3) 第八章：删掉两句重复总结 ----------
E("上述实现构成团队承担本项目核心研发的工程基础",
  "上述实现是团队承担本项目核心研发的工程基础，后续研发重点集中在输入入口、意图胶囊、"
  "返回原处、自治升级阈值与真实用户验证五个方向。")

# ---------- 4) 第一章：措辞收紧 ----------
E("第三，输入入口具备跨应用一致性",
  "第三，输入入口具备跨应用一致性。不同软件的界面各不相同，而大量桌面生产力任务最终都需要"
  "通过输入控件表达、修改或提交信息，因此输入焦点是少数天然跨应用、可长期复用的统一入口；"
  "每增加一个应用，需要适配的是语境与返回方式，交互范式本身保持稳定。")
E("现有桌面 Agent 在情境获取上存在三个结构性缺陷",
  "现有桌面 Agent 在情境获取上通常面临三类问题：每次请求都要重新交代背景，因为工作台不在任务现场；"
  "情境读取没有边界，因为系统不知道读取是为了哪一件事；任务结束后上下文残留，因为没有失效条件。"
  "IC 用 Origin、Context、Freshness 三个字段分别对应解决，并把来源与权限边界写成可校验字段。")
E("Cuttle 不以基础模型能力作为主要竞争点",
  "Cuttle 不以基础模型能力作为主要竞争点，而将重点放在意图承接、权限控制和任务交付。"
  "项目在模型与桌面应用之间构建一层可控、可验证、可替换的意图基础设施，"
  "模型能力提升可直接扩大可调度的执行能力，而情境范围、权限边界与最终结果始终由用户控制。"
  "产品需要建立与系统可靠性相匹配的用户信任：低风险任务可由用户主动授权；"
  "对于高风险任务，用户能够理解影响范围、审阅执行计划并及时接管。"
  "自动化信任研究表明，信任的设计目标应与系统可靠性对齐，而非单纯追求提高。[25][26]")

# ---------- 5) 第六章：去"补齐链路"，改小标题 ----------
E("按任务生命周期的六个环节",
  "按任务生命周期的六个环节（意图捕获、情境构建、受控执行、结果验证、返回原处、治理审计）比较，"
  "输入法产品在意图捕获与表达效率上领先，通用助手与工作台在受控执行与验证上更完整；"
  "在当前公开产品中，同时覆盖这六个环节并以输入焦点贯穿任务生命周期的完整方案仍较少见。"
  "Cuttle 因此将输入焦点、情境构建、受控执行、结果验证和原处返回作为统一产品链路，"
  "并以跨应用适配与可靠性积累作为差异化基础。")
for p in doc.paragraphs:
    if ptxt(p).strip().startswith("生态与信任：随着应用适配数量"):
        set_text(p, "适配与实施积累：随着应用适配数量和真实任务量增加，兼容性测试、安全规则与实施经验可持续积累。")
        print("  6.4 小标题改为「适配与实施积累」")

# ---------- 6) 第七章：续费动机改为商业判断 ----------
E("续费动机来自使用过程中积累的资产",
  "用户持续使用后会积累已验证工作流、技能配置、审计记录与应用适配习惯，这些迁移成本构成续费基础。"
  "来源可追溯与审批记录因此写入产品结构，而不只作为合规要求。")

# ---------- 7) 术语区分：证据 -> 按语义分开 ----------
REPL = [
 ("用户证据", "用户记录"),
 ("合同证据", "合同材料"),
 ("验证证据与恢复路径", "验收证据与恢复路径"),
 ("验证证据", "验收记录"),
 ("证据卡", "来源卡"),
 ("证据矩阵", "来源对照表"),
]
n = 0
def walk_paras():
    for p in doc.paragraphs:
        yield p
    for t in doc.tables:
        for r in t.rows:
            for c in r.cells:
                for p in c.paragraphs:
                    yield p
for p in walk_paras():
    raw = ptxt(p)
    new = raw
    for a, b in REPL:
        if a in new:
            new = new.replace(a, b)
    if new != raw:
        set_text(p, new)
        n += 1
print("term-variety edits:", n)

doc.save(SRC)
print("content refinements saved")
