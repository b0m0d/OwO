# -*- coding: utf-8 -*-
"""打印 7.5 节前后的块顺序，确定正文段落移到表格后的正确位置。"""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')
body = re.search(r'<w:body>(.*)</w:body>', xml, re.S).group(1)

blocks = []
for m in re.finditer(r'<w:tbl>.*?</w:tbl>|<w:p(?: [^>]*)?>.*?</w:p>', body, re.S):
    seg = m.group(0)
    txt = ''.join(x.group(1) for x in
                  re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', seg))
    blocks.append((('TBL' if seg.startswith('<w:tbl') else 'P'), txt, m.start(), m.end()))

for k in range(168, 180):
    kind, txt, s, e = blocks[k]
    print('[%d] %-4s 长度=%-5d %s' % (k, kind, len(txt), txt[:88] or '(空)'))
