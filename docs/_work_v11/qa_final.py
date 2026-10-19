# -*- coding: utf-8 -*-
"""Final consistency check on the rendered v13 PDF: TOC page numbers vs real headings,
caption/table pairing, image clipping, footer page numbers."""
import re

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf"
doc = pymupdf.open(PDF)

pages_text = [p.get_text() for p in doc]

print('=' * 78)
print('1. 目录页码 vs 实际页码')
print('=' * 78)
toc = pages_text[1]
entries = re.findall(r'([\u4e00-\u9fffA-Za-z0-9\.\s·、]{2,40}?)\s+(\d{1,2})\b', toc)
check = []
for name, num in entries:
    name = name.strip()
    if len(name) < 3:
        continue
    check.append((name, int(num)))

bad = 0
for name, num in check:
    # normalise spaces for matching
    needle = re.sub(r'\s+', '', name)
    hits = []
    for i, t in enumerate(pages_text):
        flat = re.sub(r'\s+', '', t)
        if needle and needle in flat:
            hits.append(i + 1)
    # the heading itself should be on the listed page (or the TOC page also matches)
    real = [h for h in hits if h != 2]
    if real and num not in real:
        print('  !! %-38s 目录标 p%-3d 实际 %s' % (name, num, real))
        bad += 1
print('  检查 %d 条；不一致 %d 条' % (len(check), bad))

print()
print('=' * 78)
print('2. 图注 / 表注 与对象同页')
print('=' * 78)
cap_re = re.compile(r'^(图|表)(\d+)\s')
for pno, t in enumerate(pages_text):
    page = doc[pno]
    imgs = page.get_image_info()
    for line in t.splitlines():
        line = line.strip()
        m = cap_re.match(line)
        if not m:
            continue
        kind = m.group(1)
        if kind == '图':
            idx = int(m.group(2))
            # figure images (larger than the 2.6cm logo)
            figs = [i['bbox'] for i in imgs if (i['bbox'][2] - i['bbox'][0]) > 200]
            # caption bbox
            rects = page.search_for(line[:20])
            ok = False
            if rects:
                cy = rects[0].y0
                ok = any(f[3] <= cy + 6 for f in figs)
            print('  p%-3d %-46s %s' % (pno + 1, line[:46], '同页有图' if ok else '!! 未见对应图'))

print()
print('=' * 78)
print('3. 图片是否被裁切')
print('=' * 78)
import zipfile
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx"
with zipfile.ZipFile(DOCX) as z:
    xmld = z.read('word/document.xml').decode('utf-8')
src_rects = re.findall(r'<a:srcRect[^>]*/>', xmld)
if src_rects:
    print('  存在 srcRect 裁剪: %s' % src_rects)
else:
    print('  无 srcRect 裁剪（图片完整显示）')
# compare drawn size vs pixel aspect
seen = 0
for pno, page in enumerate(doc):
    for info in page.get_image_info():
        w = info['bbox'][2] - info['bbox'][0]
        h = info['bbox'][3] - info['bbox'][1]
        if w < 200:
            continue
        seen += 1
        px_w, px_h = info['width'], info['height']
        place_ar = w / h
        pix_ar = px_w / px_h
        dev = abs(place_ar - pix_ar) / pix_ar * 100
        flag = '' if dev < 2 else '  <-- 比例偏差 %.1f%%' % dev
        print('  p%-3d %4dx%-4d px -> %.1f x %.1f pt  比例偏差 %.2f%%%s' % (
            pno + 1, px_w, px_h, w, h, dev, flag))
print('  共 %d 张正文图片' % seen)

print()
print('=' * 78)
print('4. 页脚页码连续性')
print('=' * 78)
nums = []
for pno, t in enumerate(pages_text):
    tail = [l.strip() for l in t.strip().splitlines()[-2:]]
    got = [l for l in tail if re.fullmatch(r'\d{1,2}', l)]
    nums.append(int(got[-1]) if got else None)
print('  页脚数字序列: %s' % nums)
exp = list(range(1, doc.page_count + 1))
if nums == exp:
    print('  连续且与页序一致 ✔')
else:
    diff = [(i + 1, a, b) for i, (a, b) in enumerate(zip(nums, exp)) if a != b]
    print('  不一致处: %s' % diff[:10])
