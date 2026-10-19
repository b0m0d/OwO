import zipfile, os, io, glob
from PIL import Image
SRC = [x for x in glob.glob(os.path.join(r"T:\创新创业\OwO-master\docs", "Cuttle*v8.docx")) if "预览" not in x][0]
NEWMAP = {"word/media/image12.png": "fig10-financial-plan.png"}
A = r"T:\创新创业\OwO-master\docs\cuttle_v8_assets"
tmp = SRC + ".tmp"
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename in NEWMAP:
            new = open(os.path.join(A, NEWMAP[item.filename]), "rb").read()
            assert Image.open(io.BytesIO(data)).size == Image.open(io.BytesIO(new)).size
            data = new
            print("  swapped", item.filename)
        zout.writestr(item, data)
os.replace(tmp, SRC)
print("zip ok:", zipfile.ZipFile(SRC).testzip() is None)