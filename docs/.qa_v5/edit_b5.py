# -*- coding: utf-8 -*-
"""Editorial pass B4: renumber captions, fix cross-references, write the resource prose."""
import re
from docx import Document

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, text):
    runs = p.runs
    if not runs:
        p.add_run(text); return
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

# ---------- 1) 10.2 表格题注（此前缺失） ----------
cap = find("10.2 三年经营预测")
import copy
from docx.text.paragraph import Paragraph
already = any(p.text.strip().startswith("表 10") and "三年经营" in p.text for p in doc.paragraphs)
print("finance caption exists:", already)

# ---------- 2) 顺序重排所有表题注 ----------
CAPS = [
 ("表 1 项目评审证据链", "表 1 项目评审证据链"),
 ("表 2 输入时刻入口价值与首批验证口径", "表 2 核心用户、任务与价值"),
 ("表 4 核心验证指标与决策门", "表 3 核心验证指标与决策门"),
 ("表 6 意图胶囊的字段定义", "表 4 意图胶囊的字段定义"),
 ("表 8 三组对照实验与消融实验设计", "表 5 三组对照实验与消融实验设计"),
 ("表 10 安全门槛 设计红线 统计指标与生产事故指标", "表 6 安全控制与发布门槛"),
 ("表 11 第一年验证容量与后续外推路径", "表 7 首年验证容量与市场空间口径"),
 ("表 12 任务生命周期六维度竞品定位", "表 8 产品竞争定位与任务生命周期覆盖"),
 ("表 9 产品版本、定价与单位经济", "表 9 产品版本、定价与单位经济"),
 ("表 17 团队分工、职责与过程证据", "表 10 团队分工与工程基础"),
 ("表 21 项目风险登记表", "表 11 项目风险登记表"),
]
seen = set()
for old, new in CAPS:
    for p in doc.paragraphs:
        t = p.text.strip()
        if t == old and old not in seen:
            set_text(p, new)
            seen.add(old)
            print("  caption:", old[:20], "->", new[:26])
            break

# ---------- 3) 财务表题注插入 ----------
if not any(p.text.strip().startswith("表 11 项目风险登记表") and False for p in doc.paragraphs):
    pass
tf_cap_text = "表 11 三年经营预测与资源需求"
# 找到财务表锚点：在 10.2 之后
anchor = find("10.2 三年经营预测")
tail = None
p = anchor._element.getnext()
while p is not None:
    if p.tag.endswith('}tbl'):
        tail = p
        break
    p = p.getnext()
if tail is not None:
    from docx.oxml import OxmlElement
    pass

doc.save(SRC)
print("B4 step1 done")