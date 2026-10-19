# -*- coding: utf-8 -*-
"""第五轮：编号补齐（表 11 -> 表 10 起顺排），最终校验。"""
import docx
from docx.oxml.ns import qn

DOC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v6.docx"
doc = docx.Document(DOC)
body = doc.element.body


def els():
    return list(body.iterchildren())


def txt_of(el):
    return ''.join(n.text or '' for n in el.iter(qn('w:t'))).strip()


def set_p(el, text):
    rs = el.findall(qn('w:r'))
    t0 = rs[0].findall(qn('w:t'))
    t0[0].text = text
    for t in t0[1:]:
        t.text = ""
    for r in rs[1:]:
        for t in r.findall(qn('w:t')):
            t.text = ""


renum = [
    ("表 11 安全门槛 设计红线 测试指标与生产事故指标", "表 10 安全门槛 设计红线 测试指标与生产事故指标"),
    ("表 12 自下而上的三层市场测算", "表 11 自下而上的三层市场测算"),
    ("表 13 竞争类别与差异化策略", "表 12 竞争类别与差异化策略"),
    ("表 14 产品版本与收入结构", "表 13 产品版本与收入结构"),
    ("表 15 单位经济模型的规划假设", "表 14 单位经济模型的规划假设"),
    ("表 16 分阶段研发与市场决策门", "表 15 分阶段研发与市场决策门"),
    ("表 17 项目十二个月核心指标", "表 16 项目十二个月核心指标"),
    ("表 18 团队分工与贡献证据", "表 17 团队分工与贡献证据"),
    ("表 19 项目驱动的人才培养路径", "表 18 项目驱动的人才培养路径"),
    ("表 20 三年经营情景测算（保守 基准 进取）", "表 19 三年经营情景测算（保守 基准 进取）"),
    ("表 21 分层资源需求与用途", "表 20 分层资源需求与用途"),
    ("表 22 项目风险登记表", "表 21 项目风险登记表"),
]
for old, new in renum:
    for el in els():
        if el.tag.endswith('}p') and txt_of(el) == old:
            set_p(el, "@@" + new)
for el in els():
    if el.tag.endswith('}p'):
        t = txt_of(el)
        if t.startswith("@@"):
            set_p(el, t[2:])

doc.save(DOC)

lab, fig = [], []
for el in els():
    if el.tag.endswith('}p'):
        t = txt_of(el)
        if t.startswith('表 '):
            lab.append(t[:24])
        elif t.startswith('图 '):
            fig.append(t[:24])
print("== 表 ==")
print("\n".join(lab))
print("== 图 ==")
print("\n".join(fig))
