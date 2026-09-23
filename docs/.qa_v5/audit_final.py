# -*- coding: utf-8 -*-
import re
from docx import Document
from docx.oxml.ns import qn
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)

def set_text(p, text):
    runs = p.runs
    runs[0].text = text
    for r in runs[1:]:
        r.text = ""; r._element.getparent().remove(r._element)

for x in doc.paragraphs:
    if x.text.strip().startswith("项目资源需求分三层表述"):
        set_text(x, "项目资源需求分三层表述。已有资源：学校实验室与实验工位、团队成员个人设备、指导教师的方法与安全指导、"
                    "开源模型与本地推理能力，这些不计入现金需求，因此资金来源中团队自筹与学校支持的占比高于对外融资。"
                    "新增现金需求分两批：第一年 50 万元，用于把原型推进到可验证的试点闭环并完成对照、消融与红队测试；"
                    "第二年追加 110 万元，用于场景复制、团队版交付与兼容矩阵扩展。首轮对外表述的资源配置总额为 160 万元，"
                    "按决策门分两批释放，任一批未达标即暂停后续投入；保守情景下前两年累计亏损约 59 万元，"
                    "基准情景约 97 万元，该额度可覆盖基准情景所需并保留安全边际。")
doc.save(SRC)

# ---- full consistency audit ----
d = Document(SRC)
issues = []
paras = d.paragraphs
for p in paras:
    if p.style.name.startswith("Heading") and not p.text.strip():
        issues.append("EMPTY HEADING")
    t = p.text.strip()
    if t.endswith(("，", "、", "：", "；")) and len(t) > 25:
        issues.append("TRUNCATION?: " + t[:60])
caps_t = [p.text.strip() for p in paras if re.match(r"^表 \d+ ", p.text.strip())]
caps_f = [p.text.strip() for p in paras if re.match(r"^图 \d+ ", p.text.strip())]
tn = [int(re.match(r"^表 (\d+)", c).group(1)) for c in caps_t]
fn = [int(re.match(r"^图 (\d+)", c).group(1)) for c in caps_f]
if tn != list(range(1, len(tn)+1)): issues.append(f"TABLE SEQ {tn}")
if fn != list(range(1, len(fn)+1)): issues.append(f"FIGURE SEQ {fn}")

full = "\n".join(p.text for p in paras) + "\n" + "\n".join(c.text for t in d.tables for r in t.rows for c in r.cells)
banned = ["Swarm", "三个核心创新", "有统计意义", "R Risk", "设备网格", "连续自治", "交易额分成 15", "命中",
          "参赛主体", "决定任务层级", "作为所有 Agent 前的意图入口"]
for b in banned:
    if b in full: issues.append("BANNED: " + b)
must = ["受控自治升级", "适当信任", "任务契约（Task Contract）", "成果契约（Artifact Contract）",
        "Scope 规定的是执行权限的上界", "禁止访问字段访问率", "越权拦截率", "学生主导、教师指导",
        "知识工作的直接价值", "可积累优势与拟形成壁垒", "参考市场边界", "首批可触达验证样本池",
        "全局快捷键 Agent", "20 份定价访谈", "第一年规模约束", "三者在权限、能力与数据位置的约束下组合",
        "入口与情境获取的组合设计"]
for mk in must:
    if mk not in full: issues.append("MISSING: " + mk)
print("paras", len(paras), "tables", len(d.tables), "refs", sum(1 for p in paras if re.match(r"^\[\d+\]", p.text.strip())))
print("ISSUES:", len(issues))
for i in issues: print("  !", i)