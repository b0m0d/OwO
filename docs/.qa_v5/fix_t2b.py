# -*- coding: utf-8 -*-
"""Batch B: 6.3 duplicate removal, finance growth logic and cost control, funding wording,
market table note, IC table de-anglicisation, expansion labelling."""
import copy
from docx import Document
from docx.text.paragraph import Paragraph
from docx.oxml.ns import qn

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

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

def E(prefix, text):
    set_text(find(prefix), text)

def drop(prefix):
    p = find(prefix); p._element.getparent().remove(p._element)
    print("  dropped:", prefix[:36])

def clone_after(anchor, text):
    model = None
    for p in doc.paragraphs:
        if p.style.name == "Normal" and len(p.text) > 60:
            model = p; break
    el = copy.deepcopy(model._element)
    anchor._element.addnext(el)
    np = Paragraph(el, anchor._parent)
    set_text(np, text)
    return np

def first_cells(t):
    return [c.text.strip() for c in t.rows[0].cells]

def get_table(sig, ncols=None):
    for t in doc.tables:
        fc = first_cells(t)
        if fc[:len(sig)] == sig and (ncols is None or len(fc) == ncols):
            return t
    raise KeyError(str(sig))

# ---- 6.3 删除重复长条目，只留四条 ----
drop("项目核心竞争力主要形成于三个方向")
E("生态与信任的长期积累将随后续版本与真实使用逐步形成",
  "生态与信任：随团队许可客户、开发者伙伴与真实任务量同步积累，形成应用适配与安全验证的正向循环。"
  "以上能力将在应用适配、真实任务与安全验证中持续积累。")

# ---- 表 3（IC 字段）去掉自创英文短语 ----
t = get_table(["IC 字段", "含义"], 5)
NEWIC = [
 ["IC 字段", "含义", "作用", "失效事件", "用户控制"],
 ["I — Intent", "任务意图", "记录用户目标与对象关系", "用户纠正、任务结束", "直接修改"],
 ["C — Context", "最小情境", "只保留完成当前任务所需的最少信息", "超出当前意图、任务结束", "按应用与字段关闭"],
 ["O — Origin", "来源位置", "标识当前应用、窗口、输入焦点与对象", "切换应用、关闭窗口", "查看、关闭感知"],
 ["P — Provenance", "证据来源", "记录每条情境结论的来源与获取方式", "来源变化、依赖失效", "查看来源"],
 ["F — Freshness", "时效", "设定有效期与失效条件", "超过期限、对象修改", "查看、手动失效"],
 ["S — Scope", "授权范围", "限定可读取范围与可执行范围，并记录同意边界", "授权变更、范围外读取请求", "确认、拒绝、撤回授权"],
]
for ri, vals in enumerate(NEWIC):
    for ci, v in enumerate(vals):
        set_cell(t.rows[ri].cells[ci], v)

# ---- 7.x 成本控制机制（表 9 后的正文说明） ----
cap9 = find("表 8 产品版本、定价与单位经济")
n = clone_after(cap9,
  "成本侧的规划假设为：本地模型承担补全、改写与情境整理等高频低成本任务；"
  "云端模型采用额度限制，超出部分按用量计费；重型智能体任务按月度额度或增量计费；"
  "团队与机构客户可以自带模型或 API。单用户模型成本的目标区间依据任务日志与用量记录持续校准，"
  "在评测数据形成之前按本表的规划假设处理。")

# ---- 10.1 前后加增长逻辑 ----
anchor = find("10.1 财务假设与测算基础")
n = clone_after(anchor,
  "收入增长来自三条可核验的路径，而不是场景数量的简单扩张：一是首批试点场景向同类组织复制，"
  "并向开发者社群扩散，形成自然新增；二是个人用户在团队内部扩散，带动团队许可采购；"
  "三是随着兼容应用范围扩大与渠道合作推进，个人订阅的转化率与留存率同步提升。"
  "第二年的增长主要来自前两条路径，第三年的增长主要来自第三条路径与团队客户的续订扩容。")

# ---- 10.3 资金表述与来源比例 ----
E("项目已有资源包括学校实验室与实验工位",
  "项目已有资源包括学校实验室与实验工位、团队成员个人设备、指导教师的方法与安全指导、"
  "开源模型与本地推理能力，这些不计入现金需求。项目未来两年预计新增资金需求 160 万元，"
  "其中第一年 50 万元、第二年 110 万元。资金来源的规划比例为：学校与竞赛创新基金支持约 40%，"
  "团队自筹约 25%，外部天使与产业合作约 35%。第一年资金用于把原型推进到可验证的试点闭环，"
  "并完成对照、消融与红队测试，其中产品与研发占 30%、评测与安全占 20%、市场与试点占 25%、"
  "模型与基础设施占 15%、知识产权与预备金占 10%；第二年资金用于场景复制、团队版交付、"
  "兼容矩阵扩展与支持运维。资金按决策门分两批释放，任一批未达标即暂停后续投入。"
  "保守情景下前两年累计亏损约 67 万元，基准情景约 103 万元，该额度可覆盖基准情景所需并保留安全边际。")

doc.save(SRC)
print("batch B done")