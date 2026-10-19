# -*- coding: utf-8 -*-
"""Crop the placed 图6 area from the 200dpi page render at 1:1 to judge legibility."""
import os

from PIL import Image

SRC = r"T:\创新创业\OwO-master\docs\_work_v11\inspect\pdf_page12_200dpi.png"
OUT = r"T:\创新创业\OwO-master\docs\_work_v11\inspect"

im = Image.open(SRC)
print('page render', im.size)
scale = 200.0 / 72.0
x0, y0, w, h = 76.05, 53.80, 443.25, 307.10
box = (int(x0 * scale), int(y0 * scale), int((x0 + w) * scale), int((y0 + h) * scale))
crop = im.crop(box)
p = os.path.join(OUT, 'fig6_as_printed.png')
crop.save(p)
print('wrote', p, crop.size, '(this is 图6 exactly as it prints, 1:1 at 200dpi)')

# also the left half magnified 2x to read the small captions
half = crop.crop((0, 0, crop.width // 2, crop.height))
big = half.resize((half.width * 2, half.height * 2), Image.LANCZOS)
p2 = os.path.join(OUT, 'fig6_printed_left_2x.png')
big.save(p2)
print('wrote', p2, big.size)
