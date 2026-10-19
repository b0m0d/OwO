# -*- coding: utf-8 -*-
"""Dump the TOC table's trPr/trHeight and one row's raw XML."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v15.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

# isolate TOC table
starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
toc = xml[starts[1]:ends[1]]

print('=== trPr 出现次数: %d ===' % len(re.findall(r'<w:trPr>', toc)))
for m in list(re.finditer(r'<w:trPr>.*?</w:trPr>', toc, re.S))[:3]:
    print('  ', m.group(0))

print('\n=== 首行原始 XML（前 1800 字符）===')
first_row = re.search(r'<w:tr>.*?</w:tr>', toc, re.S).group(0)
print(first_row[:1800])
