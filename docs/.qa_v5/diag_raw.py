# -*- coding: utf-8 -*-
"""Raw-level audit of w:jc usage + revision markup in document.xml."""
import zipfile, re, glob, os
from collections import Counter

DOCS = os.path.join(r"T:\创新创业\OwO-master\docs")
SRC = [x for x in glob.glob(os.path.join(DOCS, "Cuttle*v8.docx")) if "预览" not in x][0]
z = zipfile.ZipFile(SRC)
xml = z.read("word/document.xml").decode("utf-8")
print("document.xml length:", len(xml))

jc = re.findall(r'<w:jc\b[^>]*w:val="([^"]+)"', xml)
print("w:jc counts:", Counter(jc))

for tag in ("pPrChange", "rPrChange", "w:ins", "w:del", "w:moveFrom", "w:moveTo",
            "w:sdt", "w:cellIns", "w:cellDel", "tblPrChange", "tcPrChange",
            "w:tblStyle", "w:divId", "w:tblLayout"):
    print("  %-14s %d" % (tag, len(re.findall(r"<" + tag + r"\b", xml))))

# any jc=left / start anywhere?
left = re.findall(r'<w:jc\b[^>]*w:val="(left|start|both|distribute)"[^>]*/>', xml)
print("non-center jc samples:", left[:10], "count", len(left))

# Normal style jc
st = z.read("word/styles.xml").decode("utf-8")
m = re.search(r'<w:style [^>]*w:styleId="Normal".*?</w:style>', st, re.S)
print("Normal style jc:", re.findall(r'<w:jc\b[^>]*/>', m.group(0)) if m else "n/a")
print("styles w:jc vals:", Counter(re.findall(r'<w:jc\b[^>]*w:val="([^"]+)"', st)))
