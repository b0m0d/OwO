# -*- coding: utf-8 -*-
"""Compare the freshly built v21 against v20 to find where it grew."""
import re
import zipfile

V20 = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v20.docx"
V21 = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v21.docx"


def load(p):
    with zipfile.ZipFile(p) as z:
        return {n: z.read(n) for n in z.namelist()}


a, b = load(V20), load(V21)
print('条目数 v20=%d v21=%d' % (len(a), len(b)))
diff = [n for n in a if a[n] != b.get(n)]
print('内容不同的条目: %s' % diff)

xa = a['word/document.xml'].decode('utf-8')
xb = b['word/document.xml'].decode('utf-8')
print('v20 len=%d  v21 len=%d' % (len(xa), len(xb)))

# find first divergence
i = 0
while i < min(len(xa), len(xb)) and xa[i] == xb[i]:
    i += 1
print('首个不同字符位置: %d' % i)
print('v20 上下文: %r' % xa[max(0, i - 120):i + 200])
print()
print('v21 上下文: %r' % xb[max(0, i - 120):i + 200])
