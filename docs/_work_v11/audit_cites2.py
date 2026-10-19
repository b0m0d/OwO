# -*- coding: utf-8 -*-
"""Audit citations and terminology straight from the raw document XML."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

# body only (cut at the reference list)
cut = xml.find('参考资料')
body = xml[:cut] if cut > 0 else xml

print('=== 引用标记（原始 XML 中的 [n]）===')
marks = re.findall(r'<w:t[^>]*>([^<]*\[\d+\][^<]*)</w:t>', body)
seq = []
for s in marks:
    for n in re.findall(r'\[(\d+)\]', s):
        seq.append(int(n))
print('  共 %d 处引用标记' % len(seq))
print('  按出现顺序: %s' % seq)
first = []
for n in seq:
    if n not in first:
        first.append(n)
print('  首次出现顺序: %s' % first)
print('  是否按序: %s' % ('是' if first == sorted(first) else '否 -> 乱序'))

# where is each marker
print('\n=== 引用所在文本片段 ===')
for m in re.finditer(r'<w:t[^>]*>([^<]*\[\d+\][^<]*)</w:t>', body):
    print('   %s' % m.group(1)[-100:])

print('\n=== 术语统计（正文）===')
flat = ''.join(re.findall(r'<w:t[^>]*>([^<]*)</w:t>', body))
for term in ('输入法', '输入系统', '输入法产品', 'Input Method', 'IME'):
    print('  %-14s %d 次' % (term, flat.count(term)))

print('\n=== 含"输入法"的句子 ===')
for m in re.finditer(r'[^。\n]{0,60}输入法[^。\n]{0,60}。', flat):
    print('   %s' % m.group(0))

print('\n=== 含"输入系统"的句子 ===')
for m in re.finditer(r'[^。\n]{0,60}输入系统[^。\n]{0,60}。', flat):
    print('   %s' % m.group(0))
