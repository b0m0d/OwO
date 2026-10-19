# -*- coding: utf-8 -*-
"""Editorial pass B3: fix the merged validation table, merge finance+resources,
trim the risk register, and renumber every caption."""
import copy
from docx import Document
from docx.oxml.ns import qn
from docx.table import _Row

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
        if ci < len(t.rows[ri].cells):
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

# ---- 修好合并后的验证表（行里残留旧列） ----
t = get_table(["验证方向", "验证方式"], 5)
assert len(t.rows) == 8, len(t.rows)
for r in t.rows:
    while len(r.cells) > 5:
        r.cells[-1]._element.getparent().remove(r.cells[-1]._element)
set_row(t, 0, ["验证方向", "验证方式", "指标与门槛", "未达标时的调整", "数据来源"])

# ---- 10.2 三年经营预测 + 10.3 资源需求 合并 ----
tf = get_table(["指标", "第一年", "第二年", "第三年"], 5)
ROWS = [
 ["指标", "第一年", "第二年", "第三年", "口径与依据"],
 ["个人专业版 付费个人（年末在付）", "80 / 120 / 180", "600 / 1200 / 2000", "1300 / 2800 / 5000",
  "年费 240 元；年度续费率 60%"],
 ["团队许可（个）", "0 / 0 / 0", "3 / 5 / 10", "8 / 24 / 40",
  "4 万元每团队每年；自第二年起计入"],
 ["私有部署（个）", "0 / 0 / 0", "0 / 0 / 1", "0 / 2 / 4",
  "18 万元每项目，按验收确认"],
 ["第一年付费用户约束", "不超过 200 人", "—", "—",
  "来自首年验证容量；本表按 80 / 120 / 180 人测算"],
 ["营业收入 保守", "1.9 万元", "26.4 万元", "63.2 万元",
  "个人 1.9 / 14.4 / 31.2 万元；团队 0 / 12 / 32 万元"],
 ["营业收入 基准", "2.9 万元", "48.8 万元", "199.2 万元",
  "个人 2.9 / 28.8 / 67.2 万元；团队 0 / 20 / 96 万元；部署 0 / 0 / 36 万元"],
 ["营业收入 进取", "4.3 万元", "106.0 万元", "352.0 万元",
  "个人 4.3 / 48 / 120 万元；团队 0 / 40 / 160 万元；部署 0 / 18 / 72 万元"],
 ["经营成本 保守 / 基准 / 进取", "40 / 50 / 65 万元", "55 / 105 / 190 万元", "80 / 190 / 260 万元",
  "研发、模型、市场与交付"],
 ["经营结果 保守", "负 38.1 万元", "负 28.6 万元", "负 16.8 万元", "三年均未转正，用于观察下限"],
 ["经营结果 基准", "负 47.1 万元", "负 56.2 万元", "正 9.2 万元", "第三年进入盈亏平衡上方"],
 ["经营结果 进取", "负 60.7 万元", "负 84.0 万元", "正 92.0 万元", "第三年转正"],
]
n_add = len(ROWS) - len(tf.rows)
if n_add > 0:
    add_rows(tf, n_add)
for ri, vals in enumerate(ROWS):
    set_row(tf, ri, vals)
while len(tf.rows) > len(ROWS):
    del_row(tf, len(tf.rows) - 1)

# 删除资源需求表，资源内容改为正文段落
for cand in list(doc.tables):
    if first_cells(cand)[:2] == ["资源层级", "用途"]:
        cand._element.getparent().remove(cand._element)
        print("  removed 资源需求表")
        break
try:
    find("表 20 分阶段资源需求与用途")._element.getparent().remove(
        find("表 20 分阶段资源需求与用途")._element)
except KeyError:
    pass

# ---- 11.1 风险登记：保留六项最高风险 ----
tr_ = get_table(["风险", "概率", "影响", "预警信号"], 5)
RISKS = [
 ["风险", "概率", "影响", "预警信号", "应对措施"],
 ["输入法信任风险", "高", "极高", "用户担心输入内容被读取，安装后关闭感知",
  "默认最小读取，可逐应用关闭，明示读取范围，独立审计"],
 ["输入延迟风险", "高", "高", "正常打字出现可感延迟，输入法被禁用",
  "快速路径与智能体路径分离，打字链路不调用模型，设延迟上限并降级"],
 ["提示注入与情境污染", "中", "极高", "网页或文档内嵌指令影响计划或授权范围",
  "情境内容与指令隔离，来源标记，计划人工可见，高风险动作二次确认"],
 ["情境误判与隐私", "中", "极高", "敏感字段被读取，用户纠正上升",
  "禁止字段硬屏蔽，最小读取，独立审计"],
 ["执行误操作", "中", "极高", "越权、外发、覆盖或不可逆动作",
  "最小权限，独立审批，预览，沙箱与回滚"],
 ["平台厂商集成同类能力", "中", "高", "操作系统或头部输入法内置相似入口",
  "聚焦跨应用协议与可靠性，入口与模型保持可替换，深化团队治理能力"],
]
n_add = len(RISKS) - len(tr_.rows)
if n_add > 0:
    add_rows(tr_, n_add)
for ri, vals in enumerate(RISKS):
    set_row(tr_, ri, vals)
while len(tr_.rows) > len(RISKS):
    del_row(tr_, len(tr_.rows) - 1)

doc.save(SRC)
print("B3 core done. tables:", len(doc.tables))