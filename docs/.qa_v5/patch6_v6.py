# -*- coding: utf-8 -*-
"""第七轮：1.3 独立成节（补小节标题）、旧节号顺延、关键段拆分。"""
import copy
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
    t0 = rs[0].findall(qn('w:t'))
    t0[0].text = text
    for t in t0[1:]:
        t.text = ""
    for r in rs[1:]:
        for t in r.findall(qn('w:t')):
            t.text = ""


def make_para(template_el, text):
    el = copy.deepcopy(template_el)
    set_p(el, text)
    return el


# ---- A) 找到旧的 1.3 标题，在其前面插入新 1.3 标题；旧标题改为 1.4
old13 = find_p("1.3 项目定义与价值主张")
h13 = make_para(old13, "1.3 输入焦点为什么是 AI 意图入口")
old13.addprevious(h13)
set_p(old13, "1.4 项目定义与价值主张")

# ---- B) 拆分“第一…第四”长段
big = None
for el in els():
    if el.tag.endswith('}p') and txt_of(el).startswith("第一，输入是显式意图。"):
        big = el
        break
assert big is not None
parts = [
    "第一，输入是显式意图。屏幕内容只说明用户看到了什么，输入行为说明用户此刻想表达什么。"
    "前者是被动观察，后者是主动声明，两种信号的噪声结构与获取成本完全不同。"
    "把入口放在输入时刻，系统接收的就不再是猜测出来的“可能相关”，而是用户刚刚说出口的目标。",
    "第二，输入焦点天然带有 Origin。系统能够确定用户在哪个应用、哪个窗口、哪个输入控件、"
    "哪份文档的哪个位置发起请求，这使成果返回与责任追溯在结构上成为可能，而不是事后猜测。"
    "没有 Origin，Return to Origin 只能退化为让用户自己找地方粘贴。",
    "第三，输入入口具有跨应用一致性。不同软件的界面各不相同，但几乎所有生产力任务都要经过输入框，"
    "因此输入焦点是少数天然跨应用、可长期复用的统一入口；每增加一个应用，"
    "需要适配的是语境与返回方式，而不是重新定义交互范式。",
    "第四，输入法不能承担无限权限。正因它位于高频、全局、敏感的系统位置，"
    "项目才必须把输入层限定为意图捕获与最小情境，由独立的 Agent Core 承担执行。"
    "这条限制不是工程妥协，而是本项目架构的出发点。",
]
anchor = big
new_els = [make_para(big, p) for p in parts]
# 用第一段替换原段，其余依次插入其后
set_p(big, parts[0])
for el in new_els[1:]:
    anchor.addnext(el)
    anchor = el

# ---- C) 旧的“由此可以明确项目边界”段落重写（作为 1.3 收束）
for el in els():
    if el.tag.endswith('}p') and txt_of(el).startswith("由此可以明确项目边界"):
        set_p(el, "由此可以明确项目边界：Cuttle 的研究对象不是让输入法变得更会写字，"
                  "而是研究用户意图如何从输入时刻进入 AI 系统，并在保持控制权的前提下，"
                  "安全、连续地升级为可验证行动。输入法不是项目的终点，而是控制权与意图质量最优的切入口；"
                  "本计划书后续全部章节都只服务于这一个命题。")
        break

doc.save(DOC)
print("ok")
for el in els():
    if el.tag.endswith('}p'):
        t = txt_of(el)
        if t.startswith("1.") or t.startswith("由此可以明确项目边界"):
            print(t[:40])
        if t.startswith("第一，输入是显式意图") or t.startswith("第二，输入焦点") or t.startswith("第三，输入入口") or t.startswith("第四，输入法"):
            print("  [拆分段]", t[:24])
