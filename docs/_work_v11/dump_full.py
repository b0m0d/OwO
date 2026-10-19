# -*- coding: utf-8 -*-
"""Dump the finance-related content of v17: ch.10 paragraphs + tables 9/11/12."""
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = ET.fromstring(xml).find(W + 'body')

blocks = [b for b in body if b.tag in (W + 'p', W + 'tbl')]
print('顶层块: %d' % len(blocks))

tbl_i = 0
for i, el in enumerate(blocks):
    if el.tag == W + 'tbl':
        tbl_i += 1
        rows = el.findall(W + 'tr')
        first = ''
        for tc in rows[0].findall(W + 'tc'):
            first += ' | ' + ''.join(t.text or '' for t in tc.iter(W + 't'))
        print('\n<<< TABLE %d (%d 行): %s >>>' % (tbl_i, len(rows), first[:100]))
        for tr in rows:
            cells = [''.join(t.text or '' for t in tc.iter(W + 't')) for tc in tr.findall(W + 'tc')]
            print('   ' + ' || '.join(cells))
    else:
        t = ''.join(x.text or '' for x in el.iter(W + 't'))
        if t.strip():
            print('[P%03d] %s' % (i, t))
