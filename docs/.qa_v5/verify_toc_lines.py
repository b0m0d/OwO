# -*- coding: utf-8 -*-
"""Verify TOC cached page numbers, parsing the TOC page line by line."""
import pymupdf, sys, re

f = sys.argv[1]
pdf = pymupdf.open(f)
flat = [pdf[i].get_text().replace("\n", "").replace(" ", "") for i in range(pdf.page_count)]

# ---- collect TOC entries: line-by-line, trailing integer = cached page
entries = {}
for pno in (1, 2):
    for line in pdf[pno].get_text().split("\n"):
        s = line.strip()
        if not s:
            continue
        m = re.match(r"^(.+?)[\s.·…]*\s*(\d+)\s*$", s)
        if m:
            title = m.group(1).strip().rstrip(".·… ").replace(" ", "")
            entries.setdefault(title, m.group(2))

names = ["项目摘要", "第一章", "第二章", "第三章", "第四章", "第五章", "第六章", "第七章",
         "第八章", "第九章", "第十章", "第十一章", "第十二章", "参考资料"]
print("PDF pages:", pdf.page_count, "| TOC entries parsed:", len(entries))
ok = True
for n in names:
    cands = [(t, v) for t, v in entries.items() if t.startswith(n)]
    if not cands:
        print("  %-8s NOT FOUND IN TOC" % n); ok = False; continue
    cached = cands[0][1]
    key = n.replace(" ", "")
    actual = None
    for p in range(3, len(flat)):
        if key in flat[p]:
            actual = p + 1
            break
    good = str(actual) == cached
    ok = ok and good
    print("  %-8s toc=%-4s actual=%-4s %s" % (n, cached, actual, "OK" if good else "** MISMATCH **"))
print("ALL CHAPTERS MATCH:", ok)

# ---- subsection sample check (level-2 entries): compare a few
subs = [t for t in entries if re.match(r"^\d+\.\d+", t)]
print("level-2 entries in TOC:", len(subs))
bad = 0
for t in subs:
    num = entries[t]
    idx = flat.index(t) if t in flat else None
    if idx is None:
        found = [p + 1 for p, txt in enumerate(flat) if txt.startswith(t)]
        idx = found[0] - 1 if found else None
    if idx is None:
        continue
    if idx + 1 != int(num):
        bad += 1
        if bad <= 8:
            print("   sub mismatch %-24s toc=%s actual=%s" % (t[:24], num, idx + 1))
print("level-2 mismatches:", bad)
