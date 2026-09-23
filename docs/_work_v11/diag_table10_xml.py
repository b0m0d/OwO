# -*- coding: utf-8 -*-
"""检查表10（风险登记表）在 v23 里的原始 XML：行高、单元格宽度、边框。"""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v23.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

i = xml.find('项目风险登记表')
tbl_s = xml.rfind('<w:tbl>', 0, i)
tbl_e = xml.find('</w:tbl>', i) + len('</w:tbl>')
tbl = xml[tbl_s:tbl_e]

print('表10 长度 %d 字符' % len(tbl))
print('tblPr:', re.search(r'<w:tblPr>.*?</w:tblPr>', tbl, re.S).group(0))
print('tblGrid:', re.search(r'<w:tblGrid>.*?</w:tblGrid>', tbl, re.S).group(0))

rows = re.findall(r'<w:tr>.*?</w:tr>', tbl, re.S)
print('\n行数: %d' % len(rows))
for k, tr in enumerate(rows):
    trpr = re.search(r'<w:trPr>.*?</w:trPr>', tr, re.S)
    cells = re.findall(r'<w:tc>.*?</w:tc>', tr, re.S)
    txt = [''.join(re.findall(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', c)) for c in cells]
    widths = [re.search(r'<w:tcW[^>]*/>', c).group(0) if '<w:tcW' in c else '无' for c in cells]
    print('  r%-2d trPr=%s' % (k, trpr.group(0) if trpr else '无'))
    print('       文本=%s' % txt)
    print('       宽=%s' % widths)

print('\n=== 第 1 行（输入系统信任风险）完整 XML ===')
print(rows[1][:1500] if len(rows) > 1 else '无')
