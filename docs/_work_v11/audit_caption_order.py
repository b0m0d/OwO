# -*- coding: utf-8 -*-
"""列出所有「表N 题注 -> 正文段 -> 表格」的倒置情况（题注与表格之间夹了正文）。"""
import re
import zipfile

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v25.docx"
with zipfile.ZipFile(DOCX) as z:
    xml = z.read('word/document.xml').decode('utf-8')

# 顶层块序列（只取 body 直接子级：段落与表格）
body_m = re.search(r'<w:body>(.*)</w:body>', xml, re.S)
body = body_m.group(1)
blocks = []
pos = 0
for m in re.finditer(r'<w:tbl>.*?</w:tbl>|<w:p(?: [^>]*)?>.*?</w:p>', body, re.S):
    seg = m.group(0)
    kind = 'TBL' if seg.startswith('<w:tbl') else 'P'
    txt = ''.join(x.group(1) for x in
                  re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', seg))
    blocks.append({'kind': kind, 'txt': txt, 'seg': seg, 'abs_s': m.start(), 'abs_e': m.end()})

print('顶层块 %d 个' % len(blocks))
print()
print('=== 表格与其题注的相对位置 ===')
for i, b in enumerate(blocks):
    if b['kind'] != 'TBL':
        continue
    # 向上找最近的题注段落（题注可能被正文隔开，最多回看 3 个块）
    cap_idx = None
    for j in range(i - 1, max(-1, i - 4), -1):
        t = blocks[j]['txt'].strip()
        if blocks[j]['kind'] == 'TBL':
            break
        if re.match(r'^表\s*\d+', t):
            cap_idx = j
            break
    label = blocks[i]['txt'][:26]
    if cap_idx is None:
        print('  表格[%d] %-28s 上方未找到题注' % (i, label))
        continue
    between = [k for k in range(cap_idx + 1, i)]
    if between:
        print('  ✘ 倒置：题注[%d] %r' % (cap_idx, blocks[cap_idx]['txt'][:34]))
        for k in between:
            print('       夹在中间 %s: %s' % (blocks[k]['kind'], blocks[k]['txt'][:70] or '(空段)'))
        print('       表格[%d] %s' % (i, label))
    else:
        print('  ✔ 正常：题注[%d] %r 紧接表格[%d]' % (cap_idx, blocks[cap_idx]['txt'][:28], i))
