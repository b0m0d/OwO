# -*- coding: utf-8 -*-
from docx import Document
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
def set_text(p, text):
    runs = p.runs
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)
for p in doc.paragraphs:
    if p.text.strip().startswith("本章用于检验商业模型能否形成可持续经营"):
        set_text(p, "本章用于检验商业模型能否形成可持续经营，不代表项目已经取得收入、融资或用户规模。"
                    "收入按以下方式确认：个人专业版按年末在付用户数乘年费 240 元计算；团队许可按 4 万元每团队每年计价，"
                    "自第二年起计入；私有部署按 18 万元每项目、以验收确认；每年在付用户由上年在付规模的 60% 续订加当年新增构成。"
                    "三种情景分别设定用户与客户数量，按同一单价假设计算收入。第一年的规模由第六章的验证容量约束，"
                    "个人付费用户不超过 200 人，三种情景分别按 80、120 与 180 人测算；"
                    "深度服务与完整观察同样按约 200 人配置，第一年的重心是验证而非规模。"
                    "全部单价与转化率均属待验证假设，验证方式与门槛见表 9。")
doc.save(SRC)
print("ok")