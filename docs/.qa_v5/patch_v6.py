# -*- coding: utf-8 -*-
"""对 v6 文档做第二轮修正：表顺序、题注编号、表 6 字段、目录页码。"""
import docx
from docx.oxml.ns import qn
from docx.table import Table

DOC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v6.docx"
doc = docx.Document(DOC)
body = doc.element.body


def ptext(p):
    return ''.join(n.text or '' for n in p._element.iter(qn('w:t'))).strip()


def paras():
    return list(body.iterchildren())


def find_p(text):
    h = [el for el in paras() if el.tag.endswith('}p') and
         ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip() == text]
    assert len(h) == 1, (text, len(h))
    return h[0]


def swap(a, b):
    """交换两个 body 子元素的位置。"""
    a.getparent().remove(a)
    b.addprevious(a)


# ---- 1) 过度感知表：把表移到题注之后
ov_cap = find_p("表 10 过度感知与过度自治指标")
ov_tbl = ov_cap.getprevious()
assert ov_tbl.tag.endswith('}tbl'), ov_tbl.tag
ov_tbl.getparent().remove(ov_tbl)
ov_cap.addnext(ov_tbl)

# ---- 2) 安全门槛题注：定位安全门槛表（表头为“安全目标”）并把题注移到它前面
safe_cap = find_p("表 11 安全门槛 设计红线 测试指标与生产事故指标")
safe_tbl = None
for el in paras():
    if el.tag.endswith('}tbl'):
        hdr = ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip()
        if hdr.startswith("安全目标"):
            safe_tbl = el
            break
assert safe_tbl is not None, "safety table not found"
safe_cap.getparent().remove(safe_cap)
safe_tbl.addprevious(safe_cap)

# 校验顺序
seq = []
for el in paras():
    if el.tag.endswith('}p'):
        t = ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip()
        if t.startswith('表 1') and ('安全' in t or '过度' in t):
            seq.append(('P', t))
    elif el.tag.endswith('}tbl'):
        first = ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip()[:12]
        seq.append(('T', first))
i = [n for n, s in enumerate(seq) if s[0] == 'P' and s[1].startswith('表 10 过度')]
print("sequence check:", seq[max(0, i[0] - 3): i[0] + 6] if i else "??")

# ---- 3) 表 6 关键字段重建（六元组对齐）
def rebuild_by_caption(caption_prefix, rows):
    cur = None
    for el in paras():
        if el.tag.endswith('}p'):
            t = ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip()
            cur = t if t.startswith('表 ') else None
        elif el.tag.endswith('}tbl') and cur and cur.startswith(caption_prefix):
            el = el
            break
    else:
        raise KeyError(caption_prefix)
    ncols = len(rows[0])
    trs = el.findall(qn('w:tr'))
    while len(trs) < len(rows):
        new_tr = __import__('copy').deepcopy(trs[-1])
        trs[-1].addnext(new_tr)
        trs = el.findall(qn('w:tr'))
    while len(trs) > len(rows):
        trs[-1].getparent().remove(trs[-1])
        trs = el.findall(qn('w:tr'))
    for tr in trs:
        tcs = tr.findall(qn('w:tc'))
        while len(tcs) < ncols:
            new_tc = __import__('copy').deepcopy(tcs[-1])
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
            cell = t.rows[ri].cells[ci]
            ps = cell.paragraphs
            while len(ps) > 1:
                cell._tc.remove(ps[-1]._p)
                ps = cell.paragraphs
            p = cell.paragraphs[0]
            if p.runs:
                p.runs[0].text = val
                for r in p.runs[1:]:
                    r.text = ""
            else:
                p.add_run(val)
        for tc in trs[ri].findall(qn('w:tc')):
            tcpr = tc.find(qn('w:tcPr'))
            if tcpr is None:
                continue
            tcw = tcpr.find(qn('w:tcW'))
            if tcw is not None:
                tcw.set(qn('w:w'), str(per))
                tcw.set(qn('w:type'), 'dxa')


rebuild_by_caption("表 6 意图胶囊关键字段", [
    ["六元组分量", "字段", "作用", "失效事件", "用户控制"],
    ["I Intent", "Relation Intent", "记录用户目标与对象关系", "用户纠正 任务结束", "直接修改"],
    ["C Context", "Minimal Context", "只保留完成当前任务所需的最少情境", "超出当前意图 任务结束", "按应用与字段关闭"],
    ["O Origin", "Application Focus", "标识当前应用 窗口 输入焦点与对象", "切换应用 关闭窗口", "查看 关闭感知"],
    ["P Provenance", "Provenance Record", "记录每条情境结论的来源与证据", "来源变化 依赖失效", "查看来源"],
    ["F Freshness", "TTL and Invalidation", "设定有效期与失效条件", "超过期限 对象修改", "查看 手动失效"],
    ["R Risk and Scope", "Scope and Confidence", "限定权限范围并决定是否可自动使用", "置信度下降 权限变化", "确认 拒绝 撤回授权"],
])

# ---- 4) 图 10 题注
p = find_p("图 10 Cuttle 二十四个月产品与市场路线")
p_runs = p.findall(qn('w:r'))
t = p_runs[0].find(qn('w:t'))
t.text = "图 10 Cuttle 二十四个月研发与市场路线"

# ---- 5) 目录页码（按新增内容后移估算）
toc = doc.tables[1]
new_pages = ["4", "5", "6", "8", "10", "12", "14", "16", "18", "20", "21", "23", "24", "25"]
for i, row in enumerate(toc.rows):
    cell = row.cells[1]
    p = cell.paragraphs[0]
    if p.runs:
        p.runs[0].text = new_pages[i]
        for r in p.runs[1:]:
            r.text = ""

doc.save(DOC)
print("patched:", DOC, "tables:", len(doc.tables))
