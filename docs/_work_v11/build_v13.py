# -*- coding: utf-8 -*-
"""Build v13 from v12 (string-level XML editing, namespace prefixes preserved).

Changes
  1. 图1 / 图2 / 图3  -> the '-改' artwork (原位交付 terminology)
  2. 图6              -> new IC capsule artwork
  3. 图8              -> deleted (competition scatter plot); 图9/10/11 -> 图8/9/10
  4. drawing extents recomputed for the four new images
"""
import os
import re
import zipfile

from PIL import Image, ImageChops

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v12.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx"
PICS = r"T:\创新创业\OwO-master\docs\pics"
WORK = r"T:\创新创业\OwO-master\docs\_work_v11"
OUT_MEDIA = os.path.join(WORK, 'new_media_v13')

CONTENT_EMU = 5742432
MAX_H_EMU = 3900000

NEW_ART = [
    ('image2.png', '图1-改.png'),
    ('image3.png', '图2-改.png'),
    ('image4.png', '图3-改.png'),
    ('image7.png', '图6.png'),
]

# drawings in document order after 图8 is removed
SLOT_ORDER = ['image1.png', 'image2.png', 'image3.png', 'image4.png',
              'image5.png', 'image6.png', 'image7.png', 'image8.png',
              'image10.png', 'image11.png', 'image12.png']

CAPTION_RENUMBER = [
    ('<w:t>图9</w:t>', '<w:t>图8</w:t>'),
    ('<w:t>图10</w:t>', '<w:t>图9</w:t>'),
    ('<w:t>图11</w:t>', '<w:t>图10</w:t>'),
]


def trim_white(im, tol=8, pad_ratio=0.012):
    rgb = im.convert('RGB')
    bg = Image.new('RGB', rgb.size, (255, 255, 255))
    diff = ImageChops.difference(rgb, bg).convert('L')
    bbox = diff.point(lambda p: 255 if p > tol else 0).getbbox()
    if not bbox:
        return im
    l, t, r, b = bbox
    w, h = rgb.size
    pad = int(round(max(w, h) * pad_ratio))
    l, t = max(0, l - pad), max(0, t - pad)
    r, b = min(w, r + pad), min(h, b + pad)
    if (r - l) >= w * 0.985 and (b - t) >= h * 0.985:
        return im
    return im.crop((l, t, r, b))


def extent_for(im):
    w, h = im.size
    cx = CONTENT_EMU
    cy = int(round(cx * h / w))
    if cy > MAX_H_EMU:
        cy = MAX_H_EMU
        cx = int(round(cy * w / h))
    return cx, cy


def prepare_art():
    os.makedirs(OUT_MEDIA, exist_ok=True)
    extents = {}
    for slot, art in NEW_ART:
        src = os.path.join(PICS, art)
        if not os.path.exists(src):
            raise SystemExit('ABORT: missing artwork %s' % src)
        im = trim_white(Image.open(src).convert('RGB'))
        dst = os.path.join(OUT_MEDIA, slot)
        im.save(dst, format='PNG', optimize=True)
        extents[slot] = extent_for(im)
        print('  %-12s <- %-12s %sx%-5s %7.1f KB  %.2f x %.2f cm' % (
            slot, art, im.size[0], im.size[1], os.path.getsize(dst) / 1024.0,
            extents[slot][0] / 360000, extents[slot][1] / 360000))
    return extents


def locator(xml, needle):
    """Return (start, end) of the <w:p ...>...</w:p> containing needle once."""
    if xml.count(needle) != 1:
        raise SystemExit('ABORT: needle not unique (%d): %s' % (xml.count(needle), needle[:60]))
    i = xml.find(needle)
    s = xml.rfind('<w:p ', 0, i)
    if s < 0:
        s = xml.rfind('<w:p>', 0, i)
    e = xml.find('</w:p>', i) + len('</w:p>')
    if s < 0 or e <= s:
        raise SystemExit('ABORT: could not bound paragraph for %s' % needle[:60])
    return s, e


def drop_block(xml, needle, label):
    s, e = locator(xml, needle)
    print('  dropped %s: %d chars' % (label, e - s))
    return xml[:s] + xml[e:]


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    print('=== 1. artwork ===')
    extents = prepare_art()

    xml = data['word/document.xml'].decode('utf-8')

    print('\n=== 2. delete 图8 ===')
    xml = drop_block(xml, '<w:t>图8</w:t>', '图8 caption')
    if xml.count('rId16') == 0:
        raise SystemExit('ABORT: rId16 not found')
    i = xml.find('rId16')
    s = xml.rfind('<w:p ', 0, i)
    e = xml.find('</w:p>', i) + len('</w:p>')
    print('  dropped 图8 image paragraph: %d chars' % (e - s))
    xml = xml[:s] + xml[e:]
    if 'rId16' in xml:
        raise SystemExit('ABORT: rId16 still referenced after deletion')

    print('\n=== 3. renumber captions ===')
    for old, new in CAPTION_RENUMBER:
        n = xml.count(old)
        if n != 1:
            raise SystemExit('ABORT: caption run %r found %d times' % (old, n))
        xml = xml.replace(old, new)
        print('  %s -> %s' % (old, new))
    if xml.count('<w:t>图11</w:t>'):
        raise SystemExit('ABORT: 图11 label still present')
    for lab in ('图8', '图9', '图10'):
        c = xml.count('<w:t>%s</w:t>' % lab)
        if c != 1:
            raise SystemExit('ABORT: caption %s appears %d times' % (lab, c))

    print('\n=== 4. resize new drawings ===')
    wps = list(re.finditer(r'<wp:extent cx="(\d+)" cy="(\d+)"\s*/?>', xml))
    aes = list(re.finditer(r'<a:ext cx="(\d+)" cy="(\d+)"\s*/?>', xml))
    if len(wps) != len(aes):
        raise SystemExit('ABORT: wp:extent=%d a:ext=%d' % (len(wps), len(aes)))
    if len(wps) != len(SLOT_ORDER):
        raise SystemExit('ABORT: %d drawings left, expected %d' % (len(wps), len(SLOT_ORDER)))
    edits = []
    for slot, pm, am in zip(SLOT_ORDER, wps, aes):
        if slot not in extents:
            continue
        cx, cy = extents[slot]
        edits.append((pm.start(), pm.end(), '<wp:extent cx="%d" cy="%d"/>' % (cx, cy)))
        edits.append((am.start(), am.end(), '<a:ext cx="%d" cy="%d"/>' % (cx, cy)))
        print('  %-12s -> %.2f x %.2f cm' % (slot, cx / 360000, cy / 360000))
    edits.sort(key=lambda x: x[0], reverse=True)
    for s, e, rep in edits:
        xml = xml[:s] + rep + xml[e:]

    data['word/document.xml'] = xml.encode('utf-8')

    print('\n=== 5. media + relationships ===')
    for slot, art in NEW_ART:
        with open(os.path.join(OUT_MEDIA, slot), 'rb') as f:
            data['word/media/' + slot] = f.read()
        print('  %s <- %s' % (slot, art))

    rels = data['word/_rels/document.xml.rels'].decode('utf-8')
    rels2 = re.sub(r'<Relationship Id="rId16"[^>]*/>', '', rels)
    if rels2 == rels:
        raise SystemExit('ABORT: rId16 relationship not removed')
    data['word/_rels/document.xml.rels'] = rels2.encode('utf-8')
    names = [n for n in names if n != 'word/media/image9.png']
    data.pop('word/media/image9.png', None)
    print('  removed word/media/image9.png + rId16 relationship')

    if os.path.exists(DST):
        os.remove(DST)
    with zipfile.ZipFile(DST, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])

    print('\nOUTPUT: %s (%.2f MB)' % (DST, os.path.getsize(DST) / 1024.0 / 1024.0))


if __name__ == '__main__':
    main()
