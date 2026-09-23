# -*- coding: utf-8 -*-
"""Inspect the 3.4 TOC cell runs in the unpacked document to learn the number-run shape."""
import re

P = r"T:\创新创业\OwO-master\docs\_work_v13\word\document.xml"
with open(P, encoding='utf-8') as f:
    xml = f.read()

for label in ('3.4 原位交付机制', '6.4 核心竞争力', '7.3 获客、转化与留存',
              '8.5 知识产权与成果计划', '9.3 协作与质量机制', '6.1 行业机会与目标市场'):
    i = xml.find('>' + label + '<')
    if i < 0:
        print('%-24s NOT FOUND' % label)
        continue
    seg = xml[i:i + 900]
    # find the following <w:t>NUMBER</w:t>
    m = re.search(r'<w:t>(\d{1,2})</w:t>', seg)
    print('%-24s @%-7d next number run value = %s' % (label, i, m.group(1) if m else None))
    if m:
        before = seg[:m.start()]
        tail = before[-260:]
        print('     before number: ...%s' % tail.replace('\n', ' '))
    print()
