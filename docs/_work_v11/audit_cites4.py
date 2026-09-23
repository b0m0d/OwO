# -*- coding: utf-8 -*-
"""Correctly ordered citation audit from the raw XML, with surrounding text."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

i = xml.find('>参考资料<')
if i < 0:
    i = xml.find('参考资料')
body = xml[:i]
refs = xml[i:]

def plain(seg):
    seg = re.sub(r'<w:tab[^>]*/>', ' ', seg)
    seg = re.sub(r'</w:p>', '\n', seg)
    return re.sub(r'<[^>]+>', '', seg)

bp = plain(body)
print('=== 正文引用标记顺序（含前后文）===')
seq = []
for m in re.finditer(r'\[\d+\](\[\d+\])*', bp):
    nums = re.findall(r'\d+', m.group(0))
    seq.extend(int(n) for n in nums)
    ctx = bp[max(0, m.start() - 46):m.start()].replace('\n', ' ')
    print('   %-10s  ← …%s' % (m.group(0), ctx[-46:]))

order = []
for n in seq:
    if n not in order:
        order.append(n)
print('\n出现序列        : %s' % seq)
print('首次出现顺序    : %s' % order)
print('是否按序        : %s' % ('是' if order == sorted(order) else '否 —— 乱序'))
print('正文未引用的条目: %s' % [n for n in range(1, 31) if n not in order])
print('引用了但列表没有: %s' % [n for n in order if n > 30])

print('\n=== 参考文献条数 ===')
for m in re.finditer(r'\[(\d+)\]\s*([^<\n]{0,60})', plain(refs)):
    pass
items = re.findall(r'\[(\d+)\]', plain(refs))
print('  列表编号: %s' % items)
