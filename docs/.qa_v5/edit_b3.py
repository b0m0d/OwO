# -*- coding: utf-8 -*-
"""Editorial pass B2: merge overlapping tables and compress the remaining ones."""
import copy
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement
from docx.table import _Row
from docx.text.paragraph import Paragraph

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, text):
    runs = p.runs
    if not runs:
        p.add_run(text); return
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

def set_cell(cell, text):
    ps = cell.paragraphs
    set_text(ps[0], text)
    for extra in ps[1:]:
        extra._element.getparent().remove(extra._element)

def set_row(t, ri, vals):
    for ci, v in enumerate(vals):
        set_cell(t.rows[ri].cells[ci], v)

def first_cells(t):
    return [c.text.strip() for c in t.rows[0].cells]

def get_table(sig, ncols=None):
    for t in doc.tables:
        fc = first_cells(t)
        if fc[:len(sig)] == sig and (ncols is None or len(fc) == ncols):
            return t
    raise KeyError(str(sig))

def add_rows(t, n):
    src = t.rows[-1]._element
    for _ in range(n):
        new = copy.deepcopy(src)
        t.rows[-1]._element.addnext(new)
    return t

def del_row(t, ri):
    tr = t.rows[ri]._element
    tr.getparent().remove(tr)

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

def E(prefix, text):
    set_text(find(prefix), text)

# ============ 1) 摘要证据链表：5 行 -> 4 行 ============
t = get_table(["项目要素", "核心内容"], 3)
set_row(t, 0, ["项目要素", "核心内容", "验证依据"])
set_row(t, 1, ["真实问题", "AI 入口、情境、行动与结果彼此割裂", "访谈、任务日记、跨应用流程观察"])
set_row(t, 2, ["项目方案", "在输入时刻理解情境并升级为受控行动", "交互原型、演示闭环、任务日志"])
set_row(t, 3, ["技术创新", "意图胶囊 IC 与受控自治升级闭环", "三组入口对照、消融实验、情境过度读取测试"])
set_row(t, 4, ["产业路径", "知识工作场景切入、团队许可、私有部署", "试点、留存、付费验证与合作材料"])
del_row(t, 5)

# ============ 2) 2.3 预设验证标准 + 八月指标 -> 核心验证指标与决策门 ============
t4 = get_table(["假设", "验证方法"], 5)
E("表 4 预设验证标准与证伪条件（与三组对照实验一致）", "表 4 核心验证指标与决策门")
ROWS4 = [
 ["验证方向", "验证方式", "指标与门槛", "未达标时的调整"],
 ["入口机制优于独立工作台与全局快捷 Agent",
  "三组入口同任务对照，顺序平衡",
  "主指标：上下文重复说明次数降低 15% 以上；约束：Origin 绑定准确率不下降，"
  "发起步骤数与结果返回成本至少一项显著改善",
  "缩小入口范围，保留快捷指令"],
 ["意图胶囊减少重复说明并抑制多余读取",
  "对照任务计时与记录，情境标注集",
  "Context Recall 不低于 90%，情境过度读取率低于 5%，重复输入减少 30%",
  "减少自动感知，增强手动选区"],
 ["三维自治决策优于固定档位",
  "同任务计时与接管计数，标注集评测",
  "三维决策 Macro-F1 不低于 0.85，过度升级率低于 10%，高风险低估率低于 3%",
  "只保留高价值任务升级"],
 ["可见审批提高影响范围理解",
  "敏感任务可用性测试",
  "影响范围判断正确率不低于 95%，误批准率低于 3%",
  "增加解释与二次确认"],
 ["端到端任务可稳定完成",
  "固定任务套件与真实任务",
  "任务成功率不低于 85%，可逆任务恢复率不低于 90%，完成时间降低 25%",
  "收敛任务范围并加固恢复路径"],
 ["团队与机构客户愿意为治理能力付费",
  "试点与报价测试",
  "取得至少 3 份付费意向或采购流程",
  "转向个人订阅与开发者生态"],
 ["纵向留存与首用接受度",
  "产品分析与合作记录",
  "纵向组四周留存率 40%，独立组首用意愿达标",
  "修复新用户上手路径与首用价值"],
]
n_add = len(ROWS4) - len(t4.rows)
if n_add > 0:
    add_rows(t4, n_add)
for ri, vals in enumerate(ROWS4):
    set_row(t4, ri, vals)
while len(t4.rows) > len(ROWS4):
    del_row(t4, len(t4.rows) - 1)

# 删除旧的 表 16 十二个月核心指标
for t in list(doc.tables):
    fc = first_cells(t)
    if fc[:2] == ["维度", "指标"] and len(fc) == 4:
        t._element.getparent().remove(t._element)
        print("  merged away: 表 16 十二个月核心指标")
        break
try:
    p = find("表 16 项目十二个月核心指标")
    p._element.getparent().remove(p._element)
except KeyError:
    pass

# ============ 3) 6.1 首年验证容量：压缩为三行 ============
t = get_table(["测算层级", "测算对象", "测算过程与依据"], 4)
set_row(t, 0, ["口径", "测算对象", "测算依据", "结果与用途"])
set_row(t, 1, ["首年可交付容量", "团队可稳定服务并完整观察的用户数",
               "首批可触达知识工作者样本池约 3000 人，其中高频 PC 资料处理任务约占 40%，"
               "原型试用意愿约 50%，可稳定纳入试点观察约三分之二",
               "约 200 人；用于配置试点人力，并约束第一年付费用户规模与实验样本"])
set_row(t, 2, ["验证与实验容量", "支撑对照实验与纵向观察的样本规模",
               "纵向核心组 15 至 25 人全程跟踪，独立验证组另招 20 至 30 人",
               "50 至 80 人形成深度数据，其中 35 至 55 人进入正式实验"])
set_row(t, 3, ["中长期市场空间", "产品扩展潜力的参考量级",
               "我国生成式人工智能用户规模 6.02 亿，软件业务收入 15.48 万亿元等公开数据",
               "用于判断长期天花板，随试点转化率、留存率与复制效率持续校准"])

# ============ 4) 6.2 竞品表：聚焦四类核心产品与六维覆盖 ============
t = get_table(["类别", "代表产品", "强项"], 5)
set_row(t, 0, ["类别", "代表产品", "强项", "当前主要交互形态与价值重心", "任务生命周期覆盖"])
set_row(t, 1, ["AI 输入法", "搜狗、讯飞", "高频入口、补全改写、语音",
               "价值集中于输入与表达效率", "意图捕获强，情境构建与成果返回弱"])
set_row(t, 2, ["全局语音输入", "Wispr Flow", "跨应用输入、情境化表达",
               "以跨应用转写与风格适配为主", "意图捕获与情境构建强，受控执行与治理弱"])
set_row(t, 3, ["通用 AI 助手", "ChatGPT、Claude", "模型能力与知识服务",
               "以独立对话与工作空间为主", "受控执行与验证中，入口情境与返回弱"])
set_row(t, 4, ["Agent 工作台", "Codex、MiMo Desktop", "长任务、多 Agent、桌面执行",
               "以显式任务与项目工作区为入口", "执行与验证强，输入时刻与返回原处弱"])
set_row(t, 5, ["Cuttle", "本项目", "输入焦点原生、最小情境、受控执行与返回原处",
               "以输入时刻为原生入口", "六个环节组成一条系统链路"])
while len(t.rows) > 6:
    del_row(t, len(t.rows) - 1)

# ============ 5) 7.4 产品版本 + 单位经济 合并 ============
t = get_table(["收入来源", "规划定价与成本假设"], 5)
E("表 14 单位经济模型与验证状态", "表 9 产品版本、定价与单位经济")
ROWS_U = [
 ["产品版本", "目标用户", "规划定价", "成本与毛利假设", "验证状态"],
 ["个人免费版", "轻度用户与新用户", "免费", "模型成本由预算上限控制",
  "获客与验证；以试用转化率与成本上限约束"],
 ["个人专业版", "运营、项目、咨询、研究、行政、开发等高频知识工作者",
  "240 元每年", "模型与支持成本约 60 元每人，毛利率约 75%",
  "主收入方式；年费价位须经 20 份定价访谈与真实付费验证"],
 ["团队版", "需要统一策略与审计的团队与机构", "4 万元每团队每年起",
  "交付支持成本约 1.2 万元每团队，毛利率约 70%",
  "第二收入方式；须取得至少 3 份试点意向与首次报价反馈"],
 ["私有部署版", "有数据合规采购需求的机构", "18 万元每项目起",
  "实施与维护成本约 8 万元每项目，毛利率约 55%",
  "按验收确认收入；第二轮试点之后启动"],
 ["技能生态", "开发者与行业伙伴", "暂不定价", "—",
  "远期方向；前两年不设为独立收入来源"],
]
n_add = len(ROWS_U) - len(t.rows)
if n_add > 0:
    add_rows(t, n_add)
for ri, vals in enumerate(ROWS_U):
    set_row(t, ri, vals)
while len(t.rows) > len(ROWS_U):
    del_row(t, len(t.rows) - 1)

# ============ 6) 9.1 团队表 ============
t = get_table(["成员", "角色", "主要职责"], 4)
set_row(t, 0, ["成员", "角色与专业方向", "主要职责", "过程证据"])
set_row(t, 1, ["杨俊熙", "项目负责人", "项目定位、产品设计、架构统筹、赛事整合", "需求文档、设计决策、版本里程碑"])
set_row(t, 2, ["张子豪", "智能体工程与评测", "Agent Runtime、工具接入、评测与故障恢复", "代码提交、测试记录、评测报告"])
set_row(t, 3, ["吴栩彪", "用户研究与运营", "用户研究、交互验证、内容与商业试点", "访谈记录、原型反馈、试点材料"])
set_row(t, 4, ["谈世钊", "智能体开发", "执行链路开发、桌面接口适配、集成与回归测试", "代码提交、接口文档、回归测试记录"])
set_row(t, 5, ["赵恒军", "指导教师", "研究方法、安全边界、学术与赛事指导", "评审意见、指导记录、资源协调"])

doc.save(SRC)
print("pass B2 done. tables:", len(doc.tables), "paragraphs:", len(doc.paragraphs))