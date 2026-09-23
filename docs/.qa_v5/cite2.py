# -*- coding: utf-8 -*-
"""Cite the last five sources where they genuinely belong, then renumber the list."""
import copy, re
from docx import Document
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

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

def E(prefix, text):
    set_text(find(prefix), text)

# 1) 表 6 首年容量行补教育统计引用（原 [4] 的合理落点）
for t in doc.tables:
    fc = [c.text.strip() for c in t.rows[0].cells]
    if fc[:2] == ["口径", "测算对象"]:
        for row in t.rows:
            if row.cells[0].text.strip().startswith("中长期市场空间"):
                ps = row.cells[2].paragraphs
                set_text(ps[0], "引用我国生成式人工智能用户规模、软件业务收入以及高等教育在学总规模等公开统计数据作为行业背景[4]")
                print("  表 6 引用补充")
        break

# 2) 4.6 实验设计引用评测基准
E("表 5 三组对照实验与消融实验设计" if False else "为保证三组对照的可比性",
  "为保证三组对照的可比性，各组使用相同的模型版本、工具权限、任务材料与结果要求，"
  "仅改变任务入口与情境获取机制；实验采用同组内交叉设计并对顺序做平衡处理，以消除学习效应。"
  "任务套件的构建参照软件工程、网页操作与桌面环境三类公开评测的任务化设计方式，"
  "以真实任务而非合成样例作为评价对象。[18][19][20]")

# 3) 8.4 指标体系引用质量模型
E("核心验证指标与阶段目标见表 2",
  "指标体系参照软件产品质量模型中的功能适合性、可靠性、易用性与安全性等维度组织。[29]"
  "核心验证指标与阶段目标见表 2，三年经营口径见表 9。全部指标共用一套统计口径："
  "以配对样本比较为主，报告效应量与置信区间；样本量在正式实验前依据预实验效应量做统计功效分析后确定；"
  "所有指标注明数据来源、标注规则与样本量。原地闭环完成率是首要指标，其余指标用于解释其高低变化。")

doc.save(SRC)
print("extra citations inserted")