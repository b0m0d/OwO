# -*- coding: utf-8 -*-
"""Show the raw XML of the 私有部署 row in table 9 of v17."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

i = xml.find('私有部署（个）')
row_start = xml.rfind('<w:tr>', 0, i)
row_end = xml.find('</w:tr>', i) + len('</w:tr>')
row = xml[row_start:row_end]
print('row length %d' % len(row))
# print each cell compactly
for k, m in enumerate(re.finditer(r'<w:tc>.*?</w:tc>', row, re.S)):
    seg = m.group(0)
    txt = ''.join(re.findall(r'<w:t[^>]*>([^<]*)</w:t>', seg))
    tcw = re.search(r'<w:tcW[^>]*/>', seg)
    print('  cell%d %-24r %s' % (k, txt, tcw.group(0) if tcw else ''))
print()
print('原始片段（第 2、3 个单元格）:')
cells = [m.group(0) for m in re.finditer(r'<w:tc>.*?</w:tc>', row, re.S)]
for k in (1, 2):
    print('--- cell %d ---' % k)
    print(cells[k][:600])
