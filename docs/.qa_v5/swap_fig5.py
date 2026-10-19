import zipfile, os, io, shutil
from PIL import Image
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
shutil.copy2(SRC, r"T:\创新创业\OwO-master\docs\.qa_v5\v8_before_final.docx")
new = open(r"T:\创新创业\OwO-master\docs\cuttle_v8_assets\fig06-intent-escalation.png", "rb").read()
tmp = SRC + ".tmp"
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename == "word/media/image6.png":
            assert Image.open(io.BytesIO(data)).size == Image.open(io.BytesIO(new)).size
            data = new
            print("  swapped image6.png")
        zout.writestr(item, data)
os.replace(tmp, SRC)
print("zip ok:", zipfile.ZipFile(SRC).testzip() is None)