# -*- coding: utf-8 -*-
"""P0 fixes: (1) split team members from faculty advisor, (2) update evaluation-rules reference,
(3) replace the static TOC table with a real Word TOC field (dot leaders + PAGEREF hyperlinks)."""
import copy
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement
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

def first_cells(t):
    return [c.text.strip() for c in t.rows[0].cells]

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

# ============ 1) 团队成员 / 指导教师 分开 ============
team = None
for t in doc.tables:
    fc = first_cells(t)
    if fc[:2] == ["成员", "角色与专业方向"]:
        team = t
        break
assert team is not None
# 删除指导教师行，并把表头改为“学生团队成员”
rows = team.rows
for i, r in enumerate(list(rows)):
    if r.cells[0].text.strip() == "赵恒军":
        r._element.getparent().remove(r._element)
set_cell(team.rows[0].cells[0], "学生团队成员")

# 表题注
cap = find("表 8 团队分工与工程基础")
set_text(cap, "表 8 学生团队分工与工程基础")

# 在表格之后插入“指导教师与指导职责”小节
model = None
for p in doc.paragraphs:
    if p.style.name == "Normal" and len(p.text) > 60:
        model = p; break

def add_after(anchor_el, text, style=None):
    el = copy.deepcopy(model._element)
    anchor_el.addnext(el)
    np = Paragraph(el, doc._body)
    set_text(np, text)
    if style:
        np.style = doc.styles[style]
    return np

tbl_el = team._element
n1 = add_after(tbl_el, "指导教师与指导职责", style="Heading 3")
n2 = add_after(n1._element,
    "指导教师赵恒军负责研究方法、安全合规与阶段评审三方面工作：在立项与方法环节指导学生完成问题定义、"
    "研究设计与实验方案；在安全与合规环节审核数据处理边界、隐私保护措施与发布门槛；"
    "在阶段评审环节参与里程碑决策，对是否进入下一阶段提出意见。"
    "指导教师不列入项目团队成员名单，不参与项目成果的知识产权归属，"
    "核心研发、产品决策与成果表达均由学生团队承担。")
print("  team table split done")

# 正文中的团队表述同步
for p in doc.paragraphs:
    t = p.text.strip()
    if t.startswith("项目由学生团队主导研发"):
        set_text(p, "项目由学生团队主导研发，教师以指导身份参与。产品面向高频 PC 知识工作者，"
                    "团队围绕产品架构、智能体工程、用户研究与商业验证形成明确分工，"
                    "每个里程碑保留任务单、版本记录、测试报告与用户证据。"
                    "核心研发、产品决策与成果表达均由学生团队承担，指导教师负责研究方法、安全合规与阶段评审。")
        print("  9.1 表述同步")

# ============ 2) 赛事文件与评审规则引用 ============
for p in doc.paragraphs:
    t = p.text.strip()
    if t.startswith("[1] 教育部"):
        set_text(p, "[1] 教育部. 关于举办中国国际大学生创新大赛（2026）的通知[Z]. 2026.")
    elif t.startswith("[2] 中国国际大学生创新大赛"):
        set_text(p, "[2] 中国国际大学生创新大赛组织委员会. 中国国际大学生创新大赛（2026）高教主赛道创意组评审规则[Z]. 2026.")
print("  references 1-2 updated to 2026")

# ============ 3) 目录改为真正的 TOC 域 ============
toc_tbl = doc.tables[1]
# 收集现有条目
entries = []
for row in toc_tbl.rows:
    left = row.cells[0].text.strip()
    right = row.cells[1].text.strip()
    if left:
        entries.append((left, right))
print("  toc entries:", len(entries))

# 记录锚点：目录表之前的段落（“目录”标题）
anchor_el = toc_tbl._element.getprevious()
# 删除旧目录表
toc_tbl._element.getparent().remove(toc_tbl._element)

# 目标段落格式模型（正文 Normal，去掉首行缩进）
def make_toc_para(text_left, page, bold=False, tag=None):
    el = copy.deepcopy(model._element)
    p = Paragraph(el, doc._body)
    pf = p.paragraph_format
    pf.first_line_indent = 0
    pf.space_after = 0
    pf.line_spacing = 1.15
    # 右对齐制表位 + 点线前导符
    pPr = el.get_or_add_pPr()
    tabs = OxmlElement('w:tabs')
    tab = OxmlElement('w:tab')
    tab.set(qn('w:val'), 'right')
    tab.set(qn('w:leader'), 'dot')
    tab.set(qn('w:pos'), '9638')
    tabs.append(tab)
    pPr.append(tabs)
    set_text(p, text_left + "\t" + page)
    runs = p.runs
    if runs:
        for r in runs:
            r.font.name = "Microsoft YaHei"
            r.font.size = doc.styles['Normal'].font.size
        if bold:
            runs[0].bold = True
    if tag:
        el.set(qn('w14:paraId'), tag) if False else None
    return p, el

# 构建域的 XML：begin -> instrText -> separate -> 缓存条目 -> end
first_el = None
prev_el = anchor_el
cache_paras = []

for idx, (left, page) in enumerate(entries):
    is_chapter = (left.startswith("第") and "章" in left) or left in ("项目摘要", "参考资料")
    p, el = make_toc_para(left, page, bold=is_chapter)
    prev_el.addnext(el)
    prev_el = el
    cache_paras.append(el)
    if first_el is None:
        first_el = el

# 第一个缓存段落前插入 begin + instrText + separate
def field_run(kind=None, instr=None):
    r = OxmlElement('w:r')
    rPr = OxmlElement('w:rPr')
    rf = OxmlElement('w:rFonts')
    for a in ('w:ascii', 'w:hAnsi', 'w:eastAsia'):
        rf.set(qn(a), 'Microsoft YaHei')
    rPr.append(rf)
    r.append(rPr)
    if instr is not None:
        t = OxmlElement('w:instrText')
        t.set(qn('xml:space'), 'preserve')
        t.text = instr
        r.append(t)
    else:
        fc = OxmlElement('w:fldChar')
        fc.set(qn('w:fldCharType'), kind)
        r.append(fc)
    return r

def para_with(runs):
    p = OxmlElement('w:p')
    pPr = OxmlElement('w:pPr')
    ind = OxmlElement('w:ind')
    ind.set(qn('w:firstLine'), '0')
    pPr.append(ind)
    sp = OxmlElement('w:spacing')
    sp.set(qn('w:after'), '0')
    pPr.append(sp)
    p.append(pPr)
    for r in runs:
        p.append(r)
    return p

# 在第一个缓存段落内注入 begin/instrText/separate（作为该段落的前导 run）
p0 = Paragraph(first_el, doc._body)
runs_before = [field_run('begin'), field_run(instr=' TOC \\o "1-2" \\h \\z \\u '), field_run('separate')]
# 把已有 run 之后追加
for rb in runs_before:
    p0._element.insert(1, rb) if False else None
# 更稳妥：把这些 run 放到段落最前面（pPr 之后）
pPr = p0._element.find(qn('w:pPr'))
insert_at = 1 if pPr is not None else 0
for k, rb in enumerate(runs_before):
    p0._element.insert(insert_at + k, rb)

# 在最后一个缓存段落之后追加 end
last_el = cache_paras[-1]
end_el = para_with([field_run('end')])
last_el.addnext(end_el)

doc.save(SRC)
print("  TOC field installed; entries:", len(entries))
print("P0 fixes done")
