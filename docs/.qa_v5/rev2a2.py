# -*- coding: utf-8 -*-
"""v8 revision round 2, part A2: financial chapter rebuilt with three scenarios."""
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
        r.text = ""
        r._element.getparent().remove(r._element)

def set_cell(cell, text):
    ps = cell.paragraphs
    set_text(ps[0], text)
    for extra in ps[1:]:
        extra._element.getparent().remove(extra._element)

def set_row(table, ri, values):
    for ci, v in enumerate(values):
        set_cell(table.rows[ri].cells[ci], v)

def find_p(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

def E(prefix, text):
    p = find_p(prefix); set_text(p, text); return p

E("本章用于检验商业模型能否形成可持续经营",
  "本章用于检验商业模型能否形成可持续经营，不代表项目已经取得收入、融资或用户规模。"
  "测算采用统一口径，三种情景分别设定用户与客户数量，再按同一单价假设计算收入："
  "个人专业版按年末在付用户数乘年费 240 元确认收入；团队许可按 4 万元每年计价，自第二年起计入；"
  "私有部署按 18 万元每项目、以验收确认收入；每年在付用户由上年在付规模的 60% 续订加当年新增构成。"
  "第一年的规模上限由第六章的验证容量约束，任何情景的第一年在付用户都不超过可直接服务的用户数。"
  "全部单价与转化率均属待验证假设，验证方式与门槛见表 14。")

E("价格假设的验证优先于规模预测",
  "价格假设的验证优先于规模预测。前三个月完成三项最低限度的价格证据：一是 20 份定价访谈，"
  "对象为高频 PC 知识工作者，覆盖运营、项目、咨询、研究、行政与开发岗位，用于确认年费 240 元与"
  "席位计价 4 万元的可接受区间；二是 3 份试点意向，用于确认团队许可的采购流程与预算归属；"
  "三是首份报价反馈，记录真实客户对报价范围的异议与调整意见。上述证据用于修正定价与转化率假设；"
  "在证据取得之前，本章全部数字按情景假设处理。")

# 新增“第一年规模约束”行
t = doc.tables[20]
src_tr = t.rows[3]._element
new_tr = copy.deepcopy(src_tr)
t.rows[3]._element.addnext(new_tr)
row_new = _Row(new_tr, t)
for ci, v in enumerate(["第一年规模约束", "首批可服务用户约 200 人", "—", "—",
                        "来自表 11 的验证容量，不是市场推算"]):
    set_cell(row_new.cells[ci], v)

ROWS = [
 None,
 ["个人专业版 付费个人（年末在付）",
  "150 / 250 / 450", "500 / 1000 / 1600", "1100 / 2400 / 4000",
  "保守 / 基准 / 进取。年费 240 元；年度续费率按 60% 计提"],
 ["团队许可（个）",
  "0 / 0 / 0", "3 / 5 / 12", "8 / 24 / 40",
  "4 万元每年每客户；按路线图自第二年起产生收入"],
 ["私有部署（个）",
  "0 / 0 / 0", "0 / 0 / 1", "0 / 2 / 4",
  "18 万元每项目，按验收确认"],
 ["营业收入 保守",
  "3.6 万元", "31.8 万元", "151.8 万元",
  "个人订阅 3.6 万 + 团队 12 万 + 部署 18 万 + 个人 120 万"],
 ["营业收入 基准",
  "6.0 万元", "63.6 万元", "303.6 万元",
  "个人订阅 6 万 + 团队 20 万 + 部署 36 万 + 个人 241.6 万"],
 ["营业收入 进取",
  "10.8 万元", "116.4 万元", "577.8 万元",
  "个人订阅 10.8 万 + 团队 48 万 + 部署 72 万 + 个人 447 万"],
 ["经营成本 保守 / 基准 / 进取",
  "40 / 50 / 65 万元", "70 / 110 / 150 万元", "120 / 190 / 240 万元",
  "研发、模型、市场与交付"],
 ["经营结果 保守",
  "负 36.4 万元", "负 38.2 万元", "正 31.8 万元",
  "第三年进入盈亏平衡上方"],
 ["经营结果 基准",
  "负 44 万元", "负 46.4 万元", "正 113.6 万元",
  "第三年进入盈亏平衡上方"],
 ["经营结果 进取",
  "负 54.2 万元", "负 33.6 万元", "正 337.8 万元",
  "第二年亏损收窄，第三年转正"],
]
for ri, vals in enumerate(ROWS):
    if vals is None:
        continue
    set_row(t, ri, vals)

E("表 19 三年经营测算（三情景，统一口径）",
  "表 19 三年经营测算（三情景，各自设定用户与客户数量）")
E("图 11 Cuttle 三年经营测算（三情景，统一口径）",
  "图 11 Cuttle 三年经营测算（三情景，各自设定数量与统一单价）")

E("项目资源需求分三层表述",
  "项目资源需求分三层表述。已有资源：学校实验室与实验工位、团队成员个人设备、指导教师的方法与安全指导、"
  "开源模型与本地推理能力，这些不计入现金需求，因此资金来源中团队自筹与学校支持的占比高于对外融资。"
  "新增现金需求第一年 40 万元，用于把原型推进到可验证的试点闭环并完成对照、消融与红队测试；"
  "第二年追加 100 万元，用于场景复制、团队版交付与兼容矩阵扩展。首轮对外表述的资源配置总额为 140 万元，"
  "按决策门分两批释放，任一批未达标即暂停后续投入，该额度覆盖保守情景下前两年的累计亏损。")
set_row(doc.tables[21], 3, ["规模化资源（第二年）",
                            "场景复制、团队版交付、兼容矩阵扩展、支持与运维",
                            "100 万元", "第二批试点、团队许可交付能力与合作拓展"])

# 第八章路线图：明确首批付费团队与私有部署时点
set_row(doc.tables[16], 4, ["13 至 18 个月", "团队版与版本化成果契约接口",
                            "验证协同净收益与团队交付成本",
                            "首批付费团队（当年确认团队许可收入）",
                            "续用意愿与可控交付成本"])
set_row(doc.tables[16], 5, ["19 至 24 个月", "规模化复制与工程加固",
                            "跨设备最小数据原则验证（扩展）",
                            "机构合作、私有部署首单与场景复制",
                            "合同、回款与支持成本可控"])
doc.save(SRC)

d = Document(SRC)
t = d.tables[20]
print("rows:", len(t.rows))
for ri, r in enumerate(t.rows):
    print(f"R{ri}: " + " | ".join(c.text.strip()[:34] for c in r.cells))