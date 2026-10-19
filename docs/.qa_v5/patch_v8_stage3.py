# -*- coding: utf-8 -*-
"""v8 stage 3 : chapters 7-12, references rebuild, grading of engineering-capability section."""
import copy
from docx import Document
from docx.text.paragraph import Paragraph

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
edits = 0

def set_text(p, text):
    runs = p.runs
    if not runs:
        p.add_run(text); return p
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""
    return p

def all_p(prefix, style=None):
    return [p for p in doc.paragraphs
            if p.text.strip().startswith(prefix) and (style is None or p.style.name == style)]

def find_p(prefix, style=None, idx=0):
    hits = all_p(prefix, style)
    if not hits:
        raise KeyError(prefix)
    return hits[idx] if len(hits) > idx else hits[0]

def set_cell(cell, text):
    ps = cell.paragraphs
    set_text(ps[0], text)
    for extra in ps[1:]:
        extra._element.getparent().remove(extra._element)

def set_row(table, ri, values):
    for ci, v in enumerate(values):
        set_cell(table.rows[ri].cells[ci], v)

def E(prefix, text, style=None, idx=0):
    global edits
    p = find_p(prefix, idx=idx)
    if style:
        p.style = doc.styles[style]
    set_text(p, text)
    edits += 1
    return p

def clone_after(ref_p, text, style='Normal'):
    el = copy.deepcopy(ref_p._element)
    ref_p._element.addnext(el)
    np = Paragraph(el, ref_p._parent)
    for r in list(np.runs)[1:]:
        r._element.getparent().remove(r._element)
    np.style = doc.styles[style]
    set_text(np, text)
    return np

# ---------------- 第七章 ----------------
E("Cuttle 前两年只做两件事",
  "Cuttle 的前两年只做两件事：个人专业版订阅与团队许可。免费版承担体验与传播，只提供低风险表达与少量任务；"
  "个人专业版面向高频知识工作者，是收入的主要来源；团队版面向需要统一策略、审计与共享能力的组织，"
  "作为第二种收入方式。私有部署仅在出现明确的数据合规采购需求时启动。"
  "技能生态属于远期方向，前两年不设为独立收入来源，也不预设交易分成比例，避免在需求尚未验证时用商业模型填空。")

E("表 13 产品版本与收入结构", "表 13 产品版本、定价与验证状态")
t = doc.tables[14]
set_row(t, 0, ["产品", "目标用户", "核心权益", "规划定价", "验证状态与收入方式"])
set_row(t, 1, ["个人免费版", "轻度用户与新用户",
               "基础表达、本地情境、少量任务", "免费",
               "获客与验证；以试用转化率与模型成本上限控制"])
set_row(t, 2, ["个人专业版", "运营、项目、咨询、研究、行政、开发等高频用户",
               "高额度任务、多模型、技能与恢复", "29 元每月或 240 元每年",
               "主收入方式；年费价位须经 20 份定价访谈与真实付费验证"])
set_row(t, 3, ["团队版", "需要统一策略与审计的团队与机构",
               "成员管理、统一策略、审计、共享技能与技术支持", "按席位计价，4 万元每年起",
               "第二收入方式；须取得至少 3 份试点意向与首次报价反馈"])
set_row(t, 4, ["私有部署版", "有数据合规采购需求的机构（后续）",
               "内网模型、定制策略、运维支持", "按规模与适配范围报价，20 万元每项目起",
               "后续模式，按验收确认收入"])
# 删除“技能生态”行
tbl = doc.tables[14]
row = tbl.rows[5]
row._element.getparent().remove(row._element)

E("第一阶段选择三类可公开、可验收的校园任务",
  "第一阶段只在团队能够直接进入的场景中形成样板，选择三类可公开、可验收的任务："
  "跨来源资料取数与核验、报告与材料交付、代码修复与测试。每个试点围绕任务完成率、时间节省、"
  "安全理解与连续使用记录证据。第二阶段以开源 SDK、技能模板与开发者案例扩大高价值用户。"
  "第三阶段依据真实采购需求提供团队许可与私有部署，合同范围绑定可交付能力、数据边界与支持成本。")

E("团队扩散：个人用户将稳定流程带入课程组、实验室和学生团队",
  "团队扩散：个人用户将验证过的流程带入所在团队与机构，形成团队许可机会。")

# ---------------- 8.2 工程能力证据 ----------------
E("8.2 前期技术预研基础", "8.2 前期工程基础与团队能力证据", style="Heading 2")
E("团队前期围绕 Agent Runtime",
  "本项目的技术跨度覆盖输入系统与无障碍接口、UI Automation、权限沙箱、动作回滚与审计、失败恢复以及用户研究，"
  "因此需要说明团队具备完成这些工作的工程能力。团队此前已在相关方向上完成了可运行的自研实现："
  "一是 Agent Runtime 与任务调度，以 DAG 表达依赖并支持并发执行与失败恢复；"
  "二是工具权限与审批体系，策略默认拒绝，审批与执行相互独立，所有动作留痕可审计；"
  "三是桌面感知与结构化动作，通过文本服务框架与 UI Automation 读取控件树并完成输入、点击与选区操作；"
  "四是审计、回滚与多 Worker 协作，任务失败后可恢复到最近检查点并保留完整执行记录。")
pr = find_p("本项目的技术跨度覆盖输入系统与无障碍接口")
clone_after(pr,
  "上述实现是团队工程能力的证据，不是本项目已经完成的产品，也不作为本项目的创新结论。"
  "项目主张的创新是意图胶囊与连续自治闭环两项机制；"
  "而运行时、权限、审计、回滚与多 Worker 等基础层恰好是这两项机制能够被真实执行而非仅停留在概念的前提。"
  "后续研发重点集中在输入入口、意图胶囊、返回原处、自治升级阈值与真实用户验证五件事上。")
edits += 1

# ---------------- 第九章 ----------------
E("项目采用学生主导、教师指导、用户共同验证的协作方式",
  "项目采用成员主导、教师指导、用户共同验证的协作方式。团队分工覆盖产品与架构、智能体工程与评测、"
  "用户研究与商业验证，每个里程碑都保留任务单、版本记录、测试报告与用户证据。"
  "指导教师负责方法、伦理、安全与阶段评审，不代替成员完成核心研发与答辩。")

E("表 17 团队分工与贡献证据", "表 17 团队分工、职责与过程证据")
t = doc.tables[18]
set_row(t, 2, ["张子豪", "智能体工程与评测", "Agent Runtime、工具接入、评测与故障恢复",
               "代码提交、测试记录、评测报告"])
set_row(t, 3, ["吴栩彪", "用户研究与运营", "用户研究、交互验证、内容与商业试点",
               "访谈记录、原型反馈、试点材料"])
# 插入开发成员 谈世钊
from docx.oxml.ns import qn
tbl = doc.tables[18]
new_tr = copy.deepcopy(tbl.rows[3]._element)
tbl.rows[3]._element.addnext(new_tr)
from docx.table import _Row
row_new = _Row(new_tr, tbl)
for ci, v in enumerate(["谈世钊", "智能体开发", "执行链路开发、桌面接口适配、集成与回归测试",
                        "代码提交、接口文档、回归测试记录"]):
    set_cell(row_new.cells[ci], v)
edits += 1

E("表 18 项目驱动的人才培养路径", "表 18 项目驱动的能力成长路径")
E("9.2 学生能力成长路径", "9.2 成员能力成长路径", style="Heading 2")

# ---------------- 第十章 ----------------
E("本章用于检验商业模型能否形成可持续经营",
  "本章用于检验商业模型能否形成可持续经营，不代表项目已经取得收入、融资或用户规模。"
  "测算采用统一口径：个人专业版按年末在付用户数乘年费标价 240 元确认收入；"
  "团队许可按合同金额在服务期内确认，首年按半年计入；私有部署按验收确认收入。"
  "第二年起按上年规模的 60% 计提续订，因此“付费个人”在第二年与第三年拆分为续订与新增两部分，"
  "避免把年末存量与全年新增混算。全部单价与转化率都属于待验证假设，验证方式与门槛见表 14 与 10.2 节。")

p101 = find_p("本章用于检验商业模型能否形成可持续经营")
clone_after(p101,
  "价格假设的验证优先于规模预测。前三个月完成三项最低限度的价格证据：一是 20 份定价访谈，"
  "对象为高频 PC 知识工作者，覆盖运营、项目、咨询、研究、行政与开发岗位，用于确认年费 240 元与席位计价 4 万元的可接受区间；"
  "二是 3 份试点意向，用于确认团队许可的采购流程与预算归属；三是首份报价反馈，"
  "记录真实客户对报价范围的异议与调整意见。上述证据在取得后用于修正定价与转化率假设，"
  "并在财务表中标注来源；在证据取得之前，本章全部数字按情景假设处理。")
edits += 1

E("表 19 三年经营测算（统一口径）", "表 19 三年经营测算（三情景，统一口径）")
t = doc.tables[20]
set_row(t, 1, ["付费个人（年末在付）", "600", "4000", "15000", "年费 240 元，续费率 60%"])
set_row(t, 2, ["团队许可（个）", "6", "24", "50", "平均 4 万元每客户，首年按半年确认"])
set_row(t, 3, ["私有部署（个）", "1", "4", "10", "平均 18 万元每项目，按验收确认"])
set_row(t, 4, ["营业收入 保守", "14.4 万元", "146 万元", "372 万元", "仅个人订阅，不计团队与部署"])
set_row(t, 5, ["营业收入 基准", "60.6 万元", "272 万元", "836 万元", "个人订阅、团队许可与私有部署合计"])
set_row(t, 6, ["营业收入 进取", "104 万元", "482 万元", "1464 万元", "个人订阅、团队许可与私有部署合计"])
set_row(t, 7, ["经营成本 保守／基准／进取", "90 / 110 / 120 万元", "180 / 282 / 486 万元",
               "390 / 636 / 1320 万元", "研发、模型、市场与交付"])
set_row(t, 8, ["经营结果 保守", "负 75.6 万元", "负 34 万元", "负 18 万元", "第三年仍未转正"])
set_row(t, 9, ["经营结果 基准", "负 49.4 万元", "负 10 万元", "200 万元", "第三年进入盈亏平衡上方"])
set_row(t, 10, ["经营结果 进取", "负 16 万元", "负 4 万元", "144 万元", "第二年接近盈亏平衡"])
# 删除多余行（原 15 行 -> 11 行）
for _ in range(len(t.rows) - 11):
    r = t.rows[len(t.rows) - 1]
    r._element.getparent().remove(r._element)
edits += 1

E("表 20 分层资源需求与用途", "表 20 分阶段资源需求与用途")

# ---------------- 第十二章 ----------------
E("Cuttle 计划把资料搬运、格式返工和重复提示交给受控工具",
  "Cuttle 计划把资料搬运、格式返工与重复提示交给受控工具，使用户把时间用于理解、判断与创造。"
  "通过来源、证据、修改与验收记录，项目鼓励用户对 AI 结果保持审阅责任，避免把生成内容直接当作结论。"
  "对于项目与科研材料，版本化成果有助于团队复盘过程、发现错误并积累可复用的方法。")

E("Cuttle 的长期目标是成为个人电脑上的意图层",
  "Cuttle 的长期目标是成为个人电脑上的意图层。用户在任何应用中表达需求时，系统以最小必要情境理解任务，"
  "以适当的工具或智能体完成工作，以清晰证据说明结果，并把成果送回用户原本工作的地方。"
  "项目的衡量标准不是调用了多少模型或 Agent，而是用户是否减少了无意义的搬运、"
  "任务是否更可靠、权限是否始终掌握在用户手中。")

E("说明  本计划书以一条核心命题为主线",
  "说明：本计划书以一条核心命题为主线——AI 的执行能力持续增强，而执行所依赖的意图、情境与控制权仍散落在手工环节。"
  "全部内容收敛为两个核心创新：意图胶囊 IC，以及从输入意图到受控行动的连续自治与返回原处闭环；"
  "版本化成果契约与返回原处、策略、验证、恢复体系作为支撑其落地的工程体系；"
  "自适应组队定位为执行效率优化策略，跨设备能力与技能生态属于远期扩展。")

# ---------------- 参考资料清理 ----------------
POLLUTED = "[12] Xiaomi MiMo Desktop["
bad = [p for p in doc.paragraphs if p.text.strip().startswith(POLLUTED)]
print("polluted refs found:", len(bad))
for p in bad:
    p._element.getparent().remove(p._element)
edits += len(bad)

doc.save(SRC)
print("stage3 ok edits=", edits, "paras", len(doc.paragraphs), "tables", len(doc.tables))