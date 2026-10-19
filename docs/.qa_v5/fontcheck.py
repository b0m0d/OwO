import pymupdf, glob, re
f = glob.glob(r"T:\创新创业\OwO-master\docs\.qa_v5\pdfE\*.pdf")[0]
pdf = pymupdf.open(f)
bad = []
for pno in range(pdf.page_count):
    for blk in pdf[pno].get_text("dict").get("blocks", []):
        for line in blk.get("lines", []):
            for s in line.get("spans", []):
                fn = s["font"]
                if "Gothic" in fn or "JhengHei" in fn or "Light" in fn or "Mincho" in fn or "Ming" in fn:
                    bad.append((pno+1, fn, s["text"][:24]))
print("pages:", pdf.page_count)
print("fallback-font spans:", len(bad))
for b in bad[:20]: print("   ", b)
# font inventory
inv = {}
for pno in range(pdf.page_count):
    for blk in pdf[pno].get_text("dict").get("blocks", []):
        for line in blk.get("lines", []):
            for s in line.get("spans", []):
                inv[s["font"]] = inv.get(s["font"], 0) + 1
print("font inventory:", sorted(inv.items(), key=lambda x: -x[1]))