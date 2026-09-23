# -*- coding: utf-8 -*-
"""List every citation marker in body order, and every 输入法/输入系统 occurrence."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = ET.fromstring(xml).find(W + 'body')

# body text in order, skipping the reference list at the end
paras = []
for el in body:
    if el.tag == W + 'p':
        paras.append(''.join(t.text or '' for t in el.iter(W + 't')))
    elif el.tag == W + 'tbl':
        for p in el.iter(W + 'p'):
            paras.append(''.join(t.text or '' for t in p.iter(W + 't')))

full = '\n'.join(paras)
cut = full.find('参考资料')
bodytext = full[:cut] if cut > 0 else full

print('=== 引用标记按正文出现顺序 ===')
seq = []
for m in re.finditer(r'\[(\d+)\]', bodytext):
    seq.append(int(m.group(1)))
print('  原始序列: %s' % seq)

first_seen = []
for n in seq:
    if n not in first_seen:
        first_seen.append(n)
print('  首次出现顺序: %s' % first_seen)
missing = [i for i in range(1, 31) if i not in first_seen]
print('  正文未引用: %s' % missing)
print('  是否乱序: %s' % ('是' if first_seen != sorted(first_seen) else '否'))

print('\n=== 每个引用所在的句子 ===')
for m in re.finditer(r'[^。；\n]*\[\d+\][^。；\n]*', bodytext):
    s = m.group(0).strip()
    if s:
        print('  ...%s' % s[-90:])

print('\n=== 术语统计（正文，不含参考资料）===')
for term in ('输入法', '输入系统', '输入法产品'):
    print('  %-8s %d 次' % (term, bodytext.count(term)))
print('\n=== "输入系统" 出现的句子 ===')
for m in re.finditer(r'[^。\n]*输入系统[^。\n]*。', bodytext):
    print('  %s' % m.group(0).strip()[:120])
