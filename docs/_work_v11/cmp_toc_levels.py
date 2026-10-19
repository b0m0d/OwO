# -*- coding: utf-8 -*-
"""Compare the run formatting of chapter-level vs section-level TOC rows."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v14.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = ET.fromstring(xml).find(W + 'body')
toc = body.findall(W + 'tbl')[1]


def show(tr, label):
    print('=== %s ===' % label)
    for ci, tc in enumerate(tr.findall(W + 'tc')):
        p = tc.find(W + 'p')
        ppr = p.find(W + 'pPr')
        raw_ppr = ET.tostring(ppr, encoding='unicode') if ppr is not None else ''
        raw_ppr = re.sub(r'xmlns:\w+="[^"]*"\s*', '', raw_ppr)
        txt = ''.join(t.text or '' for t in tc.iter(W + 't'))
        print('  cell%d %-30r' % (ci, txt[:30]))
        print('        pPr: %s' % raw_ppr)
        for r in p.findall(W + 'r'):
            rpr = r.find(W + 'rPr')
            if rpr is None:
                continue
            s = ET.tostring(rpr, encoding='unicode')
            s = re.sub(r'xmlns:\w+="[^"]*"\s*', '', s)
            s = re.sub(r'<w:lang[^>]*/>', '', s)
            print('        rPr: %s' % s)
    print()


rows = toc.findall(W + 'tr')
# row 0: 项目摘要 (chapter-level), row 2: 1.1 (section-level)
show(rows[0], '第0行 项目摘要 / 6.4 核心竞争力')
show(rows[2], '第2行 1.1 / 7.1')
