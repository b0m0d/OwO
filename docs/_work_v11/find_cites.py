# -*- coding: utf-8 -*-
"""Find how citation markers are actually encoded (br[25]n8)."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

for pat in (r'\[\d+\]', r'\[3\]', r'25', r'输入法'):
    hits = [m.start() for m in re.finditer(pat, xml)]
    print('%-10s 命中 %d 处' % (pat, len(hits)))

print()
for m in list(re.finditer(r'\[\d+\]', xml))[:6]:
    print('上下文: %s' % xml[max(0, m.start() - 120):m.end() + 60].replace('\n', ' '))

print('\n=== 含 25 的片段（前 4 处）===')
for m in list(re.finditer(r'25', xml))[:4]:
    print('  %s' % xml[max(0, m.start() - 100):m.end() + 60].replace('\n', ' '))
