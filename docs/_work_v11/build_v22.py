# -*- coding: utf-8 -*-
"""v22 = 目录排版优化（字符串级精确编辑）。

  1. 中缝加一条竖线，明确分隔左右两栏：左栏页码格加右边框、右栏条目标签格加左边框
  2. 页码向本条目标签收进 0.25 cm，同时竖线两侧各留 0.16 cm，数字归属一眼可辨
"""
import os
import re
import zipfile

SRC = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v21.docx"
DST = r"T:\创新创业\OwO-master\docs\Cuttle——情境原生智能输入系统-参赛商业计划书-专业视觉精修版-v22.docx"

SEP_COLOR = 'C9CFD4'
SEP_SZ = '6'
GAP = '90'          # 竖线两侧留白 twips
INSET = '140'       # 页码收进 twips
ORDER = ['top', 'start', 'left', 'bottom', 'end', 'right']

TBL_RE = re.compile(r'<w:tbl>.*?</w:tbl>', re.S)
TR_RE = re.compile(r'<w:tr>.*?</w:tr>', re.S)
TC_RE = re.compile(r'<w:tc>.*?</w:tc>', re.S)


def cell_text(tc):
    return ''.join(m.group(1) for m in
                   re.finditer(r'<w:t(?: [^>]*)?>([^<]*)</w:t>', tc)).strip()


def add_border(tc, side):
    m = re.search(r'<w:tcBorders>.*?</w:tcBorders>', tc, re.S)
    if not m:
        return tc, False
    blk = m.group(0)
    if '<w:%s ' % side in blk:
        return tc, False
    elem = '<w:%s w:val="single" w:sz="%s" w:space="%s" w:color="%s"/>' % (
        side, SEP_SZ, GAP, SEP_COLOR)
    idx = ORDER.index(side)
    at = len(blk) - len('</w:tcBorders>')
    for pm in re.finditer(r'<w:(\w+) [^>]*/>', blk):
        if ORDER.index(pm.group(1)) > idx:
            at = pm.start()
            break
    new_blk = blk[:at] + elem + blk[at:]
    return tc[:m.start()] + new_blk + tc[m.end():], True


def add_indent(tc, attr, value):
    """给单元格内每个 <w:ind .../> 增加属性；没有则插入到 pPr 里。"""
    def patched(mm):
        ind = mm.group(0)
        if 'w:%s=' % attr in ind:
            return re.sub(r'w:%s="\d+"' % attr, 'w:%s="%s"' % (attr, value), ind)
        return ind[:-2].rstrip() + ' w:%s="%s"/>' % (attr, value)
    if '<w:ind ' in tc:
        return re.sub(r'<w:ind [^>]*/>', patched, tc)
    return re.sub(r'(<w:pPr>)(?!.*?<w:ind )',
                  lambda m: m.group(1) + '<w:ind w:%s="%s"/>' % (attr, value),
                  tc)


def main():
    with zipfile.ZipFile(SRC) as z:
        names = z.namelist()
        data = {n: z.read(n) for n in names}
        infos = {n: z.getinfo(n) for n in names}
    xml = data['word/document.xml'].decode('utf-8')

    toc_m = list(TBL_RE.finditer(xml))[1]
    toc = toc_m.group(0)

    n_sep = n_inset = 0
    out, pos = [], 0
    for tr in TR_RE.finditer(toc):
        out.append(toc[pos:tr.start()])
        row = tr.group(0)
        cells = list(TC_RE.finditer(row))
        new_row, p = [], 0
        for ci, cm in enumerate(cells):
            new_row.append(row[p:cm.start()])
            cell = cm.group(0)
            txt = cell_text(cell)
            if ci == 2:                       # 右栏条目标签
                cell, ok = add_border(cell, 'left')
                n_sep += ok
                cell = add_indent(cell, 'left', GAP)
            elif ci == 1 and txt.isdigit():   # 左栏页码
                cell, ok = add_border(cell, 'right')
                n_sep += ok
                cell = add_indent(cell, 'right', INSET)
                n_inset += 1
            new_row.append(cell)
            p = cm.end()
        new_row.append(row[p:])
        out.append(''.join(new_row))
        pos = tr.end()
    out.append(toc[pos:])
    toc_new = ''.join(out)

    xml = xml[:toc_m.start()] + toc_new + xml[toc_m.end():]
    data['word/document.xml'] = xml.encode('utf-8')

    print('中缝竖线: %d 处' % n_sep)
    print('页码收进: %d 个' % n_inset)

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
