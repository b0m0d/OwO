# -*- coding: utf-8 -*-
"""Dump one TOC cell's raw XML to see how long titles are laid out."""
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v14.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = ET.fromstring(xml).find(W + 'body')
toc = body.findall(W + 'tbl')[1]

for tr in toc.findall(W + 'tr'):
    cells = [''.join(t.text or '' for t in tc.iter(W + 't')) for tc in tr.findall(W + 'tc')]
    for i in (0, 2):
        if i < len(cells) and ('1.3 ' in cells[i] or '4.1 ' in cells[i] or '4.3 ' in cells[i] or '4.5 ' in cells[i]):
            tc = tr.findall(W + 'tc')[i]
            raw = ET.tostring(tc.find(W + 'p'), encoding='unicode')
            import re
            raw = re.sub(r'<ns0:rPr>.*?</ns0:rPr>', '<rPr/>', raw, flags=re.S)
            print('CELL TEXT: %r' % cells[i])
            print('  width twips: %s' % tc.find(W + 'tcPr/' + W + 'tcW').get(W + 'w'))
            print('  runs: %s' % [''.join(t.text or '' for t in r.iter(W + 't'))
                                  for r in tc.find(W + 'p').findall(W + 'r')])
            print()
