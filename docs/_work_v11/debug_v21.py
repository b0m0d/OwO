# -*- coding: utf-8 -*-
"""Full parameter dump + rebuilt-size arithmetic for the v21 step."""
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v20.docx"
with zipfile.ZipFile(SRC) as z:
    xml = z.read('word/document.xml').decode('utf-8')

PARA = re.compile(r'<w:p(?: [^>]*)?>.*?</w:p>', re.S)
T_TXT = re.compile(r'<w:t(?: [^>]*)?>([^<]*)</w:t>')

entries = []
for pm in PARA.finditer(xml):
    seg = pm.group(0)
    t = ''.join(m.group(1) for m in T_TXT.finditer(seg)).strip()
    m = re.match(r'\[(\d+)\]\s*(\S.*)$', t, re.S)
    if m and not re.fullmatch(r'(?:\[\d+\])+', m.group(2).strip()):
        entries.append((int(m.group(1)), pm.start(), pm.end()))

heads = [m.start() for m in re.finditer(r'<w:t[^>]*>参考资料</w:t>', xml)]
first_entry = min(s for _, s, _ in entries)
cut_head = [h for h in heads if h < first_entry]
cut = cut_head[-1]

print('len(xml)      = %d' % len(xml))
print('heads         = %s' % heads)
print('first_entry   = %d' % first_entry)
print('cut_head      = %s' % cut_head)
print('cut           = %d' % cut)
print('entries       = %d' % len(entries))

region = xml[cut:]
rel = [(n, s - cut, e - cut) for n, s, e in entries]
print('region len    = %d' % len(region))
print('rel first     = %s' % (rel[0],))
print('rel last      = %s' % (rel[-1],))

out, cursor = [], 0
for n, s, e in sorted(rel, key=lambda x: x[0]):
    out.append(region[cursor:s])
    out.append(region[s:e])
    cursor = e
out.append(region[cursor:])
new_region = ''.join(out)
print('new_region    = %d  (should equal %d, diff %+d)' % (
    len(new_region), len(region), len(new_region) - len(region)))
print('total new     = %d' % (len(xml[:cut]) + len(new_region)))
print('sum of pieces = %d' % sum(len(p) for p in out))
