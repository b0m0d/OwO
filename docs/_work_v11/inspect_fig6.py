# -*- coding: utf-8 -*-
"""Crop regions of 图6.png at full resolution for close inspection."""
import os

from PIL import Image

SRC = r"T:\创新创业\OwO-master\docs\pics\图6.png"
OUT = r"T:\创新创业\OwO-master\docs\_work_v11\inspect"
os.makedirs(OUT, exist_ok=True)

im = Image.open(SRC).convert('RGB')
w, h = im.size
print('source', w, h)

# six field boxes + three attribute boxes: crop quadrants at 1:1
regions = {
    'fig6_topleft': (0, 0, w // 2, h // 2),
    'fig6_topright': (w // 2, 0, w, h // 2),
    'fig6_bottomleft': (0, h // 2, w // 2, h),
    'fig6_bottomright': (w // 2, h // 2, w, h),
}
for name, box in regions.items():
    crop = im.crop(box)
    # upscale 1.6x for legibility
    crop = crop.resize((int(crop.width * 1.6), int(crop.height * 1.6)), Image.LANCZOS)
    p = os.path.join(OUT, name + '.png')
    crop.save(p)
    print('wrote', p, crop.size)
