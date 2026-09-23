# -*- coding: utf-8 -*-
"""Final format QC: font usage, punctuation, numbers, spacing, table/figure captions."""
import io, re, zipfile
from docx import Document
from docx.oxml.ns import qn

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
out = []

# 1) 字体统计（正文 run 级）
fonts = {}
for p in doc.paragraphs:
    for r in p.runs:
        rf = r._element.find(qn('w:rPr'))
        name = None
        if rf is not None:
            f = rf.find(qn('w:rFonts'))
            if f is not None:
                name = f.get(qn('w:eastAsia')) or f.get(qn('w:ascii'))
        key = name or "(inherit)"
        fonts[key] = fonts.get(key, 0) + 1
out.append("字体使用统计: " + str(sorted(fonts.items(), key=lambda x: -x[1])))

# 2) 中英文间距问题
bad_space = []
for i, p in enumerate(doc.paragraphs):
    t = p.text
    for m in re.finditer(r"[\u4e00-\u9fa5][A-Za-z0-9]", t):
        bad_space.append((i, m.group(0), t[max(0,m.start()-12):m.start()+14]))
    for m in re.finditer(r"[A-Za-z0-9][\u4e00-\u9fa5]", t):
        bad_space.append((i, m.group(0), t[max(0,m.start()-12):m.start()+14]))
out.append(f"中文与英文/数字紧贴处: {len(bad_space)}")
for b in bad_space[:25]:
    out.append(f"   P{b[0]:03d} {b[1]!r} :: ...{b[2]}...")

# 3) 标点规范：英文括号/半角标点混用
half = []
for i, p in enumerate(doc.paragraphs):
    t = p.text
    if re.search(r"[\u4e00-\u9fa5]\(", t) or re.search(r"\)[\u4e00-\u9fa5]", t):
        half.append((i, t[:80]))
    if re.search(r"[\u4e00-\u9fa5],", t):
        half.append((i, "comma:" + t[:80]))
out.append(f"半角括号/逗号混用: {len(half)}")
for h in half[:15]:
    out.append(f"   P{h[0]:03d} {h[1]}")

# 4) 单位与数字
units = []
for i, p in enumerate(doc.paragraphs):
    t = p.text
    if re.search(r"\d+(万元|元|人|个|页|%|万)", t):
        units.append(i)
out.append(f"含数值单位段落: {len(units)}")

# 5) 图题/表题
caps = [p.text.strip() for p in doc.paragraphs if re.match(r"^(图|表) \d+ ", p.text.strip())]
out.append("题注清单:")
for c in caps:
    out.append("   " + c)

# 6) 空段落与连续空行
empties = sum(1 for p in doc.paragraphs if not p.text.strip())
out.append(f"空段落数: {empties}")

# 7) TOC 域检查
dx = zipfile.ZipFile(SRC).read("word/document.xml").decode("utf-8")
out.append(f"TOC 域: fldChar={dx.count('fldChar')} instrText={dx.count('instrText')} TOC指令={'TOC ' in dx}")

io.open(r"T:\创新创业\OwO-master\docs\.qa_v5\fmt_qc.txt","w",encoding="utf-8").write("\n".join(out))
print("\n".join(out))