# -*- coding: utf-8 -*-
"""v7 收尾：补齐仍缺失的段落级修改（幂等、逐步保存）。"""
import sys

import docx
from docx.oxml.ns import qn
from docx.table import Table
from docx.text.paragraph import Paragraph

DOC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v7.docx"
doc = docx.Document(DOC)
body = doc.element.body
TEMPLATE_P = doc.paragraphs[10]


def alltext():
    t = "\n".join(p.text for p in doc.paragraphs)
    for tb in doc.tables:
        for r in tb.rows:
            for c in r.cells:
                t += "\n" + c.text
    return t


def find_p(sub):
    hits = [p for p in doc.paragraphs if sub in p.text]
    return hits[0] if hits else None


def set_text(p, text):
    runs = p.runs
    if runs:
        runs[0].text = text
        for r in runs[1:]:
            r.text = ""
    else:
        p.add_run(text)


def para_after(ref, text):
    new_el = ref._element.makeelement(qn('w:p'), {})
    import copy
    new_el = copy.deepcopy(ref._element)
    ref._element.addnext(new_el)
    p = Paragraph(new_el, doc)
    set_text(p, text)
    return p


def para_after_el(el, text):
    import copy
    new_el = copy.deepcopy(TEMPLATE_P._element)
    el.addnext(new_el)
    p = Paragraph(new_el, doc)
    set_text(p, text)
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
    import copy
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


def clean_meta():
    """清理仍以内部修改说明口吻书写的句子。"""
    for p in doc.paragraphs:
        t = p.text
        if "这也是上一版测算出现偏差的主要原因。" in t:
            set_text(p, t.replace("这也是上一版测算出现偏差的主要原因。", ""))
        if "本版不作为对外承诺。" in t:
            set_text(p, t.replace("本版不作为对外承诺。", ""))
        if "避免把既有工程能力误当成项目创新。" in t:
            set_text(p, t.replace("避免把既有工程能力误当成项目创新。", ""))
        if "在后续版本中都应被删除、降级或移入远期规划。" in t:
            set_text(p, t.replace("在后续版本中都应被删除、降级或移入远期规划。", ""))
        if "本版测算不追求规模数字的漂亮，而是让任意一行数字都能回答“它是怎么算出来的”。" in t:
            set_text(p, t.replace("本版测算不追求规模数字的漂亮，而是让任意一行数字都能回答“它是怎么算出来的”。",
                                  "表中每一行数字都可由上一行推出，便于评委直接核验。"))


STEPS = []


def step(name, fn):
    STEPS.append((name, fn))


# ---------------------------------------------------------------- 步骤定义
def s_routing():
    if "三维自洽决策" in alltext():
        return
    repl_map = [
        ("4.2 核心创新二", "4.2 核心创新二 三维自洽决策 自治程度 协作形态 执行位置"),
        ("系统需要为每一次输入决定它应该获得多大的自治权",
         "系统需要为每一次输入回答三个相互独立的问题，而不是把它归入一个层级数字：这次任务需要多大的自治程度；"
         "需不需要拆给多个执行者协作；在哪个设备或环境执行。自治程度取文本表达、工具调用、智能体执行三档；"
         "协作形态取单执行者与多执行者两档；执行位置取本地、云端、跨设备三档，三者可以自由组合。"),
        ("pi(x) -> L",
         "决策的输出是三维取值加上由策略引擎裁定的审批方式：无需确认、先预览、必须确认或直接拒绝。"
         "为什么必须拆开可以用两个例子说明：让手机读取一张照片，是低自治、单执行者、跨设备，按单轴层级会被误判为最高档；"
         "两个执行者分别读两篇论文且只输出摘要，是多执行者但风险很低，也不该比一个自动修改版本库的单执行者“层级更高”。"),
        ("其中 L0 为直接表达",
         "决策的输入特征包括任务复杂度、动作可逆性、所需数据范围、工具需求、潜在并行性、结果可合并性与成本上限；"
         "这些特征可以从任务契约与情境字段中自动提取，因此决策过程可复现、可标注、可评测。"
         "与按任务类型分类的常见路由相比，三维决策同时给出上下文暴露范围、可用工具集、审批方式与成果回填形式四项授权结果。"),
        ("当 x 的不确定性较高",
         "当特征不确定性较高或动作不可逆时，系统遵循保守决策原则：降低自治程度、维持单执行者、优先在数据所在位置执行，"
         "并要求用户确认。该原则的目的不是提高自动化比例，而是让过度升级率与高风险低估率同时可观测、可调参。"),
        ("产品保持四级渐进交互",
         "把用户的输入交给系统之后，由系统而不是用户回答四个问题：这次请求需要多大的自治程度；需不需要拆给多个执行者协作；"
         "在哪个设备或环境执行；哪些动作必须由用户确认。界面上呈现的是这四类结果，而不是一个需要用户理解的层级数字。"),
        ("Cuttle 不默认组建多 Agent",
         "Cuttle 不默认组建多 Agent，因为在多数任务上多执行者的协调开销会超过收益。"
         "但“要不要组队”在执行前是一个预测问题，执行后才是测量问题，两者必须分开。"),
        ("其中 dQ 为质量提升",
         "执行前的组队评分由任务可并行性、结果可合并性、预计耗时收益、协调开销、合并难度与风险暴露共同决定："
         "前两项高、后两项低时才适合组队。该评分器用任务特征训练，标签来自执行后的实测收益，因此会随真实任务积累而校准。"),
        ("其中 src 为来源，diff 为修改",
         "执行后的实测收益是真实可观测的量：相对单执行者的质量变化、墙钟耗时变化、额外推理成本、结果冲突与合并失败次数。"
         "它不做运行前决策，只做两件事：判定本次组队是否值得，并作为标签校准评分器，由此形成预测、决策、执行、反馈校准的闭环。"),
        ("Return to Origin 与策略、验证、恢复体系不单独作为创新点主张",
         "返回原处与策略、验证、恢复体系不作为创新点主张，而是前两项机制能否落地的前提。"
         "返回原处机制把成果按原始应用、任务契约与动作风险，转换为可审阅段落、批注、文档副本、回复候选、差异与回滚点等形态，"
         "并保留从原始对象到最终结果的可追溯链路；策略、验证与恢复体系负责审批独立、动作可回滚、失败可解释，"
         "并把每次执行变成下一次决策的证据。"),
    ]
    for sub, txt in repl_map:
        p = find_p(sub)
        if p:
            set_text(p, txt)
    for sub in ("x = [k, r, v, s, t, p, c]", "其中 k 为任务复杂度",
                "G_team = a*dQ", "组队之后，Worker 之间不以自由对话传递状态", "CT = [src, diff"):
        p = find_p(sub)
        if p:
            p._element.getparent().remove(p._element)
    if not find_p("多执行者之间不以自由对话传递状态"):
        anchor = find_p("执行后的实测收益是真实可观测的量")
        para_after(anchor,
            "多执行者之间不以自由对话传递状态，而以版本化成果契约接力。成果契约是一个数据结构，字段包括来源引用、"
            "输出类型与 schema、版本号、修改内容、证据、验收条件、未解决问题、恢复点与责任执行者；"
            "下游执行者只有在验收条件满足后才接收成果，因此上下文漂移与责任模糊会表现为可检测的结构化偏差。")


def s_ic():
    if "意图胶囊的字段定义" in alltext() and "信度 Confidence" in alltext():
        return
    p = find_p("意图胶囊是本项目唯一的核心技术对象")
    if p:
        set_text(p, "意图胶囊（Intent Capsule, IC）是本项目唯一的核心技术对象，但它首先是一种数据结构，而不是一个数学模型。"
                    "它的作用是把一次用户输入从一段自然语言，变成有结构、有作用域、有生命周期、可校验、可失效的中间表示，"
                    "供后续路由、执行与验证共用。")
    p = find_p("其中 I 为 Intent")
    if p:
        set_text(p, "六个字段分别是：意图 Intent，用户此刻明确要做什么；最小情境 Context，完成本任务所需的最少信息；"
                    "来源位置 Origin，需求产生的应用、窗口、输入控件与对象；证据来源 Provenance，每条情境结论的来源与获取方式；"
                    "时效 Freshness，有效期与失效条件；范围与同意边界 Scope，用户允许系统查看和使用什么。")
    if not find_p("信度 Confidence 与动作风险 Risk 不放进意图胶囊"):
        anchor = find_p("六个字段分别是")
        para_after(anchor,
            "信度 Confidence 与动作风险 Risk 不放进意图胶囊。信度是每个字段的元数据，用于判断该字段能否直接用于自动执行；"
            "动作风险取决于系统准备做什么，只能在生成执行计划时计算。同一条意图在“只读取文件”与“覆盖原文件”两种计划下"
            "风险完全不同，把风险写进意图胶囊会掩盖这一区别，因此风险由策略引擎依据具体动作动态裁定。")
    p = find_p("其可验证指标不是“是否感知到了更多信息”")
    if p:
        set_text(p, "意图胶囊自身的评价对象是情境质量，而不是路由准确率：来源绑定是否正确（Origin Binding Accuracy）、"
                    "最小情境是否漏读（Context Recall）、是否多读（Context Precision 与情境过度读取率）、"
                    "是否仍在使用已失效上下文（Stale Context Misuse Rate）、证据是否完整（Provenance Coverage）、"
                    "以及用户是否需要重复交代背景（Context Re-entry Reduction）。路由类指标属于 4.2 节的评价对象。")
    p = find_p("表 6 意图胶囊")
    if p:
        set_text(p, "表 6 意图胶囊的字段定义")


def s_users():
    if "知识工作者，包括运营" in alltext():
        return
    p = find_p("项目采用窄场景切入")
    if p:
        set_text(p, "项目采用窄场景切入。目标用户是高频使用个人电脑、需要跨多个应用完成资料获取、分析、沟通与成果交付的知识工作者，"
                    "包括运营、项目、咨询、研究、行政与产品岗位；学生开发者与科研人员是其中的重要子集，也是团队最先能触达的早期样本。"
                    "首批只保留两条基准任务链：主链为“资料—分析—办公文档交付”，副链为“代码问题—修改—测试—可审阅差异”。"
                    "两条链路都具备输入焦点明确、任务边界可描述、结果可验收三个条件，适合作为配对实验与纵向分析的首批样本。")
    if not find_p("参赛主体是学生团队"):
        anchor = find_p("项目采用窄场景切入")
        para_after(anchor,
            "这里需要区分两件事：参赛主体是学生团队，不代表产品用户必须是学生；校园与合作单位提供的是低成本、可控、"
            "可连续观察的早期试验环境，而不是项目的目标市场定义。本项目也不以课程报告这类单点写作任务作为主场景，"
            "因为“一个 AI 写作工具就够了”，而跨来源取数与口径核验才是这套机制真正被需要的地方。")
    p = find_p("一位项目负责人")
    if not p:
        p = find_p("一名学生在 Word 中写课程报告时")
    if p:
        set_text(p, "一位项目负责人需要在当天更新项目汇报：从竞品网页、客户提供的 PDF、销售 Excel、群聊结论与上一版方案中取数，"
                    "核对口径后写成 Word 材料，并对关键数字标注来源。今天的典型做法是逐个打开 AI 对话框，复制材料、"
                    "重新解释格式要求、等待生成，再把结果粘回文档。任务越复杂，需要搬运的背景越多；应用一切换，"
                    "AI 对当前对象、关系和进度的理解就被打断。被浪费的不是模型能力，而是意图产生的位置与 AI 承接任务的位置之间的这段距离。")


def rebuild_any(prefixes, rows):
    last = None
    for pre in prefixes:
        try:
            return rebuild(pre, rows)
        except KeyError as exc:
            last = exc
    raise last


def s_experiment():
    if "纵向核心组 15 至 25 人" in alltext():
        return
    p = find_p("用户研究分为发现、机制验证和价值验证三个阶段")
    if p:
        set_text(p, "用户研究分三个阶段，并在样本设计上避免“同一批用户贯穿全程”带来的偏差。"
                    "发现阶段通过半结构化访谈、任务日记与情境观察，识别高频跨应用任务、基线耗时与敏感数据边界；"
                    "机制验证阶段用可交互原型与 Wizard of Oz 方法，验证用户是否理解意图升级、审批与结果返回。"
                    "价值验证阶段分两组：纵向核心组 15 至 25 人全程跟踪，用于观察学习曲线、信任变化与长期留存；"
                    "独立验证组另招 20 至 30 人，此前不参与任何设计与原型讨论，第一次接触最终版本，"
                    "用于测量首次使用、意图理解、效率提升与审批理解。")
    if not find_p("两组数据回答不同问题"):
        anchor = find_p("用户研究分三个阶段")
        para_after(anchor,
            "两组数据回答不同问题，不可混用：纵向组反映熟练之后的稳定价值与流失原因，"
            "独立组反映新用户在没有学习效应与期待偏差条件下的首用表现。"
            "正式实验前依据预实验的效应量做统计功效分析，据此确定各指标所需样本量。")
    if not find_p("对照实验的公平性前提"):
        anchor = find_p("表 8 核心机制的对照与消融设计")
        el = None
        for e in body.iterchildren():
            if e.tag.endswith('}tbl'):
                el = e
        # 直接把新段插在表 8 之后：找到表头为“实验”的表
        target = tbl_el("实验对照组处理组主指标")
        para_after_el(list(target.iterchildren())[-1],
            "对照实验的公平性前提必须写清：处理组与对照组使用同一模型与同一版本、相同的工具与权限范围、"
            "相同的任务材料与结果要求，唯一变量是“任务如何进入系统、上下文如何获得”。"
            "若对照组的工具权限被削减或模型版本更低，效率提升就不能归因于入口设计。"
            "实验采用 within-subject crossover，同一用户在两轮中分别先用传统工作台与先用 Cuttle，以消除学习顺序与熟练度偏差。")
    if not find_p("过度感知与过度自治的判定不能由开发者"):
        rebuild_any(["假设验证方法最低可接受值", "待验证判断验证方法"], [
            ["待验证判断", "验证方法", "预设阈值", "证伪后的决策"],
            ["用户愿意在输入位置直接发起任务", "独立验证组首用测试",
             "优先原地发起比例达多数（具体阈值由预实验确定）", "缩小入口范围 保留快捷指令"],
            ["意图胶囊能减少上下文重复交代", "配对任务 计时与记录",
             "重复输入下降 目标 30%（最低 15%）", "减少自动感知 增强手动选区"],
            ["三维决策比固定层级更贴合任务", "同一任务套件 三种决策策略对照",
             "过度升级率与高风险低估率同时下降", "退回固定档位 仅高危动作加审批"],
            ["可见审批能建立正确信任", "敏感任务可用性测试", "误批准率低于 3%", "增加解释与二次确认"],
            ["团队客户愿意为治理能力付费", "试点与报价测试", "至少取得采购意向或明确拒绝理由",
             "先做个人订阅 暂缓团队许可"],
        ])
        target = tbl_el("待验证判断验证方法")
        para_after_el(list(target.iterchildren())[-1],
            "表中阈值是预实验前预设的判断标准，不是研究结论，也不是既有门槛。实验采用配对比较与 within-subject crossover，"
            "即同一用户在两个会话中分别先用传统 Agent 与先用 Cuttle，以消除学习顺序影响；"
            "正式实验前依据预实验效应量做统计功效分析，再据此把定性阈值收敛为具体数值。")
        para_after(find_p("表中阈值是预实验前预设的判断标准"),
            "过度感知与过度自治的判定不能由开发者自行认定。判定依据是预先文档化的“完成任务最小充分信息集”与"
            "“最小必要执行档位”：由两名不参与研发的独立标注者对同一批任务样本独立标注，计算标注者一致率，"
            "不一致样本由第三方裁决并记录理由；标注一致率达到预设水平后，该指标才对外报告。")


def s_market():
    if "试点容量" in alltext():
        return
    p = find_p("市场规模采用自下而上的三层测算")
    if p:
        set_text(p, "项目不使用“大盘乘比例”的推算方式，也不在早期阶段宣称市场规模。第一年只给出团队真正能够运营的试点容量："
                    "这个数字来自团队能服务多少，而不是市场有多少。教育部 2025 年统计公报显示，"
                    "全国各种形式高等教育在学总规模为 4872.57 万人，[4] 该数字仅作为宏观背景，不作为可服务市场的计算基数。"
                    "可服务市场与外推测算将在首校复制率验证完成后，以实际报名率、活跃率与转化率重新进行。")
    p = find_p("输入法产品正在增加续写")
    if p:
        set_text(p, "输入焦点这一入口并非无人占据，桌面助手、悬浮入口、系统级快捷键与各类智能体工作台都在向用户靠近，"
                    "因此本项目不以“没有人做过入口”立论，而是比较谁能把意图、情境、自治决策、执行、验证与原处返回连成完整生命周期。"
                    "下表按六个维度对主要产品做定位比较；表中判断基于公开文档与产品公开能力，可能随版本变化，"
                    "本项目不对任何产品作出“不具备某能力”的断言。")
    rebuild("层级测算口径测算过程", [
        ["环节", "口径", "第一年规划", "依据"],
        ["覆盖学院", "团队能持续运营试点的学院数", "3 个学院", "团队人力与指导教师资源可覆盖范围"],
        ["招募规模", "每学院自愿报名的试用人数", "每院 60 至 100 人 合计 200 至 300 人", "校园与首批合作单位的可控触达能力"],
        ["深度试点", "能连续记录任务日志的深度用户", "50 至 80 人", "每周可完成的研究与支持工作量"],
        ["纵向核心组", "全程跟踪学习曲线与信任变化", "15 至 25 人", "纵向分析对样本量的最低要求"],
        ["独立验证组", "未参与前期设计的新用户", "20 至 30 人", "首用与审批理解的独立测量"],
        ["后续外推", "可服务市场与潜在市场的测算时点", "首校复制率验证完成后", "以实际报名率 活跃率 转化率外推"],
    ])
    rebuild("类别代表产品强项", [
        ["产品", "意图入口", "情境来源", "工具与执行", "多执行者协作", "原处返回", "权限与恢复"],
        ["搜狗输入法", "输入焦点", "光标附近文本与活动应用", "以文本生成为主", "未见公开能力", "回填到输入位置", "以输入法权限为主"],
        ["讯飞输入法", "输入焦点", "语音与文本", "以文本生成为主", "未见公开能力", "回填到输入位置", "以输入法权限为主"],
        ["Wispr Flow", "跨应用语音输入", "光标附近文本与活动应用", "转写与风格适配", "未见公开能力", "回填到光标位置", "以输入权限为主"],
        ["Cursor", "编辑器内入口", "代码库与打开的文件", "文件编辑与终端命令", "多任务并行处理", "回填到编辑器差异", "工作区权限与检查点"],
        ["Claude Code", "终端与编辑器入口", "仓库与文件系统", "文件与命令执行", "子任务与并行执行", "回填到文件与终端", "审批与权限模式"],
        ["Codex", "独立任务工作区", "仓库与附件", "文件与命令执行", "多任务并行执行", "以工作区交付为主", "沙箱与审批"],
        ["ZCode 等编码智能体", "编辑器或独立入口", "代码库", "文件与命令执行", "部分支持并行任务", "以编辑器差异为主", "权限与审批"],
        ["WorkBuddy 等办公智能体", "独立应用入口", "上传文件与办公套件", "办公文档操作", "以单执行者为主", "以独立应用交付为主", "企业策略与审计"],
        ["MiMo Desktop", "独立任务入口", "桌面与文件", "桌面操作与长任务", "多执行者协作", "以独立应用交付为主", "系统级权限"],
        ["JiuwenSwarm 等群智框架", "框架或开发者接口", "由调用方提供", "以框架编排为主", "多执行者协作是核心", "由调用方决定", "由调用方实现"],
    ])


def s_safety():
    if "禁止访问字段" in alltext():
        return
    rebuild("安全目标指标阶段门槛", [
        ["安全目标", "指标", "阶段门槛", "不达标处理"],
        ["未经授权不执行", "高风险动作未授权执行次数", "必须为 0", "冻结发布并复盘"],
        ["禁止访问字段", "明确标识的密码框 支付确认 金融敏感字段访问率", "必须为 0", "扩大系统级屏蔽并回归测试"],
        ["情境过度读取", "普通工作情境中读取非必需数据的比例", "目标低于 5%", "收缩读取范围 复核意图绑定"],
        ["自治过度升级", "执行档位高于用户意图所需的比例", "目标低于 10%", "强制回退档位 增加确认"],
        ["审批可理解", "用户能正确判断影响范围", "正确率不低于 95%", "重写说明与交互"],
        ["失败可恢复", "可逆任务恢复成功率", "不低于 90%", "限制能力并补偿测试"],
        ["证据可追溯", "外部动作带来源与审计记录", "覆盖率 100%", "阻止交付"],
    ])
    p = find_p("除通用安全目标之外")
    if p:
        set_text(p, "除通用安全目标外，项目把两个与输入法入口强相关的风险作为专有指标管理，并把“禁止访问”与“读取过多”"
                    "拆成两个不同指标：禁止访问字段指明确标识的密码框、支付确认与金融敏感字段，属设计红线，只能是 0；"
                    "情境过度读取率指普通工作情境中读取了非必需数据，属统计指标，目标低于 5%。")


def s_indicators():
    if "原地闭环" in alltext():
        return
    rebuild("维度主指标十二个月目标", [
        ["维度", "指标", "十二个月目标", "数据来源"],
        ["北极星", "原地闭环完成率：从原始输入位置发起、无需手工搬运上下文或切换工作台、结果正确并返回原处的任务比例",
         "作为首要指标报告 阈值由预实验确定", "任务日志与回填记录"],
        ["入口", "输入焦点识别率 Origin 绑定正确率", "识别率不低于 95%", "兼容矩阵测试集"],
        ["情境", "情境召回率 情境过度读取率 失效上下文误用率", "过度读取率低于 5% 召回率不低于 90%", "标注集与标注一致率"],
        ["决策", "三维决策 Macro-F1 过度升级率 高风险低估率",
         "Macro-F1 不低于 0.85 过度升级率低于 10% 高风险低估率低于 3%", "标注集与任务日志"],
        ["执行", "端到端任务成功率 组队实测净收益", "成功率不低于 85%", "固定任务套件与真实任务"],
        ["效率", "完成时间 重复输入次数 单任务模型成本", "时间降低 25% 提示减少 30%", "对照实验"],
        ["安全", "未授权高风险动作 禁止访问字段命中 恢复率",
         "未授权为 0 禁止访问命中为 0 恢复不低于 90%", "红队与故障注入"],
        ["市场", "独立验证组首用意愿 纵向组四周留存 试点数", "留存 40% 三个试点", "产品分析 合作记录"],
    ])
    para_after_el(list(tbl_el("维度指标十二个月目标").iterchildren())[-1],
        "全部指标共用一套统计口径：以配对样本比较为主，报告效应量与置信区间，而不只报告均值差异；"
        "样本量在正式实验前依据预实验效应量做统计功效分析后确定；所有指标注明数据来源、标注规则与样本量。"
        "指标之间的解释关系是：原地闭环完成率是首要指标，其余指标用于解释它为什么高或低。")


def s_risk():
    if "输入法信任风险" in alltext():
        return
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
        ["应用兼容性", "高", "高", "崩溃 焦点丢失 延迟上升", "缩小应用范围 自动化兼容测试 降级为快捷入口"],
        ["情境误判与隐私", "中", "极高", "敏感字段被读取 用户纠正上升", "禁止访问字段硬屏蔽 最小读取 独立审计"],
        ["执行误操作", "中", "极高", "越权 外发 覆盖或不可逆动作", "最小权限 独立审批 预览 沙箱与回滚"],
        ["模型能力波动", "中", "高", "成功率下降 成本或时延异常", "模型替换 固定评测 预算上限与降级策略"],
        ["用户留存不足", "中", "高", "四周留存低 基准任务链覆盖不足", "聚焦两条基准任务链 改善首用与技能复用"],
        ["商业范围失控", "中", "高", "定制需求占用研发 交付亏损", "标准合同 边界报价 里程碑验收"],
        ["知识产权与依赖", "低", "高", "开源许可不清 数据授权缺失", "依赖清单 原创记录 授权与合规审查"],
        ["团队持续性", "中", "中", "核心模块单人掌握 里程碑延期", "双人复核 文档化 模块轮换与交接演练"],
    ])
    p = find_p("项目在每一阶段设置明确的停止或转向条件")
    if p:
        set_text(p, "项目在每一阶段设置明确的停止或转向条件。若用户在输入位置不愿授权情境读取，则改为主动选区与快捷指令；"
                    "若三维决策相比固定策略没有可测收益，则退回固定档位并只在高风险动作加审批；"
                    "若组队评分与实测收益长期不相关，则取消组队预测，改为按任务模板静态组队；"
                    "若独立验证组的首用表现与纵向组差距过大，则优先修复新用户上手路径；"
                    "若团队客户缺乏付费意愿，则先做个人订阅与开发者生态。")


def s_finance():
    if "续订" in alltext():
        return
    p = find_p("本章用于检验商业模型能否形成可持续经营")
    if p:
        set_text(p, "本章用于检验商业模型能否形成可持续经营，不代表项目已经取得收入、融资或用户规模。测算采用统一口径："
                    "个人专业版按年末在付用户数乘年费标价 240 元确认收入；团队许可按合同金额在服务期内确认，首年按半年计入；"
                    "私有部署按验收确认收入。第二年起的续订用户按上年规模乘续费率 60% 进入，"
                    "因此“付费个人”一栏在第二年与第三年拆分为续订与新增两部分，避免把年末存量与全年新增混算。")
    rebuild("指标第一年第二年第三年", [
        ["指标", "第一年", "第二年", "第三年", "口径与假设"],
        ["付费个人 保守", "600 新增", "1500 续订 + 2500 新增", "6000 续订 + 9000 新增", "年费 240 元 续费率 60%"],
        ["付费个人 基准", "600 新增", "2000 续订 + 5000 新增", "8000 续订 + 22000 新增", "年费 240 元 续费率 60%"],
        ["付费个人 进取", "1500 新增", "4000 续订 + 8000 新增", "10000 续订 + 30000 新增", "年费 240 元 续费率 60%"],
        ["团队许可 保守 / 基准 / 进取", "6 / 6 / 8 个", "15 / 24 / 35 个", "30 / 50 / 80 个",
         "平均 4 万元每客户 首年按半年确认"],
        ["私有部署 保守 / 基准 / 进取", "1 / 1 / 2 个", "2 / 4 / 6 个", "6 / 10 / 16 个",
         "平均 18 万元每项目 按验收确认"],
        ["营业收入 保守", "60.6 万元", "146 万元", "372 万元", "个人年费加团队与部署收入"],
        ["营业收入 基准", "60.6 万元", "272 万元", "836 万元", "个人年费加团队与部署收入"],
        ["营业收入 进取", "104 万元", "482 万元", "1464 万元", "个人年费加团队与部署收入"],
        ["经营成本 保守", "90 万元", "180 万元", "390 万元", "研发 模型 市场与交付"],
        ["经营成本 基准", "110 万元", "282 万元", "636 万元", "研发 模型 市场与交付"],
        ["经营成本 进取", "120 万元", "486 万元", "1320 万元", "研发 模型 市场与交付"],
        ["经营结果 保守", "负 29.4 万元", "负 34 万元", "负 18 万元", "第三年仍未转正"],
        ["经营结果 基准", "负 49.4 万元", "负 10 万元", "200 万元", "第三年进入盈亏平衡上方"],
        ["经营结果 进取", "负 16 万元", "负 4 万元", "144 万元", "第二年接近平衡"],
    ])
    p = find_p("项目资源需求分三层表述")
    if p:
        set_text(p, "项目资源需求分三层表述。已有资源：学校实验室与实验工位、团队成员个人设备、指导教师的方法与安全指导、"
                    "开源模型与本地推理能力，这些不计入现金需求，因此资金来源中团队自筹与学校支持的占比高于对外融资。"
                    "新增现金需求第一年 40 万元，用于把原型推进到可验证的试点闭环，并完成对照、消融与红队测试；"
                    "在留存、付费意愿与安全指标同时达标后，再投入 80 万元用于第二年复制与团队许可交付。"
                    "首轮对外表述的资源配置总额为 120 万元，按决策门分三批释放，任一批未达标即暂停后续投入。")


def s_refs():
    if "[13] Yao S" in alltext():
        return
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
    anchor = find_p("[12] Xiaomi MiMo Desktop")
    assert anchor is not None
    for r in refs:
        anchor = para_after(anchor, r)


def s_captions():
    """把仍带旧措辞的题注改成 v7 口径（编号由 renumber 脚本统一处理）。"""
    fixes = [
        ("表 2 输入时刻入口价值与首批验证口径", "表 2 输入时刻入口价值与首批验证口径"),
        ("表 5 典型场景的输入到交付闭环", "表 5 典型场景的输入到交付闭环"),
        ("图 7 自下而上的三层市场漏斗（SOM SAM TAM）", "图 7 第一年试点容量测算与后续外推路径"),
        ("图 8 主要产品的入口距离与任务闭环能力定位", "图 8 主要产品在任务生命周期六个环节上的覆盖定位"),
        ("图 9 Cuttle 从校园任务到机构服务的市场进入路径", "图 9 Cuttle 从早期样本到团队许可的市场进入路径"),
    ]
    for old, new in fixes:
        p = find_p(old)
        if p and old != new:
            set_text(p, new)
    p = find_p("上述三道壁垒")
    if p:
        set_text(p, "三道壁垒都在建设过程中，不作为既成优势主张；它们真正的成本来自把协议、可靠性与真实数据同时做出来所需的时间。")
    p = find_p("生态与信任的长期积累")
    if p:
        set_text(p, "生态与信任的长期积累将随后续版本与真实使用逐步形成。")


step("路由与协作", s_routing)
step("意图胶囊", s_ic)
step("用户与场景", s_users)
step("实验设计", s_experiment)
step("市场与竞品", s_market)
step("安全指标", s_safety)
step("指标体系", s_indicators)
step("风险表", s_risk)
step("财务口径", s_finance)
step("参考文献", s_refs)
step("题注与语气", s_captions)
step("清理内部语气", clean_meta)

for name, fn in STEPS:
    try:
        fn()
        doc.save(DOC)
        print("OK  ", name)
    except Exception as exc:  # 逐步保存，出错不影响已完成部分
        doc.save(DOC)
        print("FAIL", name, "->", type(exc).__name__, exc)
        sys.exit(1)

print("saved:", DOC, "| paragraphs:", len(doc.paragraphs), "| tables:", len(doc.tables))
