# -*- coding: utf-8 -*-
"""Independent arithmetic and wording check on v20."""
import re

import docx

DOCX = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v20.docx"
d = docx.Document(DOCX)

t9 = next(t for t in d.tables
          if any('营业收入 基准' in c.text for row in t.rows for c in row.cells))
R = {}
for row in t9.rows:
    key = row.cells[0].text.strip()
    if key and key not in R:
        R[key] = [c.text.strip() for c in row.cells[1:] if c.text.strip()]
print('表9 行数: %d' % len(R))
for k, v in R.items():
    print('   %-22s %s' % (k, v))

print('=== 表9 算术独立复核 ===')
NUM = re.compile(r'\d+\.?\d*')


def f(s):
    m = NUM.search(s)
    return float(m.group(0)) if m else None


cost = [f(p) for p in R['经营成本 保守 / 基准 / 进取'][0].split('/')]
print('成本 保守/基准/进取 : %s' % cost)
for scen in ('保守', '基准', '进取'):
    rev = [f(c) for c in R['营业收入 ' + scen][:3]]
    res = [(-f(c) if '负' in c else f(c)) for c in R['经营结果 ' + scen][:3]]
    calc = [round(rev[i] - cost[i], 1) for i in range(3)]
    ok = all(abs(calc[i] - res[i]) < 0.06 for i in range(3))
    print('  %s  收入%s  结果%s' % (scen, rev, res))
    print('        收入-成本=%s  %s' % (calc, '✔ 与表载一致' if ok else '✘'))
    print('        两年累计 %.1f 万元，三年累计 %.1f 万元' % (
        round(sum(rev[:2]) - sum(cost[:2]), 1), round(sum(rev) - sum(cost), 1)))

print('\n=== 正文引用的数字是否与表一致 ===')
for p in d.paragraphs:
    t = p.text
    if '年均在付个人数' in t or '前两年累计净损益' in t or '新增资金需求' in t:
        print('  · ' + t)

print('\n=== 引用编号顺序 ===')
full = '\n'.join(p.text for p in d.paragraphs)
cut = full.rfind('参考资料')
body = full[:cut]
seq = [int(x) for x in re.findall(r'\[(\d+)\]', body)]
first = []
for n in seq:
    if n not in first:
        first.append(n)
print('  首次出现: %s' % first)
print('  严格递增: %s' % ('✔' if first == sorted(first) else '✘'))
refs = re.findall(r'^\[(\d+)\]\s*(.+)$', full[cut:], re.M)
nums = [int(n) for n, _ in refs]
print('  文献 %d 条，编号 1..%d 连续: %s' % (len(refs), len(refs),
                                        '✔' if nums == list(range(1, len(refs) + 1)) else '✘'))
print('  未引用文献: %s' % (sorted(set(nums) - set(seq)) or '无 ✔'))

print('\n=== 术语与残留 ===')
print('  输入法 %d 次 / 输入系统 %d 次' % (body.count('输入法'), body.count('输入系统')))
for m in re.finditer(r'[^。\n]{0,24}输入法[^。\n]{0,24}', body):
    print('   · %s' % m.group(0))
for bad in ('上一版', '47.4', '600 × 240', '66.6', '审批于', '返回原处', '原处', '原地'):
    c = body.count(bad)
    if c:
        print('   !! 残留 %s x%d' % (bad, c))
print('  （未列出的残留项均为 0）')
