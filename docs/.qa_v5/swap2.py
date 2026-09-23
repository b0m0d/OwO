# -*- coding: utf-8 -*-
import shutil, zipfile, io, os
from PIL import Image
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
BAK = r"T:\创新创业\OwO-master\docs\.qa_v5\v8_before_figs2.docx"
shutil.copy2(SRC, BAK)
new = open(r"T:\创新创业\OwO-master\docs\cuttle_v8_assets\fig10-financial-plan.png", "rb").read()
tmp = SRC + ".tmp"
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename == "word/media/image12.png":
            old = Image.open(io.BytesIO(data)).size
            assert old == Image.open(io.BytesIO(new)).size, (old,)
            data = new
            print("swapped image12.png")
        zout.writestr(item, data)
os.replace(tmp, SRC)
with zipfile.ZipFile(SRC) as z:
    print("zip ok:", z.testzip() is None)
print("backup:", BAK)