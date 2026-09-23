import glob, os, re
from docx import Document
from lxml import etree
from docx.oxml.ns import qn

SRC = [x for x in glob.glob(os.path.join(r"T:\创新创业\OwO-master\docs", "Cuttle*v8.docx"))
       if "预览" not in x][0]
d = Document(SRC)
for p in d.paragraphs:
    el = p._element
    if el.findall('.//' + qn('w:tab')):
        x = etree.tostring(el, pretty_print=True, encoding="unicode")
        x = re.sub(r'\sxmlns:[a-zA-Z0-9]+="[^"]+"', '', x)
        print(x[:800])
        break
