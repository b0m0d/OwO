# -*- coding: utf-8 -*-
"""v7 补丁：把上次中断后残留的两段补回，并完成剩余修改。"""
from __future__ import annotations

import copy

import docx
from docx.oxml.ns import qn
from docx.table import Table
from docx.text.paragraph import Paragraph

DOC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v7.docx"
doc = docx.Document(DOC)
body = doc.element.body
TEMPLATE_P = doc.paragraphs[10]


def ptext(p):
    return p.text.strip()


def find_p(prefix):
    hits = [p for p in doc.paragraphs if ptext(p).startswith(prefix)]
    assert len(hits) == 1, (prefix, len(hits))
    return hits[0]


def has_p(prefix):
    return any(ptext(p).startswith(prefix) for p in doc.paragraphs)


def write_p(p, text):
    runs = p.runs
    if runs:
        runs[0].text = text
        for r in runs[1:]:
            r.text = ""
    else:
        p.add_run(text)


def para_after(ref, text):
    new_el = copy.deepcopy(ref._element)
    ref._element.addnext(new_el)
    p = Paragraph(new_el, doc)
    write_p(p, text)
    return p


def para_after_el(el, text):
    new_el = copy.deepcopy(TEMPLATE_P._element)
    el.addnext(new_el)
    p = Paragraph(new_el, doc)
    write_p(p, text)
    return p


def tbl_el(header_prefix):
    for el in body.iterchildren():
        if el.tag.endswith('}tbl'):
            hdr = "".join(c.text for c in Table(el, doc).rows[0].cells)
            if hdr.startswith(header_prefix):
                return el
    raise KeyError(header_prefix)


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


# ---------------------------------------------------------------- A. 补回缺失段落
if not has_p("7 至 12 个月的重点不是"):
    anchor = find_p("团队前期围绕智能体运行时")
    p = para_after(anchor,
        "研发节奏上，7 至 12 个月的重点不是“支持几个应用”，而是把输入法兼容矩阵与 Origin/Return 适配器做成标准件。"
        "首批覆盖 Word 与 WPS、浏览器、VS Code、企业微信与微信、Office 与 PDF 阅读器五类环境，"
        "每个环境测量六项：输入焦点识别率、情境捕获范围与准确率、Return to Origin 成功率、端到端延迟、"
        "崩溃与卡死次数、降级路径是否可用。任一环境未通过即不纳入对外演示范围，并以自动化兼容测试固化回归用例。")

if not has_p("全部指标共用一套统计口径"):
    para_after_el(list(tbl_el("维度指标十二个月目标").iterchildren())[-1],
        "全部指标共用一套统计口径：以配对样本比较为主，报告效应量与置信区间，而不只报告均值差异；"
        "样本量在正式实验前依据预实验效应量做统计功效分析后确定；所有指标注明数据来源、标注规则与样本量。"
        "指标之间的解释关系是：原地闭环完成率是首要指标，其余指标用于解释它为什么高或低。")

# ---------------------------------------------------------------- B. 第十章重写
write_p(find_p("本章用于检验商业模型能否形成可持续经营"),
    "本章用于检验商业模型能否形成可持续经营，不代表项目已经取得收入、融资或用户规模。"
    "测算采用统一口径：个人专业版按年末在付用户数乘年费标价 240 元确认收入；"
    "团队许可按合同金额在服务期内确认，首年按半年计入；私有部署按验收确认收入。"
    "首年新增用户按全年在付计算，第二年起的续订用户按上年规模乘续费率 60% 进入，"
    "因此“付费个人”一栏在第二年与第三年拆分为续订与新增两部分，避免把年末存量与全年新增混算。"
    "模型调用、支持、交付与合规成本随任务量动态调整。")
write_p(find_p("10.2 三年经营情景"), "10.2 三年经营测算（统一口径）")
write_p(find_p("图 11 Cuttle 三情景三年收入与成本规划"), "图 11 Cuttle 三年收入与成本（统一口径）")
write_p(find_p("表 18 三年经营情景测算"), "表 19 三年经营测算")
write_p(find_p("表 19 分层资源需求与用途"), "表 20 分层资源需求与用途")
write_p(find_p("项目资源需求分三层表述"),
    "项目资源需求分三层表述。已有资源：学校实验室与实验工位、团队成员个人设备、"
    "指导教师的方法与安全指导、开源模型与本地推理能力，这些不计入现金需求，因此在资金来源中，"
    "团队自筹与学校支持的占比高于对外融资。新增现金需求第一年 40 万元，用于把原型推进到可验证的试点闭环，"
    "并完成对照、消融与红队测试；在留存、付费意愿与安全指标同时达标后，再投入 80 万元用于第二年复制与团队许可交付。"
    "首轮对外表述的资源配置总额为 120 万元，按决策门分三批释放，任一批未达标即暂停后续投入。")

rebuild("指标第一年第二年第三年", [
    ["指标", "第一年", "第二年", "第三年", "口径与假设"],
    ["付费个人 保守", "600（新增）", "1500 续订 + 2500 新增", "6000 续订 + 9000 新增", "年费 240 元 续费率 60%"],
    ["付费个人 基准", "600（新增）", "2000 续订 + 5000 新增", "8000 续订 + 22000 新增", "年费 240 元 续费率 60%"],
    ["付费个人 进取", "1500（新增）", "4000 续订 + 8000 新增", "10000 续订 + 30000 新增", "年费 240 元 续费率 60%"],
    ["团队许可 保守 / 基准 / 进取", "6 / 6 / 10 个", "15 / 24 / 35 个", "30 / 50 / 80 个",
     "平均 4 万元每客户 首年按半年确认"],
    ["私有部署 保守 / 基准 / 进取", "1 / 1 / 2 个", "2 / 4 / 6 个", "6 / 10 / 16 个",
     "平均 18 万元每项目 按验收确认"],
    ["营业收入 保守", "60.6 万元", "146 万元", "370 万元", "个人年费加团队与部署收入"],
    ["营业收入 基准", "60.6 万元", "272 万元", "836 万元", "个人年费加团队与部署收入"],
    ["营业收入 进取", "90.6 万元", "482 万元", "1464 万元", "个人年费加团队与部署收入"],
    ["经营成本 保守", "90 万元", "180 万元", "420 万元", "研发 模型 市场与交付"],
    ["经营成本 基准", "110 万元", "282 万元", "636 万元", "研发 模型 市场与交付"],
    ["经营成本 进取", "120 万元", "486 万元", "1320 万元", "研发 模型 市场与交付"],
    ["经营结果 保守", "负 29.4 万元", "负 34 万元", "负 50 万元", "第三年仍为负"],
    ["经营结果 基准", "负 49.4 万元", "负 10 万元", "200 万元", "第三年进入盈亏平衡上方"],
    ["经营结果 进取", "负 29.4 万元", "负 4 万元", "144 万元", "第二年接近平衡"],
])

if not has_p("收入行的计算口径为"):
    t21_el = tbl_el("指标第一年第二年第三年")
    p_fin = para_after_el(list(t21_el.iterchildren())[-1],
        "收入行口径为：付费个人数乘 240 元，加团队许可客户数乘 4 万元（首年按半年计入），"
        "加私有部署项目数乘 18 万元。第一年基准为 600 乘 240 元、加 6 乘 2 万元、加 1 乘 18 万元，合计 60.6 万元。")
    para_after(p_fin,
        "对创意组项目而言，比三年营收总额更能说明问题的是四项单位经济指标：单任务模型成本（按任务类型统计并给出上四分位值）、"
        "单用户年成本、毛利率成立的时间点，以及团队许可的单客户交付成本。表中任一数字都可由上一行推出。")

rebuild("资源层级用途金额", [
    ["资源层级", "用途", "金额", "对应交付"],
    ["已有资源（不计入现金）", "学校实验室与工位 成员个人设备 指导教师指导 开源模型与本地推理",
     "不折现", "研发环境与方法指导 降低固定成本"],
    ["新增现金需求（第一年）",
     "产品与研发 30% 评测与安全 20% 市场与试点 25% 模型与基础设施 15% 知识产权与预备金 10%",
     "40 万元", "原型到试点闭环 对照与消融实验 红队 数据集与合规"],
    ["规模化投入（第二年，达标后）", "复制与交付 兼容矩阵扩展 支持与运维 团队许可交付",
     "80 万元", "第二批试点 团队许可交付能力"],
])

# ---------------------------------------------------------------- C. 第十一章 + 第十二章
write_p(find_p("项目在每一阶段设置明确的停止或转向条件"),
    "项目在每一阶段设置明确的停止或转向条件。若用户在输入位置不愿授权情境读取，则改为主动选区与快捷指令；"
    "若三维决策相比固定策略没有可测收益，则退回固定档位并只在高风险动作加审批；"
    "若预测评分与实测收益长期不相关，则取消组队预测，改为按任务模板静态组队；"
    "若独立验证组的首用表现与纵向组差距过大，则优先修复新用户上手路径；"
    "若团队客户缺乏付费意愿，则先做个人订阅与开发者生态。"
    "项目不以完成既定功能为目标，而以问题是否真实、机制是否有效、安全是否达标与经营是否可持续决定投入。")

rebuild("风险概率影响预警信号", [
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
    ["应用兼容性", "高", "高", "崩溃 焦点丢失 延迟上升",
     "缩小应用范围 自动化兼容测试 降级为快捷入口"],
    ["情境误判与隐私", "中", "极高", "敏感字段被读取 用户纠正上升",
     "禁止访问字段硬屏蔽 最小读取 独立审计"],
    ["执行误操作", "中", "极高", "越权 外发 覆盖或不可逆动作",
     "最小权限 独立审批 预览 沙箱与回滚"],
    ["模型能力波动", "中", "高", "成功率下降 成本或时延异常",
     "模型替换 固定评测 预算上限与降级策略"],
    ["用户留存不足", "中", "高", "四周留存低 基准任务链覆盖不足",
     "聚焦两条基准任务链 改善首用与技能复用"],
    ["商业范围失控", "中", "高", "定制需求占用研发 交付亏损",
     "标准合同 边界报价 里程碑验收"],
    ["知识产权与依赖", "低", "高", "开源许可不清 数据授权缺失",
     "依赖清单 原创记录 授权与合规审查"],
    ["团队持续性", "中", "中", "核心模块单人掌握 里程碑延期",
     "双人复核 文档化 模块轮换与交接演练"],
])

write_p(find_p("Cuttle 计划把资料搬运"),
    "Cuttle 计划把资料搬运、格式返工与重复交代交给受控工具，让用户把时间用于理解、判断与创造。"
    "通过来源、证据、修改与验收记录，项目鼓励用户对 AI 结果保持审阅责任，避免把生成内容直接当作答案。"
    "对研究与项目材料，版本化成果有助于团队复盘过程、发现错误并积累可复用方法。")
write_p(find_p("Cuttle 的长期目标是成为个人电脑上的意图层"),
    "Cuttle 的长期目标是成为个人电脑上的意图层：用户在任何应用表达需求时，"
    "系统以最小必要情境理解任务，以适当的执行方式完成工作，以清晰证据说明结果，并把成果送回用户原本工作的地方。"
    "衡量标准不是调用了多少模型或 Agent，而是用户是否减少了无意义搬运、任务是否更可靠、权限是否仍在用户手中。")
write_p(find_p("说明  本计划书以一条核心命题为主线"),
    "说明  本计划书以一条核心命题为主线：AI 执行能力持续增强，但仍缺少以输入焦点为原生起点、"
    "贯穿意图、情境、自治决策、执行、验证与原处返回的统一任务生命周期。"
    "内容收敛为三个核心创新（意图胶囊 三维自治决策 自适应协作与成果契约）、"
    "两个配套工程体系（Return to Origin 与策略验证恢复），以及两个远期扩展（跨设备能力节点 技能生态）。"
    "文中用户规模 定价 财务与里程碑均为规划情景，需以真实访谈 试点 付费 合同和经营数据持续校准。")

# ---------------------------------------------------------------- D. 参考资料扩充
refs = [
    "[13] Yao S, et al. ReAct: Synergizing Reasoning and Acting in Language Models. ICLR, 2023.",
    "[14] Schick T, et al. Toolformer: Language Models Can Teach Themselves to Use Tools. NeurIPS, 2023.",
    "[15] Shinn N, et al. Reflexion: Language Agents with Verbal Reinforcement Learning. NeurIPS, 2023.",
    "[16] Wu Q, et al. AutoGen: Enabling Next-Gen LLM Applications via Multi-Agent Conversation. 2023.",
    "[17] Hong S, et al. MetaGPT: Meta Programming for A Multi-Agent Collaborative Framework. ICLR, 2024.",
    "[18] Jimenez C E, et al. SWE-bench: Can Language Models Resolve Real-World GitHub Issues? ICLR, 2024.",
    "[19] Zhou S, et al. WebArena: A Realistic Web Environment for Building Autonomous Agents. ICLR, 2024.",
    "[20] Xie T, et al. OSWorld: Benchmarking Multimodal Agents for Open-Ended Tasks in Real Computer Environments. NeurIPS, 2024.",
    "[21] Dourish P. What We Talk About When We Talk About Context. Personal and Ubiquitous Computing, 2004.",
    "[22] Dey A K. Understanding and Using Context. Personal and Ubiquitous Computing, 2001.",
    "[23] Horvitz E. Principles of Mixed-Initiative User Interfaces. CHI, 1999.",
    "[24] Amershi S, et al. Guidelines for Human-AI Interaction. CHI, 2019.",
    "[25] Lee J D, See K A. Trust in Automation: Designing for Appropriate Reliance. Human Factors, 2004.",
    "[26] Parasuraman R, Riley V. Humans and Automation: Use, Misuse, Disuse, Abuse. Human Factors, 1997.",
    "[27] Microsoft. Text Services Framework (TSF) 与 UI Automation 官方架构文档.",
    "[28] NIST. Artificial Intelligence Risk Management Framework (AI RMF 1.0). 2023.",
    "[29] ISO/IEC 25010:2011. Systems and software Quality Requirements and Evaluation.",
    "[30] Shneiderman B. Human-Centered AI. Oxford University Press, 2022.",
]
if not has_p("[13] Yao S"):
    anchor = find_p("[12] Xiaomi MiMo Desktop")
    for r in refs:
        anchor = para_after(anchor, r)

# ---------------------------------------------------------------- E. 编号顺排（新表 19/20 等）
renum = [
    ("表 12 任务生命周期六维度竞品定位", "表 12 任务生命周期六维度竞品定位"),
    ("表 13 产品版本与收入结构", "表 13 产品版本与收入结构"),
    ("表 14 单位经济模型的规划假设", "表 14 单位经济模型的规划假设"),
    ("表 15 分阶段研发与市场决策门", "表 15 分阶段研发与市场决策门"),
    ("表 16 项目十二个月核心指标", "表 16 项目十二个月核心指标"),
]
for a, b in renum:
    if a != b and has_p(a):
        write_p(find_p(a), b)

doc.save(DOC)
print("saved")
print("paragraphs:", len(doc.paragraphs), "tables:", len(doc.tables))
for p in doc.paragraphs:
    t = p.text.strip()
    if t.startswith(('表 ', '图 ')):
        print("  ", t[:44])
