# -*- coding: utf-8 -*-
"""Dump the IC-related wording from v13 (4.1 fields, table 3) for a cross-check."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
root = ET.fromstring(xml)
body = root.find(W + 'body')

i = 0
for el in body:
    if el.tag == W + 'tbl':
        cells = [''.join(t.text or '' for t in tc.iter(W + 't')) for tc in el.iter(W + 'tc')]
        if any('Intent' in c for c in cells):
            print('=== TABLE 3 (IC 字段定义) ===')
            for c in cells:
                print('   |', c)
            print()
    else:
        for p in el.iter(W + 'p'):
            i += 1
            t = ''.join(x.text or '' for x in p.iter(W + 't'))
            if re.search(r'Intent|Scope|Freshness|Provenance|Origin|六个字段|授权范围|同意边界', t):
                print('P#%d %s' % (i, t[:400]))
