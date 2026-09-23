# -*- coding: utf-8 -*-
"""Full citation audit: order of first appearance, uncited entries, reference list."""
import re
import zipfile
import xml.etree.ElementTree as ET

W = '{http://schemas.openxmlformats.org/wordprocessingml/2006/main}'
DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"

with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = ET.fromstring(xml).find(W + 'body')

paras = []
for el in body:
    if el.tag == W + 'p':
        paras.append(''.join(t.text or '' for t in el.iter(W + 't')))
    elif el.tag == W + 'tbl':
        for p in el.iter(W + 'p'):
            paras.append(''.join(t.text or '' for t in p.iter(W + 't')))

# locate the reference list start
ref_start = next(i for i, p in enumerate(paras) if p.strip() == '参考资料')
bodytext = '\n'.join(paras[:ref_start])
reflist = [p for p in paras[ref_start + 1:] if p.strip()]

print('=== 正文引用标记的顺序 ===')
seq = []
for m in re.finditer(r'\[(\d+)\]', bodytext):
    seq.append((int(m.group(1)), m.start()))
order = []
for n, _ in seq:
    if n not in order:
        order.append(n)
print('  出现序列: %s' % [n for n, _ in seq])
print('  首次出现顺序: %s' % order)
print('  是否按序: %s' % ('是' if order == sorted(order) else '否 —— 乱序'))

cited = set(order)
allrefs = list(range(1, len(reflist) + 1))
print('\n  参考文献共 %d 条' % len(reflist))
print('  正文未引用的条目: %s' % [n for n in allrefs if n not in cited])

print('\n=== 每个标记所在句子（前 30 字）===')
for m in re.finditer(r'([^。\n]{0,70}\[\d+\](?:\[\d+\])?)', bodytext):
    seg = m.group(1).strip()
    if seg:
        print('   ...%s' % seg[-74:])

print('\n=== 参考文献列表 ===')
for p in reflist:
    print('   %s' % p[:110])
