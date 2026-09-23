# -*- coding: utf-8 -*-
"""把 v6 新图替换进 docx（按 media 条目整体替换，尺寸与原图一致，版式不变）。"""
import hashlib
import io
import os
import shutil
import zipfile

from PIL import Image

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v6.docx"
NEW = r"T:\创新创业\OwO-master\docs\cuttle_v6_assets\figures"
BACKUP = r"T:\创新创业\OwO-master\docs\.qa_v5\v6_before_images.docx"

REPLACE = {
    "word/media/image3.png": "fig02-closed-loop.png",
    "word/media/image7.png": "fig05-intent-capsule.png",
    "word/media/image8.png": "fig08-market-funnel.png",
    "word/media/image11.png": "fig11-roadmap.png",
    "word/media/image12.png": "fig10-financial-plan.png",
}

if not os.path.exists(BACKUP):
    shutil.copyfile(DOCX, BACKUP)

zin = zipfile.ZipFile(DOCX)
old = {name: zin.read(name) for name in zin.namelist()}
zin.close()

report = []
for slot, fig in REPLACE.items():
    with open(os.path.join(NEW, fig), "rb") as fh:
        data = fh.read()
    o = Image.open(io.BytesIO(old[slot]))
    n = Image.open(io.BytesIO(data))
    assert o.size == n.size, (slot, o.size, n.size)
    old[slot] = data
    report.append((slot, fig, o.size))

with zipfile.ZipFile(DOCX, "w", zipfile.ZIP_DEFLATED) as zout:
    for name in list(old.keys()):
        zi = zipfile.ZipInfo(name, date_time=(2026, 9, 11, 12, 0, 0))
        zi.compress_type = zipfile.ZIP_DEFLATED
        zout.writestr(zi, old[name])

for slot, fig, size in report:
    print("replaced %-26s <- %-32s %s" % (slot, fig, size))

z = zipfile.ZipFile(DOCX)
assert z.testzip() is None
for slot, fig, size in report:
    ok = hashlib.md5(z.read(slot)).hexdigest() == hashlib.md5(
        open(os.path.join(NEW, fig), "rb").read()).hexdigest()
    print("verify", slot, Image.open(z.open(slot)).size, "md5 match:", ok)
import docx
print("docx opens, paragraphs:", len(docx.Document(DOCX).paragraphs))
