# -*- coding: utf-8 -*-
"""Experiment: widen the right margin and see whether the punctuation overhang disappears.

Does not modify the deliverable; writes a throwaway copy under _work_v11/exp.
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx"
OUTDIR = r"T:\创新创业\OwO-master\docs\_work_v11\exp"
os.makedirs(OUTDIR, exist_ok=True)

# current: left=1162 right=1162  -> widen right to 1300 twips (~2.29 cm)
VARIANTS = {
    'v13_rmar1300.docx': ('<w:pgMar w:top="1077" w:right="1162"',
                          '<w:pgMar w:top="1077" w:right="1300"'),
    'v13_rmar1350.docx': ('<w:pgMar w:top="1077" w:right="1162"',
                          '<w:pgMar w:top="1077" w:right="1350"'),
}

with zipfile.ZipFile(SRC) as z:
    names = z.namelist()
    data = {n: z.read(n) for n in names}
    infos = {n: z.getinfo(n) for n in names}

xml = data['word/document.xml'].decode('utf-8')
for name, (old, new) in VARIANTS.items():
    if xml.count(old) != 1:
        raise SystemExit('ABORT: pgMar pattern found %d times' % xml.count(old))
    data['word/document.xml'] = xml.replace(old, new).encode('utf-8')
    p = os.path.join(OUTDIR, name)
    with zipfile.ZipFile(p, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])
    print('wrote', p)
data['word/document.xml'] = xml.encode('utf-8')
