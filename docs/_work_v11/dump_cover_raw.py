# -*- coding: utf-8 -*-
"""Dump raw XML of the cover table's trPr/tcPr to learn the real shapes."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
cover = xml[starts[0]:ends[0]]

print('trPr 片段:')
for m in re.finditer(r'<w:trPr>.*?</w:trPr>', cover, re.S):
    print('  ', m.group(0))

print('\ntcPr 片段（前 3 个）:')
for m in list(re.finditer(r'<w:tcPr>.*?</w:tcPr>', cover, re.S))[:3]:
    print('  ', m.group(0))

print('\nspacing 片段（前 4 个）:')
for m in list(re.finditer(r'<w:spacing[^>]*/>', cover))[:4]:
    print('  ', m.group(0))

print('\n首行 tc 的完整 XML:')
print(re.search(r'<w:tr>.*?</w:tr>', cover, re.S).group(0)[:1200])
