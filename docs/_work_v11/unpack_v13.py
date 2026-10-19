# -*- coding: utf-8 -*-
"""Unpack v13 into _work_v13 for in-place XML editing."""
import os
import shutil
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx"
DST = r"T:\创新创业\OwO-master\docs\_work_v13"

if os.path.exists(DST):
    shutil.rmtree(DST)
os.makedirs(DST)
with zipfile.ZipFile(SRC) as z:
    z.extractall(DST)
    names = z.namelist()
print('extracted %d entries to %s' % (len(names), DST))
for n in names:
    print('  ', n)
