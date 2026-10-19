# -*- coding: utf-8 -*-
"""Fix font resolution: declare Microsoft YaHei in fontTable and drop theme-font references."""
import zipfile, os, shutil, re

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
shutil.copy2(SRC, r"T:\创新创业\OwO-master\docs\.qa_v5\v8_before_fontfix.docx")
tmp = SRC + ".tmp"

FONT_ENTRY = (
 '<w:font w:name="Microsoft YaHei">'
 '<w:panose1 w:val="020B0604030504040204"/>'
 '<w:charset w:val="86"/><w:family w:val="swiss"/><w:pitch w:val="variable"/>'
 '<w:sig w:usb0="E00002FF" w:usb1="2AC7FDFF" w:usb2="00000000" w:usb3="00000000" '
 'w:csb0="0002009F" w:csb1="00000000"/>'
 '</w:font>')

CT_ENTRY = '<Override PartName="/word/fontTable.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml"/>'

changed = []
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        name = item.filename
        if name == "word/fontTable.xml":
            s = data.decode("utf-8")
            if "Microsoft YaHei" not in s:
                s = s.replace("</w:fonts>", FONT_ENTRY + "</w:fonts>")
                changed.append("fontTable: declared Microsoft YaHei")
            data = s.encode("utf-8")
        elif name == "word/styles.xml":
            s = data.decode("utf-8")
            before = s
            s = re.sub(r'\s+w:(?:ascii|hAnsi|eastAsia|cs)Theme="[^"]*"', '', s)
            if s != before:
                changed.append("styles: removed theme font references")
            data = s.encode("utf-8")
        elif name == "[Content_Types].xml":
            s = data.decode("utf-8")
            if "fontTable.xml" not in s:
                s = s.replace("</Types>", CT_ENTRY + "</Types>")
                changed.append("content types: added fontTable override")
            data = s.encode("utf-8")
        zout.writestr(item, data)
os.replace(tmp, SRC)
for c in changed:
    print("  ", c)
z = zipfile.ZipFile(SRC)
print("zip ok:", z.testzip() is None)
s = z.read("word/styles.xml").decode("utf-8")
print("remaining theme refs:", sum(s.count(k) for k in ("asciiTheme", "hAnsiTheme", "eastAsiaTheme", "cstheme")))
print("YaHei in fontTable:", "Microsoft YaHei" in z.read("word/fontTable.xml").decode("utf-8"))
print("fontTable CT override:", "fontTable.xml" in z.read("[Content_Types].xml").decode("utf-8"))