# -*- coding: utf-8 -*-
"""Render v13 page 12 (图6) at high dpi and report the figure's physical size."""
import os

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf"
OUT = r"T:\创新创业\OwO-master\docs\_work_v11\inspect"

doc = pymupdf.open(PDF)
page = doc[11]  # page 12
pix = page.get_pixmap(dpi=200)
p = os.path.join(OUT, 'pdf_page12_200dpi.png')
pix.save(p)
print('wrote', p, pix.width, pix.height)

# locate the image on the page and report its placement in cm
for img in page.get_images(full=True):
    xref = img[0]
    rects = page.get_image_rects(xref)
    for r in rects:
        print('  xref=%d placed at x0=%.2f y0=%.2f w=%.2f h=%.2f pt (%.2f x %.2f cm)' % (
            xref, r.x0, r.y0, r.width, r.height,
            r.width / 72 * 2.54, r.height / 72 * 2.54))
