# -*- coding: utf-8 -*-
"""Swap regenerated figures into v8.docx at the zip level, preserving every embed relationship."""
import shutil, zipfile, hashlib, io
from PIL import Image

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
BAK = r"T:\创新创业\OwO-master\docs\.qa_v5\v8_before_figs.docx"
ASSETS = r"T:\创新创业\OwO-master\docs\cuttle_v8_assets"

MAP = {
    "word/media/image2.png":  "fig01-opportunity.png",
    "word/media/image4.png":  "fig07-validation-plan.png",
    "word/media/image5.png":  "fig04-architecture.png",
    "word/media/image6.png":  "fig06-intent-escalation.png",
    "word/media/image7.png":  "fig05-intent-capsule.png",
    "word/media/image8.png":  "fig08-market-funnel.png",
    "word/media/image9.png":  "fig03-competition-map.png",
    "word/media/image10.png": "fig09-go-to-market.png",
    "word/media/image12.png": "fig10-financial-plan.png",
}

import os
shutil.copy2(SRC, BAK)
print("backup ->", BAK)

zin = zipfile.ZipFile(SRC, "r")
report = []
for name in sorted(zin.namelist()):
    if name.startswith("word/media/"):
        data = zin.read(name)
        try:
            im = Image.open(io.BytesIO(data)); size = im.size
        except Exception:
            size = None
        report.append(f"OLD {name} {size} {len(data)}")
zin.close()

items = zin = None
tmp = SRC + ".tmp"
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename in MAP:
            src_png = os.path.join(ASSETS, MAP[item.filename])
            new = open(src_png, "rb").read()
            old_im = Image.open(io.BytesIO(data)).size
            new_im = Image.open(io.BytesIO(new)).size
            assert old_im == new_im, f"size mismatch {item.filename}: {old_im} vs {new_im}"
            data = new
            report.append(f"SWAP {item.filename} <- {MAP[item.filename]} {new_im} {len(new)}")
        zout.writestr(item, data)
os.replace(tmp, SRC)

with zipfile.ZipFile(SRC) as z:
    bad = z.testzip()
    media = sorted(n for n in z.namelist() if n.startswith("word/media/"))
print("zip ok:", bad is None, "| media files:", len(media))
for r in report:
    print(" ", r)