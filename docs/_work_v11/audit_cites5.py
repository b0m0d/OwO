# -*- coding: utf-8 -*-
"""Extract citation markers in body order by walking <w:t> runs."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

m = re.search(r'<w:t[^>]*>\s*参考资料\s*</w:t>', xml)
print('参考资料 文本框位置: %s' % (m.start() if m else 'NOT FOUND'))
if not m:
    raise SystemExit('cannot locate reference list')

body, refs = xml[:m.start()], xml[m.end():]

def texts(seg):
    return re.findall(r'<w:t[^>]*>([^<]*)</w:t>', seg)

bt = texts(body)
print('正文 <w:t> 片段数: %d' % len(bt))

print('\n=== 引用标记（按正文顺序）===')
seq = []
for s in bt:
    found = re.findall(r'\[\d+\]', s)
    if not found:
        continue
    nums = [int(x.strip('[]')) for x in found]
    seq.extend(nums)
    print('   %-12s in %r' % (' '.join(found), s[:66]))

order = []
for n in seq:
    if n not in order:
        order.append(n)
print('\n出现序列        : %s' % seq)
print('首次出现顺序    : %s' % order)
print('是否按序        : %s' % ('是' if order == sorted(order) else '否 —— 乱序'))
print('正文未引用      : %s' % [n for n in range(1, 31) if n not in order])

print('\n=== 参考文献列表条目首 40 字 ===')
for s in texts(refs):
    if re.match(r'^\[\d+\]', s.strip()):
        print('   %s' % s.strip()[:78])
