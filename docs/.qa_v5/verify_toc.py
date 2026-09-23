import pymupdf, glob, os, re
f = glob.glob(r"T:\创新创业\OwO-master\docs\.qa_v5\pdfV\*.pdf")[0]
pdf = pymupdf.open(f)
print("pages:", pdf.page_count)
NAMES = ["项目摘要","第一章","第二章","第三章","第四章","第五章","第六章","第七章","第八章",
         "第九章","第十章","第十一章","第十二章","参考资料"]
found = {}
for pno in range(2, pdf.page_count):
    flat = pdf[pno].get_text().replace("\n", "")
    for n in NAMES:
        if n not in found and n in flat:
            found[n] = pno + 1
toc = pdf[1].get_text()
print("TOC vs actual:")
ok = True
for n in NAMES:
    if n in toc:
        seg = toc.split(n, 1)[1]
        m = re.search(r"(\d+)", seg)
        cached = m.group(1) if m else "?"
        a = found.get(n)
        flag = "OK" if str(a) == cached else "MISMATCH"
        if flag != "OK":
            ok = False
        print("  %-14s toc=%-4s actual=%-4s %s" % (n, cached, a, flag))
print("all match:", ok)
d = r"T:\创新创业\OwO-master\docs\.qa_v5\renderV"
os.makedirs(d, exist_ok=True)
pix = pdf[1].get_pixmap(dpi=105)
pix.save(os.path.join(d, "page2.png"))
print("rendered TOC page")
