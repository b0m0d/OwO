# -*- coding: utf-8 -*-
import zipfile, os
from lxml import etree

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
ATTRS = ["asciiTheme", "hAnsiTheme", "eastAsiaTheme", "cstheme"]
tmp = SRC + ".tmp"
stats = {}

with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename.startswith("word/") and item.filename.endswith(".xml"):
            try:
                root = etree.fromstring(data)
            except etree.XMLSyntaxError:
                zout.writestr(item, data)
                continue
            n = 0
            for rf in root.iter("{%s}rFonts" % W):
                for a in ATTRS:
                    key = "{%s}%s" % (W, a)
                    if rf.get(key) is not None:
                        del rf.attrib[key]
                        n += 1
                for slot in ("ascii", "hAnsi", "eastAsia", "cs"):
                    sk = "{%s}%s" % (W, slot)
                    if rf.get(sk) is None:
                        rf.set(sk, "Microsoft YaHei")
            if n:
                stats[item.filename] = n
                data = etree.tostring(root, xml_declaration=True, encoding="UTF-8", standalone=True)
        zout.writestr(item, data)
os.replace(tmp, SRC)
z = zipfile.ZipFile(SRC)
print("zip ok:", z.testzip() is None)
for k, v in stats.items():
    print("  ", k, "removed", v)
s = z.read("word/styles.xml").decode("utf-8")
print("theme refs left in styles:", sum(s.count(k) for k in ATTRS))
d = z.read("word/document.xml").decode("utf-8")
print("theme refs left in document:", sum(d.count(k) for k in ATTRS))