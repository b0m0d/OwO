# -*- coding: utf-8 -*-
"""Locate the page-number run for a TOC entry by walking table cells."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v19.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

# TOC table span
starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
s, e = starts[1], ends[1]
toc = xml[s:e]
print('TOC 表长度 %d' % len(toc))

for entry in ('协作与质量机制', '发展愿景', '参考资料'):
    hits = [m.start() for m in re.finditer(entry, toc)]
    print('\n=== %s: %d 处 ===' % (entry, len(hits)))
    for h in hits:
        # enclosing tc
        cs = toc.rfind('<w:tc>', 0, h)
        ce = toc.find('</w:tc>', h) + len('</w:tc>')
        nxt = toc[ce:ce + 2000]
        nm = re.search(r'<w:tc>(.*?)</w:tc>', nxt, re.S)
        if nm:
            num = ''.join(re.findall(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', nm.group(1)))
            rpr = re.search(r'<w:rPr>.*?</w:rPr>', nm.group(1), re.S)
            print('   紧随单元格文本 = %r' % num)
            print('   rPr = %s' % (rpr.group(0) if rpr else None))
            i = ce + nm.start(1) + (rpr.start() if rpr else 0)
            print('   rPr 绝对位置 = %d' % i)
