# -*- coding: utf-8 -*-
"""Inspect v22: sections, footers, sectPr, and current TOC geometry."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v22.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
    rels = z.read('word/_rels/document.xml.rels').decode('utf-8')
    footer = z.read('word/footer1.xml').decode('utf-8')
    print('包内文件:', [n for n in z.namelist() if 'footer' in n or 'header' in n])

print('\n=== sectPr 出现次数: %d ===' % len(re.findall(r'<w:sectPr', xml)))
for m in re.finditer(r'<w:sectPr.*?</w:sectPr>|<w:sectPr[^>]*/>', xml, re.S):
    print('  位置 %d: %s' % (m.start(), m.group(0)[:400]))

print('\n=== footer 关系 ===')
for m in re.finditer(r'<Relationship [^>]*footer[^>]*/>', rels):
    print('  ', m.group(0))

print('\n=== footer1.xml 内容 ===')
print(footer[:900])

print('\n=== TOC 表列宽 ===')
toc = list(re.finditer(r'<w:tbl>.*?</w:tbl>', xml, re.S))[1].group(0)
print('  tblGrid:', re.search(r'<w:tblGrid>.*?</w:tblGrid>', toc, re.S).group(0))
print('  tblW   :', re.search(r'<w:tblW[^>]*/>', toc).group(0))
print('  首行各格 tcW:', re.findall(r'<w:tcW [^>]*/>', re.search(r'<w:tr>.*?</w:tr>', toc, re.S).group(0)))
print('  tblCellMar :', re.search(r'<w:tblCellMar>.*?</w:tblCellMar>', toc, re.S).group(0))
