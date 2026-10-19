# -*- coding: utf-8 -*-
import re
from docx import Document
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, text):
    runs = p.runs
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

def find(prefix):
    for p in doc.paragraphs:
        if p.text.strip().startswith(prefix):
            return p
    raise KeyError(prefix)

set_text(find("表 11 团队分工与工程基础"), "表 10 团队分工与工程基础")
set_text(find("表 10 三年经营预测与资源需求"), "表 11 三年经营预测与资源需求")
set_text(find("表 12 项目风险登记表"), "表 12 项目风险登记表")

# 修正引用与 8.4 说明
for p in doc.paragraphs:
    t = p.text
    if "表 10" in t and "团队" in t:
        set_text(p, t.replace("表 10", "表 10"))
    if t.startswith("全部指标共用一套统计口径"):
        set_text(p, "核心指标与阶段目标见表 3，三年经营口径见表 11。全部指标共用一套统计口径："
                    "以配对样本比较为主，报告效应量与置信区间；样本量在正式实验前依据预实验效应量做统计功效分析后确定；"
                    "所有指标注明数据来源、标注规则与样本量。原地闭环完成率是首要指标，其余指标用于解释其高低变化。")

# 10.3 资源需求：表已删除，内容改为正文
for p in doc.paragraphs:
    if p.text.strip().startswith("项目资源需求分三层表述"):
        set_text(p, "项目已有资源包括学校实验室与实验工位、团队成员个人设备、指导教师的方法与安全指导、"
                    "开源模型与本地推理能力，这些不计入现金需求，因此资金来源中团队自筹与学校支持的占比高于对外融资。"
                    "新增现金需求分两批投入：第一年 50 万元，用于把原型推进到可验证的试点闭环，并完成对照、消融与红队测试，"
                    "其中产品与研发占 30%、评测与安全占 20%、市场与试点占 25%、模型与基础设施占 15%、"
                    "知识产权与预备金占 10%；第二年追加 110 万元，用于场景复制、团队版交付、兼容矩阵扩展与支持运维。"
                    "首轮对外表述的资源配置总额为 160 万元，按决策门分两批释放，任一批未达标即暂停后续投入。"
                    "保守情景下前两年累计亏损约 67 万元，基准情景约 103 万元，该额度可覆盖基准情景所需并保留安全边际。")
doc.save(SRC)
print("renumber fixed")
for p in doc.paragraphs:
    t = p.text.strip()
    if re.match(r"^表 \d+ ", t): print("  ", t[:52])