# -*- coding: utf-8 -*-
"""Dump drawing extent sizes per image rId in v10 docx."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v10.docx"

EMU_PER_CM = 360000

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

# find each <w:drawing> block, extract r:embed and wp:extent cx/cy
for m in re.finditer(r'<w:drawing>.*?</w:drawing>', xml, re.S):
    block = m.group(0)
    rid = re.search(r'r:embed="(rId\d+)"', block)
    cx = re.search(r'<wp:extent cx="(\d+)" cy="(\d+)"', block)
    if not rid or not cx:
        continue
    cx_v, cy_v = int(cx.group(1)), int(cx.group(2))
    print('%-6s  cx=%9d (%.2f cm)  cy=%9d (%.2f cm)  ratio=%.4f' % (
        rid.group(1), cx_v, cx_v / EMU_PER_CM, cy_v, cy_v / EMU_PER_CM, cy_v / cx_v))

print()
print('--- also check for pic:spPr ext / a:ext ---')
for m in re.finditer(r'<a:ext cx="(\d+)" cy="(\d+)"', xml):
    print('a:ext', m.group(1), m.group(2), '(%.2fcm x %.2fcm)' % (int(m.group(1))/EMU_PER_CM, int(m.group(2))/EMU_PER_CM))

# section page width
for m in re.finditer(r'<w:pgSz[^/]*/>', xml):
    print(m.group(0))
for m in re.finditer(r'<w:pgMar[^/]*/>', xml):
    print(m.group(0))
