# -*- coding: utf-8 -*-
"""v7 补丁 3：补上未落盘的四张风险/指标/安全/公平性表。"""
import copy

import docx
from docx.oxml.ns import qn
from docx.table import Table
from docx.text.paragraph import Paragraph

DOC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v7.docx"
doc = docx.Document(DOC)
body = doc.element.body
TEMPLATE_P = doc.paragraphs[10]


def el_text(el):
    return ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip()


def find_p(sub):
    hits = [p for p in doc.paragraphs if sub in p.text]
    return hits[0] if hits else None


def set_text(p_obj, text):
    runs = p_obj.runs
    if runs:
        runs[0].text = text
        for r in runs[1:]:
            r.text = ""
    else:
        p_obj.add_run(text)


def para_after_el(el, text):
    new_el = copy.deepcopy(TEMPLATE_P._element)
    el.addnext(new_el)
    p = Paragraph(new_el, doc)
    set_text(p, text)
    return p


def tbl_el(prefix):
    for el in body.iterchildren():
        if el.tag.endswith('}tbl'):
            hdr = "".join(c.text for c in Table(el, doc).rows[0].cells)
            if hdr.startswith(prefix):
                return el
    raise KeyError(prefix)


def set_cell_text(cell, text):
    while len(cell._tc.findall(qn('w:p'))) > 1:
        cell._tc.remove(cell._tc.findall(qn('w:p'))[-1])
    p = cell.paragraphs[0]
    if p.runs:
        p.runs[0].text = text
        for r in p.runs[1:]:
            r.text = ""
    else:
        p.add_run(text)


def rebuild(header_prefix, rows):
    el = tbl_el(header_prefix)
    ncols = len(rows[0])
    trs = el.findall(qn('w:tr'))
    while len(trs) < len(rows):
        new_tr = copy.deepcopy(trs[-1])
        trs[-1].addnext(new_tr)
        trs = el.findall(qn('w:tr'))
    while len(trs) > len(rows):
        trs[-1].getparent().remove(trs[-1])
        trs = el.findall(qn('w:tr'))
    for tr in trs:
        tcs = tr.findall(qn('w:tc'))
        while len(tcs) < ncols:
            new_tc = copy.deepcopy(tcs[-1])
            tcs[-1].addnext(new_tc)
            tcs = tr.findall(qn('w:tc'))
        while len(tcs) > ncols:
            tcs[-1].getparent().remove(tcs[-1])
            tcs = tr.findall(qn('w:tc'))
    grid = el.find(qn('w:tblGrid'))
    for gc in grid.findall(qn('w:gridCol')):
        grid.remove(gc)
    per = int(round(9639 / ncols))
    for _ in range(ncols):
        grid.append(grid.makeelement(qn('w:gridCol'), {qn('w:w'): str(per)}))
    t = Table(el, doc)
    for ri, row in enumerate(rows):
        for ci, val in enumerate(row):
            set_cell_text(t.rows[ri].cells[ci], val)
        for tc in trs[ri].findall(qn('w:tc')):
            tcpr = tc.find(qn('w:tcPr'))
            if tcpr is None:
                continue
            tcw = tcpr.find(qn('w:tcW'))
            if tcw is not None:
                tcw.set(qn('w:w'), str(per))
                tcw.set(qn('w:type'), 'dxa')


# ---- A) 指标表：加入北极星指标与统计口径
rebuild("维度指标", [
    ["维度", "指标", "十二个月目标", "数据来源"],
    ["北极星", "原地闭环完成率：从原始输入位置发起、无需手工搬运上下文或切换工作台、结果正确并返回原处的任务比例",
     "作为首要指标报告 阈值由预实验确定", "任务日志与回填记录"],
    ["入口", "输入焦点识别率 Origin 绑定正确率", "识别率不低于 95%", "兼容矩阵测试集"],
    ["情境", "情境召回率 情境过度读取率 失效上下文误用率", "过度读取率低于 5% 召回率不低于 90%",
     "标注集与标注一致率"],
    ["决策", "三维决策 Macro-F1 过度升级率 高风险低估率",
     "Macro-F1 不低于 0.85 过度升级率低于 10% 高风险低估率低于 3%", "标注集与任务日志"],
    ["执行", "端到端任务成功率 组队实测净收益", "成功率不低于 85%", "固定任务套件与真实任务"],
    ["效率", "完成时间 重复输入次数 单任务模型成本", "时间降低 25% 提示减少 30%", "对照实验"],
    ["安全", "未授权高风险动作 禁止访问字段命中 恢复率",
     "未授权为 0 禁止访问命中为 0 恢复不低于 90%", "红队与故障注入"],
    ["市场", "独立验证组首用意愿 纵向组四周留存 试点数", "留存 40% 三个试点", "产品分析 合作记录"],
])
print("A 指标表 OK")
doc.save(DOC)

if not find_p("全部指标共用一套统计口径"):
    para_after_el(list(tbl_el("维度指标").iterchildren())[-1],
        "全部指标共用一套统计口径：以配对样本比较为主，报告效应量与置信区间，而不只报告均值差异；"
        "样本量在正式实验前依据预实验效应量做统计功效分析后确定；所有指标注明数据来源、标注规则与样本量。"
        "指标之间的解释关系是：原地闭环完成率是首要指标，其余指标用于解释它为什么高或低。")
    print("A 统计口径说明 OK")
doc.save(DOC)

# ---- B) 安全表：拆分禁止访问与过度读取
rebuild("安全目标指标", [
    ["安全目标", "指标", "阶段门槛", "不达标处理"],
    ["未经授权不执行", "高风险动作未授权执行次数", "必须为 0", "冻结发布并复盘"],
    ["禁止访问字段", "明确标识的密码框 支付确认 金融敏感字段访问率", "必须为 0", "扩大系统级屏蔽并回归测试"],
    ["情境过度读取", "普通工作情境中读取非必需数据的比例", "目标低于 5%", "收缩读取范围 复核意图绑定"],
    ["自治过度升级", "执行档位高于用户意图所需的比例", "目标低于 10%", "强制回退档位 增加确认"],
    ["审批可理解", "用户能正确判断影响范围", "正确率不低于 95%", "重写说明与交互"],
    ["失败可恢复", "可逆任务恢复成功率", "不低于 90%", "限制能力并补偿测试"],
    ["证据可追溯", "外部动作带来源与审计记录", "覆盖率 100%", "阻止交付"],
])
print("B 安全表 OK")
doc.save(DOC)

if not find_p("两项专有指标的判定规则必须可复现"):
    para_after_el(list(tbl_el("安全目标指标").iterchildren())[-1],
        "两项专有指标的判定规则必须可复现：依据预先文档化的“完成任务最小充分信息集”与“最小必要执行档位”，"
        "由两名不参与研发的独立标注者独立标注，计算标注者一致率，不一致样本由第三方裁决并记录理由；"
        "一致率达到预设水平后该指标才对外报告。")
    print("B 标注规则 OK")
doc.save(DOC)

# ---- C) 风险表：补 Cuttle 专属风险
rebuild("风险概率", [
    ["风险", "概率", "影响", "预警信号", "应对措施"],
    ["输入法信任风险", "高", "极高", "用户担心输入内容被读取 安装后关闭感知",
     "默认最小读取 可逐应用关闭 明示读取范围 独立审计"],
    ["输入延迟风险", "高", "高", "正常打字出现可感延迟 输入法被禁用",
     "快速路径与智能体路径分离 打字链路不调用模型 设延迟上限并降级"],
    ["提示注入与情境污染", "中", "极高", "网页或文档内嵌指令影响计划或授权范围",
     "情境内容与指令隔离 来源标记 计划人工可见 高风险动作二次确认"],
    ["结果回填错位", "中", "高", "成果落到错误文档 窗口或聊天对象",
     "Origin 绑定校验 回填前预览 对象版本与焦点二次确认"],
    ["平台厂商集成同类能力", "中", "高", "操作系统或头部输入法内置相似入口",
     "聚焦跨应用协议与可靠性 入口与模型保持可替换 深化团队治理能力"],
    ["应用兼容性", "高", "高", "崩溃 焦点丢失 延迟上升", "缩小应用范围 自动化兼容测试 降级为快捷入口"],
    ["情境误判与隐私", "中", "极高", "敏感字段被读取 用户纠正上升", "禁止访问字段硬屏蔽 最小读取 独立审计"],
    ["执行误操作", "中", "极高", "越权 外发 覆盖或不可逆动作", "最小权限 独立审批 预览 沙箱与回滚"],
    ["模型能力波动", "中", "高", "成功率下降 成本或时延异常", "模型替换 固定评测 预算上限与降级策略"],
    ["用户留存不足", "中", "高", "四周留存低 基准任务链覆盖不足", "聚焦两条基准任务链 改善首用与技能复用"],
    ["商业范围失控", "中", "高", "定制需求占用研发 交付亏损", "标准合同 边界报价 里程碑验收"],
    ["知识产权与依赖", "低", "高", "开源许可不清 数据授权缺失", "依赖清单 原创记录 授权与合规审查"],
    ["团队持续性", "中", "中", "核心模块单人掌握 里程碑延期", "双人复核 文档化 模块轮换与交接演练"],
])
print("C 风险表 OK（%d 行）" % len(Table(tbl_el('风险概率'), doc).rows))

# ---- D) 补齐实验设计两段说明
if not find_p("对照实验的公平性前提"):
    para_after_el(list(tbl_el("实验对照组").iterchildren())[-1],
        "对照实验的公平性前提必须写清：处理组与对照组使用同一模型与同一版本、相同的工具与权限范围、"
        "相同的任务材料与结果要求，唯一变量是“任务如何进入系统、上下文如何获得”。"
        "若对照组的工具权限被削减或模型版本更低，效率提升就不能归因于入口设计。"
        "实验采用 within-subject crossover：同一用户在两轮中分别先用传统工作台与先用 Cuttle，"
        "以消除学习顺序与熟练度带来的偏差。")
    print("D 公平性说明 OK")

doc.save(DOC)
print("saved | paragraphs:", len(doc.paragraphs), "tables:", len(doc.tables))
