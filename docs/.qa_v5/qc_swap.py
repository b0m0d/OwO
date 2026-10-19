import zipfile, os, io, shutil
from PIL import Image
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
shutil.copy2(SRC, r"T:\创新创业\OwO-master\docs\.qa_v5\v8_before_qc.docx")
A = r"T:\创新创业\OwO-master\docs\cuttle_v8_assets"
MAP = {"word/media/image8.png": "fig08-market-funnel.png",
       "word/media/image9.png": "fig03-competition-map.png",
       "word/media/image12.png": "fig10-financial-plan.png"}
tmp = SRC + ".tmp"
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename in MAP:
            new = open(os.path.join(A, MAP[item.filename]), "rb").read()
            assert Image.open(io.BytesIO(data)).size == Image.open(io.BytesIO(new)).size, item.filename
            data = new
            print("  swapped", item.filename, "<-", MAP[item.filename])
        zout.writestr(item, data)
os.replace(tmp, SRC)
print("zip ok:", zipfile.ZipFile(SRC).testzip() is None)