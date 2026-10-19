# -*- coding: utf-8 -*-
"""Check for soft line breaks (w:br) inside TOC cells and measure each label's width."""
import re
import zipfile

import pymupdf
from PIL import ImageFont

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v15.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
toc = xml[starts[1]:ends[1]]

print('=== TOC 内的 <w:br/> 数量: %d ===' % len(re.findall(r'<w:br/>', toc)))
for m in list(re.finditer(r'<w:t[^>]*>([^<]*)</w:t><w:br/>', toc))[:8]:
    print('   换行点在: %r 之后' % m.group(1))

# measure natural width of each label at 9.5pt with the reporting font
CELL_W_PT = 2660 / 20.0          # twips -> pt
INDENT_PT = 125 / 20.0
print('\n标签列可用宽度 = %.1f pt (含缩进 %.1f pt)' % (CELL_W_PT, INDENT_PT))

try:
    font = ImageFont.truetype(r'C:\Windows\Fonts\msyh.ttc', 19)  # 9.5pt * 2 for accuracy then /2
    have = True
except Exception as e:
    have = False
    print('字体加载失败: %s' % e)

if have:
    labels = re.findall(r'<w:t(?: [^>]*)?>([^<]{2,})</w:t>', toc)
    seen = set()
    over = []
    for lab in labels:
        lab = lab.strip()
        if not lab or lab.isdigit() or lab in seen:
            continue
        seen.add(lab)
        w = font.getlength(lab) / 2.0
        if w + INDENT_PT > CELL_W_PT:
            over.append((w + INDENT_PT, lab))
    over.sort(reverse=True)
    print('\n超出列宽、会折行的条目 (%d 条):' % len(over))
    for w, lab in over:
        print('   %6.1f pt  %s' % (w, lab))
