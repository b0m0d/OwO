# -*- coding: utf-8 -*-
"""Find where the body/reference split goes wrong and how [25][26] is encoded."""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v17.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

MARKER_RE = re.compile(r'(<w:t[^>]*>)((?:\[\d+\])+)(</w:t>)')
print('整篇 MARKER_RE 命中: %d' % len(MARKER_RE.findall(xml)))

rm = re.search(r'<w:t[^>]*>\s*参考资料\s*</w:t>', xml)
print('参考资料 文本 run 位置: %s' % rm.start())
body = xml[:rm.start()]
print('body 段内 MARKER 命中: %d' % len(MARKER_RE.findall(body)))

# find the raw marker occurrences directly
raw = [m.start() for m in re.finditer(r'\[\d+\]', xml)]
print('\n原文 [n] 命中 %d 处；前 6 处位置: %s' % (len(raw), raw[:6]))
print('参考资料位置 %d，其中位于 body 段内的: %d' % (rm.start(), sum(1 for p in raw if p < rm.start())))

print('\n--- [25][26] 原始上下文 ---')
i = xml.find('[25][26]')
print(repr(xml[i - 260:i + 40]))

print('\n--- 一处正文引用（如 [4]）的上下文 ---')
for m in re.finditer(r'\[4\]', xml):
    if m.start() < rm.start():
        print(repr(xml[m.start() - 200:m.start() + 30]))
        break
