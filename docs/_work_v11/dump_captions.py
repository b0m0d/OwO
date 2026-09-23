# -*- coding: utf-8 -*-
"""Dump raw XML for the 图6..图11 caption paragraphs (text-run layout)."""
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v12.docx"
with zipfile.ZipFile(SRC) as z:
    xml = z.read('word/document.xml').decode('utf-8')

for label in ('图6', '图7', '图8', '图9', '图10', '图11'):
    # find the caption occurrence: it is the last occurrence of the label
    # preceded by <w:t> inside a short run
    for m in re.finditer(r'<w:t[^>]*>' + re.escape(label) + r'</w:t>', xml):
        seg = xml[m.start():m.start() + 420]
        seg = re.sub(r'<w:rPr>.*?</w:rPr>', '<rPr/>', seg, flags=re.S)
        print('%-5s @%-7d %s' % (label, m.start(), seg[:300]))
    print('-' * 100)
