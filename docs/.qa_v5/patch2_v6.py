# -*- coding: utf-8 -*-
"""第三轮修正：安全门槛表题注编号、市场表放到题注之后、图表编号顺排。"""
import docx
from docx.oxml.ns import qn

DOC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v6.docx"
doc = docx.Document(DOC)
body = doc.element.body


def els():
    return list(body.iterchildren())


def txt_of(el):
    return ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip()


def find_p(text):
    h = [el for el in els() if el.tag.endswith('}p') and txt_of(el) == text]
    assert len(h) == 1, (text, len(h))
    return h[0]


def set_p(el, text):
    rs = el.findall(qn('w:r'))
    ts = rs[0].findall(qn('w:t'))
    ts[0].text = text
    for t in ts[1:]:
        t.text = ""
    for r in rs[1:]:
        for t in r.findall(qn('w:t')):
            t.text = ""


def find_tbl_by_header(prefix):
    for el in els():
        if el.tag.endswith('}tbl') and txt_of(el).startswith(prefix):
            return el
    raise KeyError(prefix)


# ---- A) 安全门槛表（表头“安全目标”）改为 表 12
set_p(find_p("表 11 安全门槛 设计红线 测试指标与生产事故指标"),
      "表 12 安全门槛 设计红线 测试指标与生产事故指标")

# ---- B) 市场测算表：题注在前，表在后
market_cap = find_p("表 11 自下而上的三层市场测算")
market_tbl = market_cap.getprevious()
assert market_tbl.tag.endswith('}tbl')
if market_tbl.getnext() is not market_cap:  # 顺序已正确
    market_tbl.getparent().remove(market_tbl)
    market_cap.addnext(market_tbl)

# ---- C) 后续图表编号顺排
renum = [
    ("表 11 自下而上的三层市场测算", "表 13 自下而上的三层市场测算"),
    ("表 12 竞争类别与差异化策略", "表 14 竞争类别与差异化策略"),
    ("表 12 产品版本与收入结构", "表 15 产品版本与收入结构"),
    ("表 13 单位经济模型的规划假设", "表 16 单位经济模型的规划假设"),
    ("表 14 分阶段研发与市场决策门", "表 17 分阶段研发与市场决策门"),
    ("表 15 项目十二个月核心指标", "表 18 项目十二个月核心指标"),
    ("表 16 团队分工与贡献证据", "表 19 团队分工与贡献证据"),
    ("表 17 项目驱动的人才培养路径", "表 20 项目驱动的人才培养路径"),
    ("表 18 三年经营情景测算（保守 基准 进取）", "表 21 三年经营情景测算（保守 基准 进取）"),
    ("表 19 分层资源需求与用途", "表 22 分层资源需求与用途"),
    ("表 20 项目风险登记表", "表 23 项目风险登记表"),
]
for old, new in renum:  # 先改成占位，避免重名互相覆盖
    for el in els():
        if el.tag.endswith('}p') and txt_of(el) == old:
            set_p(el, "@@" + new)
for el in els():
    if el.tag.endswith('}p'):
        t = txt_of(el)
        if t.startswith("@@"):
            set_p(el, t[2:])

# ---- D) 正文内引用同步
refs = [
    ("第一年经营成本 120 万元与 10.3 节资源需求一致", None),
]
for el in els():
    if not el.tag.endswith('}p'):
        continue
    t = txt_of(el)
    if "上一版表格中的 47.4 万元" in t:
        t2 = t.replace("本版已重算", "本版已重算")
        set_p(el, t2)

# ---- E) 校验
order = []
for el in els():
    if el.tag.endswith('}p'):
        t = txt_of(el)
        if t.startswith('表 ') or t.startswith('图 '):
            order.append(t[:24])
print("\n".join(order))

doc.save(DOC)
print("saved")
