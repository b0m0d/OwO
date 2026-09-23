# -*- coding: utf-8 -*-
"""Normalize table geometry using the TRUE page geometry from sectPr (A4, 9638 twips content)."""
from docx import Document
from docx.oxml.ns import qn
from docx.oxml import OxmlElement

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
doc = Document(SRC)
body = doc.element.body
sect = body.find(qn('w:sectPr'))
pgSz = sect.find(qn('w:pgSz')); pgMar = sect.find(qn('w:pgMar'))
pageW = int(pgSz.get(qn('w:w')))
L = int(pgMar.get(qn('w:left'))); R = int(pgMar.get(qn('w:right')))
CONTENT = pageW - L - R
print("pageW", pageW, "margins", L, R, "-> content", CONTENT)
TW, TT = qn('w:w'), qn('w:type')

def row_widths(tr):
    out = []
    for tc in tr.findall(qn('w:tc')):
        pr = tc.find(qn('w:tcPr')); gs = 1; w = None
        if pr is not None:
            g = pr.find(qn('w:gridSpan'))
            if g is not None: gs = int(g.get(qn('w:val')))
            tcell = pr.find(qn('w:tcW'))
            if tcell is not None:
                try: w = int(tcell.get(TW))
                except (TypeError, ValueError): w = None
        out.append((w, gs))
    return out

for ti, t in enumerate(doc.tables):
    tbl = t._element
    trs = tbl.findall(qn('w:tr'))
    base = row_widths(trs[0])
    declared = [w for (w, gs) in base]
    ncols = sum(gs for (_, gs) in base)
    if ncols == 0 or any(w is None for w in declared):
        print(f"T{ti}: skipped"); continue
    total = sum(declared)
    scale = CONTENT / float(total)
    colw = [max(400, int(round(w * scale))) for w in declared]
    colw[-1] += CONTENT - sum(colw)          # make the sum exact

    grid = tbl.find(qn('w:tblGrid'))
    newgrid = OxmlElement('w:tblGrid')
    for w in colw:
        gc = OxmlElement('w:gridCol'); gc.set(TW, str(w)); newgrid.append(gc)
    tbl.replace(grid, newgrid)

    tblpr = tbl.find(qn('w:tblPr'))
    tw = tblpr.find(qn('w:tblW'))
    if tw is None:
        tw = OxmlElement('w:tblW'); tblpr.insert(0, tw)
    tw.set(TT, 'dxa'); tw.set(TW, str(CONTENT))

    for tr in trs:
        ci = 0
        for tc in tr.findall(qn('w:tc')):
            pr = tc.find(qn('w:tcPr'))
            if pr is None:
                pr = OxmlElement('w:tcPr'); tc.insert(0, pr)
            gs = 1
            g = pr.find(qn('w:gridSpan'))
            if g is not None: gs = int(g.get(qn('w:val')))
            want = sum(colw[ci:ci + gs])
            tcell = pr.find(qn('w:tcW'))
            if tcell is None:
                tcell = OxmlElement('w:tcW'); pr.insert(0, tcell)
            tcell.set(TT, 'dxa'); tcell.set(TW, str(want))
            ci += gs
    flag = "FIXED" if abs(total - CONTENT) > 5 else "ok"
    print(f"  T{ti:2d} {flag}: declared={total} scale={scale:.4f} grid={colw}")

doc.save(SRC)
print("saved")