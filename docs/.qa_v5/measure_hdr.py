import pymupdf, glob

f = glob.glob(r"T:\创新创业\OwO-master\docs\.qa_v5\pdfEE\*.pdf")[0]
doc = pymupdf.open(f)
page = doc[9]          # 表 2 所在页
print("page size:", page.rect)

# 表格竖线位置
paths = page.get_drawings()
vlines = []
for p in paths:
    for item in p["items"]:
        if item[0] == "l":
            x0, y0 = item[1]; x1, y1 = item[2]
            if abs(x0 - x1) < 0.5 and abs(y1 - y0) > 30:
                vlines.append(round(x0, 1))
vlines = sorted(set(vlines))
print("vertical rules x:", vlines)

# 表头文字的包围盒
d = page.get_text("dict")
print("--- header row spans ---")
for blk in d.get("blocks", []):
    for line in blk.get("lines", []):
        txt = "".join(s.get("text", "") for s in line.get("spans", "")).strip()
        if txt in ("验证方向（分组）", "验证方法", "判定标准", "未达标调整"):
            bbox = line["bbox"]
            print("  %-10s x0=%.1f x1=%.1f center=%.1f width=%.1f" %
                  (txt, bbox[0], bbox[2], (bbox[0] + bbox[2]) / 2, bbox[2] - bbox[0]))

# 用竖线推断列中心
if len(vlines) >= 5:
    for i in range(4):
        c = (vlines[i] + vlines[i + 1]) / 2
        print("  column %d: [%.1f, %.1f] center=%.1f width=%.1f" %
              (i, vlines[i], vlines[i + 1], c, vlines[i + 1] - vlines[i]))
