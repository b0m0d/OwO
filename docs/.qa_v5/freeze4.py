import shutil, zipfile, io, os
from PIL import Image
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
shutil.copy2(SRC, r"T:\创新创业\OwO-master\docs\.qa_v5\v8_before_freeze.docx")
new = open(r"T:\创新创业\OwO-master\docs\cuttle_v8_assets\fig10-financial-plan.png", "rb").read()
tmp = SRC + ".tmp"
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename == "word/media/image12.png":
            assert Image.open(io.BytesIO(data)).size == Image.open(io.BytesIO(new)).size
            data = new
        zout.writestr(item, data)
os.replace(tmp, SRC)
with zipfile.ZipFile(SRC) as z:
    print("zip ok:", z.testzip() is None, "| media:", sum(1 for n in z.namelist() if n.startswith("word/media/")))