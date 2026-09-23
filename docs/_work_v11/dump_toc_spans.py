# -*- coding: utf-8 -*-
"""Dump colour/size/bbox of every TOC span in the rendered v15 PDF."""
import pymupdf

PDF = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v15.pdf"
page = pymupdf.open(PDF)[1]

for blk in page.get_text('dict')['blocks']:
    if blk['type'] != 0:
        continue
    for l in blk['lines']:
        for s in l['spans']:
            t = s['text'].strip()
            if not t or 'CUTTLE' in t or 'CONTENTS' in t:
                continue
            col = s['color']
            print('x0=%-6.1f y0=%-6.1f sz=%-5.1f color=#%06x font=%-22s %s' % (
                s['bbox'][0], s['bbox'][1], s['size'], col, s['font'][:22], t[:28]))
