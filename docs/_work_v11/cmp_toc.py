# -*- coding: utf-8 -*-
"""Compare TOC page numbers between the v10 baseline and the v11 output."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
BASE = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v10.docx"
NEW = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v11.docx"


def toc_rows(path):
    with zipfile.ZipFile(path) as z:
        xml = z.read('word/document.xml').decode('utf-8')
    root = ET.fromstring(xml)
    body = root.find(W + 'body')
    tables = body.findall(W + 'tbl')
    toc = tables[1]  # 2nd table is the TOC table
    rows = []
    for tr in toc.findall(W + 'tr'):
        cells = []
        for tc in tr.findall(W + 'tc'):
            cells.append(''.join(t.text or '' for t in tc.iter(W + 't')))
        rows.append(cells)
    return rows


b = toc_rows(BASE)
n = toc_rows(NEW)
print('%-46s | %-46s' % ('v10 TOC', 'v11 TOC'))
print('-' * 100)
diffs = 0
for rb, rn in zip(b, n):
    if rb != rn:
        diffs += 1
        print('OLD %s' % (rb,))
        print('NEW %s' % (rn,))
print('\nchanged rows: %d / %d' % (diffs, len(b)))
