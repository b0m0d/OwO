# -*- coding: utf-8 -*-
"""Consistency sweep 2: team wording, competitive claim, risk list, safety 3rd class, funding, metric, citations."""
import copy
from docx import Document
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

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

def E(prefix, text):
    set_text(find(prefix), text)

def first_cells(t):
    return [c.text.strip() for c in t.rows[0].cells]

# ---- 竞品措辞软化 ----
E("按任务生命周期的六个环节",
  "按任务生命周期的六个环节（意图捕获、情境构建、受控执行、结果验证、返回原处、治理审计）比较，"
  "输入法产品在意图捕获与表达效率上领先，通用助手与工作台在受控执行与验证上更完整；"
  "在当前公开产品中，同时覆盖上述六个环节并以输入焦点贯穿任务生命周期的完整方案仍较少见。"
  "Cuttle 的设计目标正是补齐这条链路，并以跨应用适配与可靠性积累作为差异化基础。")

# ---- 团队表述去赛事文体 ----
for t in doc.tables:
    if first_cells(t)[:2] == ["成员", "角色与专业方向"]:
        for row in t.rows:
            if row.cells[0].text.strip() == "杨俊熙":
                set_cell(row.cells[2], "项目统筹、产品设计、架构协调与资源整合")
                print("  团队职责改写")
            if row.cells[0].text.strip() == "赵恒军":
                set_cell(row.cells[2], "研究方法、安全合规与阶段评审")
E("项目由学生团队主导研发",
  "项目由学生团队主导研发。产品面向高频 PC 知识工作者，团队围绕产品架构、智能体工程、"
  "用户研究与商业验证形成明确分工，每个里程碑保留任务单、版本记录、测试报告与用户证据。"
  "核心研发、产品决策与成果表达均由学生团队承担，指导教师负责研究方法、安全合规与阶段评审。")

# ---- 风险正文去掉“用户留存” ----
E("项目按影响程度对风险分级治理",
  "项目按影响程度对风险分级治理。输入法信任、隐私泄露与执行误操作列为一级风险，由发布门槛控制，"
  "不达标即冻结发布；输入延迟与平台厂商集成同类能力列为二级风险，"
  "以性能降级与差异化能力建设应对。风险状态每两周随产品评审更新，"
  "达到预警条件时触发暂停、降级或转向决策，并在下一版本以回归测试固化整改结果。")

# ---- 生产事故指标落点 ----
E("安全指标分为设计红线、测试指标与生产事故指标三类",
  "安全指标分为设计红线、测试指标与生产事故指标三类。设计红线属于发布约束，"
  "高风险动作未经授权执行次数与明确敏感字段访问次数必须为 0。测试指标用于评估未知与边界场景下的泛化能力，"
  "包括伪装敏感字段漏检率不高于 1%、可逆任务恢复成功率不低于 90%、审批影响范围判断正确率不低于 95%。"
  "生产事故指标用于衡量上线后的实际运行情况，上线后的安全事件按事件等级、影响范围与处置时效单独记录，"
  "不与实验指标混合计算。")

# ---- 资金来源改为规划表述 ----
E("项目已有资源包括学校实验室与实验工位",
  "项目已有资源包括学校实验室与实验工位、团队成员个人设备、指导教师的方法与安全指导、"
  "开源模型与本地推理能力，这些不计入现金需求。项目未来两年预计新增资金需求 160 万元，"
  "其中第一年 50 万元、第二年 110 万元。资金来源以学校及竞赛创新基金、团队自筹与外部产业合作为主，"
  "具体比例随阶段融资落实情况调整。第一年资金用于把原型推进到可验证的试点闭环，并完成对照、消融与红队测试，"
  "其中产品与研发占 30%、评测与安全占 20%、市场与试点占 25%、模型与基础设施占 15%、知识产权与预备金占 10%；"
  "第二年资金用于场景复制、团队版交付、兼容矩阵扩展与支持运维。资金按决策门分两批释放，"
  "任一批未达标即暂停后续投入。保守情景下前两年累计亏损约 67 万元，基准情景约 103 万元，"
  "该额度可覆盖基准情景所需并保留安全边际。")

# ---- 表 2 首用意愿改为行为化表述 ----
for t in doc.tables:
    if first_cells(t)[:2] == ["验证方向", "验证方法"]:
        for row in t.rows:
            if row.cells[0].text.strip().startswith("纵向留存"):
                set_cell(row.cells[2], "纵向组四周留存率 40%；独立组首次使用后继续进入下一任务的比例达到预实验设定阈值")
                print("  表2 指标改写")
        break

doc.save(SRC)
print("sweep 2 done")