# -*- coding: utf-8 -*-
"""Build the replacement image set for the v10 business plan.

Mapping (provided pic -> document figure -> pptx media slot):
    图1        -> 图1  -> word/media/image2.png
    图2        -> 图2  -> word/media/image3.png
    图3        -> 图3  -> word/media/image4.png
    图4        -> 图4  -> word/media/image5.png
    图5        -> 图5  -> word/media/image6.png
    图7（2）左 -> 图7  -> word/media/image8.png   (左侧漏斗裁剪)
    图8        -> 图8  -> word/media/image9.png
    图9        -> 图9  -> word/media/image10.png

Not replaced (no new artwork supplied): image1/2 logo, image7 (图6 IC), image11 (图10), image12 (图11)
"""
import os
from PIL import Image, ImageChops

PICS = r"T:\创新创业\OwO-master\docs\pics"
OUT = r"T:\创新创业\OwO-master\docs\_work_v11\new_media"

os.makedirs(OUT, exist_ok=True)

# Content width of the document body: A4 - left/right margins
CONTENT_EMU = 5742432
MAX_H_EMU = 3900000  # vertical safety cap (~10.8 cm)


def trim_white(im, tol=8, pad_ratio=0.012):
    """Crop surrounding uniform white, keep a small breathing margin."""
    rgb = im.convert('RGB')
    bg = Image.new('RGB', rgb.size, (255, 255, 255))
    diff = ImageChops.difference(rgb, bg).convert('L')
    bbox = diff.point(lambda p: 255 if p > tol else 0).getbbox()
    if not bbox:
        return im
    l, t, r, b = bbox
    w, h = rgb.size
    pad = int(round(max(w, h) * pad_ratio))
    l = max(0, l - pad)
    t = max(0, t - pad)
    r = min(w, r + pad)
    b = min(h, b + pad)
    if (r - l) >= w * 0.985 and (b - t) >= h * 0.985:
        return im
    return im.crop((l, t, r, b))


def prep(src_name, crop_left_half=False, trim=True):
    src = os.path.join(PICS, src_name)
    im = Image.open(src).convert('RGB')
    if crop_left_half:
        w, h = im.size
        im = im.crop((0, 0, int(round(w * 0.50)), h))
    if trim:
        im = trim_white(im)
    return im


def extent_for(im):
    w, h = im.size
    cx = CONTENT_EMU
    cy = int(round(cx * h / w))
    if cy > MAX_H_EMU:
        cy = MAX_H_EMU
        cx = int(round(cy * w / h))
    return cx, cy


JOBS = [
    ('image2.png',  '图1',     False, '图1'),
    ('image3.png',  '图2',     False, '图2'),
    ('image4.png',  '图3',     False, '图3'),
    ('image5.png',  '图4',     False, '图4'),
    ('image6.png',  '图5',     False, '图5'),
    ('image8.png',  '图7（2）', True,  '图7（裁剪左侧漏斗）'),
    ('image9.png',  '图8',     False, '图8'),
    ('image10.png', '图9',     False, '图9'),
]

results = []
for slot, pic, left_half, label in JOBS:
    im = prep(pic, crop_left_half=left_half)
    dst = os.path.join(OUT, slot)
    im.save(dst, format='PNG', optimize=True)
    cx, cy = extent_for(im)
    results.append({
        'slot': slot,
        'label': label,
        'source': pic,
        'size': im.size,
        'bytes': os.path.getsize(dst),
        'cx': cx,
        'cy': cy,
        'ratio': round(im.size[1] / im.size[0], 6),
    })
    print('%-14s <- %-10s %sx%s  %6.1f KB  extent cx=%d cy=%d (%.2f x %.2f cm)' % (
        slot, pic, im.size[0], im.size[1], os.path.getsize(dst) / 1024.0,
        cx, cy, cx / 360000, cy / 360000))

import json
with open(os.path.join(OUT, '_extents.json'), 'w', encoding='utf-8') as f:
    json.dump(results, f, ensure_ascii=False, indent=2)
print('\nwrote', os.path.join(OUT, '_extents.json'))
