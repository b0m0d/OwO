# -*- coding: utf-8 -*-
"""Determine the true page of every TOC entry in the rendered v13 PDF."""
import re
import zipfile
import xml.etree.ElementTree as ET

import pymupdf

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.docx"
PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = ET.fromstring(xml).find(W + 'body')
toc = body.findall(W + 'tbl')[1]

entries = []
for tr in toc.findall(W + 'tr'):
    cells = [''.join(t.text or '' for t in tc.iter(W + 't')) for tc in tr.findall(W + 'tc')]
    for i in (0, 2):
        if i < len(cells) and cells[i].strip() and i + 1 < len(cells) and cells[i + 1].strip():
            entries.append((cells[i].strip(), int(cells[i + 1].strip())))

doc = pymupdf.open(PDF)
# page text, excluding the TOC page (index 1) and the cover (index 0 for 项目摘要?)
flat = []
for i, p in enumerate(doc):
    flat.append(re.sub(r'\s+', '', p.get_text()))


def page_of(title, min_skip=3):
    """First page (1-based) after the TOC that contains the title verbatim."""
    needle = re.sub(r'\s+', '', title)
    # headings are rendered with the exact same text
    for i in range(min_skip, len(flat)):
        if needle in flat[i]:
            return i + 1
    return None


print('%-42s %-6s %-6s %s' % ('目录条目', '目录值', '实际', '判定'))
print('-' * 78)
fixes = []
for title, num in entries:
    real = page_of(title)
    if real is None:
        print('%-42s %-6d %-6s 未在正文找到（可能在封面/目录）' % (title, num, '-'))
        continue
    ok = '✔' if real == num else '✗ 需改为 %d' % real
    if real != num:
        fixes.append((title, num, real))
    print('%-42s %-6d %-6d %s' % (title, num, real, ok))

print()
print('需要修正 %d 条:' % len(fixes))
for t, a, b in fixes:
    print('   %-42s %d -> %d' % (t, a, b))
