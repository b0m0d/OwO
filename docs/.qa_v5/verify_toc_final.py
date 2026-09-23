import pymupdf, glob, os, re
f = glob.glob(r"T:\创新创业\OwO-master\docs\.qa_v5\pdfEE\*.pdf")[0]
pdf = pymupdf.open(f)
pages = [pdf[i].get_text().replace("\n", "").replace(" ", "") for i in range(pdf.page_count)]
toc = pages[1] + pages[2]
names = ["项目摘要","第一章","第二章","第三章","第四章","第五章","第六章","第七章","第八章",
         "第九章","第十章","第十一章","第十二章","参考资料"]
print("pages:", pdf.page_count)
ok = True
for n in names:
    key = n.replace(" ", "")
    actual = None
    for i in range(3, len(pages)):
        if key in pages[i]:
            actual = i + 1
            break
    m = re.search(re.escape(key) + r"\.*?(\d+)", toc)
    cached = m.group(1) if m else "?"
    flag = "OK" if str(actual) == cached else "MISMATCH"
    if flag != "OK":
        ok = False
    print("  %-10s toc=%-4s actual=%-4s %s" % (n, cached, actual, flag))
print("all match:", ok)
