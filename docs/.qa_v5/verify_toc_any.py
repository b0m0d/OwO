# -*- coding: utf-8 -*-
"""Verify TOC cached page numbers against actual PDF pagination for a given PDF."""
import pymupdf, sys, re, os

f = sys.argv[1]
pdf = pymupdf.open(f)
pages = [pdf[i].get_text().replace("\n", "").replace(" ", "") for i in range(pdf.page_count)]
toc = pages[1] + pages[2]
names = ["项目摘要","第一章","第二章","第三章","第四章","第五章","第六章","第七章","第八章",
         "第九章","第十章","第十一章","第十二章","参考资料"]
print("PDF:", os.path.basename(f), "| pages:", pdf.page_count)
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
    flag = "OK" if str(actual) == cached else "** MISMATCH **"
    if flag != "OK":
        ok = False
    print("  %-8s toc=%-4s actual=%-4s %s" % (n, cached, actual, flag))
print("ALL MATCH:", ok)

# also dump pages holding key tables/figures
for label in ("表1核心用户", "表3意图胶囊", "意图胶囊（IC）数据结构", "表9"):
    hits = [i + 1 for i, t in enumerate(pages) if label.replace(" ", "") in t]
    print("  %-16s pages=%s" % (label, hits[:6]))
