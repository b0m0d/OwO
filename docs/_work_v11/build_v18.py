# -*- coding: utf-8 -*-
"""Build v18 (paragraph-level rewriting; the text is split across many <w:t> runs)."""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v18.docx"

MARKER_RE = re.compile(r'(<w:t[^>]*>)((?:\[\d+\])+)(</w:t>)')
RUN_RE = re.compile(r'<w:r(?: [^>]*)?>.*?</w:r>', re.S)
PARA_RE = re.compile(r'<w:p(?: [^>]*)?>.*?</w:p>', re.S)
T_RE = re.compile(r'(<w:t(?: [^>]*)?>)([^<]*)(</w:t>)')

# 引用重排：旧 -> 新（按正文首次出现顺序）
CITE_MAP = {25: 1, 26: 2, 4: 3, 27: 4, 21: 5, 22: 6, 23: 7, 13: 8, 14: 9,
            15: 10, 16: 11, 17: 12, 18: 13, 19: 14, 20: 15, 9: 16, 10: 17,
            11: 18, 24: 19, 28: 20, 30: 21, 3: 22, 5: 23, 6: 24, 7: 25,
            8: 26, 12: 27, 29: 28}


def set_para_text(para_xml, new_text):
    """Keep the paragraph properties, drop the original runs, emit one formatted run."""
    ppr = re.search(r'<w:pPr>.*?</w:pPr>', para_xml, re.S)
    rpr = re.search(r'<w:rPr>.*?</w:rPr>', para_xml, re.S)
    open_tag = re.match(r'<w:p(?: [^>]*)?>', para_xml).group(0)
    head = open_tag + (ppr.group(0) if ppr else '')
    run = '<w:r>%s<w:t xml:space="preserve">%s</w:t></w:r>' % (
        rpr.group(0) if rpr else '', new_text)
    return head + run + '</w:p>'


# (唯一锚点片段, 新整段文本 或 None=删除该段)
PARA_EDITS = [
    ('收入行口径为：付费个人数乘年费标价',
     '收入按“年均在付个人数 × 240 元 + 校园团队数 × 4 万元 + 私有部署项目数 × 18 万元”逐年测算。'
     '第一年基准情景为 60 × 240 元，合计 1.4 万元；团队许可与私有部署自第二年起计入，'
     '第二年基准情景为 855 × 240 元 + 5 × 4 万元，合计 40.5 万元；'
     '第三年基准情景为 1850 × 240 元 + 24 × 4 万元 + 2 × 18 万元，合计 176.4 万元。'
     '表9 的营业收入、经营成本与经营结果三组数字互为加减关系，可逐行核对。'),

    ('策略、验证与恢复体系负责审批',
     '原位交付、策略控制、结果验证与失败恢复共同构成两项核心创新的工程支撑体系。'
     '原位交付机制按原始应用、任务契约与动作风险，把成果转换为可审阅段落、批注、文档副本、'
     '回复候选、差异与回滚点等形态，并保留从原始对象到最终结果的可追溯链路。'
     '策略体系负责独立审批与最小权限判定，验证体系按任务契约核对结果，'
     '恢复体系保证动作可回滚、失败可解释，并把每次执行转化为下一次决策的证据。'),

    ('财务测算采用个人订阅、团队许可和私有部署三类收入',
     '财务测算采用个人订阅、团队许可和私有部署三类收入。个人订阅收入按年度平均在付用户数乘年费 240 元测算：'
     '年均在付由上年在付规模的 60% 续订、当年新增按半年加权估算；团队许可按 4 万元每团队每年计价，'
     '自第二年起计入；私有部署按 18 万元每项目、以验收确认。本轮预测仅计入基础订阅、团队许可与私有部署收入，'
     '超额智能体调用收入及其对应的推理成本暂不计入。三种情景分别设定用户与客户数量，按同一单价假设计算收入；'
     '第一年的规模由第六章的验证容量约束，个人付费用户不超过 200 人，三种情景分别按 80、120 与 180 人测算。'
     '全部单价与转化率均为规划假设，验证方式与门槛见表 7。'),

    ('在证据取得之前，本章全部数字按情景假设处理',
     '前三个月完成三项最低限度的价格证据：一是 20 份定价访谈，对象为高频 PC 知识工作者，'
     '覆盖运营、项目、咨询、研究、行政与开发岗位，用于确认年费 240 元与团队许可 4 万元每团队每年的可接受区间；'
     '二是 3 份试点意向，用于确认团队许可的采购流程与预算归属；三是首份报价反馈，'
     '记录真实客户对报价范围的异议与调整意见。三项证据在立项后前三个月内完成，本章数字随其结论更新。'),

    ('训练的是把模糊需求转化为可检验问题的能力',
     '成员能力在真实的研发与验证过程中形成。项目启动阶段完成用户调研与需求分析，'
     '产出 20 份定价访谈与任务日记，并把“用户想少搬一次资料”这类表述收敛为可测指标：'
     '上下文重复说明次数、来源绑定准确率、发起步骤数。原型阶段完成系统设计、交互设计与权限方案，'
     '对安全边界的判断以发布门槛形式固化，即表5 的七项指标与对应处理办法。'
     '进入实验与评测阶段后，成员独立完成实验设计、数据分析、对照消融与红队故障注入，'
     '并输出可复现的评测报告；这一环节的产出直接进入产品迭代，而不是停留在文档层面。'
     '在市场与成果转化方面，成员通过场景试点、报价谈判与合作复盘了解真实商业约束，'
     '并完成软件著作权、专利交底与论文写作。全部成果以任务、代码、文档、测试与运营记录归档，'
     '贡献归属与数据授权同步明确。'),
]

# 单元格级替换（表9 私有部署第二年已是 0/0/1，无需再改）
CELL_EDITS = [
    ('<w:t>输入法信任风险</w:t>', '<w:t>输入系统信任风险</w:t>'),
]

# 段内文本替换（同一 run 内即可完成）
INLINE_EDITS = [
    ('输入法产品也在进入大模型时代', '输入系统也在进入大模型时代'),
    ('输入法接近意图产生的现场', '输入系统接近意图产生的现场'),
    ('第四，输入法不应承担无限权限', '第四，输入系统不应承担无限权限'),
    ('输入延迟与既有输入法一致', '输入延迟与既有输入系统一致'),
    ('输入法信任、隐私泄露与执行误操作列为一级风险',
     '输入系统信任、隐私泄露与执行误操作列为一级风险'),
    # 目录行 + 章节标题（同名，共 2 处）
    ('输入法场景的安全原则', '输入系统场景的安全原则'),
]


def remap_markers(segment, mapping):
    def phase1(m):
        nums = [int(x) for x in re.findall(r'\d+', m.group(2))]
        return m.group(1) + ''.join('[\x01%d\x01]' % mapping[n] for n in nums) + m.group(3)
    seg = MARKER_RE.sub(phase1, segment)
    return seg.replace('\x01', '')


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')

    print('=== 段落级修订 ===')
    for anchor, new_text in PARA_EDITS:
        hits = [m for m in PARA_RE.finditer(xml)
                if anchor in ''.join(T_RE.search(x).group(2) if False else ''
                                     for x in [])]
        # locate the paragraph containing the anchor
        idx = xml.find(anchor)
        if idx < 0:
            raise SystemExit('ABORT: 段落锚点未找到: %s' % anchor[:30])
        start = xml.rfind('<w:p ', 0, idx)
        start2 = xml.rfind('<w:p>', 0, idx)
        start = max(start, start2)
        end = xml.find('</w:p>', idx) + len('</w:p>')
        para = xml[start:end]
        # guard: the anchor must appear exactly once in this paragraph's text
        ptext = ''.join(m.group(2) for m in T_RE.finditer(para))
        if ptext.count(anchor) != 1:
            raise SystemExit('ABORT: 段落内锚点不唯一: %s' % anchor[:30])
        xml = xml[:start] + set_para_text(para, new_text) + xml[end:]
        print('  ✔ %s…' % anchor[:26])

    print('\n=== 单元格 / 段内修订 ===')
    for old, new in CELL_EDITS + INLINE_EDITS:
        n = xml.count(old)
        if n < 1:
            raise SystemExit('ABORT: %r 命中 %d 次（期望 ≥1）' % (old[:40], n))
        xml = xml.replace(old, new)
        print('  ✔ [x%d] %s' % (n, old[:34]))

    # ------------------------------------------------------------ 引用重排
    # “参考资料”在目录里也出现一次，必须取最后一次（真正的文献列表标题）
    hits = [m.start() for m in re.finditer(r'<w:t[^>]*>\s*参考资料\s*</w:t>', xml)]
    if len(hits) < 2:
        raise SystemExit('ABORT: 参考资料标题只找到 %d 处' % len(hits))
    cut = hits[-1]
    rm_end = xml.find('</w:t>', cut) + len('</w:t>')
    body, divider, refs = xml[:cut], xml[cut:rm_end], xml[rm_end:]
    print('\n正文/文献分界: 目录处 %d，文献标题处 %d' % (hits[0], cut))

    # 先扫描正文，取得“引用了哪些编号”以及首次出现顺序
    seen = []
    for m in MARKER_RE.finditer(body):
        for n in re.findall(r'\d+', m.group(2)):
            n = int(n)
            if n not in seen:
                seen.append(n)
    print('  正文引用次数: %d，涉及 %d 个编号' % (len(MARKER_RE.findall(body)), len(seen)))
    print('  首次出现顺序: %s' % seen)

    # 映射直接由 body 顺序推导，不再使用硬编码表
    mapping = {old: i + 1 for i, old in enumerate(seen)}
    dropped = sorted(set(range(1, 31)) - set(seen))
    print('  未被引用、将从列表删除: %s' % dropped)

    n_body = len(MARKER_RE.findall(body))
    body = remap_markers(body, mapping)
    n_new = len(seen)

    # 参考文献列表：按段落定位真正的条目
    ref_paras = list(PARA_RE.finditer(refs))
    by_no = {}
    for pm in ref_paras:
        ptext = ''.join(m.group(2) for m in T_RE.finditer(pm.group(0)))
        m = re.match(r'\s*\[(\d+)\]\s*(.+?)\s*$', ptext)
        if not m or not m.group(2).strip():
            continue
        no = int(m.group(1))
        if no in range(1, 31) and no not in by_no:
            by_no[no] = (pm.start(), pm.end(), m.group(2).strip())
    if len(by_no) != 30:
        raise SystemExit('ABORT: 参考文献解析到 %d 条（期望 30）' % len(by_no))

    # 从后往前改：被引用的重写为新编号，未被引用的整段删除
    edits = []
    cursor = 1
    for no in sorted(by_no):
        s, e, text = by_no[no]
        if no in mapping:
            edits.append((s, e, set_para_text(refs[s:e], '[%d] %s' % (mapping[no], text))))
        else:
            edits.append((s, e, ''))
    refs_new = refs
    for s, e, rep in sorted(edits, key=lambda x: -x[0]):
        refs_new = refs_new[:s] + rep + refs_new[e:]

    print('\n=== 引用重排 ===')
    print('  正文引用标记改写 %d 处' % n_body)
    print('  新编号 -> 原编号: %s' % [(i + 1, seen[i]) for i in range(n_new)])
    print('  列表条目: %d -> %d（删除未引用项）' % (len(by_no), n_new))

    xml = body + divider + refs_new
    data['word/document.xml'] = xml.encode('utf-8')

    if os.path.exists(DST):
        os.remove(DST)
    with zipfile.ZipFile(DST, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])
    print('\nOUTPUT: %s (%.2f MB)' % (DST, os.path.getsize(DST) / 1024.0 / 1024.0))


if __name__ == '__main__':
    main()
