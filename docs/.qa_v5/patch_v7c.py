# -*- coding: utf-8 -*-
"""v7 收尾补丁：从当前 v7 文件继续，做最后一次表号统一与残留检查。"""
import docx

DOC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v7.docx"
doc = docx.Document(DOC)


def find_p(sub):
    hits = [p for p in doc.paragraphs if sub in p.text]
    return hits[0] if hits else None


def set_text(p, text):
    runs = p.runs
    if runs:
        runs[0].text = text
        for r in runs[1:]:
            r.text = ""
    else:
        p.add_run(text)


def repl(old_sub, new_text):
    p = find_p(old_sub)
    if p is None:
        print("  skip (not found):", old_sub[:40])
        return
    print("  set :", p.text.strip()[:36], "->", new_text[:36])
    set_text(p, new_text)


# 第十章标题与图表题注统一
repl("10.2 三情景三年经营测算", "10.2 三年经营测算（统一口径）")
repl("图 11 Cuttle 三情景三年收入与成本规划", "图 11 Cuttle 三年收入与成本（统一口径）")
repl("表 19 三年经营情景测算（保守 基准 进取）", "表 20 三年经营测算")
repl("表 20 分层资源需求与用途", "表 21 分层资源需求与用途")
repl("表 16 项目十二个月核心指标", "表 18 项目十二个月核心指标")
repl("表 17 团队分工与贡献证据", "表 19 团队分工与贡献证据")
repl("表 18 项目驱动的人才培养路径", "表 20 项目驱动的人才培养路径")
repl("表 21 项目风险登记表", "表 23 项目风险登记表")

doc.save(DOC)

# ---- 编号自检
lab, fig = [], []
for p in doc.paragraphs:
    t = p.text.strip()
    if t.startswith('表 '):
        lab.append(t[:40])
    elif t.startswith('图 '):
        fig.append(t[:40])
print("\n表题注:")
for x in lab:
    print("  ", x)
print("图题注:")
for x in fig:
    print("  ", x)
