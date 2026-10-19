# -*- coding: utf-8 -*-
"""List every 图N mention in the v12 document with its paragraph/table context."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v12.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

root = ET.fromstring(xml)
body = root.find(W + 'body')

idx = 0


def walk(el, kind):
    global idx
    for p in el.iter(W + 'p'):
        idx += 1
        t = ''.join(x.text or '' for x in p.iter(W + 't'))
        if re.search(r'图\s*\d+', t):
            has_img = len(list(p.iter(W + 'drawing'))) > 0
            print('%s#%-4d %s%s' % (kind, idx, '[IMG]' if has_img else '     ', t[:110]))


for el in body:
    if el.tag == W + 'p':
        walk(el, 'P')
    elif el.tag == W + 'tbl':
        walk(el, 'T')
