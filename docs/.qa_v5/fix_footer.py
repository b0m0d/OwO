# -*- coding: utf-8 -*-
"""Rebuild the footer PAGE field as a well-formed field with a cached result."""
import zipfile, os, shutil, copy
from lxml import etree

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-国金重构版-v8.docx"
shutil.copy2(SRC, r"T:\创新创业\OwO-master\docs\.qa_v5\v8_before_footerfix.docx")
W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
def q(t): return "{%s}%s" % (W, t)

tmp = SRC + ".tmp"
with zipfile.ZipFile(SRC, "r") as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = zin.read(item.filename)
        if item.filename == "word/footer1.xml":
            root = etree.fromstring(data)
            p = root.find(q("p"))
            # locate the rPr to reuse for every run
            orig_runs = p.findall(q("r"))
            rpr = None
            for r in orig_runs:
                rp = r.find(q("rPr"))
                if rp is not None:
                    rpr = copy.deepcopy(rp); break
            for r in orig_runs:
                p.remove(r)

            def mkrun(child):
                r = etree.SubElement(p, q("r"))
                if rpr is not None:
                    r.append(copy.deepcopy(rpr))
                if child is not None:
                    r.append(child)
                return r

            b = etree.Element(q("fldChar")); b.set(q("fldCharType"), "begin")
            it = etree.Element(q("instrText")); it.set("{http://www.w3.org/XML/1998/namespace}space", "preserve"); it.text = " PAGE "
            sep = etree.Element(q("fldChar")); sep.set(q("fldCharType"), "separate")
            t = etree.Element(q("t")); t.text = "1"
            e = etree.Element(q("fldChar")); e.set(q("fldCharType"), "end")
            mkrun(b); mkrun(it); mkrun(sep); mkrun(t); mkrun(e)
            data = etree.tostring(root, xml_declaration=True, encoding="UTF-8", standalone=True)
            print("footer rebuilt")
        zout.writestr(item, data)
os.replace(tmp, SRC)

z = zipfile.ZipFile(SRC)
print("zip ok:", z.testzip() is None)
print(etree.tostring(etree.fromstring(z.read("word/footer1.xml")), pretty_print=True, encoding="unicode")[-1400:])