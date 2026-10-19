# -*- coding: utf-8 -*-
"""对比 v10 与 v25 中「表7 产品版本、定价与单位经济」的题注/表格先后关系与表格几何。"""
import re
import zipfile

TBL_RE = re.compile(r'<w:tbl>.*?</w:tbl>', re.S)
P_RE = re.compile(r'<w:p(?: [^>]*)?>.*?</w:p>', re.S)

FILES = {
    'v10': r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v10.docx",
    'v25': r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.docx",
}

for label, path in FILES.items():
    try:
        with zipfile.ZipFile(path) as z:
            xml = z.read('word/document.xml').decode('utf-8')
    except Exception as e:
        print('%s: 打不开 (%s)' % (label, e))
        continue

    # 正文块序列，找到 7.5 节标题 -> 表7 题注 -> 表格 的先后
    blocks = []
    for m in re.finditer(r'<w:tbl>.*?</w:tbl>|<w:p(?: [^>]*)?>.*?</w:p>', xml, re.S):
        seg = m.group(0)
        txt = ''.join(x.group(1) for x in
                      re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', seg))
        blocks.append(('TBL' if seg.startswith('<w:tbl') else 'P', txt, m.start(), m.end()))

    idx = next((k for k, b in enumerate(blocks) if '产品版本、定价与单位经济' in b[1]), None)
    print('=== %s ===' % label)
    if idx is None:
        print('  未找到表7 题注')
        continue
    print('  表7 题注序号 %d，前后块:' % idx)
    for k in range(max(0, idx - 4), min(len(blocks), idx + 3)):
        kind, txt, s, e = blocks[k]
        mark = '  <<< 题注' if k == idx else ''
        print('    [%s] %s%s' % (kind, txt[:52] or '(空)', mark))
    # 表格几何
    for k in range(idx, min(len(blocks), idx + 4)):
        if blocks[k][0] == 'TBL':
            tbl = xml[blocks[k][2]:blocks[k][3]]
            w = re.search(r'<w:tblW[^>]*/>', tbl)
            g = re.search(r'<w:tblGrid>.*?</w:tblGrid>', tbl, re.S)
            dy = re.search(r'<w:tblpPr[^>]*/>', tbl)
            print('    表7 tblW=%s' % (w.group(0) if w else '无'))
            print('        grid=%s' % (g.group(0) if g else '无'))
            print('        浮动定位 tblpPr=%s' % (dy.group(0) if dy else '无'))
            break
    print()
