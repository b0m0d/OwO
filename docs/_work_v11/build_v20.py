# -*- coding: utf-8 -*-
"""Build v20 = v19 + 目录页码同步（按 TOC 表格的“标签格 -> 页码格”结构精确定位）。"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v19.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v20.docx"

FIXES = [
    ('9.3 协作与质量机制', 23, 24),
    ('12.3 发展愿景', 28, 27),
    ('参考资料', 28, 27),
]

TC_RE = re.compile(r'<w:tc>.*?</w:tc>', re.S)
TR_RE = re.compile(r'<w:tr>.*?</w:tr>', re.S)


def cell_text(tc):
    return ''.join(m.group(1) for m in
                   re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', tc))


def norm(s):
    return re.sub(r'\s+', '', s)


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}

    xml = data['word/document.xml'].decode('utf-8')

    # TOC 表格 = 第 2 个 tbl
    starts = [m.start() for m in re.finditer(r'<w:tbl>', xml)]
    ends = [m.end() for m in re.finditer(r'</w:tbl>', xml)]
    s, e = starts[1], ends[1]
    toc = xml[s:e]

    # 建立 标签 -> 页码格 的映射（同一行内标签格后紧跟页码格）
    pairs = []
    for tr in TR_RE.finditer(toc):
        tcs = [(m.start(), m.end(), m.group(0)) for m in TC_RE.finditer(tr.group(0))]
        for k in range(len(tcs) - 1):
            lab = cell_text(tcs[k][2])
            num = cell_text(tcs[k + 1][2])
            if lab.strip() and norm(num).isdigit():
                pairs.append((tr.start() + tcs[k + 1][0], tr.start() + tcs[k + 1][1],
                              lab.strip(), num.strip()))
    print('目录“标签-页码”对: %d 组' % len(pairs))

    edits = []
    for entry, old, new in FIXES:
        cand = [p for p in pairs if norm(p[2]) == norm(entry)]
        if len(cand) != 1:
            raise SystemExit('ABORT: %r 匹配 %d 组' % (entry, len(cand)))
        cs, ce, lab, num = cand[0]
        if int(num) != old:
            raise SystemExit('ABORT: %r 页码为 %s，期望 %d' % (entry, num, old))
        cell = toc[cs:ce]
        m = re.search(r'(<w:rPr>.*?</w:rPr>)(<w:t(?: [^>]*)?>)(\d+)(</w:t>)', cell, re.S)
        if not m:
            raise SystemExit('ABORT: %r 页码格结构异常' % entry)
        edits.append((s + cs + m.start(3), s + cs + m.end(3), str(new)))
        print('  %-22s %2d -> %2d' % (entry, old, new))

    for st, en, rep in sorted(edits, key=lambda x: -x[0]):
        xml = xml[:st] + rep + xml[en:]

    data['word/document.xml'] = xml.encode('utf-8')

    if os.path.exists(DST):
        os.remove(DST)
    with zipfile.ZipFile(DST, 'w', zipfile.ZIP_DEFLATED) as z:
        for n in names:
            zi = zipfile.ZipInfo(n, date_time=infos[n].date_time)
            zi.compress_type = infos[n].compress_type
            zi.external_attr = infos[n].external_attr
            z.writestr(zi, data[n])
    print('\nOUTPUT: %s (%.2f MB)' % (DST, os.path.getsize(DST) / 1024.0 / 1024.0))


if __name__ == '__main__':
    main()
