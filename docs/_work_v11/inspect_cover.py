# -*- coding: utf-8 -*-
"""Dump: all shd fills, and the cover table's geometry (tblPr/tblpPr/cell widths/paragraph spacing)."""
import collections
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v16.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

print('=== 所有 <w:shd> 填充值统计 ===')
c = collections.Counter(re.findall(r'<w:shd[^>]*w:fill="([^"]+)"[^>]*/>', xml))
for k, v in c.most_common():
    print('   fill=%-10s x%d' % (k, v))
print('   总计 shd 元素: %d' % len(re.findall(r'<w:shd', xml)))

print('\n=== 前 3 个表格的 tblPr ===')
starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
for i in range(3):
    tbl = xml[starts[i]:ends[i]]
    pr = re.search(r'<w:tblPr>.*?</w:tblPr>', tbl, re.S)
    grid = re.search(r'<w:tblGrid>.*?</w:tblGrid>', tbl, re.S)
    rows = len(re.findall(r'<w:tr>', tbl))
    print('--- table %d: %d 行 ---' % (i + 1, rows))
    print('  tblPr: %s' % (pr.group(0) if pr else None))
    print('  grid : %s' % (grid.group(0) if grid else None))

print('\n=== 封面表格（第 1 个 tbl）逐单元格内容与段落设置 ===')
tbl = xml[starts[0]:ends[0]]
for ri, tr in enumerate(re.findall(r'<w:tr>.*?</w:tr>', tbl, re.S)):
    for ci, tc in enumerate(re.findall(r'<w:tc>.*?</w:tc>', tr, re.S)):
        txt = ''.join(re.findall(r'<w:t[^>]*>([^<]*)</w:t>', tc))
        tcw = re.search(r'<w:tcW[^>]*/>', tc)
        sp = re.search(r'<w:spacing[^>]*/>', tc)
        ind = re.search(r'<w:ind[^>]*/>', tc)
        print('  r%d c%d %-30r %s\n        %s\n        %s' % (
            ri, ci, txt[:30],
            (tcw.group(0) if tcw else ''),
            (sp.group(0) if sp else ''),
            (ind.group(0) if ind else '')))
