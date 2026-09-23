# -*- coding: utf-8 -*-
"""Anchor captions to their figures and scale figures slightly to remove orphan captions and blank space."""
from docx import Document
from docx.oxml.ns import qn
from docx.shared import Cm

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
NS_WP = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
NS_A = "http://schemas.openxmlformats.org/drawingml/2006/main"

TARGET_W = Cm(14.2)

def find_image_para(cap_para):
    """Return (para, inline_element) for the paragraph holding the image above the caption."""
    prev = cap_para._element.getprevious()
    while prev is not None:
        ext = prev.findall('.//{%s}extent' % NS_WP)
        if ext:
            return prev, ext[0]
        prev = prev.getprevious()
    return None, None

count = 0
for p in doc.paragraphs:
    if not p.text.strip().startswith("图 "):
        continue
    holder, ext = find_image_para(p)
    if holder is None:
        continue
    from docx.text.paragraph import Paragraph
    hp = Paragraph(holder, p._parent)
    # 1) 缩放图片
    cx = int(ext.get("cx")); cy = int(ext.get("cy"))
    ratio = cy / float(cx)
    new_cx = int(TARGET_W)
    new_cy = int(new_cx * ratio)
    ext.set("cx", str(new_cx)); ext.set("cy", str(new_cy))
    for el in holder.findall('.//{%s}ext' % NS_A):
        el.set("cx", str(new_cx)); el.set("cy", str(new_cy))
    # 2) 图与题注连在一起，避免题注孤立在一页
    hp.paragraph_format.keep_with_next = True
    hp.paragraph_format.space_after = 0
    p.paragraph_format.keep_with_next = False
    count += 1

doc.save(SRC)
print("figures rescaled & anchored:", count)