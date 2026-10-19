# -*- coding: utf-8 -*-
"""Crop 图7（2）: keep only the LEFT funnel illustration."""
from PIL import Image

SRC = r"T:\创新创业\OwO-master\docs\pics\图7（2）"
OUT = r"T:\创新创业\OwO-master\docs\_work_v11\fig7_crop_preview.png"

im = Image.open(SRC).convert('RGB')
w, h = im.size
print('source size:', w, h)

# The source is two funnels side by side with generous whitespace.
# Keep the left half, with a small safety margin so nothing is clipped.
left = im.crop((0, 0, int(w * 0.50), h))
print('crop size:', left.size)
left.save(OUT)
print('saved preview ->', OUT)
