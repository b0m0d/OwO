# -*- coding: utf-8 -*-
"""Locate the F1EFEA shading contexts, the table default style fill, and the cover layout."""
import collections
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v16.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
    styles = z.read('word/styles.xml').decode('utf-8')

print('=== F1EFEA 出现位置（前后各 90 字符）===')
seen = 0
for m in re.finditer(r'w:fill="F1EFEA"', xml):
    seg = xml[max(0, m.start() - 90):m.end() + 40]
    print('  ...%s' % seg.replace('\n', ' '))
    seen += 1
    if seen >= 4:
        break
print('  F1EFEA 总数: %d' % len(re.findall(r'w:fill="F1EFEA"', xml)))

print('\n=== styles.xml 中的表格默认底纹 ===')
for m in re.finditer(r'<w:style [^>]*w:styleId="(TableNormal|TableGrid)"[^>]*>.*?</w:style>', styles, re.S):
    print('  %s' % m.group(0)[:400])

print('\n=== 封面 body 前 14 个顶层块 ===')
W_OPEN = re.compile(r'<w:(p|tbl)[ >]')
depth = 0
i = xml.find('<w:body>')
pos = i + len('<w:body>')
count = 0
while count < 14:
    m = re.search(r'<w:(p|tbl)(?: [^>]*)?>', xml[pos:])
    if not m:
        break
    tag = m.group(1)
    start = pos + m.start()
    if tag == 'tbl':
        end = xml.find('</w:tbl>', start) + len('</w:tbl>')
    else:
        end = xml.find('</w:p>', start) + len('</w:p>')
    seg = xml[start:end]
    txt = ''.join(re.findall(r'<w:t[^>]*>([^<]*)</w:t>', seg))
    sp = re.findall(r'<w:spacing[^>]*/>', seg)
    print('  %-4s %-46r  %s' % (tag, txt[:46], sp[:2]))
    pos = end
    count += 1
