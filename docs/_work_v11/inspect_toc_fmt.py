# -*- coding: utf-8 -*-
"""Dump the TOC table's formatting: cell widths, paragraph spacing, run fonts/sizes."""
import collections
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v14.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = ET.fromstring(xml).find(W + 'body')
tables = body.findall(W + 'tbl')
toc = tables[1]

pr = toc.find(W + 'tblPr')
print('=== TOC tblPr ===')
print(ET.tostring(pr, encoding='unicode')[:900])

grid = toc.find(W + 'tblGrid')
print('\n=== TOC tblGrid ===')
print([g.get(W + 'w') for g in grid.findall(W + 'gridCol')])

print('\n=== 首行 4 个单元格的段落/字体设置 ===')
tr = toc.findall(W + 'tr')[0]
for ci, tc in enumerate(tr.findall(W + 'tc')):
    tcw = tc.find(W + 'tcPr/' + W + 'tcW')
    p = tc.find(W + 'p')
    ppr = p.find(W + 'pPr')
    print('--- cell %d  (w=%s) ---' % (ci, tcw.get(W + 'w') if tcw is not None else '?'))
    print('  pPr : %s' % (ET.tostring(ppr, encoding='unicode')[:400] if ppr is not None else None))
    for r in p.findall(W + 'r'):
        t = ''.join(x.text or '' for x in r.iter(W + 't'))
        rpr = r.find(W + 'rPr')
        sz = rpr.find(W + 'sz') if rpr is not None else None
        rf = rpr.find(W + 'rFonts') if rpr is not None else None
        print('  run %-34r sz=%s eastAsia=%s' % (
            t[:34], sz.get(W + 'val') if sz is not None else '-',
            rf.get(W + 'eastAsia') if rf is not None else '-'))

print('\n=== 字号统计（TOC 内所有 run） ===')
c = collections.Counter()
for r in toc.iter(W + 'r'):
    if not ''.join(x.text or '' for x in r.iter(W + 't')).strip():
        continue
    rpr = r.find(W + 'rPr')
    sz = rpr.find(W + 'sz') if rpr is not None else None
    lvl = rpr.find(W + 'szCs') if rpr is not None else None
    c[(sz.get(W + 'val') if sz is not None else None, lvl.get(W + 'val') if lvl is not None else None)] += 1
print(dict(c))

print('\n=== 行距/段距设置统计 ===')
sp = collections.Counter()
for p in toc.iter(W + 'p'):
    ppr = p.find(W + 'pPr')
    s = ppr.find(W + 'spacing') if ppr is not None else None
    if s is None:
        sp[None] += 1
    else:
        sp[(s.get(W + 'before'), s.get(W + 'after'), s.get(W + 'line'), s.get(W + 'lineRule'))] += 1
for k, v in sp.items():
    print('  before=%s after=%s line=%s rule=%s  -> %d 段' % (k + (v,) if k else (None, None, None, None, v)))
