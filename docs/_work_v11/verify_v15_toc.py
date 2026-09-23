# -*- coding: utf-8 -*-
"""Verify per-row level detection and resulting run formatting in v15's TOC."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v15.docx"
CHAPTER_RE = re.compile(r'^(第[一二三四五六七八九十]+章|项目摘要|参考资料)')

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = ET.fromstring(xml).find(W + 'body')
toc = body.findall(W + 'tbl')[1]

levels = {'left': [], 'right': []}
for ri, tr in enumerate(toc.findall(W + 'tr')):
    cells = tr.findall(W + 'tc')
    texts = [''.join(t.text or '' for t in c.iter(W + 't')).strip() for c in cells]
    for side, idx in (('left', 0), ('right', 2)):
        if idx < len(texts) and texts[idx]:
            lab = texts[idx].replace('\u3000', ' ').strip()
            lvl = 'chapter' if CHAPTER_RE.match(lab) else 'section'
            levels[side].append((ri, lvl, lab, texts[idx + 1] if idx + 1 < len(texts) else ''))
            print('%s r%-3d %-8s %-34s %s' % (side, ri, lvl, lab[:34],
                                               texts[idx + 1] if idx + 1 < len(texts) else ''))

for side in ('left', 'right'):
    ch = sum(1 for _, l, _, _ in levels[side] if l == 'chapter')
    se = sum(1 for _, l, _, _ in levels[side] if l == 'section')
    print('\n%s 列: 章节级 %d，小节级 %d，合计 %d' % (side, ch, se, ch + se))

print('\n=== 抽查 v15 中 3 行的字号 ===')
for ri in (0, 2, 14):
    tr = toc.findall(W + 'tr')[ri]
    for ci, tc in enumerate(tr.findall(W + 'tc')):
        txt = ''.join(t.text or '' for t in tc.iter(W + 't'))
        if not txt.strip():
            continue
        for r in tc.find(W + 'p').findall(W + 'r'):
            rpr = r.find(W + 'rPr')
            if rpr is None:
                continue
            sz = rpr.find(W + 'sz')
            rf = rpr.find(W + 'rFonts')
            b = rpr.find(W + 'b')
            line = tc.find(W + 'p').find(W + 'pPr/' + W + 'spacing')
            print('  r%-3d cell%d %-24r sz=%s ea=%s b=%s line=%s before=%s' % (
                ri, ci, txt[:24], sz.get(W + 'val') if sz is not None else '-',
                rf.get(W + 'eastAsia') if rf is not None else '-',
                (b.get(W + 'val') if b is not None and b.get(W + 'val') else ('on' if b is not None else '-')),
                line.get(W + 'line') if line is not None else '-',
                line.get(W + 'before') if line is not None else '-'))
