# -*- coding: utf-8 -*-
"""第八轮：5.3 段落顺序与说明整合。"""
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


def set_p(el, text):
    rs = el.findall(qn('w:r'))
    t0 = rs[0].findall(qn('w:t'))
    t0[0].text = text
    for t in t0[1:]:
        t.text = ""
    for r in rs[1:]:
        for t in r.findall(qn('w:t')):
            t.text = ""


def find_p(prefix):
    h = [el for el in els() if el.tag.endswith('}p') and txt_of(el).startswith(prefix)]
    assert len(h) == 1, (prefix, len(h))
    return h[0]


# 找到三段相关段落与题注
lead = find_p("除通用安全目标之外")
note_ov = find_p("过度感知与过度自治是 Cuttle 特有的安全问题")
note_safe = find_p("安全门槛分为三组")
cap = find_p("表 10 安全门槛")

# 合并说明：删除旧的 note_ov，重写 lead 与 note_safe
set_p(lead,
    "除通用安全目标之外，项目把两个与输入法入口强相关的风险作为专有指标管理："
    "情境过度读取率（Context Overreach Rate）指为完成当前任务读取了并不必需的数据，"
    "典型反例是用户只要求润色一句话，系统却读取整篇文档；"
    "自治过度升级率（Autonomy Overreach Rate）指系统采取了超出用户意图所需的执行层级，"
    "典型反例是用户只要求想一句回复，系统却直接打开会话发送。"
    "两者的直接后果不是任务失败，而是信任受损，因此在指标体系中的优先级高于普通效率指标。"
    "对应的设计约束是：情境读取必须与具体意图绑定，任何超出当前意图的读取都需要用户当次确认。")
set_p(note_safe,
    "表中门槛分为三组，避免把设计原则与实验统计混为一谈。"
    "设计红线属于实现约束，不满足即不允许发布：明确标识的密码框、支付确认与金融窗口禁止读取率须为 100%，"
    "高风险动作未经授权执行次数须为 0。测试指标属于红队与故障注入的统计结果，"
    "用于度量系统在未知、伪装与边界场景下的泛化能力，例如伪装敏感字段漏检率不高于 1%、"
    "可逆任务恢复成功率不低于 90%、审批影响范围判断正确率不低于 95%。"
    "生产事故指标属于运营阶段的目标值，允许出现个例，但必须分级定责、限时收敛，并在下个版本以回归测试固化。"
    "把三组分开表述的原因很简单：把“绝对为 0”当作泛化能力的统计描述，既不可验证，也会掩盖真实风险。")

# 删除旧的 note_ov，并把 lead/note_safe 排到题注之前
note_ov.getparent().remove(note_ov)
par = note_safe.getparent()
for el in (lead, note_safe):
    el.getparent().remove(el)
    cap.addprevious(el)
# 顺序：lead -> note_safe -> cap

doc.save(DOC)
print("ok")
for el in els():
    if el.tag.endswith('}p'):
        t = txt_of(el)
        if t.startswith(("5.3", "除通用安全目标", "表中门槛", "表 10 安全门槛")):
            print(t[:60])
