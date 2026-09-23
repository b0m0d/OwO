import pymupdf, sys, os
pdf = pymupdf.open(sys.argv[1]); out=sys.argv[2]
for p in (8, 15):
    pix = pdf[p-1].get_pixmap(dpi=140)
    fp = os.path.join(out, "p%02d.png" % p); pix.save(fp); print("saved", fp)
