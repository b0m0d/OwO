# -*- coding: utf-8 -*-
"""Verify TOC cached page numbers against actual PDF pagination (robust parsing)."""
import pymupdf, sys, re, os

f = sys.argv[1]
pdf = pymupdf.open(f)
raw = [pdf[i].get_text() for i in range(pdf.page_count)]
flat = [t.replace("\n", "").replace(" ", "") for t in raw]
toc = flat[1] + flat[2]

names = ["项目摘要","第一章","第二章","第三章","第四章","第五章","第六章","第七章","第八章",
         "第九章","第十章","第十一章","第十二章","参考资料"]
print("PDF: pages=%d" % pdf.page_count)
pos = []
for n in names:
    k = n.replace(" ", "")
    i = toc.find(k)
    if i < 0:
        print("  %-8s NOT IN TOC" % n); continue
    pos.append((i, n, k))
pos.sort()
ok = True
for idx, (i, n, k) in enumerate(pos):
    end = pos[idx + 1][0] if idx + 1 < len(pos) else len(toc)
    seg = toc[i + len(k):end]
    nums = re.findall(r"(\d+)", seg)
    cached = nums[0] if nums else "?"   # first number after the chapter title = its own page
    actual = None
    for p in range(3, len(flat)):
        if k in flat[p]:
            actual = p + 1
            break
    good = str(actual) == cached
    ok = ok and good
    print("  %-8s toc=%-4s actual=%-4s %s" % (n, cached, actual, "OK" if good else "** MISMATCH **"))
print("ALL MATCH:", ok)
