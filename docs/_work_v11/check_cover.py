# -*- coding: utf-8 -*-
"""Check whether the cover logo in v14 differs from v10's, and identify its media slot."""
import hashlib
import zipfile

V10 = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v10.docx"
V14 = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v14.docx"

for label, path in (('v10', V10), ('v14', V14)):
    with zipfile.ZipFile(path) as z:
        raw = z.read('word/media/image1.png')
    print('%s cover image1.png: %d bytes  md5=%s' % (
        label, len(raw), hashlib.md5(raw).hexdigest()))
