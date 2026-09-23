# -*- coding: utf-8 -*-
"""List the TOC rows of v13 with their stored page numbers."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
root = ET.fromstring(xml)
body = root.find(W + 'body')
tables = body.findall(W + 'tbl')
toc = tables[1]

rows = toc.findall(W + 'tr')
print('TOC 行数: %d' % len(rows))
for ri, tr in enumerate(rows):
    cells = []
    for tc in tr.findall(W + 'tc'):
        cells.append(''.join(t.text or '' for t in tc.iter(W + 't')))
    print('  %2d  %s' % (ri, cells))
