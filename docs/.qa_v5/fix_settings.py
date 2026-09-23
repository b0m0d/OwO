# -*- coding: utf-8 -*-
"""Drop the updateFields flag so Word stops prompting, and loosen the fixed table layout."""
import zipfile, os, shutil
SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
shutil.copy2(SRC, r"T:\创新创业\OwO-master\docs\.qa_v5\v8_before_settingsfix.docx")
tmp = SRC + ".tmp"
changed = []
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename == "word/settings.xml":
            s = data.decode("utf-8")
            for pat in ('<w:updateFields w:val="true"/>', '<w:updateFields w:val="1"/>',
                        '<w:updateFields/>'):
                if pat in s:
                    s = s.replace(pat, "")
                    changed.append(pat)
            data = s.encode("utf-8")
        zout.writestr(item, data)
os.replace(tmp, SRC)
print("removed:", changed)
z = zipfile.ZipFile(SRC)
print("zip ok:", z.testzip() is None)
s = z.read("word/settings.xml").decode("utf-8")
print("updateFields still present:", "updateFields" in s)