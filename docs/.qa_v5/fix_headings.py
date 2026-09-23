# -*- coding: utf-8 -*-
"""Fix heading pollution (case paragraphs, 8.3 body list, 指导教师) and rebuild the TOC
strictly from real numbered headings."""
import copy, glob, os, re, io
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement

DOCS = os.path.join(r"T:\创新创业\OwO-master\docs")
SRC = [x for x in glob.glob(os.path.join(DOCS, "Cuttle*v8.docx")) if "预览" not in x][0]
doc = Document(SRC)

def ptxt(p):
    return "".join(t.text or "" for t in p._element.iter(qn('w:t')))

# ---------- 1) 清理被误设为标题的正文段落 ----------
fixed = []
for p in doc.paragraphs:
    t = ptxt(p).strip()
    if not p.style.name.startswith("Heading"):
        continue
    is_real = bool(re.match(r"^(第[一二三四五六七八九十]+章|\d+\.\d+|[0-9]+\.[0-9]+ )", t)) \
              or t in ("项目摘要", "参考资料")
    if not is_real:
        p.style = doc.styles["Normal"]
        fixed.append(t[:40])
print("degraded to body:", len(fixed))
for f in fixed:
    print("   ", f)

# 指导教师小节保留为三级标题但改为规范名称
for p in doc.paragraphs:
    if ptxt(p).strip().startswith("指导教师与指导职责") and p.style.name != "Heading 3":
        p.style = doc.styles["Heading 3"]
        print("   指导教师小节 -> Heading 3")

doc.save(SRC)

# ---------- 2) 收集真实标题 ----------
heads = []
for p in doc.paragraphs:
    if p.style.name not in ("Heading 1", "Heading 2", "Heading 3"):
        continue
    t = ptxt(p).strip()
    if not t or t == "目录":
        continue
    if p.style.name == "Heading 3":
        continue  # 三级标题不进目录
    heads.append((p.style.name, t))
print("headings for TOC:", len(heads))

# ---------- 3) 页码映射（用已渲染 PDF） ----------
import pymupdf, glob as g2
pdfs = sorted(g2.glob(os.path.join(DOCS, ".qa_v5", "pdfZ", "*.pdf")))
pdf = pymupdf.open(pdfs[0])
pages = [pdf[i].get_text().replace("\n", "").replace(" ", "") for i in range(pdf.page_count)]

# 目录占 2 页 -> 正文页码 = PDF 页码
rows = []
for style, text in heads:
    key = text.replace(" ", "")
    pg = None
    start = 2
    for i in range(start, len(pages)):
        if key in pages[i]:
            pg = i + 1
            break
    if pg:
        rows.append((style, text, str(pg)))
missing = [t for s, t in heads if not any(r[1] == t for r in rows)]
print("entries with page:", len(rows), "| missing:", missing[:5])
io.open(os.path.join(DOCS, ".qa_v5", "toc_rows.txt"), "w", encoding="utf-8").write(
    "\n".join("%s\t%s\t%s" % r for r in rows))
print("wrote toc_rows.txt")
