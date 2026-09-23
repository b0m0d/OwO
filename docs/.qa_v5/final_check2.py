import pymupdf, glob, os, re

f = glob.glob(r"T:\创新创业\OwO-master\docs\.qa_v5\pdfHH\*.pdf")[0]
pdf = pymupdf.open(f)
d = r"T:\创新创业\OwO-master\docs\.qa_v5\renderHH"
os.makedirs(d, exist_ok=True)

for i in range(pdf.page_count):
    t = pdf[i].get_text()
    if "营业收入" in t and "经营结果" in t:
        pix = pdf[i].get_pixmap(dpi=110)
        pix.save(os.path.join(d, "fin2.png"))
        print("financial table page", i + 1)
        break

toc = pdf[1].get_text() + pdf[2].get_text()
pages = [pdf[i].get_text().replace("\n", "") for i in range(pdf.page_count)]
names = ["项目摘要","第一章","第二章","第三章","第四章","第五章","第六章","第七章","第八章",
         "第九章","第十章","第十一章","第十二章","参考资料"]
bad = []
for n in names:
    a = None
    for i in range(3, len(pages)):
        if n in pages[i]:
            a = i + 1
            break
    seg = toc.split(n, 1)
    c = "?"
    if len(seg) > 1:
        m = re.search(r"(\d+)", seg[1])
        c = m.group(1) if m else "?"
    if str(a) != c:
        bad.append((n, c, a))
print("pages:", pdf.page_count)
print("TOC mismatches:", bad if bad else "none")
