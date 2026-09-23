# -*- coding: utf-8 -*-
"""Compare table geometry between v10 (baseline) and v13 to see what is new vs inherited."""
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'

FILES = {
    'v10': r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v10.docx",
    'v13': r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx",
}


def report(path):
    with zipfile.ZipFile(path) as z:
        xml = z.read('word/document.xml').decode('utf-8')
    root = ET.fromstring(xml)
    body = root.find(W + 'body')
    sect = body.find(W + 'sectPr')
    pg = sect.find(W + 'pgSz')
    mar = sect.find(W + 'pgMar')
    text_w = int(pg.get(W + 'w')) - int(mar.get(W + 'left')) - int(mar.get(W + 'right'))
    out = []
    for i, tbl in enumerate(body.findall(W + 'tbl'), 1):
        pr = tbl.find(W + 'tblPr')
        tblw = pr.find(W + 'tblW') if pr is not None else None
        layout = pr.find(W + 'tblLayout') if pr is not None else None
        ind = pr.find(W + 'tblInd') if pr is not None else None
        grid = tbl.find(W + 'tblGrid')
        gws = [int(g.get(W + 'w')) for g in grid.findall(W + 'gridCol')]
        out.append({
            'n': i,
            'tblW': (tblw.get(W + 'w'), tblw.get(W + 'type')) if tblw is not None else None,
            'layout': layout.get(W + 'type') if layout is not None else None,
            'ind': ind.get(W + 'w') if ind is not None else None,
            'grid_sum': sum(gws),
            'grid': gws,
        })
    return text_w, out


for label, path in FILES.items():
    text_w, tables = report(path)
    print('=== %s  版心 %d twips (%.2f cm) ===' % (label, text_w, text_w / 567.0))
    for t in tables:
        over = t['grid_sum'] - text_w
        print('  表%-3d tblW=%-14s layout=%-10s ind=%-6s grid合计=%-6d 差=%+d twips (%+.3f cm)' % (
            t['n'], t['tblW'], t['layout'], t['ind'], t['grid_sum'], over, over / 567.0))
    print()
