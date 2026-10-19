# -*- coding: utf-8 -*-
"""精确定位：表10（风险登记表）是哪几个 <w:tbl>，并逐个报告。"""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v23.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

opens = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
closes = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
print('tbl 开合数: %d / %d' % (len(opens), len(closes)))

for k, (s, e) in enumerate(zip(opens, closes), 1):
    tbl = xml[s:e]
    rows = re.findall(r'<w:tr>.*?</w:tr>', tbl, re.S)
    first = ''.join(re.findall(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', rows[0]))[:46] if rows else ''
    print('表%-3d 行数=%-3d 首行=%r' % (k, len(rows), first))

print('\n=== 含“风险登记表”的表 ===')
for k, (s, e) in enumerate(zip(opens, closes), 1):
    tbl = xml[s:e]
    if '风险登记表' in tbl or '输入系统信任风险' in tbl:
        rows = re.findall(r'<w:tr>.*?</w:tr>', tbl, re.S)
        print('  表%d 行数=%d' % (k, len(rows)))
        for r, tr in enumerate(rows):
            txt = [''.join(re.findall(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', c))
                   for c in re.findall(r'<w:tc>.*?</w:tc>', tr, re.S)]
            print('    r%-2d %s' % (r, txt))
