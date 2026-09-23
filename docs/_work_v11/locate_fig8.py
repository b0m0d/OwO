# -*- coding: utf-8 -*-
"""Locate the 图8 caption paragraph and its preceding image paragraph in v12."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
A = '{http://schemas.openxmlformats.org/drawingml/2006/main}'
REL = '{http://schemas.openxmlformats.org/officeDocument/2006/relationships}'

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v12.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

root = ET.fromstring(xml)
body = root.find(W + 'body')

# enumerate top-level block elements with index
blocks = list(body)
for i, el in enumerate(blocks):
    if el.tag not in (W + 'p', W + 'tbl'):
        continue
    text = ''.join(t.text or '' for t in el.iter(W + 't'))
    rid = ''
    for b in el.iter(A + 'blip'):
        rid = b.get(REL + 'embed')
    marker = '[IMG:%s]' % rid if rid else ''
    if '图8 ' in text or '图9 ' in text or '图7 ' in text or '图6 ' in text:
        # print this block plus neighbours
        for j in range(max(0, i - 2), min(len(blocks), i + 3)):
            e2 = blocks[j]
            t2 = ''.join(x.text or '' for x in e2.iter(W + 't'))
            r2 = ''
            for b in e2.iter(A + 'blip'):
                r2 = b.get(REL + 'embed')
            print('block[%d] tag=%s %s %s' % (
                j, e2.tag.replace(W, 'w:'), '[IMG:%s]' % r2 if r2 else '', t2[:80]))
        print('-' * 90)
