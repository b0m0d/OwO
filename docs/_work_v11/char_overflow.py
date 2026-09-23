# -*- coding: utf-8 -*-
"""Characterize the overflowing lines: what do they end with, and where do they start?"""
import collections

import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\_work_v11\pdf\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v13.pdf"
RIGHT = 595.276 - (2.05 / 2.54 * 72)
LEFT = 2.05 / 2.54 * 72

doc = pymupdf.open(PDF)
ends = collections.Counter()
starts = collections.Counter()
samples = []
for pno in range(doc.page_count):
    page = doc[pno]
    for blk in page.get_text('dict')['blocks']:
        if blk['type'] != 0:
            continue
        for line in blk['lines']:
            txt = ''.join(s['text'] for s in line['spans']).strip()
            if len(txt) < 12:
                continue
            if line['bbox'][2] > RIGHT + 1:
                ends[txt[-1]] += 1
                starts[round(line['bbox'][0], 1)] += 1
                samples.append((pno + 1, round(line['bbox'][2] - RIGHT, 1), txt[-14:]))
print('越界行结尾字符分布:', dict(ends))
print('越界行左边界分布:', dict(starts))
print()
print('样例（页, 超出pt, 行尾）:')
for s in samples[:20]:
    print('   p%-3d +%-5s …%s' % s)
